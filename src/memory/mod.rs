//! Memory substrate — `journal.jsonl`, `intents.jsonl`, and snapshot building.
//!
//! `Memory` is a cheap-to-clone handle that holds the journal writer and the
//! intent store. Server handlers and the reactor share one instance.
//!
//! On-disk files (under `config.data_dir`):
//! - `journal.jsonl` — append-only history (every signal, worker event,
//!   approval, intent fire).
//! - `intents.jsonl` — pending deferred intents (rewritten on add/remove).
//!
//! See `impl.md` § Memory for the data model. Forgetting curve, significance
//! scoring, and indexing are deferred for v0.

use std::path::Path;

pub mod intents;
pub mod journal;
pub mod media;
pub mod snapshot;

pub use intents::IntentStore;
pub use journal::Journal;
pub use snapshot::{PendingApproval, Snapshot, WorkerSummary, build_for_peer};

const JOURNAL_FILE: &str = "journal.jsonl";
const INTENTS_FILE: &str = "intents.jsonl";

/// Memory facade. Clone freely.
#[derive(Clone)]
pub struct Memory {
    pub journal: Journal,
    pub intents: IntentStore,
}

impl Memory {
    /// Open (or create) the memory files under `data_dir`.
    pub async fn open(data_dir: &Path) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(data_dir).await?;
        let journal = Journal::open(data_dir.join(JOURNAL_FILE)).await?;
        let intents = IntentStore::open(data_dir.join(INTENTS_FILE)).await?;
        Ok(Self { journal, intents })
    }
}
