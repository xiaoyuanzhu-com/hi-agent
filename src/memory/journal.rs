//! The lossless raw signal store.
//!
//! Every signal in and out is appended to its scene's day-log,
//! `<data_dir>/memory/raw/<scene_enc>/signals/<YYYY-MM-DD>/log.jsonl` (see
//! [`super::layout`]). One JSON `JournalEntry` per line. The first signal in a
//! scene also writes `scene.json` recording the true id. Reads scan only the
//! day-folders a query's time window touches; compaction and indexing are
//! deferred.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::types::{JournalEntry, Scene};

use super::layout;

#[derive(Clone)]
pub struct Journal {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    /// Serializes all appends so concurrent scenes never interleave a line.
    write_lock: Mutex<()>,
}

/// Per-scene sidecar recording the true (un-encoded) scene id, written once when
/// a scene first journals. The directory name is a lossy percent-encoding; this
/// is the authoritative id.
#[derive(Serialize)]
struct SceneMeta {
    id: String,
    created_at: DateTime<Utc>,
}

impl Journal {
    pub async fn open(data_dir: PathBuf) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(layout::raw_root(&data_dir)).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                write_lock: Mutex::new(()),
            }),
        })
    }

    /// The data directory this journal writes under — the root for the whole
    /// memory store (`<data_dir>/memory/…`).
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// Append one entry to its scene's day-log, fsynced before returning.
    pub async fn append(&self, entry: JournalEntry) -> anyhow::Result<()> {
        let scene = entry_scene(&entry).clone();
        let ts = entry_ts(&entry);
        let day = layout::day_dir(&self.inner.data_dir, &scene, ts);

        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');

        let _guard = self.inner.write_lock.lock().await;
        tokio::fs::create_dir_all(&day).await?;
        ensure_scene_meta(&self.inner.data_dir, &scene, ts).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(day.join("log.jsonl"))
            .await?;
        file.write_all(&line).await?;
        file.flush().await?;
        file.sync_data().await?;
        Ok(())
    }

    /// The entries at or after `since`, oldest first, capped at the most recent
    /// `limit`. With a scene, only that scene's day-folders from `since`'s day
    /// onward are read; without one, every scene's are.
    pub async fn recent(
        &self,
        scene: Option<&Scene>,
        since: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<Vec<JournalEntry>> {
        let mut entries = Vec::new();
        match scene {
            Some(s) => {
                let signals = layout::signals_dir(&self.inner.data_dir, s);
                read_signals_dir(&signals, since, &mut entries).await?;
            }
            None => read_all_scenes(&self.inner.data_dir, since, &mut entries).await?,
        }
        entries.sort_by_key(entry_ts);
        entries.retain(|e| entry_ts(e) >= since);
        if entries.len() > limit {
            let drop = entries.len() - limit;
            entries.drain(0..drop);
        }
        Ok(entries)
    }
}

pub fn entry_ts(entry: &JournalEntry) -> DateTime<Utc> {
    match entry {
        JournalEntry::SignalIn { ts, .. } | JournalEntry::SignalOut { ts, .. } => *ts,
    }
}

pub fn entry_scene(entry: &JournalEntry) -> &Scene {
    match entry {
        JournalEntry::SignalIn { scene, .. } | JournalEntry::SignalOut { scene, .. } => scene,
    }
}

/// Write `scene.json` if it does not yet exist — a best-effort identity sidecar;
/// the day-log is the actual signal record.
async fn ensure_scene_meta(
    data_dir: &Path,
    scene: &Scene,
    ts: DateTime<Utc>,
) -> anyhow::Result<()> {
    let path = layout::scene_dir(data_dir, scene).join("scene.json");
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(());
    }
    let meta = SceneMeta {
        id: scene.0.clone(),
        created_at: ts,
    };
    tokio::fs::write(&path, serde_json::to_vec_pretty(&meta)?).await?;
    Ok(())
}

/// Read every day-folder of one scene's `signals/` dir whose day is `since`'s or
/// later, appending parsed entries. A missing dir yields nothing.
async fn read_signals_dir(
    signals: &Path,
    since: DateTime<Utc>,
    out: &mut Vec<JournalEntry>,
) -> anyhow::Result<()> {
    let since_day = layout::day_key(since);
    let mut rd = match tokio::fs::read_dir(signals).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut days: Vec<String> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        if let Ok(name) = ent.file_name().into_string() {
            // Day-folders are named YYYY-MM-DD, so a byte compare is a date
            // compare: keep `since`'s day and everything after.
            if name.as_str() >= since_day.as_str() {
                days.push(name);
            }
        }
    }
    days.sort();
    for day in days {
        read_log_into(&signals.join(day).join("log.jsonl"), out).await?;
    }
    Ok(())
}

/// Read each scene under `raw/` through [`read_signals_dir`].
async fn read_all_scenes(
    data_dir: &Path,
    since: DateTime<Utc>,
    out: &mut Vec<JournalEntry>,
) -> anyhow::Result<()> {
    let root = layout::raw_root(data_dir);
    let mut rd = match tokio::fs::read_dir(&root).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    while let Some(ent) = rd.next_entry().await? {
        if ent.file_type().await?.is_dir() {
            read_signals_dir(&ent.path().join("signals"), since, out).await?;
        }
    }
    Ok(())
}

/// Parse one `log.jsonl` into `out`, skipping malformed lines. A missing file is
/// fine — a day-folder may hold only blobs (e.g. un-journaled vision frames).
async fn read_log_into(path: &Path, out: &mut Vec<JournalEntry>) -> anyhow::Result<()> {
    let buf = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for line in buf.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalEntry>(trimmed) {
            Ok(entry) => out.push(entry),
            Err(err) => {
                tracing::warn!(error = %err, line = %trimmed, "skipping malformed journal line");
            }
        }
    }
    Ok(())
}
