//! The lossless raw signal store.
//!
//! Every signal in and out is appended to its scene's per-channel day-log,
//! `<data_dir>/memory/raw/<scene_enc>/<channel>/<YYYY-MM-DD>/<channel>.jsonl`
//! (see [`super::layout`]). One JSON `JournalEntry` per line. The first signal in
//! a scene also writes `scene.json` recording the true id. A read scans the
//! channel folders a query's time window touches and merges them by `(ts, id)`;
//! compaction and indexing are deferred.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::types::{Channel, JournalEntry, Scene};

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

    /// Append one entry to its scene's per-channel day-log, fsynced before
    /// returning.
    pub async fn append(&self, entry: JournalEntry) -> anyhow::Result<()> {
        let scene = entry_scene(&entry).clone();
        let channel = entry_channel(&entry);
        let ts = entry_ts(&entry);
        let log_path = layout::channel_log_path(&self.inner.data_dir, &scene, channel, ts);

        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');

        let _guard = self.inner.write_lock.lock().await;
        if let Some(dir) = log_path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        ensure_scene_meta(&self.inner.data_dir, &scene, ts).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await?;
        file.write_all(&line).await?;
        file.flush().await?;
        file.sync_data().await?;
        Ok(())
    }

    /// The entries at or after `since`, oldest first, capped at the most recent
    /// `limit`. With a scene, only that scene's channel folders are read; without
    /// one, every scene's are. Entries from all channels are merged by `(ts, id)`.
    pub async fn recent(
        &self,
        scene: Option<&Scene>,
        since: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<Vec<JournalEntry>> {
        let mut entries = Vec::new();
        match scene {
            Some(s) => {
                let dir = layout::scene_dir(&self.inner.data_dir, s);
                read_scene_dir(&dir, since, &mut entries).await?;
            }
            None => read_all_scenes(&self.inner.data_dir, since, &mut entries).await?,
        }
        entries.sort_by(|a, b| (entry_ts(a), entry_id(a)).cmp(&(entry_ts(b), entry_id(b))));
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

pub fn entry_channel(entry: &JournalEntry) -> Channel {
    match entry {
        JournalEntry::SignalIn { channel, .. } | JournalEntry::SignalOut { channel, .. } => *channel,
    }
}

pub fn entry_id(entry: &JournalEntry) -> &str {
    match entry {
        JournalEntry::SignalIn { id, .. } | JournalEntry::SignalOut { id, .. } => id,
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

/// Walk every channel folder of one scene, appending parsed entries. Each
/// immediate sub-directory is a channel (`text/`, `audio/`, …); `files/` is
/// skipped (artifacts, not signals) and `appearance/` self-skips (its day-folders
/// hold state snapshots, not an `appearance.jsonl`). A missing scene dir yields
/// nothing.
async fn read_scene_dir(
    scene_dir: &Path,
    since: DateTime<Utc>,
    out: &mut Vec<JournalEntry>,
) -> anyhow::Result<()> {
    let mut rd = match tokio::fs::read_dir(scene_dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    while let Some(ent) = rd.next_entry().await? {
        if !ent.file_type().await?.is_dir() {
            continue;
        }
        let name = match ent.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name == "files" {
            continue;
        }
        read_channel_dir(&ent.path(), &name, since, out).await?;
    }
    Ok(())
}

/// Read one channel folder's day-shards whose day is `since`'s or later, parsing
/// the `<channel>.jsonl` in each. A channel with no log for a day (e.g.
/// `appearance/`) simply contributes nothing.
async fn read_channel_dir(
    channel_dir: &Path,
    channel_name: &str,
    since: DateTime<Utc>,
    out: &mut Vec<JournalEntry>,
) -> anyhow::Result<()> {
    let since_day = layout::day_key(since);
    let mut rd = match tokio::fs::read_dir(channel_dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let log_file = format!("{channel_name}.jsonl");
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
        read_log_into(&channel_dir.join(day).join(&log_file), out).await?;
    }
    Ok(())
}

/// Walk each scene under `raw/` through [`read_scene_dir`].
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
            read_scene_dir(&ent.path(), since, out).await?;
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
