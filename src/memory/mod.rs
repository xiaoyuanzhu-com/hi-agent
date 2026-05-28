//! Memory substrate — `journal.jsonl` and snapshot building.
//!
//! `Memory` is a cheap-to-clone handle that holds the journal writer. Server
//! handlers and the reactor share one instance.

use std::path::Path;

pub mod journal;
pub mod media;
pub mod snapshot;

pub use journal::Journal;
pub use snapshot::{Snapshot, build_for_peer};

const JOURNAL_FILE: &str = "journal.jsonl";

#[derive(Clone)]
pub struct Memory {
    pub journal: Journal,
}

impl Memory {
    pub async fn open(data_dir: &Path) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(data_dir).await?;
        let journal = Journal::open(data_dir.join(JOURNAL_FILE)).await?;
        Ok(Self { journal })
    }
}
