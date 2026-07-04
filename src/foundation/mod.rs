//! foundation — the engine. Factory-sealed machinery with no agent-meaning,
//! beneath the faculties (`identity`/`mind`/`body`); nothing here imports a
//! faculty, faculties are built on it.
//!
//! - LLM/agent gateway: `acp` (JSON-RPC wire + sessions), `agent` (one subprocess
//!   per session), `mcp` (tool transport).
//! - Transport + serving: `server` (HTTP front, channels, carriers).
//! - Vendor adapters: `vendors` (macOS, doubao, volcengine, ffmpeg, onnx fronts).
//! - Plumbing: `config`, `credentials`, `models`, `observatory`, `channel_log`, `pcm`, `segment`.
//!
//! Three engine-ish modules deliberately stay at the crate root rather than move
//! here, because relocating them would break hardcoded build paths the rest of the
//! toolchain depends on:
//! - `appearance` — carries the whole SPA build project (`web/`) and embeds
//!   `src/appearance/web/dist/` via RustEmbed; `runtime` — embeds its npm
//!   `package.json`/`-lock.json` by `CARGO_MANIFEST_DIR` path and is what the
//!   Makefile/dev-server build against.
//! - `types` — shared cross-faculty vocabulary (Channel/Scene/Signal/…); domain
//!   data crossing every boundary, not engine machinery.

pub mod acp;
pub mod agent;
pub mod auth;
pub mod broker;
pub mod channel_log;
pub mod config;
pub mod credentials;
pub mod energy_state;
pub mod mcp;
pub mod models;
pub mod observatory;
pub mod pcm;
pub mod segment;
pub mod server;
pub mod vendors;
