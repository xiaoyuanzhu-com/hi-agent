//! Append-only journal — every signal in/out, worker event, approval, intent.
//!
//! On-disk format: one JSON object per line (JSONL). Encoded as `JournalEntry`.
//! Reads scan the whole file for v0; impl.md notes that journal compaction,
//! significance scoring, and indexing are deferred.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::types::{JournalEntry, PeerId};

/// Append-only writer + recent-history reader for `journal.jsonl`.
///
/// Cloning shares the writer Mutex and the path so handlers and the reactor
/// can both append.
#[derive(Clone)]
pub struct Journal {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    writer: Mutex<File>,
}

impl Journal {
    /// Open (or create) the journal file in append mode.
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

    /// Append a single entry. Serialized JSON + `\n`, then fsync.
    ///
    /// Write errors are returned; callers (the reactor, server handlers) log
    /// and decide whether to fail or continue. For the inbound POST path we
    /// log and accept the signal anyway (see `server::thought`).
    pub async fn append(&self, entry: JournalEntry) -> anyhow::Result<()> {
        let mut buf = serde_json::to_vec(&entry)?;
        buf.push(b'\n');
        let mut writer = self.inner.writer.lock().await;
        writer.write_all(&buf).await?;
        writer.flush().await?;
        writer.sync_data().await?;
        Ok(())
    }

    /// Read all entries from disk in chronological order. Used by `recent` and
    /// `search`; v0 does a full-file read each time.
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

    /// Recent entries newer than `since`, optionally filtered to a peer.
    /// Returns at most `limit` entries in chronological order.
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

    /// Substring search across body / what / content fields. Used by the
    /// `recall` MCP tool (Step 4 will extend).
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<JournalEntry>> {
        let q = query.to_lowercase();
        let all = self.read_all().await?;
        let mut hits: Vec<JournalEntry> = all
            .into_iter()
            .filter(|e| entry_matches(e, &q))
            .collect();
        if hits.len() > limit {
            let drop = hits.len() - limit;
            hits.drain(0..drop);
        }
        Ok(hits)
    }
}

/// Extract the timestamp from any journal variant.
pub fn entry_ts(entry: &JournalEntry) -> DateTime<Utc> {
    match entry {
        JournalEntry::SignalIn { ts, .. }
        | JournalEntry::SignalOut { ts, .. }
        | JournalEntry::WorkerSpawn { ts, .. }
        | JournalEntry::WorkerCancel { ts, .. }
        | JournalEntry::WorkerComplete { ts, .. }
        | JournalEntry::ApprovalRequest { ts, .. }
        | JournalEntry::ApprovalDecision { ts, .. }
        | JournalEntry::ApprovalExpired { ts, .. }
        | JournalEntry::IntentSet { ts, .. }
        | JournalEntry::IntentFired { ts, .. }
        | JournalEntry::Note { ts, .. } => *ts,
    }
}

/// True when the entry involves the given peer (sender, recipient, or owner).
///
/// Entries with no peer association (worker cancel/complete by id only,
/// approval decisions/expiries by id only, intent fires by id only) are
/// surfaced regardless — they typically refer to events the snapshot
/// already includes via their matching request/spawn/set entry.
fn entry_involves_peer(entry: &JournalEntry, peer: &PeerId) -> bool {
    match entry {
        JournalEntry::SignalIn { from, .. } => from == peer,
        JournalEntry::SignalOut { to, .. } => to == peer,
        JournalEntry::WorkerSpawn { peer: p, .. } => p == peer,
        JournalEntry::ApprovalRequest { peer: p, .. } => p == peer,
        JournalEntry::IntentSet { peer: p, .. } => p == peer,
        JournalEntry::Note { peer: Some(p), .. } => p == peer,
        // Bare-id entries: include them; they're correlated by id in the snapshot.
        JournalEntry::WorkerCancel { .. }
        | JournalEntry::WorkerComplete { .. }
        | JournalEntry::ApprovalDecision { .. }
        | JournalEntry::ApprovalExpired { .. }
        | JournalEntry::IntentFired { .. }
        | JournalEntry::Note { peer: None, .. } => true,
    }
}

fn entry_matches(entry: &JournalEntry, q_lower: &str) -> bool {
    let body: Option<&str> = match entry {
        JournalEntry::SignalIn { body, .. } => Some(body.as_str()),
        JournalEntry::SignalOut { body, .. } => Some(body.as_str()),
        JournalEntry::WorkerSpawn { brief, .. } => Some(brief.as_str()),
        JournalEntry::ApprovalRequest { summary, .. } => Some(summary.as_str()),
        JournalEntry::IntentSet { what, .. } => Some(what.as_str()),
        JournalEntry::Note { content, .. } => Some(content.as_str()),
        _ => None,
    };
    match body {
        Some(b) => b.to_lowercase().contains(q_lower),
        None => false,
    }
}
