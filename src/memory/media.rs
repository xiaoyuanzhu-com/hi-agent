//! Media blob storage — out-of-log bytes for audio, vision frames, etc.
//!
//! A blob is co-located with the day-log that references it, named
//! `<channel>-<id>.<ext>` so the file is self-describing and links to its
//! `JournalEntry` by the shared id (see [`super::layout`]). The day-log records
//! only the filename and metadata; the bytes never enter the JSONL stream
//! (which would blow up readers and bloat snapshots).
//!
//! v0 has no TTL or cleanup — append-only matches the log. A future
//! garbage-collection pass would walk the logs, collect referenced filenames,
//! and unlink everything else.

use std::path::Path;

use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;

use crate::types::{Channel, Scene};

use super::layout;

/// Persist `bytes` as `<channel>-<id>.<ext>` in the scene's day-folder for `ts`
/// — the same folder that holds `log.jsonl` — and return the filename. The
/// caller records it in the entry's `media.file`; with the shared `id`, the blob
/// and its log line are linked by name.
pub async fn store_blob(
    data_dir: &Path,
    scene: &Scene,
    ts: DateTime<Utc>,
    channel: Channel,
    id: &str,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let dir = layout::day_dir(data_dir, scene, ts);
    tokio::fs::create_dir_all(&dir).await?;
    let file = format!("{}-{id}.{ext}", channel.as_str());

    let mut f = tokio::fs::File::create(dir.join(&file)).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    f.sync_data().await?;
    Ok(file)
}
