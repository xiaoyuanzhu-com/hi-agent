//! Cognition config → child env + settings.json. The LLM credential (base URL,
//! key, model) and the cognition tunables (effort, permission mode, pulse,
//! reflection cadence, …) all come from the config store (Settings). The tunables
//! are read via [`tunables`] (a startup snapshot for the reactor's argless helpers)
//! or [`crate::foundation::credentials::get_setting`] directly where a data dir is
//! in scope. Only infra vars (e.g. the server base URL) remain env-driven.

use std::path::Path;

use anyhow::Context;

/// Default upstream base URL when the stored LLM base URL is empty.
pub const DEFAULT_AI_API_BASE: &str = "https://api.anthropic.com";

// Keys under which the cognition tunables live in the config store's `app_settings`
// table. Shared by the readers (reactor, `resolve`) and the settings handler so the
// names can't drift. Each is optional; an absent key → the built-in default.
/// Adapter `effortLevel` in settings.json (e.g. low | medium | high).
pub const KEY_EFFORT: &str = "effort";
/// Adapter `permissions.defaultMode` in settings.json (e.g. acceptEdits).
pub const KEY_PERMISSION_MODE: &str = "permission_mode";
/// Reactor heartbeat hot-swap character ceiling. Blank / non-positive → default.
pub const KEY_COMPACT: &str = "compact";
/// Idle interval between host pulses. Alarm-delay grammar (`90s`/`30m`/`1h`);
/// `0`/`off` disables pulses; unset / unparseable → the built-in default.
pub const KEY_PULSE: &str = "pulse";
/// Master switch for the reflection ("sleep") pass; `off` disables it entirely.
pub const KEY_REFLECT: &str = "reflect";
/// Base reflection cadence — how often a scene with fresh input consolidates.
/// Alarm-delay grammar; `0`/`off` disables; unset → the built-in default (1m).
pub const KEY_REFLECT_EVERY: &str = "reflect_every";
/// Ceiling on the idle reflection backoff. Alarm-delay grammar; unset → default (8h).
pub const KEY_REFLECT_MAX: &str = "reflect_max";
/// Consecutive terminal-turn failures before flipping to vendor-down ("mailbox")
/// mode. Each terminal failure is already 3 failed model calls, so 2 (the default)
/// = 6 failures across two turns. `0`/unparseable → default.
pub const KEY_VENDOR_DOWN_AFTER: &str = "vendor_down_after";
/// Recovery-probe cadence while in vendor-down mode. Alarm-delay grammar;
/// `off`/`0`/unset/unparseable → the 30s default.
pub const KEY_VENDOR_PROBE: &str = "vendor_probe";

/// Env var (set on the cognition subprocess) carrying hi-agent's own HTTP base
/// URL, so sessions can read input channels and write the overlay over the same
/// wire the browser uses. See [`AgentConfig::child_env`]. Infra, not user config.
pub const ENV_SERVER_BASE_URL: &str = "HI_AGENT_BASE_URL";

/// The cognition tunables loaded once from the config store at startup into a
/// process global, so the reactor's argless helpers can read them without threading
/// a data dir. Changes apply on restart — like every other setting.
pub mod tunables {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::OnceLock;

    static TUNABLES: OnceLock<HashMap<String, String>> = OnceLock::new();

    /// Snapshot the config store's `app_settings` into the global. Idempotent (first
    /// wins); the composition root calls this once before the reactor spawns.
    pub fn init(data_dir: &Path) {
        let _ = TUNABLES.set(crate::foundation::credentials::all_settings(data_dir));
    }

    /// A stored tunable (trimmed, non-empty), or `None` when unset / before
    /// [`init`] — callers then apply their built-in default.
    pub fn get(key: &str) -> Option<String> {
        let map = TUNABLES.get()?;
        let v = map.get(key)?.trim();
        (!v.is_empty()).then(|| v.to_string())
    }
}

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

