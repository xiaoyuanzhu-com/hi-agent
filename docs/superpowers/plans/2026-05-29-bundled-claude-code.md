# Bundled Claude Code + Managed Parameters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship hi-agent as a self-contained binary that bundles Node + the Claude ACP adapter + the `claude` CLI, routes cognition through a local Rust LLM proxy to a dev-configured upstream, and drives model/effort/mode/thinking from an in-repo config.

**Architecture:** Three new Rust modules inside the existing process — `config` (in-repo `AgentConfig` → env + generated `settings.json`), `llm_proxy` (thin axum reverse proxy to an Anthropic-compatible upstream, injecting the real key), and `runtime` (`RuntimeBundle` that extracts an embedded, version-pinned Node+adapter+claude archive to an OS cache dir). Startup wires them together and spawns the adapter via the existing ACP layer. A `make bundle` step + `build.rs` produce and embed the per-OS archive.

**Tech Stack:** Rust (tokio, axum 0.8, reqwest 0.12 — all already present), `toml`, `tar`, `zstd`, `directories` crates; Node 22 LTS + `@agentclientprotocol/claude-agent-acp` bundled at build time.

---

## Design reference

Spec: `docs/superpowers/specs/2026-05-29-bundled-claude-code-design.md`. Read it before starting.

Verified facts the plan relies on:
- The adapter (`@agentclientprotocol/claude-agent-acp@0.36.1`) reads `ANTHROPIC_MODEL`, `MAX_THINKING_TOKENS`, `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_EXECUTABLE`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` from env, and `effortLevel` + `permissions.defaultMode` from `<CLAUDE_CONFIG_DIR>/settings.json`.
- `agent_client_protocol::AcpAgent::from_args` treats leading `NAME=value` entries as child env vars, applied on top of inherited parent env (`acp_agent.rs:159-163`).
- The hardcoded spawn to replace is at `src/lib.rs:42`.

## File structure

- Create `config.toml` (repo root) — committed dev-tunable defaults + upstream URL.
- Create `src/config/mod.rs` — `AgentConfig`: load, secret from env, `child_env()`, `render_settings_json()`.
- Create `src/llm_proxy/mod.rs` — `LlmProxy`: bind, reverse-proxy handler, streaming pass-through.
- Create `src/runtime/mod.rs` — `RuntimeBundle`: embedded archive, `ensure()` extraction, path resolution, `RuntimeManifest`.
- Create `runtime/manifest.toml` — pinned Node version, adapter version, per-target Node URLs + SHA256.
- Create `runtime/package.json` + `runtime/package-lock.json` — exact adapter pin for `npm ci`.
- Create `scripts/bundle.sh` — fetch Node (verify SHA256), `npm ci`, write `runtime.json`, pack `runtime/embed/<target>.tar.zst`.
- Modify `build.rs` — copy the target's archive to `OUT_DIR/runtime.tar.zst` (placeholder if absent), stamp `cargo:rustc-env` version constants.
- Modify `src/acp/process.rs` — add an `env` parameter to `AcpProcess::spawn`.
- Modify `src/lib.rs` — extend `Config`, wire config→runtime→proxy→spawn in `run()`.
- Modify `src/main.rs` — `--version` reporting of bundled component versions.
- Modify `Cargo.toml` — add `toml`, `tar`, `zstd`, `directories`.
- Create tests under `tests/` and `#[cfg(test)]` modules as specified per task.

---

## Task 1: Add dependencies and module scaffolding

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs:7-14` (module declarations)
- Create: `src/config/mod.rs`, `src/llm_proxy/mod.rs`, `src/runtime/mod.rs`

- [ ] **Step 1: Add crates to `Cargo.toml`**

In the `[dependencies]` section add:

```toml
toml = "0.8"
tar = "0.4"
zstd = "0.13"
directories = "5"
```

- [ ] **Step 2: Declare the new modules**

In `src/lib.rs`, in the `pub mod` block (currently lines 7-14), add:

```rust
pub mod config;
pub mod llm_proxy;
pub mod runtime;
```

- [ ] **Step 3: Create empty module files with a doc comment each**

`src/config/mod.rs`:
```rust
//! Dev-managed cognition config: in-repo `config.toml` → child env + settings.json.
```

`src/llm_proxy/mod.rs`:
```rust
//! Local Anthropic-compatible reverse proxy. Injects the real upstream key so
//! it never lands in any on-disk claude/adapter config.
```

`src/runtime/mod.rs`:
```rust
//! Embedded Node + ACP adapter + claude CLI runtime, extracted to an OS cache
//! dir on first run and reused thereafter (keyed by build-stamped bundle_id).
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build`
Expected: PASS (warnings about unused modules are fine).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/config/mod.rs src/llm_proxy/mod.rs src/runtime/mod.rs
git commit -m "chore: scaffold config/llm_proxy/runtime modules and deps"
```

---

## Task 2: `AgentConfig` loading

**Files:**
- Modify: `src/config/mod.rs`
- Create: `config.toml`

- [ ] **Step 1: Write the failing test**

Append to `src/config/mod.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests`
Expected: FAIL — `AgentConfig` / `from_toml_str` not found.

- [ ] **Step 3: Write the implementation**

At the top of `src/config/mod.rs` (under the doc comment), add:

```rust
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

#[allow(unused_imports)]
use std::path::Path as _PathUnused; // placeholder removed in Task 3
```

