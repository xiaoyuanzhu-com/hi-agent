//! Media blob storage — out-of-log bytes for audio, vision frames, etc.
//!
//! A blob lives inside the channel-day folder that holds the channel's log, on a
//! wall-clock grid (see [`super::layout::media_rel_path`]): a one-off capture is
//! `<HH>/<MM>-<SS>.<ext>`, a streamed minute `<HH>/<MM>.<ext>`. The day-log
//! records only the path (relative to the channel-day folder) and metadata; the
//! bytes never enter the JSONL stream (which would blow up readers and bloat
//! snapshots).
//!
//! v0 has no TTL or cleanup — append-only matches the log. A future
//! garbage-collection pass would walk the logs, collect referenced paths, and
//! unlink everything else.

use std::path::Path;

use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;

use crate::types::{Channel, Scene};

use super::layout::{self, MediaSlot};

/// Persist `bytes` inside the scene's channel-day folder for `ts` — the same
/// folder that holds `<channel>.jsonl` — at the grid slot `slot` dictates, and
/// return the path **relative to that folder**. The caller records it in the
/// entry's `media.file`; a reader resolves it as
/// `channel_day_dir(..).join(media.file)`.
pub async fn store_blob(
    data_dir: &Path,
    scene: &Scene,
    channel: Channel,
    ts: DateTime<Utc>,
    slot: MediaSlot,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let dir = layout::channel_day_dir(data_dir, scene, channel, ts);
    let rel = layout::media_rel_path(ts, slot, ext);
    let path = dir.join(&rel);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut f = tokio::fs::File::create(&path).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    f.sync_data().await?;
    Ok(rel)
}
