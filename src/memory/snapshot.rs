//! Snapshot — the per-peer view passed into each routing ACP session.
//!
//! Built by reading the journal slice for a peer, deriving pending approvals
//! from request/decision/expiry pairs, listing pending intents, and accepting
//! the running-workers list from the caller (Step 3/5 own the registry; the
//! snapshot doesn't read it directly).
//!
//! Tunables (constants below):
//! - `RECENT_WINDOW_MIN` — how far back the snapshot reaches (30 minutes).
//! - `RECENT_ENTRY_LIMIT` — cap on entries returned (200).
//!
//! These are intentionally small for v0; the router prompt stays tight.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::memory::Memory;
use crate::memory::journal::entry_ts;
use crate::types::{ApprovalId, Intent, IntentTrigger, JournalEntry, PeerId, WorkerId};

/// Window of journal history surfaced to each routing turn.
pub const RECENT_WINDOW_MIN: i64 = 30;

/// Maximum number of journal entries per snapshot.
pub const RECENT_ENTRY_LIMIT: usize = 200;

/// One running worker, as known to the reactor's worker registry. Step 3/5
/// will populate this from the registry; the snapshot itself is registry-free.
#[derive(Debug, Clone)]
pub struct WorkerSummary {
    pub id: WorkerId,
    pub brief: String,
    pub started: DateTime<Utc>,
}

/// An approval request that has not yet been decided or expired.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub id: ApprovalId,
    pub action: String,
    pub summary: String,
    pub requested: DateTime<Utc>,
}

/// What the router sees alongside the incoming signal.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub peer: PeerId,
    pub recent_entries: Vec<JournalEntry>,
    pub running_workers: Vec<WorkerSummary>,
    pub pending_approvals: Vec<PendingApproval>,
    pub pending_intents: Vec<Intent>,
    pub now: DateTime<Utc>,
}

/// Assemble the routing snapshot for one peer.
pub async fn build_for_peer(
    memory: &Memory,
    peer: &PeerId,
    workers: &[WorkerSummary],
) -> anyhow::Result<Snapshot> {
    let now = Utc::now();
    let since = now - Duration::minutes(RECENT_WINDOW_MIN);

    let recent_entries = memory
        .journal
        .recent(Some(peer), since, RECENT_ENTRY_LIMIT)
        .await?;
    let pending_approvals = derive_pending_approvals(&recent_entries);
    let pending_intents = memory.intents.list_for_peer(peer).await;

    Ok(Snapshot {
        peer: peer.clone(),
        recent_entries,
        running_workers: workers.to_vec(),
        pending_approvals,
        pending_intents,
        now,
    })
}

/// Walk the recent journal: collect every request, mark the ones with a
/// matching decision or expiry, return the rest as pending.
fn derive_pending_approvals(entries: &[JournalEntry]) -> Vec<PendingApproval> {
    let mut requests: HashMap<ApprovalId, PendingApproval> = HashMap::new();
    let mut resolved: HashSet<ApprovalId> = HashSet::new();

    for e in entries {
        match e {
            JournalEntry::ApprovalRequest {
                ts,
                id,
                action,
                summary,
                ..
            } => {
                requests.insert(
                    *id,
                    PendingApproval {
                        id: *id,
                        action: action.clone(),
                        summary: summary.clone(),
                        requested: *ts,
                    },
                );
            }
            JournalEntry::ApprovalDecision { id, .. } | JournalEntry::ApprovalExpired { id, .. } => {
                resolved.insert(*id);
            }
            _ => {}
        }
    }

    let mut out: Vec<PendingApproval> = requests
        .into_iter()
        .filter(|(id, _)| !resolved.contains(id))
        .map(|(_, v)| v)
        .collect();
    out.sort_by_key(|a| a.requested);
    out
}

impl Snapshot {
    /// Compact markdown-ish rendering for inclusion in the router prompt.
    pub fn render_for_prompt(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();

        let _ = writeln!(s, "## Recent (last {} minutes)", RECENT_WINDOW_MIN);
        if self.recent_entries.is_empty() {
            s.push_str("(none)\n");
        } else {
            for e in &self.recent_entries {
                let line = render_entry(e);
                let _ = writeln!(s, "{}", line);
            }
        }

        s.push('\n');
        s.push_str("## Pending approvals\n");
        if self.pending_approvals.is_empty() {
            s.push_str("(none)\n");
        } else {
            for a in &self.pending_approvals {
                let ago = humanize_ago(self.now - a.requested);
                let _ = writeln!(
                    s,
                    "- {} action: {} \u{2014} \"{}\" (requested {})",
                    a.id, a.action, a.summary, ago
                );
            }
        }

        s.push('\n');
        s.push_str("## Pending intents\n");
        if self.pending_intents.is_empty() {
            s.push_str("(none)\n");
        } else {
            for i in &self.pending_intents {
                let when = match &i.when {
                    IntentTrigger::Absolute { ts } => format!("{} absolute", ts.format("%H:%M")),
                };
                let _ = writeln!(s, "- {}: \"{}\"", when, i.what);
            }
        }

        s.push('\n');
        s.push_str("## Running workers\n");
        if self.running_workers.is_empty() {
            s.push_str("(none)\n");
        } else {
            for w in &self.running_workers {
                let _ = writeln!(
                    s,
                    "- {} since {}: \"{}\"",
                    w.id,
                    w.started.format("%H:%M"),
                    w.brief
                );
            }
        }

        s
    }
}

fn render_entry(e: &JournalEntry) -> String {
    let ts = entry_ts(e).format("%H:%M:%S");
    match e {
        JournalEntry::SignalIn {
            channel, from, body, ..
        } => format!("[{}] {}\u{2192}agent on /{}: \"{}\"", ts, from, channel, truncate(body, 200)),
        JournalEntry::SignalOut {
            channel, to, body, ..
        } => format!("[{}] agent\u{2192}{} on /{}: \"{}\"", ts, to, channel, truncate(body, 200)),
        JournalEntry::WorkerSpawn { id, brief, .. } => {
            format!("[{}] worker_spawn {}: \"{}\"", ts, id, truncate(brief, 200))
        }
        JournalEntry::WorkerCancel { id, .. } => format!("[{}] worker_cancel {}", ts, id),
        JournalEntry::WorkerComplete { id, .. } => format!("[{}] worker_complete {}", ts, id),
        JournalEntry::ApprovalRequest {
            id, action, summary, ..
        } => format!(
            "[{}] approval_request {} action={} \"{}\"",
            ts,
            id,
            action,
            truncate(summary, 200)
        ),
        JournalEntry::ApprovalDecision {
            id, allow, reason, ..
        } => format!(
            "[{}] approval_decision {} allow={} reason={:?}",
            ts, id, allow, reason
        ),
        JournalEntry::ApprovalExpired { id, .. } => format!("[{}] approval_expired {}", ts, id),
        JournalEntry::IntentSet { id, what, .. } => {
            format!("[{}] intent_set {}: \"{}\"", ts, id, truncate(what, 200))
        }
        JournalEntry::IntentFired { id, .. } => format!("[{}] intent_fired {}", ts, id),
        JournalEntry::Note { content, .. } => {
            format!("[{}] note: \"{}\"", ts, truncate(content, 200))
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}\u{2026}", truncated)
    }
}

fn humanize_ago(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}
