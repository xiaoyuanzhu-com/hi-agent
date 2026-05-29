# Bundled Claude Code + Managed Parameters — Design

design v0.1 · 2026-05-29

## Goal

Two outcomes:

1. **Works out of the box.** A user runs the hi-agent binary on macOS, Windows,
   or Linux and cognition works with nothing else installed — no Node, no
   `claude`, no npm, no manual `claude login`.
2. **Tunable configs.** The hi-agent developers control cognition parameters
   (model, effort, permission mode, thinking budget, system prompt) from an
   in-repo config, and control which upstream LLM endpoint is used.

Non-goals (v0): per-end-user runtime tuning of parameters; runtime upgrade of
the bundled runtime; OpenAI-shaped upstreams; federation/multi-tenant key
management.

## Background: the cognition chain

hi-agent (Rust) delegates cognition by spawning an ACP-speaking subprocess.
Today that is hardcoded at `src/lib.rs:42`:

```rust
acp::AcpProcess::spawn("claude-agent-acp".into(), Vec::new()).await?
```

The real dependency chain behind that name is:

```
hi-agent (Rust)
  └─ spawns: node <adapter>/dist/index.js          ← @agentclientprotocol/claude-agent-acp
       └─ uses: @anthropic-ai/claude-agent-sdk
            └─ drives: claude CLI (CLAUDE_CODE_EXECUTABLE)
                 └─ HTTP: POST /v1/messages → api.anthropic.com   (Anthropic Messages API)
```

So "bundle everything" means shipping a **Node runtime**, the **adapter +
its `node_modules`** (which includes the Agent SDK and the bundled `claude`
CLI), and redirecting the LLM HTTP calls to an endpoint we control.

The adapter resolves every parameter we care about at session start from **env
vars + a `settings.json`** in `CLAUDE_CONFIG_DIR` (verified against
`@agentclientprotocol/claude-agent-acp@0.36.1`):

| Parameter        | Source the adapter reads                                   |
|------------------|------------------------------------------------------------|
| model            | `ANTHROPIC_MODEL` env, or `settings.json: availableModels` |
| effort           | `settings.json: effortLevel`                               |
| permission mode  | `settings.json: permissions.defaultMode`                   |
| thinking budget  | `MAX_THINKING_TOKENS` env                                  |
| LLM endpoint     | `ANTHROPIC_BASE_URL` env (Agent SDK)                       |
| LLM credential   | `ANTHROPIC_API_KEY` env (Agent SDK)                        |
| claude binary    | `CLAUDE_CODE_EXECUTABLE` env                                |
| config location  | `CLAUDE_CONFIG_DIR` env                                     |

This means managed parameters need **no ACP protocol changes** for v0 — they
flow through env + a generated `settings.json`.

## Architecture

Three new pieces inside the existing Rust process, plus a build-time bundling
step.

```
                       hi-agent (Rust process)
  ┌──────────────────────────────────────────────────────────────┐
  │  startup:                                                      │
  │   1. RuntimeBundle::ensure()  ──► extract embedded runtime to  │
  │                                   <cache>/hi-agent/<bundle_id> │
  │   2. LlmProxy::start()        ──► 127.0.0.1:<ephemeral>        │
  │   3. render settings.json     ──► <managed CLAUDE_CONFIG_DIR>  │
  │   4. AcpProcess::spawn(node, [adapter], env)                   │
  │                                                                │
  │   ┌───────────────┐   /v1/messages    ┌──────────────────┐    │
  │   │  LlmProxy     │◄──────────────────│  ACP child:      │    │
  │   │  (axum)       │   (Anthropic SSE) │  node + adapter  │    │
  │   │  inject key,  │──────────────────►│  + claude CLI    │    │
  │   │  forward      │                   └──────────────────┘    │
  │   └──────┬────────┘                                           │
  └──────────┼───────────────────────────────────────────────────┘
             │ HTTPS, real upstream key
             ▼
       upstream LLM (Anthropic-compatible /v1/messages)
```

### Component 1 — Embedded runtime (`src/runtime/`)

**Responsibility:** make a working `node` + adapter + `claude` available on the
user's disk, extracted from bytes embedded in the hi-agent binary.

- **What is embedded:** a single compressed archive (`.tar.zst`) containing the
  platform's Node runtime, the adapter, and its full `node_modules` (Agent SDK
  + `claude` CLI). Produced by the build step (below) and embedded via
  `rust-embed` / `include_bytes!`.
- **`bundle_id`:** `hash(node_version + lockfile_hash + bundle_version)`,
  stamped into the binary at build time via `cargo:rustc-env`. Identifies the
  exact runtime contents.