Note: drop the `_PathUnused` line and the `use std::path::Path;` import now if `cargo` warns they are unused — they are used in Task 3.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test config::tests`
Expected: PASS (3 tests).

- [ ] **Step 5: Create `config.toml`**

```toml
# hi-agent cognition config (dev-managed). The upstream credential is NOT here —
# it is read from HI_AGENT_UPSTREAM_KEY (see .env).

upstream_base_url = "https://api.anthropic.com"

# Managed parameters pushed to the bundled Claude adapter.
model               = "claude-opus-4-8"
effort              = "high"          # adapter settings.json: effortLevel
permission_mode     = "acceptEdits"   # adapter settings.json: permissions.defaultMode
max_thinking_tokens = 10000
```

- [ ] **Step 6: Commit**

```bash
git add src/config/mod.rs config.toml
git commit -m "feat: AgentConfig loading from config.toml + env secret"
```

---

## Task 3: Render `settings.json`

**Files:**
- Modify: `src/config/mod.rs`

- [ ] **Step 1: Write the failing test**

Add inside the `tests` module in `src/config/mod.rs`:

```rust
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
```

Add `tempfile` to `[dev-dependencies]` is already present (`tempfile = "3"`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests::renders_settings_json_with_set_fields`
Expected: FAIL — `render_settings_json` not found.

- [ ] **Step 3: Implement `render_settings_json`**

Add to the `impl AgentConfig` block:

```rust
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
```

Remove the `_PathUnused` placeholder line from Task 2 now that `Path` is used.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test config::tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs
git commit -m "feat: render managed settings.json from AgentConfig"
```

---

## Task 4: `child_env` assembly

**Files:**
- Modify: `src/config/mod.rs`

- [ ] **Step 1: Write the failing test**

Add inside the `tests` module:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test config::tests::child_env_sets_proxy_and_managed_vars`
Expected: FAIL — `child_env` not found.

- [ ] **Step 3: Implement `child_env`**

Add to the `impl AgentConfig` block:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test config::tests`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs
git commit -m "feat: assemble ACP child env from AgentConfig"
```

---

## Task 5: Local LLM proxy

**Files:**
- Modify: `src/llm_proxy/mod.rs`
- Create: `tests/llm_proxy.rs`

- [ ] **Step 1: Write the failing integration test**

Create `tests/llm_proxy.rs`:

```rust
//! Proxy forwards to a mock upstream, injecting the real key, streaming body back.

use std::net::SocketAddr;

use axum::routing::post;
use axum::{Router, extract::State};
use hi_agent::llm_proxy::LlmProxy;

/// Minimal mock upstream: asserts the injected key, echoes a canned SSE body.
async fn spawn_mock_upstream() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/messages",
        post(|State(()): State<()>, headers: axum::http::HeaderMap, body: String| async move {
            assert_eq!(headers.get("x-api-key").unwrap(), "REAL-KEY");
            assert!(body.contains("hello"));
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from("data: {\"type\":\"ok\"}\n\n"))
                .unwrap()
        }),
    ).with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, handle)
}

#[tokio::test]
async fn forwards_injects_key_and_streams_body() {
    let (upstream_addr, _up) = spawn_mock_upstream().await;
    let upstream = format!("http://{upstream_addr}");

    let proxy = LlmProxy::start(upstream, "REAL-KEY".to_string()).await.unwrap();
    let port = proxy.port();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", "hi-agent-proxy") // placeholder; proxy overwrites
        .header("content-type", "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"type\":\"ok\""));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test llm_proxy`
Expected: FAIL — `LlmProxy` not found.

- [ ] **Step 3: Implement the proxy**

Replace the contents of `src/llm_proxy/mod.rs` (keep the doc comment) with:

```rust
//! Local Anthropic-compatible reverse proxy. Injects the real upstream key so
//! it never lands in any on-disk claude/adapter config.

use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;

/// Headers we never forward upstream (hop-by-hop or auth we replace).
const STRIP_REQUEST_HEADERS: &[&str] = &["host", "authorization", "x-api-key", "content-length"];

#[derive(Clone)]
struct ProxyState {
    client: reqwest::Client,
    upstream_base_url: Arc<str>,
    upstream_key: Arc<str>,
}

/// A running local proxy. Drop to stop serving.
pub struct LlmProxy {
    port: u16,
    _server: tokio::task::JoinHandle<()>,
}

impl LlmProxy {
    /// Bind on 127.0.0.1:0 and start serving. `upstream_base_url` is the origin
    /// (no trailing `/v1/...`); `upstream_key` is the real credential.
    pub async fn start(upstream_base_url: String, upstream_key: String) -> anyhow::Result<Self> {
        let state = ProxyState {
            client: reqwest::Client::new(),
            upstream_base_url: Arc::from(upstream_base_url.trim_end_matches('/')),
            upstream_key: Arc::from(upstream_key),
        };
        let app = Router::new()
            .route("/{*path}", any(forward))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding llm proxy")?;
        let port = listener.local_addr()?.port();
        let server = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "llm proxy server exited");
            }
        });
        tracing::info!(port, "llm proxy listening");
        Ok(Self { port, _server: server })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Forward any request to the upstream, replacing auth with the real key and
/// streaming the response body straight back.
async fn forward(
    State(state): State<ProxyState>,
    req: axum::extract::Request,
) -> Result<Response, StatusCode> {
    let method = req.method().clone();
    let path_q = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let in_headers = req.headers().clone();

    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let url = format!("{}{}", state.upstream_base_url, path_q);
    let mut out = state.client.request(method, &url).body(body_bytes);

    for (name, value) in in_headers.iter() {
        if STRIP_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out = out.header(name, value);
    }
    out = out.header("x-api-key", state.upstream_key.as_ref());

    let upstream_resp = out.send().await.map_err(|e| {
        tracing::warn!(error = %e, "upstream request failed");
        StatusCode::BAD_GATEWAY
    })?;

    let status = upstream_resp.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers().iter() {
        // content-length will not match a streamed body; let axum recompute.
        if name.as_str() == "content-length" || name.as_str() == "transfer-encoding" {
            continue;
        }
        if let Ok(hn) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            builder = builder.header(hn, value);
        }
    }
    let stream = upstream_resp.bytes_stream();
    builder
        .body(Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[allow(unused_imports)]
use axum::http::header as _header_unused; // keep imports tidy across edits
```

