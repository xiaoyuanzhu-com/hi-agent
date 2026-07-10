//! Direct Anthropic Messages call — the **reactor**'s fast, controlled LLM wire.
//!
//! The reactor (hi-agent's always-present conversational voice) does *not* run the
//! agentic claude-CLI loop that the cognition session uses. It makes **one direct
//! Messages call**: `system` is the whole reactor prompt (`speaking.md` + the turn's
//! context), there are **no tools**, and the model is the small/fast slot. Two things
//! fall out of that, and they are the whole reason this wire exists next to the ACP one:
//!
//! - **Fast.** No subprocess, no `node`→CLI→HTTPS double indirection, no per-turn
//!   re-sent tool schema, no agentic tool loop — a single HTTPS request on a small model.
//! - **Speaking-rule conformance.** `speaking.md` is the *real* `system` prompt, not
//!   first-user-turn content sitting underneath the CLI's coding-agent persona. The ACP
//!   path cannot achieve this: [`SessionOpts::system_prompt`](crate::foundation::acp)
//!   is only *prepended* to the first prompt (ACP has no system-prompt slot), so the
//!   CLI's built-in prompt always wins the framing. A direct call has nothing underneath.
//!
//! Auth + routing reuse the same gateway the ACP child uses: a `Authorization: Bearer`
//! token (the broker-minted key in managed mode, the BYOK key otherwise — the Anthropic
//! wire behind every gateway we front authenticates with Bearer, never native
//! `x-api-key`; see `foundation/config::AgentConfig::auth_child_env`), against the
//! configured base **host root** (songguo in managed mode — a transparent proxy that
//! fronts the Anthropic wire at its native `/v1/messages` path — or `api.anthropic.com`).
//!
//! Non-streaming today: a reactor turn is a sentence or two, so a single round-trip on a
//! small model is already far below the agentic path. Token-streaming (fast first word
//! into the sequencer) is the planned follow-up; the seam is [`complete`].

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};

/// Anthropic API host root used when no gateway base is configured (BYOK straight to
/// Anthropic). Managed mode passes the songguo base instead. Mirrors
/// `foundation::config::DEFAULT_AI_API_BASE`, kept local so this vendor stays a leaf.
const DEFAULT_API_BASE: &str = "https://api.anthropic.com";
/// Messages API version, sent as the `anthropic-version` header.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Output cap for a reactor turn — it speaks in a sentence or two (`speaking.md`), so a
/// tight cap keeps a runaway generation from stalling the voice.
const DEFAULT_MAX_TOKENS: u32 = 1024;
/// Whole-call ceiling: the reactor's reply must be quick and the voice must never hang
/// on a wedged upstream. Deliberately far tighter than the agentic path's open budget.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A role in the Messages `messages` array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One conversational turn handed to the Messages call. `system` is passed
/// separately (it is not a turn); these are the alternating user/assistant messages.
#[derive(Clone, Debug)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into() }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, text: text.into() }
    }
}

/// Resolved config for the reactor's Messages wire. Stateless free-function vendor,
/// like the rest of `foundation::vendors`: build a `Config`, then call [`complete`].
pub struct Config {
    client: reqwest::Client,
    bearer_token: String,
    endpoint: String,
    model: String,
}

impl Config {
    /// Build from resolved credentials. `token` is the bearer (`ANTHROPIC_AUTH_TOKEN`
    /// — the broker-minted key in managed mode, the BYOK key otherwise). `base_url` is
    /// the gateway **host root** (songguo, or empty → the Anthropic default); the
    /// Messages path is appended by [`messages_endpoint`]. `model` is the **raw**
    /// fast-model id — pass the small slot, and never the CLI's `[1m]` context-window
    /// suffix, which is a CLI-ism and not a valid API model id.
    pub fn new(token: &str, base_url: Option<&str>, model: &str) -> anyhow::Result<Self> {
        let bearer_token = token.trim().to_string();
        if bearer_token.is_empty() {
            anyhow::bail!("reactor Messages call requires a bearer token");
        }
        let model = model.trim().to_string();
        if model.is_empty() {
            anyhow::bail!("reactor Messages call requires a model");
        }
        let base = base_url.map(str::trim).filter(|b| !b.is_empty()).unwrap_or(DEFAULT_API_BASE);
        let endpoint = messages_endpoint(base);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building reactor Messages HTTP client")?;
        Ok(Self { client, bearer_token, endpoint, model })
    }
}

/// The Messages endpoint for a gateway **host root**. Matches the Anthropic
/// convention (`ANTHROPIC_BASE_URL` + `/v1/messages`): a bare host gets the full
/// version path; a base that already carries `/v1` (some gateways store it that way)
/// gets only `/messages`, so the version segment is never doubled.
fn messages_endpoint(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// Build the Messages request body. Pure (no I/O) so the wire shape is unit-testable
/// without a network call.
fn build_request(cfg: &Config, system: &str, messages: &[Turn], max_tokens: u32) -> Value {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|t| json!({ "role": t.role.as_str(), "content": t.text }))
        .collect();
    json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": msgs,
        "stream": false,
    })
}

