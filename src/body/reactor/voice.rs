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

/// Prototype toggle for the reactor split. **Env-gated and default off**, so the
/// agentic path is byte-for-byte unchanged unless a developer opts in for
/// measurement (`HI_AGENT_REACTOR_SPLIT=1`). To be promoted to a config-store
/// tunable once the split is validated on a real box. (An env flag is deliberately
/// temporary — the project otherwise keeps tunables in the config store.)
pub(super) fn split_enabled() -> bool {
    std::env::var("HI_AGENT_REACTOR_SPLIT")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
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
    let model = cfg
        .small
        .clone()
        .or_else(|| cfg.model.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("reactor voice: no model configured (small and model both unset)")
        })?;
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
