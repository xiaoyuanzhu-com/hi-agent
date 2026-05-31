//! Media blob storage — out-of-journal bytes for audio, future images, etc.
//!
//! Lives under `data/media/<kind>/<direction>/<uuidv7>.<ext>`. The journal
//! records the resulting path; the bytes themselves never enter the JSONL
//! stream (which would blow up readers and bloat memory snapshots).
//!
//! v0 has no TTL or cleanup — append-only matches the journal. A future
//! garbage-collection pass would walk the journal, collect referenced paths,
//! and unlink everything else.

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// Whether the blob arrived from a peer (`In`) or was rendered by the agent
/// (`Out`). The two go in sibling folders so a `du -sh data/media/audio/in`
/// answers "how much voice has the peer sent us" without globbing.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// Persist `bytes` under `data_dir/media/audio/<direction>/<uuidv7>.<ext>` and
/// return the relative path. See [`store`].
pub async fn store_audio(
    data_dir: &Path,
    direction: Direction,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    store(data_dir, "audio", direction, ext, bytes).await
}

/// Persist a vision frame under `data_dir/media/image/<direction>/…`.
pub async fn store_image(
    data_dir: &Path,
    direction: Direction,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    store(data_dir, "image", direction, ext, bytes).await
}

/// Persist `bytes` under `data_dir/media/<kind>/<direction>/<uuidv7>.<ext>` and
/// return the relative path (relative to `data_dir`). The relative form is
/// what we journal — absolute paths leak the data_dir into the JSONL.
pub async fn store(
    data_dir: &Path,
    kind: &str,
    direction: Direction,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let id = Uuid::now_v7();
    let rel: PathBuf = ["media", kind, direction.as_str()]
        .iter()
        .collect::<PathBuf>()
        .join(format!("{id}.{ext}"));

    let abs = data_dir.join(&rel);
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::File::create(&abs).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(rel.to_string_lossy().into_owned())
}
