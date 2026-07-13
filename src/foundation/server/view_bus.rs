//! Per-scene retained appearance state for `/out/view`, replacing a lossy
//! broadcast.
//!
//! A scene's appearance is *state*, not a stream of utterances: the set of
//! views currently mounted, in z-order. The previous design broadcast each
//! envelope over a `tokio::broadcast`, so a view shown before a client's GET
//! opened — or while a page was refreshing, or before a second device joined —
//! was simply never seen. This bus folds the reactor's show/replace/dismiss
//! envelopes into an ordered map per scene and serves the whole state to any
//! subscriber, so every client in a scene converges on the same screen no
//! matter when it connects.
//!
//! Sync is a versioned long-poll: `wait_state(scene, since)` returns the full
//! state as soon as the scene's version exceeds `since` (immediately when
//! `since` is absent or behind). State is tiny — a few ids and module URLs —
//! so resending it whole kills the missed-delta bug class outright.
//!
//! A view persists until the agent dismisses or replaces it: there is no
//! auto-expiry, lifetime is the reactor's decision.
//!
//! The state also survives restarts: every mutation appends a whole-state
//! snapshot to the memory store at
//! `raw/<scene>/appearance/<date>/appearance-<HHMMSSZ>.json`, and
//! [`ViewBus::load`] restores each scene from its newest snapshot on boot. The
//! snapshots double as the scene's appearance history (the screen as
//! expression, for later reflection). Module URLs stay valid across restarts
//! because compiled views are content-addressed on disk and never collected
//! (see [`crate::mind::views`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use crate::mind::memory::layout;
use crate::types::{Geometry, Scene, ViewEnvelope, ViewOp};

/// Cap on active views per scene. Bounds growth if the agent keeps showing
/// distinct ids without dismissing; the oldest (bottom of the z-order) are
/// evicted first.
const MAX_ACTIVE_VIEWS_PER_SCENE: usize = 16;

/// Per-scene appearance state, keyed by scene. Cloneable handle over shared
/// state.
#[derive(Clone)]
pub struct ViewBus {
    inner: Arc<Mutex<HashMap<Scene, SceneAppearance>>>,
    /// The memory data dir; snapshots live under `raw/<scene>/appearance/`.
    data_dir: PathBuf,
}

