//! Snapshot — the per-scene view passed into a scene's reactor session.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use crate::mind::memory::Memory;
use crate::types::{Channel, JournalEntry, Scene};

pub const RECENT_WINDOW_MIN: i64 = 30;
pub const RECENT_ENTRY_LIMIT: usize = 200;

/// The durable working set the reactor carries so it is fast but never blind: who it is
/// to this install (`self.md`), its standing duties (`commitments.md`), and what's
/// lately been on its mind (`hot.md`). The reactor is tools-off — it cannot Read these
/// itself the way an agentic session does via [`crate::identity::load_soul`] — so the
/// durable memory is inlined into its prompt. Assembled when a reactor session opens
/// (its warm-up) and retained by the session across the turns that follow; the cost is
/// three small file reads, not a retrieval, so it never spends the turn's latency.
///
/// Every source is optional and read independently: a fresh install has no
/// `commitments.md`, an unreflected store no `hot.md`, an operator who authored nothing
/// no `self.md`. A missing or blank file is skipped, never an error — the reactor
/// degrades to less context, it does not fail a turn. Returns `""` when nothing is
/// present, which the reactor's `join_sections` drops.
pub async fn working_set(data_dir: &Path) -> String {
    use std::fmt::Write as _;

    use crate::identity::{commitments_path, self_path};
    use crate::mind::memory::layout::hot_path;

    let sources = [
        ("Who I am to this person", self_path(data_dir)),
        ("My standing commitments", commitments_path(data_dir)),
        ("Lately on my mind", hot_path(data_dir)),
    ];
    let mut s = String::new();
    for (title, path) in sources {
        if let Ok(body) = tokio::fs::read_to_string(&path).await {
            let body = body.trim();
            if !body.is_empty() {
                let _ = write!(s, "## {title}\n{body}\n\n");
            }
        }
    }
    s.truncate(s.trim_end().len());
    s
}

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