- **`ensure()` at startup:**
  - target dir = `<cache>/hi-agent/<bundle_id>/` (`<cache>` =
    `directories`-style OS cache dir, overridable by config).
  - if the dir exists and is marked complete → reuse, return resolved paths.
  - else extract the archive to a sibling temp dir, write a `COMPLETE` marker,
    `rename` into place (atomic against concurrent/interrupted starts).
  - GC sibling dirs whose name ≠ current `bundle_id` (best-effort).
- **Returns** resolved absolute paths: `node_bin`, `adapter_entry`
  (`.../dist/index.js`), `claude_bin`.
- **Per-OS detail:** Windows uses `node.exe` and `;`-separated `PATH`; the
  embedded archive is platform-specific (one hi-agent binary per OS/arch).

### Component 2 — Local LLM proxy (`src/llm_proxy/`)

**Responsibility:** terminate the adapter's Anthropic API calls locally and
forward them to the configured upstream with the real credential, so the
upstream key never lives in any `claude`/adapter config on disk.

- Binds `127.0.0.1:0` (ephemeral port); the bound port is read back and passed
  to the ACP child as `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>`.
- Reverse-proxies `POST /v1/messages` and `POST /v1/messages/count_tokens`
  (and any other path the adapter calls) to `<upstream_base_url>` using
  `reqwest` (already a dependency).
- **Header handling:** strips the placeholder client credential, injects the
  real upstream key (`x-api-key` or `Authorization: Bearer`, configurable),
  preserves `anthropic-version` / `anthropic-beta` and content headers.
- **Streaming:** passes the SSE response body through unbuffered
  (`reqwest::Response::bytes_stream` → axum `Body::from_stream`).
- **Thin by default.** Optional, behind a clearly-marked seam: rewrite the
  `model` field. Off in v0 — model is set via the managed config path instead.
- Errors from upstream are forwarded verbatim (status + body) so the adapter's
  own retry/error handling behaves as it would against the real API.

### Component 3 — Managed config (`src/config/`)

**Responsibility:** one in-repo source of dev-tunable cognition parameters, plus
runtime wiring of env + `settings.json`.

- **`AgentConfig`** — an in-repo config file (`config.toml`, committed) holding
  non-secret tunables:
  ```toml
  model            = "claude-opus-4-8"   # → ANTHROPIC_MODEL
  effort           = "high"              # → settings.json effortLevel
  permission_mode  = "acceptEdits"       # → settings.json permissions.defaultMode
  max_thinking_tokens = 10000            # → MAX_THINKING_TOKENS
  upstream_base_url = "https://..."      # proxy target
  # system_prompt composed by the reactor as today (ACP has no system slot)
  ```
- **Secret stays out of git.** The upstream key is read from an env var
  (`HI_AGENT_UPSTREAM_KEY`), loaded via the existing `dotenvy` `.env` flow
  (`.env` is gitignored). The committed config holds only the URL and
  non-secret tunables.