impl AgentConfig {
    /// Resolve the upstream LLM credential + adapter tunables for startup from the
    /// config store — the user's BYOK key, or (xiaoyuanzhu) the broker-minted bundle,
    /// plus the stored `effort` / `permission_mode`. There is no `.env` fallback: a
    /// fresh install works out of the box because xiaoyuanzhu auto-bootstraps a device
    /// account and the broker mints the key. Never errors — with no key the agent
    /// boots **unconfigured** (see [`is_configured`](Self::is_configured)), the server
    /// + Settings UI come up, and prompts fail clearly until a key is set.
    pub fn resolve(data_dir: &Path) -> Self {
        let store = crate::foundation::credentials::Credentials::load(data_dir);
        let llm = store.effective().map(|e| e.llm.clone()).unwrap_or_default();
        let model = llm.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        use crate::foundation::credentials::get_setting;
        Self::new(
            model,
            get_setting(data_dir, KEY_EFFORT),
            get_setting(data_dir, KEY_PERMISSION_MODE),
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

    /// The **volatile** env vars — the upstream endpoint + key + model — that the
    /// child sends to the LLM gateway. Split out from [`child_env`](Self::child_env)
    /// because these are the only vars sourced from the credential store, and the
    /// store changes under a running app (broker re-mint, Settings edit, mode
    /// switch). Callers re-resolve these at each session spawn (see
    /// [`crate::foundation::agent`]) so a fresh child never carries a stale key,
    /// rather than freezing them at boot.
    ///
    /// The key rides `ANTHROPIC_AUTH_TOKEN`, which the CLI sends as
    /// `Authorization: Bearer <key>` — the scheme the managed songguo gateway (and
    /// most gateways) require. We deliberately do *not* set `ANTHROPIC_API_KEY`
    /// (which would send Anthropic's native `x-api-key` header): songguo rejects that
    /// with `401 missing authorization`. The trade-off: a BYOK user pointing at
    /// Anthropic's *native* endpoint (which wants `x-api-key`) would need this
    /// revisited — today every path goes through a Bearer gateway.
    pub fn auth_child_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                self.upstream_base_url.clone(),
            ),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), self.upstream_key.clone()),
        ];
        if let Some(model) = &self.model {
            env.push(("ANTHROPIC_MODEL".to_string(), model.clone()));
        }
        env
    }

    /// Build the **static** env var pairs for the ACP child process — everything
    /// fixed for the process lifetime (resolved runtime paths, config dir, the
    /// server URL). The volatile upstream credential vars come from
    /// [`auth_child_env`](Self::auth_child_env), re-resolved per spawn and merged in
    /// by the agent layer.
    ///
    /// `server_port` is hi-agent's own HTTP port (handed to the child as
    /// `HI_AGENT_BASE_URL` so a session can reach the channels); `config_dir` is the
    /// managed `CLAUDE_CONFIG_DIR`; `node_bin_dir` is the directory containing the
    /// resolved `node`; `claude_bin` is the resolved claude executable.
    pub fn child_env(
        &self,
        server_port: u16,
        config_dir: &Path,
        node_bin_dir: &Path,
        claude_bin: &Path,
    ) -> Vec<(String, String)> {
        let mut env = vec![
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
    fn child_env_sets_static_vars() {
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
        assert_eq!(map["HI_AGENT_BASE_URL"], "http://127.0.0.1:8080");
        assert_eq!(map["CLAUDE_CONFIG_DIR"], "/cache/config");
        assert_eq!(map["CLAUDE_CODE_EXECUTABLE"], "/cache/runtime/claude");
        assert!(map["PATH"].starts_with("/cache/runtime/node/bin"));
        // The volatile credential vars are NOT frozen into the static env — they
        // come from `auth_child_env`, re-resolved per session spawn.
        assert!(!map.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!map.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!map.contains_key("ANTHROPIC_MODEL"));
    }

    #[test]
    fn auth_child_env_carries_the_upstream_credential() {
        let cfg = AgentConfig::new(
            Some("claude-opus-4-8".to_string()),
            None,
            None,
            "https://x/v1".to_string(),
            "k".to_string(),
        );
        let map: std::collections::HashMap<_, _> = cfg.auth_child_env().into_iter().collect();
        // The child talks to the upstream directly — no local proxy in between.
        assert_eq!(map["ANTHROPIC_BASE_URL"], "https://x/v1");
        // Key rides AUTH_TOKEN (→ `Authorization: Bearer`), not API_KEY (→ x-api-key).
        assert_eq!(map["ANTHROPIC_AUTH_TOKEN"], "k");
        assert!(!map.contains_key("ANTHROPIC_API_KEY"));
        assert_eq!(map["ANTHROPIC_MODEL"], "claude-opus-4-8");
    }

    #[test]
    fn auth_child_env_omits_model_when_unset() {
        let cfg = AgentConfig::new(None, None, None, "https://x/v1".to_string(), "k".to_string());
        let map: std::collections::HashMap<_, _> = cfg.auth_child_env().into_iter().collect();
        assert!(!map.contains_key("ANTHROPIC_MODEL"));
    }

    #[test]
    fn resolve_reflects_the_current_stored_key() {
        // The whole point of re-resolving per spawn: a key change written to the
        // store (broker re-mint, Settings edit) is visible on the next resolve,
        // without freezing anything at boot. Uses BYOK so the stored key is the
        // effective one directly.
        use crate::foundation::credentials::{Credentials, LlmCredentials, Mode};
        let dir = tempfile::tempdir().unwrap();

        let mut store = Credentials {
            mode: Mode::Byok,
            llm: LlmCredentials { api_key: "key-A".into(), ..Default::default() },
            ..Default::default()
        };
        store.save(dir.path()).unwrap();
        let a: std::collections::HashMap<_, _> =
            AgentConfig::resolve(dir.path()).auth_child_env().into_iter().collect();
        assert_eq!(a["ANTHROPIC_AUTH_TOKEN"], "key-A");

        // Rotate the stored key; a fresh resolve must carry the new one.
        store.llm.api_key = "key-B".into();
        store.save(dir.path()).unwrap();
        let b: std::collections::HashMap<_, _> =
            AgentConfig::resolve(dir.path()).auth_child_env().into_iter().collect();
        assert_eq!(b["ANTHROPIC_AUTH_TOKEN"], "key-B");
    }
}
