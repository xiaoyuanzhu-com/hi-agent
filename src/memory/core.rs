//! The always-loaded memory core: `self.md` (identity) + `hot.md` (working set).
//!
//! These two files are what every reactor session loads on top of the soul, so
//! the agent carries a persistent sense of who it is and what is current.
//! `self.md` is sticky and hand-/agent-authored; `hot.md` is a **regenerable
//! projection** of the most recent episodes — every refresh rebuilds it, never
//! patches it.

use std::path::Path;

use super::{Memory, episodes, layout};

/// How many recent episode gists `hot.md` projects.
const HOT_EPISODE_COUNT: usize = 8;

/// Read `self.md` + `hot.md` into a prompt block, each under a heading. Empty
/// string when neither exists yet (a fresh install has no core).
pub async fn load_core(memory: &Memory) -> String {
    let dir = memory.data_dir();
    let mut out = String::new();
    if let Some(s) = read_nonempty(&layout::self_path(dir)).await {
        out.push_str("## Who you are\n");
        out.push_str(s.trim());
        out.push_str("\n\n");
    }
    if let Some(h) = read_nonempty(&layout::hot_path(dir)).await {
        out.push_str("## What's been on your mind lately\n");
        out.push_str(h.trim());
        out.push('\n');
    }
    out
}

/// Regenerate `hot.md` from the most recent episode gists. Disposable: each call
/// rewrites it wholesale (regenerate, don't patch). A no-op while there are no
/// episodes, so a fresh install doesn't get an empty file.
pub async fn refresh_hot(memory: &Memory) -> anyhow::Result<()> {
    let gists = episodes::recent_gists(memory, HOT_EPISODE_COUNT).await?;
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

async fn read_nonempty(path: &Path) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) if !s.trim().is_empty() => Some(s),
        _ => None,
    }
}
