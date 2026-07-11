//! The reactor's fast **voice** — the seam behind the reactor/cognition split
//! (see `docs/reactor-cognition-split.md`).
//!
//! The reactor is the always-present conversational voice. It runs as a **tools-off
//! ACP session** (`SessionRole::ReactorVoice`): `speaking.md` is its whole system
//! prompt and it has no tool surface, so a turn is a single generation with no tool
//! loop — fast — and it speaks via its plain message text (`agent_message_chunk`).
//! Cognition (the agentic worker) does the actual work in parallel. The turn logic
//! lives in [`super::run_reactor_turn`]; this module is now just the split gate.
//!
//! (An earlier iteration made the reactor a *direct* Anthropic Messages HTTP call
//! instead of an ACP session. That hand-rolled request to the songguo gateway hung,
//! and the agentic *loop* — not the ACP adapter — was the real latency, so the reactor
//! moved back onto a tools-off ACP session, reusing the CLI's proven gateway path.)

/// Whether the reactor split is active. **Split is now the default** — the
/// `HI_AGENT_REACTOR_SPLIT` env flag is retired. Kept as a single seam (rather than
/// deleting the call sites) so the legacy agentic reactor-session path stays compiled
/// and reachable-in-source until it's removed in a follow-up cleanup.
pub(super) fn split_enabled() -> bool {
    true
}
