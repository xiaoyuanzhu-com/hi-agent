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
#[derive(Clone)]
pub struct AgentConfig {
    pub upstream_base_url: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub max_thinking_tokens: Option<u32>,
    pub upstream_key: String,
}

// Hand-written so the upstream credential never lands in logs (`Config` derives
// Debug and is traced at startup). The key is reduced to a redaction marker.
impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("upstream_base_url", &self.upstream_base_url)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("permission_mode", &self.permission_mode)
            .field("max_thinking_tokens", &self.max_thinking_tokens)
            .field("upstream_key", &"<redacted>")
            .finish()
    }
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

    /// Write a managed `settings.json` into `config_dir` (the adapter's
    /// `CLAUDE_CONFIG_DIR`). Only fields that are set are emitted.
    pub fn render_settings_json(&self, config_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        let mut root = serde_json::Map::new();
        if let Some(effort) = &self.effort {
            root.insert("effortLevel".into(), serde_json::json!(effort));
        }
        if let Some(mode) = &self.permission_mode {
            root.insert(
                "permissions".into(),
                serde_json::json!({ "defaultMode": mode }),
            );
        }
        let value = serde_json::Value::Object(root);
        let path = config_dir.join("settings.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&value)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Placeholder API key handed to the adapter. The proxy supplies the real
    /// upstream key; the SDK only requires *some* non-empty value here.
    pub const PLACEHOLDER_KEY: &'static str = "hi-agent-proxy";

    /// Build the env var pairs for the ACP child process.
    ///
    /// `proxy_port` is the local proxy's bound port; `config_dir` is the managed
    /// `CLAUDE_CONFIG_DIR`; `node_bin_dir` is the directory containing the
    /// bundled `node`; `claude_bin` is the bundled claude executable.
    pub fn child_env(
        &self,
        proxy_port: u16,
        config_dir: &Path,
        node_bin_dir: &Path,
        claude_bin: &Path,
    ) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                format!("http://127.0.0.1:{proxy_port}"),
            ),
            ("ANTHROPIC_API_KEY".to_string(), Self::PLACEHOLDER_KEY.to_string()),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CODE_EXECUTABLE".to_string(),
                claude_bin.to_string_lossy().into_owned(),
            ),
        ];
        if let Some(model) = &self.model {
            env.push(("ANTHROPIC_MODEL".to_string(), model.clone()));
        }
        if let Some(tokens) = self.max_thinking_tokens {
            env.push(("MAX_THINKING_TOKENS".to_string(), tokens.to_string()));
        }
        // Prepend the bundled node dir to PATH so the adapter resolves `node`.
        let sep = if cfg!(windows) { ';' } else { ':' };
        let existing = std::env::var("PATH").unwrap_or_default();
        env.push((
            "PATH".to_string(),
            format!("{}{sep}{existing}", node_bin_dir.to_string_lossy()),
        ));
        env
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
    fn debug_redacts_the_upstream_key() {
        let cfg = AgentConfig::from_toml_str(
            r#"upstream_base_url = "https://x/v1""#,
            "super-secret-key".to_string(),
        )
        .unwrap();
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("super-secret-key"), "key leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
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

    #[test]
    fn renders_settings_json_with_set_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig::from_toml_str(
            r#"
                upstream_base_url = "https://x/v1"
                effort = "high"
                permission_mode = "acceptEdits"
            "#,
            "k".to_string(),
        )
        .unwrap();
        cfg.render_settings_json(dir.path()).unwrap();
        let written = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["effortLevel"], "high");
        assert_eq!(v["permissions"]["defaultMode"], "acceptEdits");
    }

    #[test]
    fn omits_unset_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig::from_toml_str(
            r#"upstream_base_url = "https://x/v1""#,
            "k".to_string(),
        )
        .unwrap();
        cfg.render_settings_json(dir.path()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert!(v.get("effortLevel").is_none());
        assert!(v.get("permissions").is_none());
    }

    #[test]
    fn child_env_sets_proxy_and_managed_vars() {
        let cfg = AgentConfig::from_toml_str(
            r#"
                upstream_base_url = "https://x/v1"
                model = "claude-opus-4-8"
                max_thinking_tokens = 10000
            "#,
            "k".to_string(),
        )
        .unwrap();
        let env = cfg.child_env(
            7777,
            std::path::Path::new("/cache/config"),
            std::path::Path::new("/cache/runtime/node/bin"),
            std::path::Path::new("/cache/runtime/claude"),
        );
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["ANTHROPIC_BASE_URL"], "http://127.0.0.1:7777");
        assert_eq!(map["ANTHROPIC_API_KEY"], "hi-agent-proxy");
        assert_eq!(map["ANTHROPIC_MODEL"], "claude-opus-4-8");
        assert_eq!(map["MAX_THINKING_TOKENS"], "10000");
        assert_eq!(map["CLAUDE_CONFIG_DIR"], "/cache/config");
        assert_eq!(map["CLAUDE_CODE_EXECUTABLE"], "/cache/runtime/claude");
        assert!(map["PATH"].starts_with("/cache/runtime/node/bin"));
    }
}
