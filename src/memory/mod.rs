//! Memory substrate — the lossless raw signal store and snapshot building.
//!
//! `Memory` is a cheap-to-clone handle that holds the journal writer. Server
//! handlers and the reactor share one instance. On-disk, signals live under
//! `<data_dir>/memory/raw/` (see [`layout`]); blobs are co-located with the
//! day-log that references them.

use std::path::Path;

pub mod core;
pub mod episodes;
pub mod facets;
pub mod journal;
pub mod layout;
pub mod media;
pub mod people_vectors;
pub mod snapshot;

pub use self::core::refresh_hot;
pub use journal::Journal;
pub use snapshot::{Snapshot, build_for_scene};

#[derive(Clone)]
pub struct Memory {
    pub journal: Journal,
}

impl Memory {
    pub async fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let journal = Journal::open(data_dir.to_path_buf()).await?;
        Ok(Self { journal })
    }

    /// The data directory backing this store (root of `<data_dir>/memory/…`).
    pub fn data_dir(&self) -> &Path {
        self.journal.data_dir()
    }
}