#[derive(Default)]
struct SceneAppearance {
    /// Active views in z-order (first = bottom).
    views: Vec<RetainedView>,
    /// Bumped on every state change; the long-poll's `since` compares against it.
    version: u64,
    /// Pulsed whenever `version` bumps so parked readers re-check.
    notify: Arc<Notify>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetainedView {
    id: String,
    module_url: String,
    /// Where/how this view sits on the stage. `#[serde(default)]` is the
    /// back-compat lever: snapshots written before geometry existed reload as
    /// `None` → the host's floor layout.
    #[serde(default)]
    geometry: Option<Geometry>,
}

/// On-disk whole-state snapshot of one scene's appearance at a moment. Carries
/// the true scene id (the path is its percent-encoding, never decoded back) and
/// `as_of` so the history reads as a step-function of what was on screen.
#[derive(Serialize, Deserialize)]
struct SceneSnapshot {
    scene: Scene,
    version: u64,
    as_of: DateTime<Utc>,
    views: Vec<RetainedView>,
}

/// One active view as delivered to the browser.
#[derive(Debug, Clone, Serialize)]
pub struct WireView {
    pub id: String,
    pub module_url: String,
    /// Where/how the view sits; absent = the client's floor layout (centered card).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Geometry>,
}

/// A scene's full appearance state — the body of one `GET /api/out/view`
/// response. `views` is in z-order (first = bottom).
#[derive(Debug, Clone, Serialize)]
pub struct ViewState {
    pub version: u64,
    pub views: Vec<WireView>,
}

impl ViewBus {
    /// Open the bus, restoring each scene's appearance from its newest snapshot
    /// under `raw/<scene>/appearance/`.
    pub fn load(data_dir: &Path) -> Self {
        let mut map = HashMap::new();
        let raw = layout::raw_root(data_dir);
        if let Ok(scenes) = std::fs::read_dir(&raw) {
            for scene_ent in scenes.flatten() {
                let app_dir = scene_ent.path().join("appearance");
                if let Some(snap) = newest_snapshot(&app_dir) {
                    map.insert(
                        snap.scene,
                        SceneAppearance {
                            views: snap.views,
                            version: snap.version,
                            notify: Arc::new(Notify::new()),
                        },
                    );
                }
            }
        }
        Self {
            inner: Arc::new(Mutex::new(map)),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Fold one reactor-emitted envelope into the scene's appearance: `show`
    /// upserts and raises to the top of the z-order, `replace` swaps in place
    /// (falling back to show for an unknown id), `dismiss` removes. Bumps the
    /// version, appends a snapshot, and wakes parked readers.
    pub async fn apply(&self, scene: &Scene, envelope: ViewEnvelope) {
        let mut map = self.inner.lock().await;
        let entry = map.entry(scene.clone()).or_default();

        match envelope.op {
            ViewOp::Dismiss => {
                entry.views.retain(|v| v.id != envelope.id);
            }
            ViewOp::Show | ViewOp::Replace => {
                let Some(module_url) = envelope.module_url else {
                    tracing::warn!(id = %envelope.id, "view envelope without module_url; dropping");
                    return;
                };
                let view = RetainedView {
                    id: envelope.id.clone(),
                    module_url,
                    geometry: envelope.geometry,
                };
                let pos = entry.views.iter().position(|v| v.id == envelope.id);
                match (envelope.op, pos) {
                    (ViewOp::Replace, Some(i)) => entry.views[i] = view,
                    (_, Some(i)) => {
                        entry.views.remove(i);
                        entry.views.push(view);
                    }
                    (_, None) => entry.views.push(view),
                }
                while entry.views.len() > MAX_ACTIVE_VIEWS_PER_SCENE {
                    entry.views.remove(0);
                }
            }
        }
        entry.version += 1;
        entry.notify.notify_waiters();
        persist(&self.data_dir, scene, entry).await;
    }

    /// Clear the scene's appearance — remove all views, back to the default
    /// empty room. A user control: the screen is the agent's presentation, but
    /// the user can reclaim it. Bumps the version and persists the empty
    /// snapshot so every device + a refresh converge on the cleared screen (and
    /// the appearance history records it). No-op when already empty, so it
    /// doesn't churn the version or write a redundant snapshot.
    pub async fn clear(&self, scene: &Scene) {
        let mut map = self.inner.lock().await;
        let entry = map.entry(scene.clone()).or_default();
        if entry.views.is_empty() {
            return;
        }
        entry.views.clear();
        entry.version += 1;
        entry.notify.notify_waiters();
        persist(&self.data_dir, scene, entry).await;
    }

    /// The ids currently on screen for a scene, in z-order (last = top-most). The
    /// reactor reads this into each turn so the agent can *see* its own presentation
    /// surface — what it has shown — instead of guessing ids from the transcript. This
    /// is the read side of the same authoritative state [`apply`](Self::apply) writes,
    /// so a view dismissed last turn is gone from this list the next, giving the agent
    /// the confirmation it otherwise lacks. Empty when nothing is shown.
    pub async fn on_screen(&self, scene: &Scene) -> Vec<String> {
        let map = self.inner.lock().await;
        map.get(scene)
            .map(|a| a.views.iter().map(|v| v.id.clone()).collect())
            .unwrap_or_default()
    }

    /// The scene's appearance, as soon as its version exceeds `since`.
    /// `since: None` returns the present state immediately — even when empty —
    /// so a fresh page knows it is synced; passing the last seen version parks
    /// until the state changes.
    pub async fn wait_state(&self, scene: &Scene, since: Option<u64>) -> ViewState {
        loop {
            let mut map = self.inner.lock().await;
            let entry = map.entry(scene.clone()).or_default();
            if since.is_none_or(|s| entry.version > s) {
                return ViewState {
                    version: entry.version,
                    views: entry
                        .views
                        .iter()
                        .map(|v| WireView {
                            id: v.id.clone(),
                            module_url: v.module_url.clone(),
                            geometry: v.geometry,
                        })
                        .collect(),
                };
            }
            // Enroll on the notify *while still holding the lock* so a
            // `notify_waiters()` between here and the await cannot be lost,
            // then release the lock and park (same pattern as TextBus).
            let notify = entry.notify.clone();
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            drop(map);
            notified.await;
        }
    }
}

/// The newest parseable snapshot under a scene's `appearance/` dir, or `None`.
/// Walks day-folders newest-first, then `appearance-*.json` newest-first, so a
/// torn final write falls back to the prior snapshot.
fn newest_snapshot(appearance_dir: &Path) -> Option<SceneSnapshot> {
    let mut days: Vec<String> = std::fs::read_dir(appearance_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    days.sort();
    for day in days.iter().rev() {
        let day_dir = appearance_dir.join(day);
        let mut files: Vec<String> = std::fs::read_dir(&day_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("appearance-") && n.ends_with(".json"))
            .collect();
        files.sort();
        for f in files.iter().rev() {
            if let Ok(bytes) = std::fs::read(day_dir.join(f)) {
                if let Ok(snap) = serde_json::from_slice::<SceneSnapshot>(&bytes) {
                    return Some(snap);
                }
            }
        }
    }
    None
}

/// Append a whole-state snapshot to `raw/<scene>/appearance/<date>/`. The file
/// is named for the wall-clock second; on the rare same-second collision the
/// second is bumped until free, so no snapshot in the history is overwritten.
/// Tempfile + rename so a crash mid-write never leaves a torn snapshot at a real
/// name. Failures are logged, not fatal — the live state stays authoritative.
async fn persist(data_dir: &Path, scene: &Scene, entry: &SceneAppearance) {
    let now = Utc::now();
    let snap = SceneSnapshot {
        scene: scene.clone(),
        version: entry.version,
        as_of: now,
        views: entry.views.clone(),
    };
    let bytes = match serde_json::to_vec_pretty(&snap) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "encoding appearance snapshot failed");
            return;
        }
    };
    let dir = layout::appearance_day_dir(data_dir, scene, now);
    if let Err(err) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(scene = %scene, error = %err, "creating appearance dir failed");
        return;
    }
    let mut slot = now;
    let path = loop {
        let p = dir.join(format!("appearance-{}.json", slot.format("%H%M%SZ")));
        if !tokio::fs::try_exists(&p).await.unwrap_or(false) {
            break p;
        }
        slot += Duration::seconds(1);
    };
    let tmp = dir.join(format!(".tmp.{}.{}", std::process::id(), slot.format("%H%M%S")));
    let result = async {
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await
    }
    .await;
    if let Err(err) = result {
        tracing::warn!(scene = %scene, path = %path.display(), error = %err, "persisting appearance snapshot failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        Scene("boss".into())
    }

    fn show(id: &str, url: &str) -> ViewEnvelope {
        ViewEnvelope {
            id: id.into(),
            op: ViewOp::Show,
            module_url: Some(url.into()),
            geometry: None,
        }
    }

    fn ids(state: &ViewState) -> Vec<&str> {
        state.views.iter().map(|v| v.id.as_str()).collect()
    }

    #[tokio::test]
    async fn late_subscriber_receives_retained_state() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        bus.apply(&scene(), show("a", "/m/a.mjs")).await;

        // No subscriber existed at apply time — the state is still served.
        let state = bus.wait_state(&scene(), None).await;
        assert_eq!(state.version, 1);
        assert_eq!(ids(&state), vec!["a"]);
        assert_eq!(state.views[0].module_url, "/m/a.mjs");
    }

