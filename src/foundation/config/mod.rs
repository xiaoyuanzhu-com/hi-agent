//! Cognition config → child env + settings.json. The LLM credential (base URL,
//! key, model) is resolved from the config store (Settings / broker); the cognition
//! tunables (effort, permission mode, pulse, reflection cadence, …) are still
//! dev-managed from the environment (`.env`).

use std::path::Path;

use anyhow::Context;

/// Default upstream base URL when the stored LLM base URL is empty.
pub const DEFAULT_AI_API_BASE: &str = "https://api.anthropic.com";
/// Env var setting the adapter's `effortLevel` in its managed settings.json.
pub const ENV_EFFORT: &str = "HI_AGENT_EFFORT";
/// Env var setting the adapter's `permissions.defaultMode` in settings.json.
pub const ENV_PERMISSION_MODE: &str = "HI_AGENT_PERMISSION_MODE";
/// Env var overriding the reactor heartbeat's hot-swap character ceiling
/// (`HI_AGENT_COMPACT`). Unset / blank / non-positive → the built-in default.
pub const ENV_COMPACT: &str = "HI_AGENT_COMPACT";
/// Env var overriding the idle interval between host pulses (`HI_AGENT_PULSE`).
/// Accepts the alarm-delay grammar (`90s`, `30m`, `1h`; bare integer = seconds);
/// `0` or `off` disables pulses. Unset / unparseable → the built-in default.
pub const ENV_PULSE: &str = "HI_AGENT_PULSE";
/// Env var to disable the reflection ("sleep") pass entirely
/// (`HI_AGENT_REFLECT=off`). Any other value (or unset) leaves it on; reflection
/// then runs on one adaptive clock (see [`ENV_REFLECT_EVERY`]), consolidating the
/// raw frontier into episodes/facets.
pub const ENV_REFLECT: &str = "HI_AGENT_REFLECT";
/// Env var setting the **base** reflection cadence (`HI_AGENT_REFLECT_EVERY`):
/// how often a scene with fresh input consolidates. Accepts the alarm-delay
/// grammar (`90s`, `30m`, `1h`; bare integer = seconds); `0` or `off` disables
/// reflection. Unset / unparseable → the built-in default (1m). When a scene goes
/// quiet the gap backs off from this base (doubling) up to [`ENV_REFLECT_MAX`].
pub const ENV_REFLECT_EVERY: &str = "HI_AGENT_REFLECT_EVERY";
/// Env var capping the idle backoff (`HI_AGENT_REFLECT_MAX`): once a scene is
/// caught up and quiet, the reflection gap doubles from the base each pass but
/// never exceeds this, so a long-idle scene stops re-checking in vain. Alarm-delay
/// grammar; unset / unparseable → the built-in default (8h).
pub const ENV_REFLECT_MAX: &str = "HI_AGENT_REFLECT_MAX";
/// Env var (set on the cognition subprocess) carrying hi-agent's own HTTP base
/// URL, so sessions can read input channels and write the overlay over the same
/// wire the browser uses. See [`AgentConfig::child_env`].
pub const ENV_SERVER_BASE_URL: &str = "HI_AGENT_BASE_URL";
/// Env var setting the consecutive terminal-turn-failure count that flips the
/// reactor into vendor-down ("mailbox") mode (`HI_AGENT_VENDOR_DOWN_AFTER`).
/// Each terminal failure is already the product of the 3-attempt retry inside
/// `run_turn`, so 2 (the default) means 6 model calls failed across two
/// separate turns — a strong signal of a sustained outage, not a blip. `1` is
/// the aggressive setting. Unset / unparseable / `0` → default.
pub const ENV_VENDOR_DOWN_AFTER: &str = "HI_AGENT_VENDOR_DOWN_AFTER";
/// Env var setting the recovery-probe cadence while in vendor-down mode
/// (`HI_AGENT_VENDOR_PROBE`). Alarm-delay grammar (`30s`, `1m`; bare integer =
/// seconds). Unset / unparseable / `0` / `off` → 30s default. Probes only fire
/// when a scene has pending mail, so an idle outage costs no vendor calls.
pub const ENV_VENDOR_PROBE: &str = "HI_AGENT_VENDOR_PROBE";

/// HTTP headers a session's MCP attach carries on every tool call, so the `/mcp`
/// server can route a call back to the right scene loop and tool surface. Set
/// when the session is opened (see `agent::AgentLayer::session`) and read by the
/// MCP handler (see `crate::foundation::mcp`). The scene is the isolation key; the role
/// selects which tools are exposed; the worker id (workers only) names which
/// working session raised an `ask`.
pub const HEADER_SCENE: &str = "X-HI-Scene";
pub const HEADER_ROLE: &str = "X-HI-Role";
pub const HEADER_WORKER_ID: &str = "X-HI-Worker-Id";

