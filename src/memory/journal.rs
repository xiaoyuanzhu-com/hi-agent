//! Append-only journal of every signal in/out.
//!
//! On-disk format: one JSON `JournalEntry` per line (JSONL). Reads scan the
//! whole file; impl notes that compaction and indexing are deferred.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::types::{JournalEntry, PeerId};

#[derive(Clone)]
pub struct Journal {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    writer: Mutex<File>,
}

impl Journal {
    pub async fn open(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                writer: Mutex::new(writer),
            }),
        })
    }

    pub async fn append(&self, entry: JournalEntry) -> anyhow::Result<()> {
        let mut buf = serde_json::to_vec(&entry)?;
        buf.push(b'\n');
        let mut writer = self.inner.writer.lock().await;
        writer.write_all(&buf).await?;
        writer.flush().await?;
        writer.sync_data().await?;
        Ok(())
    }

    async fn read_all(&self) -> anyhow::Result<Vec<JournalEntry>> {
        let mut file = match File::open(&self.inner.path).await {
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
            match serde_json::from_str::<JournalEntry>(trimmed) {
                Ok(entry) => out.push(entry),
                Err(err) => {
                    tracing::warn!(error = %err, line = %trimmed, "skipping malformed journal line");
                }
            }
        }
        Ok(out)
    }

    pub async fn recent(
        &self,
        peer: Option<&PeerId>,
        since: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<Vec<JournalEntry>> {
        let all = self.read_all().await?;
        let mut filtered: Vec<JournalEntry> = all
            .into_iter()
            .filter(|e| entry_ts(e) >= since)
            .filter(|e| match peer {
                Some(p) => entry_involves_peer(e, p),
                None => true,
            })
            .collect();
        if filtered.len() > limit {
            let drop = filtered.len() - limit;
            filtered.drain(0..drop);
        }
        Ok(filtered)
    }
}

pub fn entry_ts(entry: &JournalEntry) -> DateTime<Utc> {
    match entry {
        JournalEntry::SignalIn { ts, .. } | JournalEntry::SignalOut { ts, .. } => *ts,
    }
}

fn entry_involves_peer(entry: &JournalEntry, peer: &PeerId) -> bool {
    match entry {
        JournalEntry::SignalIn { from, .. } => from == peer,
        JournalEntry::SignalOut { to, .. } => to == peer,
    }
}