Remove the `_header_unused` and any unused import lines if `cargo` warns. `HeaderMap` import may be unused — drop it if so.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test llm_proxy`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/llm_proxy/mod.rs tests/llm_proxy.rs
git commit -m "feat: local Anthropic-compatible LLM proxy with key injection"
```

---

## Task 6: `RuntimeBundle` extraction

**Files:**
- Modify: `src/runtime/mod.rs`
- Create: `tests/runtime_bundle.rs`

This task tests extraction logic against a synthetic archive built in-test, so it does not need the real (large) bundle.

- [ ] **Step 1: Write the failing test**

Create `tests/runtime_bundle.rs`:

```rust
//! Extraction is atomic, idempotent, and resolves paths from runtime.json.

use std::io::Write;

use hi_agent::runtime::{extract_bundle, ResolvedRuntime};

/// Build a tiny .tar.zst in memory with a runtime.json and stub files.
fn synthetic_archive() -> Vec<u8> {
    let mut tar_buf = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut tar_buf);
        let manifest = br#"{"node":"node/bin/node","adapter":"adapter/index.js","claude":"adapter/claude"}"#;
        let mut add = |path: &str, data: &[u8], tar: &mut tar::Builder<&mut Vec<u8>>| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, data).unwrap();
        };
        add("runtime.json", manifest, &mut tar);
        add("node/bin/node", b"#!stub", &mut tar);
        add("adapter/index.js", b"// stub", &mut tar);
        add("adapter/claude", b"#!stub", &mut tar);
        tar.finish().unwrap();
    }
    let mut zst = Vec::new();
    let mut enc = zstd::Encoder::new(&mut zst, 1).unwrap();
    enc.write_all(&tar_buf).unwrap();
    enc.finish().unwrap();
    zst
}

#[test]
fn extracts_then_reuses() {
    let cache = tempfile::tempdir().unwrap();
    let archive = synthetic_archive();

    let r1: ResolvedRuntime =
        extract_bundle(&archive, "bundleA", cache.path()).unwrap();
    assert!(r1.node_bin.ends_with("node/bin/node"));
    assert!(r1.adapter_entry.ends_with("adapter/index.js"));
    assert!(r1.claude_bin.ends_with("adapter/claude"));
    assert!(r1.node_bin.exists());

    // Second call reuses (COMPLETE marker present) and returns the same paths.
    let r2 = extract_bundle(&archive, "bundleA", cache.path()).unwrap();
    assert_eq!(r1.node_bin, r2.node_bin);

    // Different bundle_id extracts into a different dir.
    let r3 = extract_bundle(&archive, "bundleB", cache.path()).unwrap();
    assert_ne!(r1.node_bin, r3.node_bin);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test runtime_bundle`
Expected: FAIL — `extract_bundle` / `ResolvedRuntime` not found.

- [ ] **Step 3: Implement the runtime module**

Replace the contents of `src/runtime/mod.rs` (keep doc comment) with:

