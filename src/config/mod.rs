//! Dev-managed cognition config: in-repo `config.toml` → child env + settings.json.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

/// Env var holding the upstream LLM credential (kept out of git; loaded via .env).
pub const ENV_UPSTREAM_KEY: &str = "HI_AGENT_UPSTREAM_KEY";
/// Env var overriding the config file path. Defaults to `./config.toml`.
pub const ENV_CONFIG_PATH: &str = "HI_AGENT_CONFIG";

/// Dev-managed cognition parameters. Non-secret fields come from `config.toml`;
/// `upstream_key` is injected from the environment so it never lives in git.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub upstream_base_url: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub max_thinking_tokens: Option<u32>,
    pub upstream_key: String,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    upstream_base_url: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    max_thinking_tokens: Option<u32>,
}

impl AgentConfig {
    /// Load from the path in `HI_AGENT_CONFIG` (default `./config.toml`) and the
    /// `HI_AGENT_UPSTREAM_KEY` env var.
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var(ENV_CONFIG_PATH).unwrap_or_else(|_| "config.toml".to_string());
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config file {path}"))?;
        let key = std::env::var(ENV_UPSTREAM_KEY).unwrap_or_default();
        Self::from_toml_str(&text, key)
    }

    /// Parse config text and attach the upstream key. Errors if the key is empty.
    pub fn from_toml_str(text: &str, upstream_key: String) -> anyhow::Result<Self> {
        if upstream_key.trim().is_empty() {
            anyhow::bail!(
                "{ENV_UPSTREAM_KEY} is empty — set it in the environment or .env"
            );
        }
        let raw: RawConfig = toml::from_str(text).context("parsing config.toml")?;
        Ok(Self {
            upstream_base_url: raw.upstream_base_url,
            model: raw.model,
            effort: raw.effort,
            permission_mode: raw.permission_mode,
            max_thinking_tokens: raw.max_thinking_tokens,
            upstream_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_and_takes_key_from_arg() {
        let toml = r#"
            upstream_base_url = "https://upstream.example/v1"
            model = "claude-opus-4-8"
            effort = "high"
            permission_mode = "acceptEdits"
            max_thinking_tokens = 10000
        "#;
        let cfg = AgentConfig::from_toml_str(toml, "secret-key".to_string()).unwrap();
        assert_eq!(cfg.upstream_base_url, "https://upstream.example/v1");
        assert_eq!(cfg.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(cfg.effort.as_deref(), Some("high"));
        assert_eq!(cfg.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(cfg.max_thinking_tokens, Some(10000));
        assert_eq!(cfg.upstream_key, "secret-key");
    }

    #[test]
    fn empty_key_is_an_error() {
        let toml = r#"upstream_base_url = "https://x/v1""#;
        let err = AgentConfig::from_toml_str(toml, "".to_string()).unwrap_err();
        assert!(err.to_string().contains("HI_AGENT_UPSTREAM_KEY"));
    }

    #[test]
    fn minimal_toml_defaults_optionals_to_none() {
        let cfg = AgentConfig::from_toml_str(
            r#"upstream_base_url = "https://x/v1""#,
            "k".to_string(),
        )
        .unwrap();
        assert!(cfg.model.is_none());
        assert!(cfg.effort.is_none());
        assert!(cfg.permission_mode.is_none());
        assert!(cfg.max_thinking_tokens.is_none());
    }
}
