//! Snapshot — the per-scene view passed into a scene's reactor session.

use chrono::{DateTime, Duration, Utc};

use crate::mind::memory::Memory;
use crate::types::{Channel, JournalEntry, Scene};

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
    match e {
        JournalEntry::SignalIn { channel, body, stream, .. } => {
            transcript_line(Speaker::Them, &channel.with_stream(stream.as_deref()), &truncate(body, 200))
        }
        JournalEntry::SignalOut { channel, body, .. } => {
            transcript_line(Speaker::You, channel.as_str(), &truncate(body, 200))
        }
    }
}

/// Who said a line. Rendered as a single leading glyph — `>` for the person,
/// `<` for the agent — so the speaker costs one character, not a repeated word.
/// The glyphs are documented once in the soul (the system prompt), not per line.
#[derive(Clone, Copy)]
pub(crate) enum Speaker {
    /// The person — an inbound signal. Renders as `>`.
    Them,
    /// The agent itself — an outbound signal. Renders as `<`.
    You,
}

/// Format one transcript line for a prompt: `>body` (or `</chan body` off the
/// default text channel). No timestamp — within a 30-minute window the wall
/// clock rarely carries meaning, and the glyph + ordering is the whole signal.
/// The channel is shown only when it isn't text, so an ordinary text exchange
/// reads as a bare back-and-forth. This is the single place the line shape is
/// defined; both the `## Recent` snapshot and the `## New signals` batch render
/// through it, so they stay identical.
pub(crate) fn transcript_line(who: Speaker, chan: &str, body: &str) -> String {
    let mark = match who {
        Speaker::Them => '>',
        Speaker::You => '<',
    };
    if chan == Channel::Text.as_str() {
        format!("{mark}{body}")
    } else {
        format!("{mark}/{chan} {body}")
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
