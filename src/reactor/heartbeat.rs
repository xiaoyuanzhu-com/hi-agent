//! Heartbeat hot-swap — bound the persistent reactor session's growth without
//! the conversation ever seeing a cold restart.
//!
//! A persistent session is a warm, continuous mind, but it also grows without
//! bound: every turn appends to its context. Left alone it eventually rots
//! (early context crowded out) or overflows the model's window. The heartbeat
//! keeps it bounded *invisibly*: once a session has accumulated enough, the
//! next gap between turns is used to (1) ask the live session for a compact
//! self-briefing, (2) open a replacement seeded with that briefing plus the
//! recent journal tail, and (3) hand it back so the loop swaps it in. The
//! conversation experiences continuity, never a cold restart; the journal stays
//! the durable backstop if a swap fails.

use std::sync::Arc;

use crate::acp::{AcpSession, SessionOpts};
use crate::memory::build_for_scene;
use crate::observatory::EventKind;
use crate::types::Scene;

use super::Reactor;

/// Soft ceiling on a session's accumulated prompt+reply characters before the
/// heartbeat swaps it. A coarse proxy for context pressure — we don't see the
/// model's token count, and an over-estimate just swaps a little early, which
/// is harmless (the replacement is seeded). Kept well below a typical model
/// window so the briefing-plus-tail seed always fits with room to grow.
pub(crate) const SWAP_AFTER_CHARS: usize = 48_000;

/// Tracks how much the live session has accumulated since it was opened, so the
/// per-scene loop can decide when to hot-swap. Cheap; lives in that loop.
pub(super) struct ContextBudget {
    chars: usize,
}

impl ContextBudget {
    pub(super) fn new() -> Self {
        Self { chars: 0 }
    }

    /// Fold one completed turn's prompt and reply sizes into the running total.
    pub(super) fn record_turn(&mut self, prompt_chars: usize, reply_chars: usize) {
        self.chars = self
            .chars
            .saturating_add(prompt_chars)
            .saturating_add(reply_chars);
    }

    pub(super) fn should_swap(&self) -> bool {
        self.chars >= SWAP_AFTER_CHARS
    }

    /// Current accumulated chars, mirrored into the observatory for display.
    pub(super) fn chars(&self) -> usize {
        self.chars
    }

    /// Reset after a swap (or after the session is discarded on error).
    pub(super) fn reset(&mut self) {
        self.chars = 0;
    }
}

/// Instruction the live session answers to brief its successor. Framed as an
/// internal request so the model produces a dense briefing, not a spoken reply.
const SUMMARIZE_PROMPT: &str = "[internal request — this is not from the person you're talking \
with, and you are not speaking to anyone; produce no spoken reply] Write a compact briefing of our \
conversation so far for your future self: who you're talking with, what they are working on, \
decisions and facts established, anything still open or promised, and where the current \
thread stands. Be terse and information-dense — this seeds a fresh session that must \
continue the conversation seamlessly. Output only the briefing.";

/// Summarize the live session and open a fresh replacement for `scene`, seeded
/// with that briefing plus the recent journal tail. Runs between turns, so the
/// session is free to take the summarize prompt. On any failure the caller
/// keeps the existing warm session — the swap is best-effort.
pub(super) async fn swap(
    reactor: &Reactor,
    scene: &Scene,
    current: &Arc<AcpSession>,
) -> anyhow::Result<Arc<AcpSession>> {
    // Ask the live session to brief its successor. The reply is captured here
    // and never emitted to the channel or spoken — it only seeds the new session.
    let briefing = {
        let run = current.prompt(SUMMARIZE_PROMPT.to_string()).await?;
        run.wait().await?.text
    };
    let briefing_chars = briefing.chars().count();

    // The verbatim recent tail from the journal — the immediate thread the
    // briefing might compress away. Together they seed the replacement so it
    // continues without a visible seam.
    let tail = build_for_scene(&reactor.inner.memory, scene)
        .await
        .map(|snap| snap.render_for_prompt())
        .unwrap_or_default();

    let seeded_system_prompt = format!(
        "{}\n\n\
         ## Briefing from your earlier conversation\n{}\n\n\
         {}",
        reactor.inner.soul,
        briefing.trim(),
        tail.trim(),
    );

    let fresh = reactor
        .inner
        .agent
        .session(
            scene,
            crate::agent::SessionRole::Reactor,
            None,
            SessionOpts {
                system_prompt: Some(seeded_system_prompt),
                cwd: None,
            },
        )
        .await?;

    reactor
        .inner
        .observatory
        .record(
            scene,
            EventKind::HotSwap {
                old_id: current.id().0.to_string(),
                new_id: fresh.id().0.to_string(),
                briefing_chars,
            },
        )
        .await;

    Ok(Arc::new(fresh))
}
