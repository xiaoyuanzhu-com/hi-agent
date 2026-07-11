//! The reactor's fast **voice** — the mechanism behind the reactor/cognition split
//! (see `docs/reactor-cognition-split.md`).
//!
//! The reactor is the always-present conversational voice. Unlike cognition (the
//! agentic ACP session that thinks, uses tools, and delegates), the reactor makes a
//! single **direct Anthropic Messages call** per turn: `speaking.md` is its whole
//! system prompt, it has no tools, and it runs on the small/fast model. That is what
//! makes it fast (no subprocess, no agentic loop) and speaking-rule-conformant (the
//! rules are the entire context, not one buried file). This module is the thin glue
//! from the resolved credential to [`crate::foundation::vendors::anthropic_messages`].

use crate::foundation::config::{AgentConfig, LlmWire};
use crate::foundation::vendors::anthropic_messages::{self, Turn};

/// Fallback fast model when the config leaves both the small slot and the main model
/// unset (i.e. it relies on the ACP adapter's own default, which a *direct* Messages
/// call cannot inherit). Without this the reactor would go mute on such a config.
const DEFAULT_FAST_MODEL: &str = "claude-haiku-4-5-20251001";

/// Whether the reactor split (the fast, non-agentic direct-Messages voice) is active.
/// **Split is now the default** — the `HI_AGENT_REACTOR_SPLIT` env flag is retired.
/// Kept as a single seam (rather than deleting the call sites) so the legacy
/// ACP-reactor-session path stays compiled and reachable-in-source until it's removed
/// in a follow-up cleanup; the callers below decide the behaviour off this one bool.
pub(super) fn split_enabled() -> bool {
    true
}

/// Build the Messages config for the reactor's fast voice from the resolved upstream
/// credential. Uses the small/fast model slot (raw id — never the CLI's `[1m]`
/// context-window suffix). Errors when unconfigured or on a non-Claude wire (the
/// Messages API is the Anthropic shape; the Codex wire has no equivalent here yet).
pub(super) fn config_from(cfg: &AgentConfig) -> anyhow::Result<anthropic_messages::Config> {
    if !matches!(cfg.wire, LlmWire::Claude) {
        anyhow::bail!(
            "reactor voice needs the Claude wire (Anthropic Messages); got {:?}",
            cfg.wire
        );
    }
    // Prefer the small/fast slot, then the main model; if the config carries neither
    // (leaving ANTHROPIC_MODEL to the adapter default), fall back to a known fast model
    // rather than failing — a direct call has no adapter default to inherit.
    let model = cfg
        .small
        .clone()
        .or_else(|| cfg.model.clone())
        .unwrap_or_else(|| DEFAULT_FAST_MODEL.to_string());
    tracing::info!(model = %model, base = %cfg.upstream_base_url, "reactor voice: resolved wire");
    anthropic_messages::Config::new(&cfg.upstream_key, Some(cfg.upstream_base_url.as_str()), &model)
}

/// Produce the reactor's spoken words for a turn: one direct Messages call with
/// `speaking.md` as the system prompt (see [`crate::identity::reactor_system_prompt`])
/// and the assembled turn `context` as the user message. Non-streaming for now;
/// token-streaming (fast first word into the sequencer) is the planned follow-up.
pub(super) async fn speak(
    cfg: &anthropic_messages::Config,
    system: &str,
    context: &str,
) -> anyhow::Result<String> {
    anthropic_messages::complete(cfg, system, &[Turn::user(context)]).await
}
