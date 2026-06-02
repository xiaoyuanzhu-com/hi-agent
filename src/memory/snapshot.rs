//! Snapshot — the per-scene view passed into a scene's reactor session.

use chrono::{DateTime, Duration, Utc};

use crate::memory::Memory;
use crate::memory::journal::entry_ts;
use crate::types::{JournalEntry, Scene};

pub const RECENT_WINDOW_MIN: i64 = 30;
pub const RECENT_ENTRY_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub scene: Scene,
    pub recent_entries: Vec<JournalEntry>,
    pub now: DateTime<Utc>,
}

pub async fn build_for_scene(memory: &Memory, scene: &Scene) -> anyhow::Result<Snapshot> {
    let now = Utc::now();
    let since = now - Duration::minutes(RECENT_WINDOW_MIN);
    let recent_entries = memory
        .journal
        .recent(Some(scene), since, RECENT_ENTRY_LIMIT)
        .await?;
    Ok(Snapshot {
        scene: scene.clone(),
        recent_entries,
        now,
    })
}

impl Snapshot {
    pub fn render_for_prompt(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "## Recent (last {} minutes)", RECENT_WINDOW_MIN);
        if self.recent_entries.is_empty() {
            s.push_str("(none)\n");
        } else {
            for e in &self.recent_entries {
                let _ = writeln!(s, "{}", render_entry(e));
            }
        }
        s
    }
}

fn render_entry(e: &JournalEntry) -> String {
    let ts = entry_ts(e).format("%H:%M:%S");
    match e {
        JournalEntry::SignalIn { channel, scene, body, .. } => {
            format!("[{}] {}\u{2192}agent on /{}: \"{}\"", ts, scene, channel, truncate(body, 200))
        }
        JournalEntry::SignalOut { channel, scene, body, .. } => {
            format!("[{}] agent\u{2192}{} on /{}: \"{}\"", ts, scene, channel, truncate(body, 200))
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}\u{2026}", truncated)
    }
}