/// Dev-managed cognition parameters. Everything comes from the environment
/// (loaded from `.env` in dev); the upstream credential never lives in git.
#[derive(Clone)]
pub struct AgentConfig {
    pub upstream_base_url: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
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
            .field("upstream_key", &"<redacted>")
            .finish()
    }
}

/// Read an env var, treating unset *and* blank/whitespace-only as absent.
fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl AgentConfig {
    /// Resolve the upstream LLM credential for startup from the credential store —
    /// the user's BYOK key, or (xiaoyuanzhu) the broker-minted bundle. There is no
    /// `.env` fallback: a fresh install works out of the box because xiaoyuanzhu
    /// auto-bootstraps a device account and the broker mints the key. Never errors —
    /// with no key the agent boots **unconfigured** (see
    /// [`is_configured`](Self::is_configured)), the server + Settings UI come up, and
    /// prompts fail clearly until a key is set (via Settings or by signing in).
    /// `effort` / `permission_mode` stay env-driven (they aren't credentials).
    pub fn resolve(data_dir: &Path) -> Self {
        let store = crate::foundation::credentials::Credentials::load(data_dir);
        let llm = store.effective().map(|e| e.llm.clone()).unwrap_or_default();
        let model = llm.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        Self::new(
            model,
            env_opt(ENV_EFFORT),
            env_opt(ENV_PERMISSION_MODE),
            llm.base_url,
            llm.api_key,
        )
    }

    /// Whether an upstream key is configured. When false the agent is inert: it
    /// boots so the user can set a key in Settings, but prompts will fail until then.
    pub fn is_configured(&self) -> bool {
        !self.upstream_key.trim().is_empty()
    }

    /// Assemble from explicit parts. The base URL falls back to
    /// [`DEFAULT_AI_API_BASE`] when unset; an empty key is allowed (the
    /// **unconfigured** state — BYOK before the user has pasted a key).
    pub fn new(
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<String>,
        upstream_base_url: String,
        upstream_key: String,
    ) -> Self {
        let upstream_base_url = if upstream_base_url.trim().is_empty() {
            DEFAULT_AI_API_BASE.to_string()
        } else {
            upstream_base_url
        };
        Self {
            upstream_base_url,
            model,
            effort,
            permission_mode,
            upstream_key,
        }
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

    /// Pre-approve the upstream key in the managed config dir's `.claude.json`.
    ///
    /// Claude Code treats any key supplied via `ANTHROPIC_API_KEY` as a "custom"
    /// key and refuses to use it unless its last-20-char fingerprint appears in
    /// `customApiKeyResponses.approved`. Without this, `session/prompt` fails with
    /// "Please run /login", which the ACP adapter surfaces as
    /// `-32000 Authentication required`. We seed the approval so the child's
    /// `ANTHROPIC_API_KEY` is accepted without an interactive `/login`.
    ///
    /// We pin `approved` to exactly the current key's fingerprint — this dir is
    /// hi-agent-owned and the only custom key is ours, so there is nothing else to
    /// preserve and the list stays bounded. Re-run whenever the key changes.
    pub fn approve_api_key(&self, config_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        let path = config_dir.join(".claude.json");

        // Read-modify-write: `.claude.json` also holds userID, caches, etc.
        let mut root: serde_json::Map<String, serde_json::Value> = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        // Claude matches approvals by the key's last 20 chars (`key.slice(-20)`).
        let key = self.upstream_key.as_str();
        let fingerprint = &key[key.len().saturating_sub(20)..];

        root.insert(
            "customApiKeyResponses".to_string(),
            serde_json::json!({ "approved": [fingerprint], "rejected": [] }),
        );

        std::fs::write(&path, serde_json::to_vec_pretty(&serde_json::Value::Object(root))?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Build the env var pairs for the ACP child process.
    ///
    /// `server_port` is hi-agent's own HTTP port (handed to the child as
    /// `HI_AGENT_BASE_URL` so a session can reach the channels); `config_dir` is the
    /// managed `CLAUDE_CONFIG_DIR`; `node_bin_dir` is the directory containing the
    /// resolved `node`; `claude_bin` is the resolved claude executable. The child
    /// talks to the upstream LLM directly via `ANTHROPIC_BASE_URL` / the real
    /// `ANTHROPIC_API_KEY` (pre-approved by [`approve_api_key`]).
    pub fn child_env(
        &self,
        server_port: u16,
        config_dir: &Path,
        node_bin_dir: &Path,
        claude_bin: &Path,
    ) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                self.upstream_base_url.clone(),
            ),
            ("ANTHROPIC_API_KEY".to_string(), self.upstream_key.clone()),
            (
                ENV_SERVER_BASE_URL.to_string(),
                format!("http://127.0.0.1:{server_port}"),
            ),
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
        // Prepend the resolved node dir to PATH so the adapter resolves `node`.
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
    fn takes_all_parts_from_args() {
        let cfg = AgentConfig::new(
            Some("claude-opus-4-8".to_string()),
            Some("high".to_string()),
            Some("acceptEdits".to_string()),
            "https://upstream.example/v1".to_string(),
            "secret-key".to_string(),
        );
        assert_eq!(cfg.upstream_base_url, "https://upstream.example/v1");
        assert_eq!(cfg.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(cfg.effort.as_deref(), Some("high"));
        assert_eq!(cfg.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(cfg.upstream_key, "secret-key");
    }

    #[test]
    fn empty_base_url_falls_back_to_default() {
        let cfg =
            AgentConfig::new(None, None, None, "".to_string(), "k".to_string());
        assert_eq!(cfg.upstream_base_url, DEFAULT_AI_API_BASE);
    }

    #[test]
    fn debug_redacts_the_upstream_key() {
        let cfg = AgentConfig::new(
            None,
            None,
            None,
            "https://x/v1".to_string(),
            "super-secret-key".to_string(),
        );
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("super-secret-key"), "key leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn empty_key_means_unconfigured() {
        let cfg = AgentConfig::new(None, None, None, "https://x/v1".to_string(), "".to_string());
        assert!(!cfg.is_configured());
        let cfg = AgentConfig::new(None, None, None, "https://x/v1".to_string(), "k".to_string());
        assert!(cfg.is_configured());
    }

    #[test]
    fn unset_optionals_default_to_none() {
        let cfg = AgentConfig::new(None, None, None, "https://x/v1".to_string(), "k".to_string());
        assert!(cfg.model.is_none());
        assert!(cfg.effort.is_none());
        assert!(cfg.permission_mode.is_none());
    }

    #[test]
    fn renders_settings_json_with_set_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig::new(
            None,
            Some("high".to_string()),
            Some("acceptEdits".to_string()),
            "https://x/v1".to_string(),
            "k".to_string(),
        );
        cfg.render_settings_json(dir.path()).unwrap();
        let written = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["effortLevel"], "high");
        assert_eq!(v["permissions"]["defaultMode"], "acceptEdits");
    }

    #[test]
    fn omits_unset_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            AgentConfig::new(None, None, None, "https://x/v1".to_string(), "k".to_string());
        cfg.render_settings_json(dir.path()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert!(v.get("effortLevel").is_none());
        assert!(v.get("permissions").is_none());
    }

    #[test]
    fn child_env_sets_upstream_and_managed_vars() {
        let cfg = AgentConfig::new(
            Some("claude-opus-4-8".to_string()),
            None,
            None,
            "https://x/v1".to_string(),
            "k".to_string(),
        );
        let env = cfg.child_env(
            8080,
            std::path::Path::new("/cache/config"),
            std::path::Path::new("/cache/runtime/node/bin"),
            std::path::Path::new("/cache/runtime/claude"),
        );
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        // The child talks to the upstream directly — no local proxy in between.
        assert_eq!(map["ANTHROPIC_BASE_URL"], "https://x/v1");
        assert_eq!(map["ANTHROPIC_API_KEY"], "k");
        assert_eq!(map["HI_AGENT_BASE_URL"], "http://127.0.0.1:8080");
        assert_eq!(map["ANTHROPIC_MODEL"], "claude-opus-4-8");
        assert!(!map.contains_key("MAX_THINKING_TOKENS"));
        assert_eq!(map["CLAUDE_CONFIG_DIR"], "/cache/config");
        assert_eq!(map["CLAUDE_CODE_EXECUTABLE"], "/cache/runtime/claude");
        assert!(map["PATH"].starts_with("/cache/runtime/node/bin"));
    }

    #[test]
    fn approve_api_key_seeds_fingerprint_and_preserves_other_fields() {
        let cfg = AgentConfig::new(
            None,
            None,
            None,
            "https://x/v1".to_string(),
            "sk-ant-super-secret-key-1234567890".to_string(),
        );
        let dir = std::env::temp_dir().join(format!("hi-agent-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-existing managed state that must survive the read-modify-write.
        std::fs::write(dir.join(".claude.json"), br#"{"userID":"abc"}"#).unwrap();

        cfg.approve_api_key(&dir).unwrap();

        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["userID"], "abc");
        let key = &cfg.upstream_key;
        let fp = &key[key.len().saturating_sub(20)..];
        assert_eq!(v["customApiKeyResponses"]["approved"][0], fp);

        std::fs::remove_dir_all(&dir).ok();
    }
}
