//! Derived event bundles — `memory/episodes/<date>-<short>/episode.md`.
//!
//! An episode is a coherent event within a scene, recorded as a gist plus
//! frontmatter (the scene and the time window it covers). Episodes are a derived
//! projection over the raw log: regenerable, never the source of truth. Today the
//! only producer is the heartbeat, which persists its conversation briefing as
//! the episode gist; the cursor for "what has been consolidated" is implicitly
//! the newest episode's `to_ts`.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{Memory, layout};
use crate::types::Scene;

/// Persist one episode for `scene`: `gist` under frontmatter recording the scene
/// and the `[from, to]` window it covers. The directory is date-prefixed so a
/// lexical sort is chronological.
pub async fn write_episode(
    memory: &Memory,
    scene: &Scene,
    gist: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> anyhow::Result<()> {
    let id = Uuid::now_v7().simple().to_string();
    let dir = layout::episodes_dir(memory.data_dir()).join(format!("{}-{}", to.format("%Y-%m-%d"), &id[..8]));
    tokio::fs::create_dir_all(&dir).await?;

    let body = format!(
        "---\nscene: \"{}\"\nfrom_ts: {}\nto_ts: {}\nkind: heartbeat-briefing\n---\n\n{}\n",
        scene.0,
        from.to_rfc3339(),
        to.to_rfc3339(),
        gist.trim(),
    );
    tokio::fs::write(dir.join("episode.md"), body).await?;
    Ok(())
}

/// The gists (episode bodies, frontmatter stripped) of the most recent `limit`
/// episodes, oldest first. Empty if there are no episodes yet.
pub async fn recent_gists(memory: &Memory, limit: usize) -> anyhow::Result<Vec<String>> {
    let dir = layout::episodes_dir(memory.data_dir());
    let mut names: Vec<String> = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    while let Some(ent) = rd.next_entry().await? {
        if ent.file_type().await?.is_dir() {
            if let Ok(name) = ent.file_name().into_string() {
                names.push(name);
            }
        }
    }
    names.sort();
    let start = names.len().saturating_sub(limit);

    let mut gists = Vec::new();
    for name in &names[start..] {
        let path = dir.join(name).join("episode.md");
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            gists.push(strip_frontmatter(&content).trim().to_owned());
        }
    }
    Ok(gists)
}

/// Strip a leading `---\n…\n---\n` YAML frontmatter block, returning the body.
fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    match rest.find("\n---\n") {
        Some(i) => &rest[i + "\n---\n".len()..],
        None => content,
    }
}
