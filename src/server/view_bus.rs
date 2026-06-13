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
//! The state also survives restarts: every mutation rewrites
//! `<data_dir>/appearance/<scene_enc>.json` (tempfile + rename), and
//! [`ViewBus::load`] reads those back on boot. Module URLs stay valid across
//! restarts because compiled views are content-addressed on disk and never
//! collected (see [`crate::views`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use crate::memory::layout::encode_scene;
use crate::types::{Scene, ViewEnvelope, ViewOp};

/// Cap on active views per scene. Bounds growth if the agent keeps showing
/// distinct ids without dismissing; the oldest (bottom of the z-order) are
/// evicted first.
const MAX_ACTIVE_VIEWS_PER_SCENE: usize = 16;

/// Per-scene appearance state, keyed by scene. Cloneable handle over shared
/// state.
#[derive(Clone)]
pub struct ViewBus {
    inner: Arc<Mutex<HashMap<Scene, SceneAppearance>>>,
    /// `<data_dir>/appearance` — one snapshot file per scene.
    dir: PathBuf,
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
    /// Absolute deadline derived from the envelope's `ttl_ms`, so expiry holds
    /// across restarts and for clients that connect late.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
}

/// On-disk snapshot of one scene's appearance. Carries the true scene id (the
/// filename is its percent-encoding, never decoded back).
#[derive(Serialize, Deserialize)]
struct SceneSnapshot {
    scene: Scene,
    version: u64,
    views: Vec<RetainedView>,
}

/// One active view as delivered to the browser. `ttl_ms` is the *remaining*
/// lifetime at response time.
#[derive(Debug, Clone, Serialize)]
pub struct WireView {
    pub id: String,
    pub module_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

/// A scene's full appearance state — the body of one `GET /api/out/view`
/// response. `views` is in z-order (first = bottom).
#[derive(Debug, Clone, Serialize)]
pub struct ViewState {
    pub version: u64,
    pub views: Vec<WireView>,
}

impl ViewBus {
    /// Open the bus, reloading every scene's persisted appearance from
    /// `<data_dir>/appearance/`. Entries already past their TTL are dropped.
    pub fn load(data_dir: &Path) -> Self {
        let dir = data_dir.join("appearance");
        let mut map = HashMap::new();
        let now = Utc::now();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let snap: SceneSnapshot = match std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                {
                    Some(snap) => snap,
                    None => {
                        tracing::warn!(path = %path.display(), "unreadable appearance snapshot; skipping");
                        continue;
                    }
                };
                let mut state = SceneAppearance {
                    views: snap.views,
                    version: snap.version,
                    notify: Arc::new(Notify::new()),
                };
                evict_expired(&mut state, now);
                map.insert(snap.scene, state);
            }
        }
        Self {
            inner: Arc::new(Mutex::new(map)),
            dir,
        }
    }

    /// Fold one reactor-emitted envelope into the scene's appearance: `show`
    /// upserts and raises to the top of the z-order, `replace` swaps in place
    /// (falling back to show for an unknown id), `dismiss` removes. Bumps the
    /// version, persists the snapshot, and wakes parked readers.
    pub async fn apply(&self, scene: &Scene, envelope: ViewEnvelope) {
        let mut map = self.inner.lock().await;
        let entry = map.entry(scene.clone()).or_default();
        let now = Utc::now();
        evict_expired(entry, now);

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
                    expires_at: envelope
                        .ttl_ms
                        .map(|ms| now + chrono::Duration::milliseconds(ms as i64)),
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
        persist(&self.dir, scene, entry).await;
    }

    /// The scene's appearance, as soon as its version exceeds `since`.
    /// `since: None` returns the present state immediately — even when empty —
    /// so a fresh page knows it is synced; passing the last seen version parks
    /// until the state changes. Expired views are evicted on the way out (a
    /// version bump of its own, so other parked readers learn too).
    pub async fn wait_state(&self, scene: &Scene, since: Option<u64>) -> ViewState {
        loop {
            let mut map = self.inner.lock().await;
            let entry = map.entry(scene.clone()).or_default();
            let now = Utc::now();
            if evict_expired(entry, now) {
                entry.version += 1;
                entry.notify.notify_waiters();
                persist(&self.dir, scene, entry).await;
            }
            if since.is_none_or(|s| entry.version > s) {
                return ViewState {
                    version: entry.version,
                    views: entry
                        .views
                        .iter()
                        .map(|v| WireView {
                            id: v.id.clone(),
                            module_url: v.module_url.clone(),
                            ttl_ms: v
                                .expires_at
                                .map(|t| (t - now).num_milliseconds().max(0) as u64),
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

/// Drop views past their deadline; `true` when anything was removed.
fn evict_expired(entry: &mut SceneAppearance, now: DateTime<Utc>) -> bool {
    let before = entry.views.len();
    entry.views.retain(|v| v.expires_at.is_none_or(|t| t > now));
    entry.views.len() != before
}

/// Rewrite the scene's snapshot file. Tempfile + rename so a crash mid-write
/// never leaves a torn snapshot; failures are logged, not fatal — the live
/// state is authoritative until the next successful write.
async fn persist(dir: &Path, scene: &Scene, entry: &SceneAppearance) {
    let snap = SceneSnapshot {
        scene: scene.clone(),
        version: entry.version,
        views: entry.views.clone(),
    };
    let bytes = match serde_json::to_vec_pretty(&snap) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(scene = %scene, error = %err, "encoding appearance snapshot failed");
            return;
        }
    };
    let enc = encode_scene(scene);
    let path = dir.join(format!("{enc}.json"));
    let tmp = dir.join(format!("{enc}.json.tmp.{}", std::process::id()));
    let result = async {
        tokio::fs::create_dir_all(dir).await?;
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
            ttl_ms: None,
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
                ttl_ms: None,
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
                ttl_ms: None,
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
                ttl_ms: None,
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
    async fn ttl_expires_server_side_and_reports_remaining() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = ViewBus::load(tmp.path());
        let s = scene();
        bus.apply(
            &s,
            ViewEnvelope {
                id: "flash".into(),
                op: ViewOp::Show,
                module_url: Some("/m/f.mjs".into()),
                ttl_ms: Some(60_000),
            },
        )
        .await;
        bus.apply(
            &s,
            ViewEnvelope {
                id: "expired".into(),
                op: ViewOp::Show,
                module_url: Some("/m/e.mjs".into()),
                ttl_ms: Some(0),
            },
        )
        .await;

        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["flash"]);
        let remaining = state.views[0].ttl_ms.unwrap();
        assert!(remaining > 0 && remaining <= 60_000);
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

        // "Restart": a fresh bus over the same data dir.
        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&s, None).await;
        assert_eq!(state.version, version);
        assert_eq!(ids(&state), vec!["a", "b"]);
        assert_eq!(state.views[1].module_url, "/m/b.mjs");
    }

    #[tokio::test]
    async fn reload_drops_expired_and_handles_unsafe_scene_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Scene("alice@phone/1".into());
        {
            let bus = ViewBus::load(tmp.path());
            bus.apply(&s, show("keep", "/m/k.mjs")).await;
            bus.apply(
                &s,
                ViewEnvelope {
                    id: "gone".into(),
                    op: ViewOp::Show,
                    module_url: Some("/m/g.mjs".into()),
                    ttl_ms: Some(0),
                },
            )
            .await;
        }
        let bus = ViewBus::load(tmp.path());
        let state = bus.wait_state(&s, None).await;
        assert_eq!(ids(&state), vec!["keep"]);
    }
}