/// One non-streaming Messages completion. `system` is the whole reactor prompt
/// (`speaking.md` + the turn's context) — a real system prompt, unlike the ACP path
/// where it could only prefix the first user turn. `messages` is the conversation.
/// Returns the assistant's concatenated text (the words to speak), or an error.
pub async fn complete(cfg: &Config, system: &str, messages: &[Turn]) -> anyhow::Result<String> {
    let body = build_request(cfg, system, messages, DEFAULT_MAX_TOKENS);

    let resp = cfg
        .client
        .post(&cfg.endpoint)
        .bearer_auth(&cfg.bearer_token)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&body)
        .send()
        .await
        .context("reactor Messages request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("reading reactor Messages response")?;
    if !status.is_success() {
        anyhow::bail!("reactor Messages HTTP {status}: {text}");
    }

    let parsed: MessagesReply = serde_json::from_str(&text)
        .with_context(|| format!("parsing reactor Messages response: {text}"))?;
    parsed
        .text()
        .ok_or_else(|| anyhow::anyhow!("reactor Messages returned no text content"))
}

/// Minimal view of the Messages reply — the reactor needs only the assistant text,
/// which arrives in `content[]` as `text` blocks. With no tools registered no
/// `tool_use` blocks come back; any non-text block (e.g. `thinking`) is skipped.
#[derive(Debug, Deserialize)]
struct MessagesReply {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

impl MessagesReply {
    fn text(&self) -> Option<String> {
        let mut acc = String::new();
        for block in &self.content {
            if block.kind == "text"
                && let Some(t) = &block.text
            {
                acc.push_str(t);
            }
        }
        let acc = acc.trim().to_string();
        if acc.is_empty() { None } else { Some(acc) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            client: reqwest::Client::new(),
            bearer_token: "test-token".to_string(),
            endpoint: messages_endpoint(DEFAULT_API_BASE),
            model: "claude-haiku-4-5-20251001".to_string(),
        }
    }

    #[test]
    fn endpoint_appends_version_path_to_a_bare_host() {
        assert_eq!(
            messages_endpoint("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        // Trailing slash is trimmed before the path is appended.
        assert_eq!(
            messages_endpoint("https://songguo.xiaoyuanzhu.com/"),
            "https://songguo.xiaoyuanzhu.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_never_doubles_the_version_segment() {
        assert_eq!(messages_endpoint("https://gw.example/v1"), "https://gw.example/v1/messages");
        assert_eq!(
            messages_endpoint("https://gw.example/v1/messages"),
            "https://gw.example/v1/messages"
        );
    }

    #[test]
    fn new_rejects_empty_token_or_model() {
        assert!(Config::new("  ", Some("https://x"), "m").is_err());
        assert!(Config::new("t", Some("https://x"), "   ").is_err());
    }

    #[test]
    fn new_defaults_the_base_to_anthropic() {
        let cfg = Config::new("t", None, "m").unwrap();
        assert_eq!(cfg.endpoint, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn build_request_carries_system_messages_and_no_stream() {
        let msgs = vec![Turn::user("what time is it?")];
        let body = build_request(&config(), "you are warm and brief", &msgs, 512);
        assert_eq!(body["model"], "claude-haiku-4-5-20251001");
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["system"], "you are warm and brief");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "what time is it?");
    }

    #[test]
    fn build_request_preserves_turn_roles() {
        let msgs = vec![Turn::user("hi"), Turn::assistant("hey"), Turn::user("you there?")];
        let body = build_request(&config(), "sys", &msgs, 256);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], "hey");
        assert_eq!(body["messages"][2]["role"], "user");
    }

    #[test]
    fn parses_and_concatenates_text_blocks() {
        let raw = r#"{
            "content": [
                { "type": "text", "text": "on it" },
                { "type": "text", "text": " — the flights" }
            ],
            "stop_reason": "end_turn"
        }"#;
        let parsed: MessagesReply = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.text().as_deref(), Some("on it — the flights"));
    }

    #[test]
    fn skips_non_text_blocks_and_trims() {
        let raw = r#"{
            "content": [
                { "type": "thinking", "thinking": "hmm" },
                { "type": "text", "text": "  hey there  " }
            ]
        }"#;
        let parsed: MessagesReply = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.text().as_deref(), Some("hey there"));
    }

    #[test]
    fn empty_content_yields_none() {
        let parsed: MessagesReply = serde_json::from_str(r#"{ "content": [] }"#).unwrap();
        assert!(parsed.text().is_none());
    }
}
