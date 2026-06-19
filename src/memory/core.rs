//! The recency digest `hot.md` — a regenerable projection of the most recent
//! episodes. The soul seed references `hot.md` (and `self.md`) by absolute path and
//! the mind Reads them itself, so nothing here is inlined into a prompt. This module
//! only (re)builds `hot.md`: every refresh rewrites it wholesale (regenerate, don't
//! patch).

use super::{Memory, episodes, layout};

/// How many recent episode gists `hot.md` projects.
const HOT_EPISODE_COUNT: usize = 8;

/// Regenerate `hot.md` from the most recent episode gists. Disposable: each call
/// rewrites it wholesale (regenerate, don't patch). A no-op while there are no
/// episodes, so a fresh install doesn't get an empty file.
pub async fn refresh_hot(memory: &Memory) -> anyhow::Result<()> {
    let gists = episodes::recent_gists(memory, None, HOT_EPISODE_COUNT).await?;
    if gists.is_empty() {
        return Ok(());
    }

    let mut s = String::from("# Recent memory\n\n_Projected from your latest episodes; newest last._\n\n");
    for g in &gists {
        let line = g.trim().replace('\n', " ");
        s.push_str("- ");
        s.push_str(&line);
        s.push('\n');
    }

    let path = layout::hot_path(memory.data_dir());
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, s).await?;
    Ok(())
}