```rust
//! Embedded Node + ACP adapter + claude CLI runtime, extracted to an OS cache
//! dir on first run and reused thereafter (keyed by build-stamped bundle_id).

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

/// The compressed runtime archive embedded at build time. `build.rs` writes
/// either the real bundle or a zero-byte placeholder to this path.
const EMBEDDED_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.tar.zst"));

/// Build-stamped identity of the embedded archive.
pub const BUNDLE_ID: &str = env!("HI_AGENT_BUNDLE_ID");

/// Relative paths inside the archive, recorded by the bundle script.
#[derive(Debug, Deserialize)]
struct RuntimeManifest {
    node: String,
    adapter: String,
    claude: String,
}

/// Absolute paths to the extracted runtime components.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pub node_bin: PathBuf,
    pub adapter_entry: PathBuf,
    pub claude_bin: PathBuf,
}

impl ResolvedRuntime {
    /// Directory containing the `node` binary (for PATH prefixing).
    pub fn node_bin_dir(&self) -> &Path {
        self.node_bin.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// Resolve the embedded runtime: extract on first run, reuse thereafter.
///
/// Errors if the embedded archive is empty (a dev build produced without
/// `make bundle`); set the dev escape-hatch env vars in that case (see
/// `resolve_dev_override`).
pub fn ensure() -> anyhow::Result<ResolvedRuntime> {
    if let Some(dev) = resolve_dev_override() {
        tracing::warn!("using HI_AGENT_DEV_* runtime override (unsupported, debug only)");
        return Ok(dev);
    }
    if EMBEDDED_ARCHIVE.is_empty() {
        return Err(anyhow!(
            "no runtime bundled (empty embedded archive); run `make bundle` or set \
             HI_AGENT_DEV_NODE / HI_AGENT_DEV_ADAPTER / HI_AGENT_DEV_CLAUDE"
        ));
    }
    let cache_root = cache_root()?;
    extract_bundle(EMBEDDED_ARCHIVE, BUNDLE_ID, &cache_root)
}

/// Base cache dir, overridable by `HI_AGENT_RUNTIME_DIR`.
fn cache_root() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("HI_AGENT_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("dev", "human-interface", "hi-agent")
        .ok_or_else(|| anyhow!("cannot determine OS cache dir"))?;
    Ok(dirs.cache_dir().join("runtime"))
}

/// Extract `archive` (a .tar.zst) for `bundle_id` under `cache_root`, atomically
/// and idempotently. Reuses an existing complete extraction.
pub fn extract_bundle(
    archive: &[u8],
    bundle_id: &str,
    cache_root: &Path,
) -> anyhow::Result<ResolvedRuntime> {
    let target = cache_root.join(bundle_id);
    let marker = target.join(".complete");

    if marker.exists() {
        return resolve(&target);
    }

    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("creating cache root {}", cache_root.display()))?;

    // Extract into a unique temp dir, then rename into place.
    let tmp = cache_root.join(format!(".{bundle_id}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;

    let decoder = zstd::Decoder::new(archive).context("opening zstd decoder")?;
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(&tmp).context("unpacking runtime archive")?;

    std::fs::write(tmp.join(".complete"), b"")?;

    // Atomic publish. If another process won the race, drop ours and reuse.
    match std::fs::rename(&tmp, &target) {
        Ok(()) => {}
        Err(_) if marker.exists() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(anyhow!("publishing runtime to {}: {e}", target.display()));
        }
    }

    // GC stale sibling bundles (best effort).
    if let Ok(entries) = std::fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name != bundle_id && !name.starts_with('.') {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    resolve(&target)
}

/// Read `runtime.json` from an extracted dir and build absolute paths.
fn resolve(target: &Path) -> anyhow::Result<ResolvedRuntime> {
    let manifest_path = target.join("runtime.json");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let m: RuntimeManifest = serde_json::from_str(&text).context("parsing runtime.json")?;
    Ok(ResolvedRuntime {
        node_bin: target.join(m.node),
        adapter_entry: target.join(m.adapter),
        claude_bin: target.join(m.claude),
    })
}

/// Dev escape hatch (debug only): point at an external runtime via env so
/// `cargo run` works before the bundle pipeline exists. Returns `Some` only if
/// all three vars are set.
fn resolve_dev_override() -> Option<ResolvedRuntime> {
    let node = std::env::var("HI_AGENT_DEV_NODE").ok()?;
    let adapter = std::env::var("HI_AGENT_DEV_ADAPTER").ok()?;
    let claude = std::env::var("HI_AGENT_DEV_CLAUDE").ok()?;
    Some(ResolvedRuntime {
        node_bin: PathBuf::from(node),
        adapter_entry: PathBuf::from(adapter),
        claude_bin: PathBuf::from(claude),
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test runtime_bundle`
Expected: PASS. (This requires Task 7's `build.rs` to provide `OUT_DIR/runtime.tar.zst` and `HI_AGENT_BUNDLE_ID`; if it fails to compile with "environment variable not found", do Task 7 first, then return here.)

> **Ordering note:** `include_bytes!(concat!(env!("OUT_DIR"), ...))` and `env!("HI_AGENT_BUNDLE_ID")` need Task 7's build.rs. If the worker is doing tasks strictly in order, the implementation in Step 3 will not compile until Task 7 lands. **Do Task 7 before running this task's Step 4.** Steps 1-3 here (writing files) are still valid in order; just defer the `cargo test` to after Task 7.

- [ ] **Step 5: Commit**

```bash
git add src/runtime/mod.rs tests/runtime_bundle.rs
git commit -m "feat: RuntimeBundle extraction (atomic, idempotent, gc)"
```

---

## Task 7: `build.rs` — embed archive + stamp versions

**Files:**
- Modify: `build.rs`

- [ ] **Step 1: Write the new `build.rs`**

Replace `build.rs` with:

```rust
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/appearance/web/dist");
    println!("cargo:rerun-if-changed=runtime/embed");
    println!("cargo:rerun-if-env-changed=HI_AGENT_TARGET");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("runtime.tar.zst");

    // Pick the archive for the active (or overridden) target triple.
    let target = std::env::var("HI_AGENT_TARGET")
        .or_else(|_| std::env::var("TARGET"))
        .unwrap_or_default();
    let candidate = PathBuf::from("runtime/embed").join(format!("{target}.tar.zst"));

    if candidate.exists() {
        std::fs::copy(&candidate, &dest).expect("copying runtime archive to OUT_DIR");
        println!("cargo:warning=embedding runtime bundle: {}", candidate.display());
    } else {
        // Dev build without a bundle: embed a zero-byte placeholder so the
        // binary still compiles and runs (with the dev runtime override).
        std::fs::write(&dest, b"").expect("writing placeholder runtime archive");
        println!(
            "cargo:warning=no runtime bundle for target '{target}' — embedding empty placeholder \
             (run `make bundle`)"
        );
    }

    // Version stamps surfaced by `--version` and used as the cache key.
    let manifest = read_manifest_versions();
    println!("cargo:rustc-env=HI_AGENT_BUNDLE_ID={}", manifest.bundle_id);
    println!("cargo:rustc-env=HI_AGENT_NODE_VERSION={}", manifest.node_version);
    println!("cargo:rustc-env=HI_AGENT_ADAPTER_VERSION={}", manifest.adapter_version);
    println!("cargo:rustc-env=HI_AGENT_CLAUDE_VERSION={}", manifest.claude_version);
}

struct ManifestVersions {
    bundle_id: String,
    node_version: String,
    adapter_version: String,
    claude_version: String,
}

/// Minimal manifest read. Avoids extra build-deps by scanning for keys; falls
/// back to "dev" placeholders when the manifest or bundle is absent.
fn read_manifest_versions() -> ManifestVersions {
    let text = std::fs::read_to_string("runtime/manifest.toml").unwrap_or_default();
    let get = |key: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let l = l.trim();
            let prefix = format!("{key} =");
            l.strip_prefix(&prefix)
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    };
    let node_version = get("node_version").unwrap_or_else(|| "dev".to_string());
    let bundle_version = get("bundle_version").unwrap_or_else(|| "dev".to_string());
    let adapter_version = get("adapter_version").unwrap_or_else(|| "dev".to_string());
    let claude_version = get("claude_version").unwrap_or_else(|| "dev".to_string());

    // The archive's own bytes are the real key; here we approximate with the
    // pinned versions. The bundle script writes claude_version into the
    // manifest after `npm ci` resolves it.
    let bundle_id = format!("{bundle_version}-node{node_version}-adapter{adapter_version}");

    ManifestVersions { bundle_id, node_version, adapter_version, claude_version }
}
```

- [ ] **Step 2: Verify it compiles and stamps**

Run: `cargo build 2>&1 | grep -i "runtime bundle"`
Expected: a `cargo:warning` line about embedding an empty placeholder (no bundle yet).

- [ ] **Step 3: Run the previously-deferred runtime test**

Run: `cargo test --test runtime_bundle`
Expected: PASS (now that `OUT_DIR/runtime.tar.zst` and `HI_AGENT_BUNDLE_ID` exist).

- [ ] **Step 4: Commit**

```bash
git add build.rs
git commit -m "feat: build.rs embeds runtime archive and stamps versions"
```

---

## Task 8: Bundle manifest + script

**Files:**
- Create: `runtime/manifest.toml`
- Create: `runtime/package.json`
- Create: `scripts/bundle.sh`
- Modify: `Makefile`
- Modify: `.gitignore`

- [ ] **Step 1: Create `runtime/manifest.toml`**

```toml
# Pinned runtime bundle. Bump bundle_version whenever any pin changes.
bundle_version  = "1"
node_version    = "22.14.0"
adapter_version = "0.36.1"
# claude_version is filled in by scripts/bundle.sh after npm ci resolves it.
claude_version  = "unknown"

# Per-target Node download + checksum. Fill node_sha256 from
# https://nodejs.org/dist/v22.14.0/SHASUMS256.txt
[targets.aarch64-apple-darwin]
node_url    = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-darwin-arm64.tar.gz"
node_sha256 = "FILL_ME"

[targets.x86_64-apple-darwin]
node_url    = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-darwin-x64.tar.gz"
node_sha256 = "FILL_ME"

[targets.x86_64-unknown-linux-gnu]
node_url    = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-linux-x64.tar.gz"
node_sha256 = "FILL_ME"

[targets.aarch64-unknown-linux-gnu]
node_url    = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-linux-arm64.tar.gz"
node_sha256 = "FILL_ME"

[targets.x86_64-pc-windows-msvc]
node_url    = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-win-x64.zip"
node_sha256 = "FILL_ME"
```

- [ ] **Step 2: Create `runtime/package.json`**

```json
{
  "name": "hi-agent-runtime",
  "private": true,
  "dependencies": {
    "@agentclientprotocol/claude-agent-acp": "0.36.1"
  }
}
```

(Generate `runtime/package-lock.json` once via `cd runtime && npm install --package-lock-only`, then commit it — this is the reproducibility lock.)

- [ ] **Step 3: Create `scripts/bundle.sh`**

```bash
#!/usr/bin/env bash
# scripts/bundle.sh — produce runtime/embed/<target>.tar.zst for one target.
#
# Usage: scripts/bundle.sh <rust-target-triple>
# Requires: node+npm (host, for `npm ci`), curl, shasum, tar, zstd, jq.
set -euo pipefail

TARGET="${1:?usage: bundle.sh <rust-target-triple>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/runtime/manifest.toml"
STAGE="$ROOT/runtime/staging/$TARGET"
OUT="$ROOT/runtime/embed/$TARGET.tar.zst"

read_key() { grep -E "^$1 *=" "$MANIFEST" | head -1 | sed 's/.*= *"\{0,1\}//; s/"\{0,1\} *$//'; }
read_target_key() {
  awk -v t="[targets.$TARGET]" -v k="$1" '
    $0==t {inblk=1; next}
    /^\[/ {inblk=0}
    inblk && $0 ~ "^"k" *=" { sub("^"k" *= *\"",""); sub("\" *$",""); print; exit }
  ' "$MANIFEST"
}

NODE_URL="$(read_target_key node_url)"
NODE_SHA="$(read_target_key node_sha256)"
[ -n "$NODE_URL" ] || { echo "no node_url for target $TARGET in manifest"; exit 1; }

rm -rf "$STAGE"; mkdir -p "$STAGE/node" "$STAGE/adapter"

# 1. Node — download, verify checksum, unpack into $STAGE/node (strip top dir).
TMP="$(mktemp -d)"; ARCHIVE="$TMP/node.archive"
curl -fsSL "$NODE_URL" -o "$ARCHIVE"
echo "$NODE_SHA  $ARCHIVE" | shasum -a 256 -c -
case "$NODE_URL" in
  *.zip) (cd "$STAGE/node" && unzip -q "$ARCHIVE" && mv */* . 2>/dev/null || true) ;;
  *)     tar -xzf "$ARCHIVE" -C "$STAGE/node" --strip-components=1 ;;
esac

# 2. Adapter + claude — npm ci against the committed lockfile.
cp "$ROOT/runtime/package.json" "$ROOT/runtime/package-lock.json" "$STAGE/adapter/"
(cd "$STAGE/adapter" && npm ci --omit=dev)

# 3. Resolve relative paths for runtime.json.
NODE_BIN_REL="node/bin/node"; [ -f "$STAGE/node/node.exe" ] && NODE_BIN_REL="node/node.exe"
ADAPTER_REL="adapter/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js"
CLAUDE_REL="$(cd "$STAGE/adapter" && node -e "process.stdout.write(require('path').relative('$STAGE', require.resolve('@anthropic-ai/claude-agent-sdk/cli.js')))" 2>/dev/null || true)"
[ -n "$CLAUDE_REL" ] || CLAUDE_REL="adapter/node_modules/@anthropic-ai/claude-agent-sdk/cli.js"

cat > "$STAGE/runtime.json" <<JSON
{ "node": "$NODE_BIN_REL", "adapter": "$ADAPTER_REL", "claude": "$CLAUDE_REL" }
JSON

# 4. Record resolved claude version back into the manifest (best effort).
CLAUDE_VER="$(cd "$STAGE/adapter" && node -e "process.stdout.write(require('@anthropic-ai/claude-agent-sdk/package.json').version)" 2>/dev/null || echo unknown)"
echo "resolved claude/agent-sdk version: $CLAUDE_VER (update manifest claude_version)"

# 5. Pack.
mkdir -p "$ROOT/runtime/embed"
tar -C "$STAGE" -cf - . | zstd -19 -o "$OUT" -f
echo "wrote $OUT"
```

- [ ] **Step 4: Add a Makefile target**

In `Makefile`, add (use the host triple by default):

```makefile
bundle: ## Build the embedded runtime archive for the host target
	@scripts/bundle.sh $$(rustc -vV | sed -n 's/host: //p')
```

- [ ] **Step 5: Ignore staging artifacts**

Append to `.gitignore`:

```
/runtime/staging/
/runtime/embed/
```

(The committed sources are `manifest.toml`, `package.json`, `package-lock.json`, and `bundle.sh`. The large per-target archives are build outputs, fetched/built in CI, not committed.)

- [ ] **Step 6: Make the script executable and smoke-test on the host**

```bash
chmod +x scripts/bundle.sh
# Fill node_sha256 for your host target in runtime/manifest.toml first, then:
make bundle
```

Expected: `wrote runtime/embed/<host-triple>.tar.zst`. (If `node_sha256` is still `FILL_ME`, the checksum step fails — fill it from the Node SHASUMS file.)

- [ ] **Step 7: Commit**

```bash
git add runtime/manifest.toml runtime/package.json runtime/package-lock.json scripts/bundle.sh Makefile .gitignore
git commit -m "feat: runtime bundle manifest + build script (pinned + checksummed)"
```

---

## Task 9: `AcpProcess::spawn` accepts child env

**Files:**
- Modify: `src/acp/process.rs:56-78` (the `spawn` fn signature + argv assembly)
- Modify: `examples/acp_spike.rs` (caller) and any other callers

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/acp/process.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_argv_prepends_env_pairs() {
        // White-box the argv assembly used by spawn(): env pairs come first as
        // NAME=value so AcpAgent::from_args treats them as child env.
        let env = vec![
            ("FOO".to_string(), "bar".to_string()),
            ("BAZ".to_string(), "qux".to_string()),
        ];
        let argv = super::build_argv("node", &["adapter.js".to_string()], &env);
        assert_eq!(argv, vec!["FOO=bar", "BAZ=qux", "node", "adapter.js"]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test acp::process::tests`
Expected: FAIL — `build_argv` not found.

- [ ] **Step 3: Refactor `spawn` to take env and extract `build_argv`**

In `src/acp/process.rs`, change the `spawn` signature and argv assembly. Replace the current head of the function (lines ~56-66):

```rust
    pub async fn spawn(program: PathBuf, args: Vec<String>) -> anyhow::Result<Self> {
        let program_str = program
            .to_str()
            .ok_or_else(|| anyhow!("program path is not valid UTF-8: {}", program.display()))?
            .to_string();
        let mut argv: Vec<String> = Vec::with_capacity(1 + args.len());
        argv.push(program_str);
        argv.extend(args);
```

with:

```rust
    pub async fn spawn(
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<Self> {
        let program_str = program
            .to_str()
            .ok_or_else(|| anyhow!("program path is not valid UTF-8: {}", program.display()))?
            .to_string();
        let argv = build_argv(&program_str, &args, &env);
```

Then add this free function below the `impl AcpProcess` block (before the `Drop` impl):

```rust
/// Assemble the argv for `AcpAgent::from_args`: leading `NAME=value` env pairs,
/// then the program and its args. `from_args` parses leading env pairs into the
/// child process env (applied atop the inherited parent env).
fn build_argv(program: &str, args: &[String], env: &[(String, String)]) -> Vec<String> {
    let mut argv = Vec::with_capacity(env.len() + 1 + args.len());
    for (k, v) in env {
        argv.push(format!("{k}={v}"));
    }
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());
    argv
}
```

- [ ] **Step 4: Update existing callers**

In `examples/acp_spike.rs:33`, change:

```rust
    let process = Arc::new(AcpProcess::spawn(binary, args).await?);
```
to:
```rust
    let process = Arc::new(AcpProcess::spawn(binary, args, Vec::new()).await?);
```

(The `src/lib.rs:42` caller is updated in Task 10.)

- [ ] **Step 5: Run test + build to verify**

Run: `cargo test acp::process::tests && cargo build --examples`
Expected: PASS; examples compile. (`src/lib.rs` still calls the old 2-arg form and will fail to build the lib — that is fixed in Task 10. To check this task in isolation run `cargo test acp::process::tests --lib 2>&1` and expect the test itself to pass; the lib build error at `src/lib.rs:42` is expected until Task 10.)

- [ ] **Step 6: Commit**

```bash
git add src/acp/process.rs examples/acp_spike.rs
git commit -m "feat: AcpProcess::spawn accepts child env vars"
```

---

## Task 10: Wire startup + `--version`

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Extend `Config` and rewrite the spawn wiring in `src/lib.rs`**

Replace the `Config` struct (lines 16-20) with:

```rust
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    pub agent: config::AgentConfig,
}
```

In `run()`, replace the ACP spawn block (currently lines 40-44):

```rust
    let acp = Arc::new(
        acp::AcpProcess::spawn("claude-agent-acp".into(), Vec::new()).await?,
    );
    tracing::info!("ACP subprocess up");
```

with:

```rust
    // Resolve the bundled runtime (extract on first run).
    let runtime = runtime::ensure()?;
    tracing::info!(bundle_id = runtime::BUNDLE_ID, "runtime resolved");

    // Start the local LLM proxy; the adapter talks to it instead of the upstream.
    let proxy = llm_proxy::LlmProxy::start(
        config.agent.upstream_base_url.clone(),
        config.agent.upstream_key.clone(),
    )
    .await?;

    // Render the managed settings.json into a hi-agent-owned config dir.
    let claude_config_dir = config.data_dir.join("claude-config");
    config.agent.render_settings_json(&claude_config_dir)?;

    // Spawn the adapter via the bundled node, with managed env.
    let child_env = config.agent.child_env(
        proxy.port(),
        &claude_config_dir,
        runtime.node_bin_dir(),
        &runtime.claude_bin,
    );
    let adapter_entry = runtime
        .adapter_entry
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("adapter path not UTF-8"))?
        .to_string();
    let acp = Arc::new(
        acp::AcpProcess::spawn(runtime.node_bin.clone(), vec![adapter_entry], child_env).await?,
    );
    tracing::info!("ACP subprocess up (bundled node + adapter)");

    // Keep the proxy alive for the life of the process.
    let _proxy = proxy;
```

- [ ] **Step 2: Update `src/main.rs` to build the new `Config` and report versions**

Replace `src/main.rs` with:

```rust
use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "hi-agent", about = "Reference implementation of the human-interface spec")]
#[command(version = version_string())]
struct Cli {
    /// HTTP port to bind on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Where journal.jsonl lives.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

/// Version line including bundled component versions.
fn version_string() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (bundle ", env!("HI_AGENT_BUNDLE_ID"),
        "; node ", env!("HI_AGENT_NODE_VERSION"),
        "; adapter ", env!("HI_AGENT_ADAPTER_VERSION"),
        "; claude ", env!("HI_AGENT_CLAUDE_VERSION"), ")"
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let agent = hi_agent::config::AgentConfig::load()?;
    let config = hi_agent::Config {
        port: cli.port,
        data_dir: cli.data_dir,
        agent,
    };

    hi_agent::run(config).await
}
```

- [ ] **Step 3: Build the whole workspace**

Run: `cargo build`
Expected: PASS.

- [ ] **Step 4: Verify `--version` reports bundle info**

Run: `cargo run -- --version`
Expected: a line like `hi-agent 0.1.0 (bundle 1-node22.14.0-adapter0.36.1; node 22.14.0; adapter 0.36.1; claude unknown)`.

- [ ] **Step 5: Update any integration tests that construct `Config`**

Run: `grep -rn "hi_agent::Config\|Config {" tests/ src/` and add `agent: <AgentConfig>` to any `Config { ... }` literal. For test construction use:
```rust
agent: hi_agent::config::AgentConfig::from_toml_str(
    r#"upstream_base_url = "https://x/v1""#, "test-key".to_string()).unwrap(),
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo test`
Expected: PASS (integration tests that actually start the agent need either a bundle or the `HI_AGENT_DEV_*` override and a reachable upstream — see Task 11 for how those are gated).

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/main.rs tests/
git commit -m "feat: wire runtime+proxy+config into startup, report bundle in --version"
```

---

## Task 11: Gated end-to-end test + docs

**Files:**
- Create: `tests/e2e_cognition.rs`
- Modify: `README.md`, `.env.example`

- [ ] **Step 1: Write the gated e2e test**

Create `tests/e2e_cognition.rs`. It only runs when `RUN_E2E=1` and a bundle (or dev override) plus a stub upstream are available, mirroring the existing `RUN_INTEGRATION_TESTS` convention:

```rust
//! Full-stack smoke: real bundled adapter ↔ local proxy ↔ stub upstream.
//! Opt-in: `RUN_E2E=1 cargo test --test e2e_cognition -- --nocapture`.

#[tokio::test]
async fn thought_round_trips_through_bundle() {
    if std::env::var("RUN_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping e2e (set RUN_E2E=1 to run)");
        return;
    }
    // A real run requires:
    //   - a built bundle (`make bundle`) OR HI_AGENT_DEV_NODE/ADAPTER/CLAUDE set,
    //   - HI_AGENT_UPSTREAM_KEY set,
    //   - upstream_base_url pointing at a reachable Anthropic-compatible endpoint
    //     (or a local stub that returns a canned Messages SSE stream).
    // Build a Config, call hi_agent::run on a random port in a task, POST a
    // /thought, and assert a journal line / thought-bus emission appears.
    // (Left as the single heavy integration check; keep it deterministic by
    //  pointing upstream at a local stub rather than the real API.)
    eprintln!("e2e harness placeholder — implement against a local SSE stub");
}
```

> This is the one place a stub is described rather than fully coded, because the assertion target (journal vs thought-bus) depends on reactor internals the worker should read at implementation time. Keep it gated so `cargo test` stays green without a bundle.

- [ ] **Step 2: Update `.env.example`**

Add under the Core section:

```
# Upstream LLM credential — injected by the local proxy, never written to disk
# in any claude/adapter config. Required.
# HI_AGENT_UPSTREAM_KEY=

# Path to the dev config file (default ./config.toml).
# HI_AGENT_CONFIG=config.toml

# Dev-only runtime override (point at an external node/adapter/claude when
# building without `make bundle`). Unsupported; debug use only.
# HI_AGENT_DEV_NODE=
# HI_AGENT_DEV_ADAPTER=
# HI_AGENT_DEV_CLAUDE=

# Override the runtime extraction cache dir (default: OS cache dir).
# HI_AGENT_RUNTIME_DIR=
```

Remove the now-obsolete `CLAUDE_CODE_BIN` / `CLAUDE_CODE_ARGS` lines from `.env.example`.

- [ ] **Step 3: Update `README.md`**

- In Prerequisites: replace the Node / `claude-code on PATH` requirements with "none — the runtime is bundled (build from source needs Node only to produce the bundle via `make bundle`)."
- In Configuration: replace the `CLAUDE_CODE_BIN` / `CLAUDE_CODE_ARGS` table rows with `HI_AGENT_UPSTREAM_KEY`, `HI_AGENT_CONFIG`, `HI_AGENT_RUNTIME_DIR`, and a pointer to `config.toml` for managed parameters. Add a short "Bundled runtime & versioning" subsection summarizing `runtime/manifest.toml`, `make bundle`, and `--version`.

- [ ] **Step 4: Verify docs build/links and tests stay green**

Run: `cargo test && cargo run -- --version`
Expected: tests PASS; version prints. `RUN_E2E` unset → e2e test prints skip and passes.

- [ ] **Step 5: Commit**

```bash
git add tests/e2e_cognition.rs .env.example README.md
git commit -m "docs+test: gated e2e harness, env + README for bundled runtime"
```

---

## Self-review notes (for the implementer)

- **Spec coverage:** embedded single-binary runtime (Tasks 6-8), local Anthropic proxy with key injection (Task 5), env+settings.json managed params (Tasks 2-4), startup wiring replacing `src/lib.rs:42` (Task 10), versioning pinned+checksummed manifest + build params + `--version` (Tasks 7-8, 10), error handling on missing key / extraction / proxy bind (Tasks 2, 6, 10), tests for config/proxy/runtime + gated e2e (Tasks 2-6, 11). Approach C's per-session ACP seam is intentionally *not* implemented (spec Open Questions).
- **Build ordering caveat:** Task 6's implementation uses `include_bytes!(OUT_DIR)` + `env!("HI_AGENT_BUNDLE_ID")` which only exist after Task 7. The plan flags this in Task 6 Step 4 — do Task 7 before running Task 6's tests. Likewise Task 9's lib build is only green after Task 10 updates the `src/lib.rs:42` caller.
- **Secret hygiene:** the upstream key is read from env only (`HI_AGENT_UPSTREAM_KEY`), never written to `config.toml`, `settings.json`, or the child env beyond the proxy. The adapter receives a placeholder key.
- **`node_sha256 = "FILL_ME"`:** intentional — checksums must be filled from the official Node SHASUMS file when versions are chosen; `make bundle` fails loudly until they are.
```