    #[tokio::test]
    async fn empty_scene_returns_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&scene(), None).await;
        assert_eq!(state.version, 0);
        assert!(state.views.is_empty());
    }

    #[tokio::test]
    async fn since_parks_until_next_change() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        bus.apply(&scene(), show("a", "/m/a.mjs")).await;
        let v = bus.wait_state(&scene(), None).await.version;

        let waiter = {
            let bus = bus.clone();
            tokio::spawn(async move { bus.wait_state(&scene(), Some(v)).await })
        };
        tokio::task::yield_now().await;
        bus.apply(&scene(), show("b", "/m/b.mjs")).await;

        let state = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter should wake")
            .unwrap();
        assert_eq!(state.version, v + 1);
        assert_eq!(ids(&state), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn clear_empties_and_wakes() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        let s = scene();

        // Clearing an already-empty scene is a no-op: version stays at 0.
        bus.clear(&s).await;
        assert_eq!(bus.wait_state(&s, None).await.version, 0);

        bus.apply(&s, show("a", "/m/a.mjs")).await;
        bus.apply(&s, show("b", "/m/b.mjs")).await;
        let v = bus.wait_state(&s, None).await.version;

        // A parked reader wakes on the clear with the empty, version-bumped state.
        let waiter = {
            let bus = bus.clone();
            tokio::spawn(async move { bus.wait_state(&scene(), Some(v)).await })
        };
        tokio::task::yield_now().await;
        bus.clear(&s).await;

        let state = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter should wake")
            .unwrap();
        assert_eq!(state.version, v + 1);
        assert!(state.views.is_empty());
    }

    #[tokio::test]
    async fn show_raises_replace_keeps_position_dismiss_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        let s = scene();
        bus.apply(&s, show("a", "/m/a1.mjs")).await;
        bus.apply(&s, show("b", "/m/b.mjs")).await;
        bus.apply(&s, show("c", "/m/c.mjs")).await;

        // Replace keeps a's z-position at the bottom.
        bus.apply(
            &s,
            ViewEnvelope {
                id: "a".into(),
                op: ViewOp::Replace,
                module_url: Some("/m/a2.mjs".into()),
                geometry: None,
            },
        )
        .await;
        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["a", "b", "c"]);
        assert_eq!(state.views[0].module_url, "/m/a2.mjs");

        // Re-show raises a to the top.
        bus.apply(&s, show("a", "/m/a3.mjs")).await;
        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["b", "c", "a"]);

        // Replace of an unknown id falls back to show (appends on top).
        bus.apply(
            &s,
            ViewEnvelope {
                id: "d".into(),
                op: ViewOp::Replace,
                module_url: Some("/m/d.mjs".into()),
                geometry: None,
            },
        )
        .await;
        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["b", "c", "a", "d"]);

        bus.apply(
            &s,
            ViewEnvelope {
                id: "b".into(),
                op: ViewOp::Dismiss,
                module_url: None,
                geometry: None,
            },
        )
        .await;
        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["c", "a", "d"]);
    }

    #[tokio::test]
    async fn cap_evicts_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        let s = scene();
        for i in 0..=MAX_ACTIVE_VIEWS_PER_SCENE {
            bus.apply(&s, show(&format!("v{i}"), "/m/x.mjs")).await;
        }
        let state = bus.wait_state(&s, None).await;
        assert_eq!(state.views.len(), MAX_ACTIVE_VIEWS_PER_SCENE);
        assert_eq!(state.views[0].id, "v1"); // v0 evicted from the bottom
    }

    #[tokio::test]
    async fn persists_and_reloads_across_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let s = scene();
        let version = {
            let bus = ViewBus::load(tmp.path());
            bus.apply(&s, show("a", "/m/a.mjs")).await;
            bus.apply(&s, show("b", "/m/b.mjs")).await;
            bus.wait_state(&s, None).await.version
        };

        // "Restart": a fresh bus over the same data dir restores the newest snapshot.
        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&s, None).await;
        assert_eq!(state.version, version);
        assert_eq!(ids(&state), vec!["a", "b"]);
        assert_eq!(state.views[1].module_url, "/m/b.mjs");
    }

    #[tokio::test]
    async fn reload_handles_unsafe_scene_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Scene("alice@phone/1".into());
        {
            let bus = ViewBus::load(tmp.path());
            bus.apply(&s, show("keep", "/m/k.mjs")).await;
        }
        // A fresh bus restores the path-unsafe scene from its snapshot.
        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["keep"]);
    }

    #[tokio::test]
    async fn loads_pre_geometry_snapshot_as_floor() {
        // A snapshot written before geometry existed has no `geometry` key in its
        // views; `#[serde(default)]` must reload it as None (the floor layout)
        // rather than failing the whole snapshot parse.
        let tmp = tempfile::tempdir().unwrap();
        let s = scene();
        let dir = layout::appearance_day_dir(tmp.path(), &s, Utc::now());
        std::fs::create_dir_all(&dir).unwrap();
        let old = r#"{"scene":"boss","version":3,"as_of":"2026-06-21T12:00:00Z","views":[{"id":"a","module_url":"/m/a.mjs"}]}"#;
        std::fs::write(dir.join("appearance-120000Z.json"), old).unwrap();

        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&s, None).await;
        assert_eq!(state.version, 3);
        assert_eq!(ids(&state), vec!["a"]);
        assert!(state.views[0].geometry.is_none());
    }

    #[tokio::test]
    async fn apply_carries_geometry_through_wire_and_reload() {
        use crate::types::{Region, SizeClass};
        let tmp = tempfile::tempdir().unwrap();
        let s = scene();
        let geo = Geometry {
            region: Region::Right,
            size: SizeClass::Wide,
            owns_captions: true,
        };

        let version = {
            let bus = ViewBus::load(tmp.path());
            bus.apply(
                &s,
                ViewEnvelope {
                    id: "g".into(),
                    op: ViewOp::Show,
                    module_url: Some("/m/g.mjs".into()),
                    geometry: Some(geo),
                },
            )
            .await;
            let state = bus.wait_state(&s, None).await;
            assert_eq!(state.views[0].geometry, Some(geo));
            state.version
        };

        // Geometry rides the snapshot, so it survives a restart.
        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&s, None).await;
        assert_eq!(state.version, version);
        assert_eq!(state.views[0].geometry, Some(geo));
    }
}
