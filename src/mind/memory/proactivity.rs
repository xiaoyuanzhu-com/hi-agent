//! `proactivity.md` — the learned read on speaking up unprompted.
//!
//! A single derived file: which subjects the person welcomes a proactive word on
//! and which they don't, distilled by the reflection ("sleep") pass from how the
//! agent's own unprompted utterances landed. The soul seed references it by absolute
//! path (see [`super::layout::proactivity_path`]) and the agent Reads it before it
//! ever volunteers something, so nothing here is inlined into a prompt. Like
//! [`super::core`]'s `hot.md`, it's a projection — rewritten wholesale by the
//! reflection pass, never patched — and absent until the first unprompted word has
//! been judged.

use std::path::Path;

use super::layout;

/// The current license text, or `None` when nothing has been recorded yet (so the
/// reflection pass starts a fresh file and the reactor treats every topic as
/// unproven).
pub async fn read(data_dir: &Path) -> anyhow::Result<Option<String>> {
    match tokio::fs::read_to_string(layout::proactivity_path(data_dir)).await {
        Ok(s) => Ok(Some(s)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Replace `proactivity.md` with `content` wholesale — the reflection pass
/// regenerates the whole file each time, it never patches.
pub async fn write(data_dir: &Path, content: &str) -> anyhow::Result<()> {
    let path = layout::proactivity_path(data_dir);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, content).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_absent_is_none_then_round_trips_after_write() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).await.unwrap().is_none());
        write(dir.path(), "## family-reminders — welcomed\n").await.unwrap();
        assert_eq!(read(dir.path()).await.unwrap().as_deref(), Some("## family-reminders — welcomed\n"));
        // Regenerate wholesale, not patched.
        write(dir.path(), "## oil-price — muted\n").await.unwrap();
        assert_eq!(read(dir.path()).await.unwrap().as_deref(), Some("## oil-price — muted\n"));
    }
}
