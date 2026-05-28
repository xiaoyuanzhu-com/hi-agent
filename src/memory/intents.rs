//! Pending-intent store — `intents.jsonl`.
//!
//! Holds the CURRENT pending intents. When an intent fires the heartbeat
//! (Step 8) calls `remove(id)` and journals the firing separately. The
//! journal is the historical record; this file is the live working set.
//!
//! Storage model: rewrite-on-mutate. The active set is loaded into memory at
//! `open()`. `add` / `remove` mutate the in-memory Vec and rewrite the file
//! atomically (write to `intents.jsonl.tmp`, then rename). The active set is
//! small (tens of entries), so a full rewrite per mutation is fine for v0.
//! This trades write throughput for read simplicity and crash safety.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::types::{Intent, IntentId, IntentTrigger, PeerId};

/// Pending-intent store backed by `intents.jsonl`.
#[derive(Clone)]
pub struct IntentStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    path: PathBuf,
    active: Vec<Intent>,
}

impl IntentStore {
    /// Open (or create) the intents file and load active intents into memory.
    pub async fn open(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Touch the file so consumers can rely on it existing.
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;

        let active = load_all(&path).await?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { path, active })),
        })
    }

    /// Add an intent to the active set; rewrites the file.
    pub async fn add(&self, intent: Intent) -> anyhow::Result<()> {
        let mut g = self.inner.lock().await;
        g.active.push(intent);
        rewrite(&g.path, &g.active).await
    }

    /// Drop an intent from the active set by id; rewrites the file.
    /// Silently succeeds if the id isn't present.
    pub async fn remove(&self, id: &IntentId) -> anyhow::Result<()> {
        let mut g = self.inner.lock().await;
        let before = g.active.len();
        g.active.retain(|i| i.id != *id);
        if g.active.len() != before {
            rewrite(&g.path, &g.active).await?;
        }
        Ok(())
    }

    /// Intents whose trigger condition is met at `now`. v0 only supports
    /// `IntentTrigger::Absolute { ts }` — fires when `ts <= now`. Does not
    /// remove them; the caller (heartbeat) removes after acting.
    pub async fn due_intents(&self, now: DateTime<Utc>) -> Vec<Intent> {
        let g = self.inner.lock().await;
        g.active
            .iter()
            .filter(|i| match i.when {
                IntentTrigger::Absolute { ts } => ts <= now,
            })
            .cloned()
            .collect()
    }

    /// All active intents addressed to a peer.
    pub async fn list_for_peer(&self, peer: &PeerId) -> Vec<Intent> {
        let g = self.inner.lock().await;
        g.active.iter().filter(|i| i.peer == *peer).cloned().collect()
    }

    /// All active intents, regardless of peer. Used by the heartbeat snapshot.
    pub async fn list_all(&self) -> Vec<Intent> {
        let g = self.inner.lock().await;
        g.active.clone()
    }
}

async fn load_all(path: &Path) -> anyhow::Result<Vec<Intent>> {
    let mut file = match File::open(path).await {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut buf = String::new();
    file.read_to_string(&mut buf).await?;
    let mut out = Vec::new();
    for line in buf.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Intent>(trimmed) {
            Ok(intent) => out.push(intent),
            Err(err) => {
                tracing::warn!(error = %err, line = %trimmed, "skipping malformed intent line");
            }
        }
    }
    Ok(out)
}

/// Atomic file rewrite: write to `<path>.tmp`, fsync, rename over the target.
async fn rewrite(path: &Path, intents: &[Intent]) -> anyhow::Result<()> {
    let tmp_path = {
        let mut p = path.to_path_buf();
        let name = p.file_name().map(|n| n.to_owned()).unwrap_or_default();
        let mut tmp_name = name;
        tmp_name.push(".tmp");
        p.set_file_name(tmp_name);
        p
    };

    let mut tmp = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)
        .await?;

    for intent in intents {
        let mut line = serde_json::to_vec(intent)?;
        line.push(b'\n');
        tmp.write_all(&line).await?;
    }
    tmp.flush().await?;
    tmp.sync_data().await?;
    drop(tmp);

    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}