- **`render_settings_json()`** — writes a managed `settings.json` into a
  hi-agent-owned `CLAUDE_CONFIG_DIR` (under the data/cache dir, isolated from
  the user's personal `~/.claude`) from `AgentConfig`.
- **Child env assembly** — produces the env map for the ACP child:
  `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` (placeholder; proxy supplies the
  real one), `ANTHROPIC_MODEL`, `MAX_THINKING_TOKENS`, `CLAUDE_CONFIG_DIR`,
  `CLAUDE_CODE_EXECUTABLE`, and `PATH` prefixed with the extracted `node` dir.

### Wiring change (`src/lib.rs`)

`AcpProcess::spawn` currently takes `(program, args)`. It gains the ability to
pass an env map (new field on a spawn-opts struct, or a new
`spawn_with_env(program, args, envs)`). Startup sequence in `run()`:

```
config      = AgentConfig::load()?            // in-repo config + .env secret
runtime     = RuntimeBundle::ensure(&config)? // extract embedded runtime
proxy       = LlmProxy::start(&config).await? // 127.0.0.1:<port>
config.render_settings_json(&managed_dir)?
let env = config.child_env(&runtime, proxy.port(), &managed_dir);
let acp = AcpProcess::spawn(runtime.node_bin, vec![runtime.adapter_entry], env).await?;
```

## Versioning & build

**Pin policy: fully pinned + checksums (reproducible).**

- **`runtime/manifest.toml`** (committed) is the single source of truth:
  ```toml
  bundle_version = "1"             # bump when any pin below changes
  node_version   = "22.14.0"       # exact LTS
  adapter        = "@agentclientprotocol/claude-agent-acp@0.36.1"  # exact
  # claude CLI + Agent SDK locked transitively via committed package-lock.json

  [targets.aarch64-apple-darwin]
  node_url    = "https://nodejs.org/dist/v22.14.0/node-v22.14.0-darwin-arm64.tar.gz"
  node_sha256 = "..."
  # one block per target: aarch64-apple-darwin, x86_64-apple-darwin,
  # x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
  ```
- **`make bundle TARGET=<triple>`** resolves the manifest, downloads Node,
  **verifies SHA256**, runs `npm ci` against the committed `package-lock.json`
  into `runtime/staging/<triple>/`, and packs `runtime/embed/<triple>.tar.zst`.
- **`build.rs`** embeds the staged archive for the active target and stamps
  `cargo:rustc-env` constants: `HI_AGENT_BUNDLE_ID`, `HI_AGENT_NODE_VERSION`,
  `HI_AGENT_ADAPTER_VERSION`, `HI_AGENT_CLAUDE_VERSION`.
- **Build parameters**, priority order: env overrides
  (`HI_AGENT_NODE_VERSION`, `HI_AGENT_ADAPTER_VERSION`, `HI_AGENT_TARGET`) →
  `manifest.toml` → `package-lock.json`. Env overrides are for dev
  experimentation; reproducible release builds use the manifest unchanged.
- **`hi-agent --version`** reports the crate version plus bundled Node, adapter,
  `claude`, and `bundle_id`.
- **Upgrade path: build-time only.** Changing any bundled version requires a new
  hi-agent build (new `bundle_id` → automatic re-extract on the user's machine).
  No runtime fetch/override in the supported model. (A dev-only escape to point
  at an external `node`/`claude` may exist for local debugging but is not part
  of the shipped contract.)
- **CI matrix** builds one artifact per OS/arch target listed in the manifest.

## Data flow (one routing turn)

1. Peer `POST /thought` → reactor (unchanged).
2. Reactor opens an ACP session on the already-spawned child (unchanged API).
3. Adapter/`claude` issues `POST /v1/messages` to `ANTHROPIC_BASE_URL` (the
   local proxy).
4. Proxy injects the real upstream key, forwards, streams the SSE reply back.
5. Adapter streams ACP `SessionUpdate`s back to the reactor (unchanged).

Only step 3–4 are new; the reactor/session/memory layers are untouched.

## Error handling

- **Extraction failure** (disk full, permissions): fatal at startup with a clear
  message naming the cache dir; no partial dir is left marked complete.
- **Proxy bind failure:** fatal at startup (cognition cannot work without it).
- **Missing upstream key:** fatal at startup with a message pointing at
  `HI_AGENT_UPSTREAM_KEY` / `.env`.
- **Upstream errors (4xx/5xx):** forwarded verbatim to the adapter, which
  surfaces them through normal ACP error paths; logged at `warn`.
- **bundle_id mismatch / corrupt archive:** re-extract; if the embedded archive
  itself fails to decompress, that's a build defect → fatal with `bundle_id` in
  the message.
- **Checksum mismatch at build:** `make bundle` aborts.

## Testing

- **Config:** `AgentConfig` load + `.env` secret resolution + missing-key error;
  `settings.json` rendering golden test.
- **Proxy:** unit test against a mock upstream (httpmock/wiremock) — header
  injection, SSE pass-through, error forwarding. No real network.
- **Runtime extraction:** `ensure()` into a tempdir — fresh extract, reuse,
  interrupted (no marker) re-extract, stale-`bundle_id` GC.
- **Integration (gated, opt-in like existing `RUN_INTEGRATION_TESTS`):** full
  startup against a stub upstream that returns a canned Messages SSE stream;
  assert a `/thought` round-trips. Heavy (extracts the real runtime) → behind a
  feature/env gate, not in the default `cargo test`.
- **`--version`:** asserts stamped constants are non-empty.

## Open questions / future work

- **Per-session params over ACP** (approach B): when router vs worker need
  different model/effort, drive ACP config options
  (`unstable_setSessionModel`, `effort`) per session. Seam left in
  `AgentConfig` → `SessionOpts`; not built in v0.
- **Binary size:** embedding Node + `node_modules` is ~150–250 MB pre-compression;
  `.tar.zst` mitigates. Measure on first build; revisit if release artifacts are
  too large.
- **Self-update:** cache layout (`<bundle_id>` dirs) is already compatible with a
  future fetch-and-swap, if the build-time-only stance ever changes.
