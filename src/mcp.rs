//! In-process MCP server — router toolbelt.
//!
//! Impl.md § "Router toolbelt (via in-process MCP)": each routing ACP session
//! is given a set of tools (`speak`, `spawn_worker`, `cancel_worker`,
//! `list_workers`, `set_intent`, `recall`, `note`) that mutate the runtime
//! through the memory and reactor APIs. Sessions never touch journal or
//! intents files directly — every write goes through these handlers.
//!
//! ## Transport
//!
//! ACP requires Stdio MCP transport (`McpServerStdio` is mandatory; HTTP/SSE
//! are capability-gated). To keep the actual tool implementations in this
//! process we re-exec the hi-agent binary as `hi-agent mcp-shim`, passing
//! through the per-session tag in an environment variable. The shim opens a
//! Unix socket back to the hub, writes a one-line handshake carrying the
//! session tag, then bidirectionally relays line-framed JSON-RPC between its
//! stdio (which claude-code drives) and the socket (which the hub serves).
//!
//! The hub:
//!   - listens on `data/mcp.sock` (or `$HI_AGENT_MCP_SOCK`);
//!   - accepts one connection per routing session;
//!   - matches the handshake tag to a registered peer;
//!   - runs a hand-rolled JSON-RPC 2.0 loop that implements `initialize`,
//!     `tools/list`, `tools/call`, and `notifications/cancelled` (no-op).
//!
//! ## Why not rmcp?
//!
//! Impl.md leaves the choice open. rmcp's macro-driven server surface adds
//! one more dependency and one more compile-time abstraction for a v0 with
//! seven tools and a single transport. A direct JSON-RPC loop is ~250 lines
//! and matches the rest of the codebase's pragmatic style.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::acp::McpServerCfg;
use crate::memory::Memory;
use crate::types::{Channel, Intent, IntentTrigger, JournalEntry, PeerId};

pub use shim::run_shim_from_stdio;

/// The MCP server name reported to the ACP agent.
const SERVER_NAME: &str = "hi-agent-router";

/// MCP protocol version we speak. claude-code accepts the 2024-11-05 line.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Env var passed to the shim subprocess so it can find the hub.
const ENV_SOCK: &str = "HI_AGENT_MCP_SOCK";

/// Env var passed to the shim subprocess so the hub can identify the session.
const ENV_TAG: &str = "HI_AGENT_MCP_SESSION_TAG";

/// Env var conveying the session's role (router | worker). The shim does not
/// read it directly — it is plumbed through so the hub can keep the role
/// alongside the peer registration if a future refactor takes that path. The
/// hub today tracks the role on `register_session_with_role`, so this env var
/// is purely informational.
const ENV_ROLE: &str = "HI_AGENT_MCP_ROLE";

/// argv[1] sentinel for the shim mode. See `main.rs`.
pub const SHIM_FLAG: &str = "mcp-shim";

/// Which set of tools a session sees. Routers get the full toolbelt; workers
/// get only the emission/memory tools — they cannot spawn or cancel other
/// workers, and they do not list workers. See impl.md § Working layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Router,
    Worker,
}

// ---------------------------------------------------------------------------
// Reactor handle — the hub holds a weak reference to avoid an Arc cycle
// (reactor holds the hub; hub holds the reactor).
// ---------------------------------------------------------------------------

/// The subset of reactor methods the toolbelt needs. Defined as a trait so the
/// hub does not have to import the reactor's internal types and so tests can
/// substitute a mock.
#[async_trait::async_trait]
pub trait ReactorHandle: Send + Sync {
    async fn list_workers_for_peer(&self, peer: &PeerId) -> Vec<crate::memory::WorkerSummary>;
    /// `self: Arc<Self>` so the implementation can hand a strong reference
    /// to the pump task it spawns for the worker. The hub stores
    /// `Arc<dyn ReactorHandle>` so calls naturally have an Arc on hand.
    async fn spawn_worker(
        self: Arc<Self>,
        brief: String,
        peer: PeerId,
        channel: Channel,
    ) -> anyhow::Result<crate::types::WorkerId>;
    async fn cancel_worker(
        self: Arc<Self>,
        id: crate::types::WorkerId,
    ) -> anyhow::Result<()>;
    /// Emit on the thought broadcast and journal a SignalOut. Used by `speak`
    /// with `channel="thought"`.
    async fn emit_thought(&self, peer: &PeerId, body: String);
    /// Synthesize `body` via the configured TTS provider, persist the rendered
    /// audio under `data/media/audio/out/`, journal a SignalOut on the Audio
    /// channel with the media_path, and broadcast on `/audio`. Returns an
    /// error string (surfaced to the LLM in the tool result) when TTS is not
    /// configured — by design, so the agent retries with channel="thought".
    async fn emit_audio(&self, peer: &PeerId, body: String) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// A short tag identifying one routing session for the duration of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionTag(pub String);

impl SessionTag {
    pub fn new_random() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        // Method name is also a reserved keyword in Rust 2024; r# prefix
        // disambiguates against the future `gen` keyword.
        let bytes: [u8; 8] = rng.r#gen();
        let s: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        SessionTag(s)
    }
}

/// In-process MCP hub. Cheap to clone — internal state is `Arc`-shared.
pub struct McpHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    memory: Memory,
    /// Set after the reactor is constructed; the hub is created first so the
    /// `Reactor::start` signature can take the hub by value, but the reactor
    /// needs to inject itself once it exists.
    ///
    /// Note: this is a strong handle, which forms a cycle (reactor → hub →
    /// reactor). For a v0 single-process binary that lives for the
    /// lifetime of the program this is acceptable; the cycle is broken at
    /// process exit. If we ever need the reactor to drop independently of
    /// the hub, swap this for `Weak<dyn ReactorHandle>` and wrap the
    /// reactor in an outer Arc.
    reactor: Mutex<Option<Arc<dyn ReactorHandle>>>,
    /// session_tag → (peer, role) mapping registered before `session/prompt`
    /// and cleared on session end. The role gates tools/list and tools/call.
    sessions: Mutex<HashMap<SessionTag, (PeerId, SessionRole)>>,
    /// Filesystem path the listener is bound on. Passed to spawned shims.
    sock_path: PathBuf,
    /// Path to the hi-agent binary that should be re-exec'd as the shim.
    shim_program: PathBuf,
}

/// Spawn the listener task and return the hub handle.
///
/// `sock_path` is the Unix socket the hub binds. `shim_program` is the path
/// to the hi-agent binary (so we can re-exec it with the `mcp-shim` flag);
/// pass `std::env::current_exe()?` from `lib.rs`.
pub async fn start(
    memory: Memory,
    sock_path: PathBuf,
    shim_program: PathBuf,
) -> anyhow::Result<Arc<McpHub>> {
    // Remove any stale socket from a previous run.
    if sock_path.exists() {
        if let Err(err) = tokio::fs::remove_file(&sock_path).await {
            tracing::warn!(error = %err, path = %sock_path.display(), "removing stale mcp socket");
        }
    }
    if let Some(parent) = sock_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(&sock_path)?;
    tracing::info!(path = %sock_path.display(), "mcp hub listening");

    let inner = Arc::new(HubInner {
        memory,
        reactor: Mutex::new(None),
        sessions: Mutex::new(HashMap::new()),
        sock_path: sock_path.clone(),
        shim_program,
    });
    let hub = Arc::new(McpHub {
        inner: inner.clone(),
    });

    // Listener task — one connection per routing session.
    let accept_inner = inner.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let conn_inner = accept_inner.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_connection(conn_inner, stream).await {
                            tracing::warn!(error = %err, "mcp connection ended with error");
                        }
                    });
                }
                Err(err) => {
                    tracing::warn!(error = %err, "mcp accept failed");
                }
            }
        }
    });

    Ok(hub)
}

impl McpHub {
    /// Inject the reactor handle. Called once, right after the reactor is
    /// constructed in `lib.rs`.
    pub async fn attach_reactor(&self, reactor: Arc<dyn ReactorHandle>) {
        let mut slot = self.inner.reactor.lock().await;
        *slot = Some(reactor);
    }

    /// Register a session_tag → peer mapping for a routing session. Workers
    /// use [`register_session_worker`] instead so the toolbelt filters
    /// correctly. Call before `session/prompt`.
    pub async fn register_session(&self, tag: SessionTag, peer: PeerId) {
        let mut g = self.inner.sessions.lock().await;
        g.insert(tag, (peer, SessionRole::Router));
    }

    /// Register a session_tag → peer mapping for a worker session. Workers
    /// see a reduced toolbelt (no spawn_worker/cancel_worker/list_workers).
    pub async fn register_session_worker(&self, tag: SessionTag, peer: PeerId) {
        let mut g = self.inner.sessions.lock().await;
        g.insert(tag, (peer, SessionRole::Worker));
    }

    /// Drop a session mapping. Call after the session completes.
    pub async fn unregister_session(&self, tag: &SessionTag) {
        let mut g = self.inner.sessions.lock().await;
        g.remove(tag);
    }

    /// Build an `McpServerCfg` for a router session.
    pub fn router_mcp_server_cfg(&self, tag: &SessionTag) -> McpServerCfg {
        self.mcp_server_cfg(tag, SessionRole::Router)
    }

    /// Build an `McpServerCfg` for a worker session. Same transport as the
    /// router config; the role is enforced server-side via the session-tag
    /// registration, the env var is informational only.
    pub fn worker_mcp_server_cfg(&self, tag: &SessionTag) -> McpServerCfg {
        self.mcp_server_cfg(tag, SessionRole::Worker)
    }

    fn mcp_server_cfg(&self, tag: &SessionTag, role: SessionRole) -> McpServerCfg {
        let role_str = match role {
            SessionRole::Router => "router",
            SessionRole::Worker => "worker",
        };
        McpServerCfg {
            name: SERVER_NAME.to_string(),
            command: self.inner.shim_program.clone(),
            args: vec![SHIM_FLAG.to_string()],
            env: vec![
                (ENV_SOCK.to_string(), self.inner.sock_path.display().to_string()),
                (ENV_TAG.to_string(), tag.0.clone()),
                (ENV_ROLE.to_string(), role_str.to_string()),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    /// Notifications omit `id`.
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

const ERR_INVALID_PARAMS: i32 = -32602;
const ERR_INTERNAL: i32 = -32603;
const ERR_METHOD_NOT_FOUND: i32 = -32601;

async fn handle_connection(inner: Arc<HubInner>, stream: UnixStream) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Handshake: first line is `{"tag":"..."}`. The hub uses this to look up
    // the peer for subsequent tool calls.
    let mut handshake_line = String::new();
    let n = reader.read_line(&mut handshake_line).await?;
    if n == 0 {
        anyhow::bail!("mcp connection closed before handshake");
    }
    let handshake: Value = serde_json::from_str(handshake_line.trim())?;
    let tag = handshake
        .get("tag")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("mcp handshake missing tag field"))?
        .to_string();
    let tag = SessionTag(tag);
    tracing::debug!(tag = %tag.0, "mcp shim connected");

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            tracing::debug!(tag = %tag.0, "mcp shim disconnected");
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, line = %trimmed, "mcp received invalid JSON-RPC");
                continue;
            }
        };

        let response = dispatch(&inner, &tag, req).await;
        if let Some(resp) = response {
            let mut buf = serde_json::to_vec(&resp)?;
            buf.push(b'\n');
            write_half.write_all(&buf).await?;
            write_half.flush().await?;
        }
    }
}

async fn dispatch(inner: &Arc<HubInner>, tag: &SessionTag, req: RpcRequest) -> Option<RpcResponse> {
    // Notifications (no id) get no reply.
    let id = match req.id {
        Some(v) => v,
        None => {
            // notifications/cancelled and notifications/initialized are no-ops.
            tracing::trace!(method = %req.method, "mcp notification");
            return None;
        }
    };

    // Resolve role once per request — routers see seven tools, workers four.
    // If the tag has not been registered yet the table lookup fails; default
    // to Router so an early `initialize` does not error out (the binding is
    // installed before any tool call would resolve a peer).
    let role = {
        let g = inner.sessions.lock().await;
        g.get(tag).map(|(_, r)| *r).unwrap_or(SessionRole::Router)
    };

    let result: Result<Value, (i32, String)> = match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "tools/list" => Ok(json!({ "tools": tool_list(role) })),
        "tools/call" => match handle_tool_call(inner, tag, role, req.params).await {
            Ok(text) => Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })),
            Err(ToolError::User(msg)) => Ok(json!({
                "content": [{ "type": "text", "text": msg }],
                "isError": true,
            })),
            Err(ToolError::Internal(msg)) => Err((ERR_INTERNAL, msg)),
            Err(ToolError::BadParams(msg)) => Err((ERR_INVALID_PARAMS, msg)),
        },
        other => {
            tracing::debug!(method = %other, "mcp method not handled");
            Err((ERR_METHOD_NOT_FOUND, format!("method not found: {other}")))
        }
    };

    let resp = match result {
        Ok(value) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(value),
            error: None,
        },
        Err((code, message)) => RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError { code, message }),
        },
    };
    Some(resp)
}

// ---------------------------------------------------------------------------
// Tool schemas + dispatch
// ---------------------------------------------------------------------------

fn tool_list(role: SessionRole) -> Value {
    // Common to both roles.
    let speak = json!({
        "name": "speak",
        "description": "Send a reply to the peer on a channel. Choose the channel deliberately: `thought` is text (the default — best for long, technical, or quoted answers and anything the peer may want to re-read). `audio` is voice (best for short, personal, or urgent replies; the text you pass is what will be spoken, so keep it natural and conversational — no markdown, no code). If audio is not configured the call returns an error string; retry with channel=\"thought\".",
        "inputSchema": {
            "type": "object",
            "properties": {
                "channel": { "type": "string", "enum": ["thought", "audio"], "description": "Output channel — `thought` for text, `audio` for voice." },
                "to": { "type": "string", "description": "The peer to address. Usually the peer this session is for." },
                "body": { "type": "string", "description": "The text to emit (also the text that will be spoken when channel=\"audio\")." }
            },
            "required": ["channel", "to", "body"]
        }
    });
    let set_intent = json!({
        "name": "set_intent",
        "description": "Schedule a deferred intention to fire at a UTC instant. The heartbeat injects it as a synthetic signal when due.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "when": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["absolute"] },
                        "ts": { "type": "string", "description": "RFC3339 UTC instant." }
                    },
                    "required": ["type", "ts"]
                },
                "what": { "type": "string", "description": "What the agent should do when the intent fires." }
            },
            "required": ["when", "what"]
        }
    });
    let recall = json!({
        "name": "recall",
        "description": "Search the journal across all peers for entries whose body contains the query.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
            },
            "required": ["query"]
        }
    });
    let note = json!({
        "name": "note",
        "description": "Drop a journal note for this peer without emitting on any channel.",
        "inputSchema": {
            "type": "object",
            "properties": { "content": { "type": "string" } },
            "required": ["content"]
        }
    });

    if role == SessionRole::Worker {
        return json!([speak, set_intent, recall, note]);
    }

    // Router-only tools: spawn_worker, cancel_worker, list_workers.
    let spawn_worker = json!({
        "name": "spawn_worker",
        "description": "Spawn a long-running worker ACP session for research or multi-step work.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "brief": { "type": "string", "description": "Plain-language brief for the worker." },
                "channel_out": { "type": "string", "enum": ["thought"], "description": "Channel the worker will emit on." }
            },
            "required": ["brief", "channel_out"]
        }
    });
    let cancel_worker = json!({
        "name": "cancel_worker",
        "description": "Cancel a running worker by id.",
        "inputSchema": {
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        }
    });
    let list_workers = json!({
        "name": "list_workers",
        "description": "List running workers attached to this peer.",
        "inputSchema": { "type": "object", "properties": {} }
    });

    json!([
        speak,
        spawn_worker,
        cancel_worker,
        list_workers,
        set_intent,
        recall,
        note,
    ])
}

#[derive(Debug)]
enum ToolError {
    /// Tool ran but returned a user-facing error string (e.g. unknown peer).
    User(String),
    /// Internal failure (journal write, etc.).
    Internal(String),
    /// Malformed parameters from the caller.
    BadParams(String),
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

async fn handle_tool_call(
    inner: &Arc<HubInner>,
    tag: &SessionTag,
    role: SessionRole,
    params: Value,
) -> Result<String, ToolError> {
    let call: ToolCallParams = serde_json::from_value(params)
        .map_err(|e| ToolError::BadParams(format!("tools/call params: {e}")))?;
    let peer = inner
        .sessions
        .lock()
        .await
        .get(tag)
        .map(|(p, _)| p.clone())
        .ok_or_else(|| ToolError::User(format!("session tag not registered: {}", tag.0)))?;

    // Worker role gating — workers may not spawn, cancel, or list workers.
    if role == SessionRole::Worker
        && matches!(call.name.as_str(), "spawn_worker" | "cancel_worker" | "list_workers")
    {
        return Err(ToolError::User(format!(
            "tool '{}' is not available to workers",
            call.name
        )));
    }

    match call.name.as_str() {
        "speak" => tool_speak(inner, &peer, call.arguments).await,
        "spawn_worker" => tool_spawn_worker(inner, &peer, call.arguments).await,
        "cancel_worker" => tool_cancel_worker(inner, call.arguments).await,
        "list_workers" => tool_list_workers(inner, &peer).await,
        "set_intent" => tool_set_intent(inner, &peer, call.arguments).await,
        "recall" => tool_recall(inner, call.arguments).await,
        "note" => tool_note(inner, &peer, call.arguments).await,
        other => Err(ToolError::User(format!("unknown tool: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SpeakArgs {
    channel: String,
    to: String,
    body: String,
}

async fn tool_speak(
    inner: &Arc<HubInner>,
    peer: &PeerId,
    args: Value,
) -> Result<String, ToolError> {
    let args: SpeakArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::BadParams(format!("speak args: {e}")))?;
    let to = PeerId(args.to);
    // The session is registered as acting for `peer`. We respect the
    // caller's `to` field but warn (in logs) if it diverges — v0 does not
    // forbid cross-peer addressing, but the typical case is to == peer.
    if to != *peer {
        tracing::debug!(
            session_peer = %peer,
            speak_to = %to,
            "speak addresses a peer other than this session's owner"
        );
    }
    let reactor = match resolve_reactor(inner).await {
        Some(r) => r,
        None => return Err(ToolError::Internal("reactor handle gone".into())),
    };
    match args.channel.as_str() {
        "thought" => {
            reactor.emit_thought(&to, args.body).await;
            Ok(format!("emitted on /thought to {}", to))
        }
        "audio" => match reactor.emit_audio(&to, args.body).await {
            Ok(()) => Ok(format!("spoken on /audio to {}", to)),
            Err(err) => Err(ToolError::User(err.to_string())),
        },
        other => Err(ToolError::User(format!(
            "channel '{other}' is not implemented in v0 (use 'thought' or 'audio')"
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct SpawnWorkerArgs {
    brief: String,
    channel_out: String,
}

async fn tool_spawn_worker(
    inner: &Arc<HubInner>,
    peer: &PeerId,
    args: Value,
) -> Result<String, ToolError> {
    let args: SpawnWorkerArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::BadParams(format!("spawn_worker args: {e}")))?;
    let channel: Channel = args
        .channel_out
        .parse()
        .map_err(|e: crate::types::ChannelParseError| ToolError::BadParams(e.to_string()))?;
    let reactor = match resolve_reactor(inner).await {
        Some(r) => r,
        None => return Err(ToolError::Internal("reactor handle gone".into())),
    };
    match reactor.spawn_worker(args.brief, peer.clone(), channel).await {
        Ok(id) => Ok(id.to_string()),
        Err(err) => Err(ToolError::User(err.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct CancelWorkerArgs {
    id: String,
}

async fn tool_cancel_worker(
    inner: &Arc<HubInner>,
    args: Value,
) -> Result<String, ToolError> {
    let args: CancelWorkerArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::BadParams(format!("cancel_worker args: {e}")))?;
    let id = args
        .id
        .parse::<crate::types::WorkerId>()
        .map_err(|e| ToolError::BadParams(format!("worker id: {e}")))?;
    let reactor = match resolve_reactor(inner).await {
        Some(r) => r,
        None => return Err(ToolError::Internal("reactor handle gone".into())),
    };
    reactor
        .cancel_worker(id)
        .await
        .map_err(|err| ToolError::User(err.to_string()))?;
    Ok(format!("cancelled worker {}", id))
}

async fn tool_list_workers(inner: &Arc<HubInner>, peer: &PeerId) -> Result<String, ToolError> {
    let reactor = match resolve_reactor(inner).await {
        Some(r) => r,
        None => return Err(ToolError::Internal("reactor handle gone".into())),
    };
    let workers = reactor.list_workers_for_peer(peer).await;
    if workers.is_empty() {
        return Ok("(no running workers)".to_string());
    }
    let mut out = String::new();
    for w in workers {
        out.push_str(&format!(
            "- {} since {}: {}\n",
            w.id,
            w.started.format("%H:%M:%S"),
            w.brief
        ));
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct SetIntentArgs {
    when: IntentTrigger,
    what: String,
}

async fn tool_set_intent(
    inner: &Arc<HubInner>,
    peer: &PeerId,
    args: Value,
) -> Result<String, ToolError> {
    let args: SetIntentArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::BadParams(format!("set_intent args: {e}")))?;
    let intent = Intent {
        id: crate::types::IntentId::new(),
        created: Utc::now(),
        peer: peer.clone(),
        when: args.when.clone(),
        what: args.what.clone(),
    };
    let id = intent.id;
    inner
        .memory
        .intents
        .add(intent)
        .await
        .map_err(|e| ToolError::Internal(format!("intent add: {e}")))?;
    let entry = JournalEntry::IntentSet {
        ts: Utc::now(),
        id,
        peer: peer.clone(),
        when: args.when,
        what: args.what,
    };
    if let Err(err) = inner.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed for set_intent");
    }
    Ok(id.to_string())
}

#[derive(Debug, Deserialize)]
struct RecallArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

async fn tool_recall(inner: &Arc<HubInner>, args: Value) -> Result<String, ToolError> {
    let args: RecallArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::BadParams(format!("recall args: {e}")))?;
    let limit = args.limit.unwrap_or(20).min(200);
    let hits = inner
        .memory
        .journal
        .search(&args.query, limit)
        .await
        .map_err(|e| ToolError::Internal(format!("journal search: {e}")))?;
    if hits.is_empty() {
        return Ok("(no matches)".to_string());
    }
    let mut out = String::new();
    for e in hits {
        out.push_str(&format_entry(&e));
        out.push('\n');
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct NoteArgs {
    content: String,
}

async fn tool_note(
    inner: &Arc<HubInner>,
    peer: &PeerId,
    args: Value,
) -> Result<String, ToolError> {
    let args: NoteArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::BadParams(format!("note args: {e}")))?;
    let entry = JournalEntry::Note {
        ts: Utc::now(),
        peer: Some(peer.clone()),
        content: args.content,
    };
    inner
        .memory
        .journal
        .append(entry)
        .await
        .map_err(|e| ToolError::Internal(format!("journal append: {e}")))?;
    Ok("noted".to_string())
}

fn format_entry(e: &JournalEntry) -> String {
    use crate::memory::journal::entry_ts;
    let ts = entry_ts(e).format("%Y-%m-%d %H:%M:%S");
    match e {
        JournalEntry::SignalIn { channel, from, body, .. } => {
            format!("[{}] in /{} from {}: {}", ts, channel, from, body)
        }
        JournalEntry::SignalOut { channel, to, body, .. } => {
            format!("[{}] out /{} to {}: {}", ts, channel, to, body)
        }
        JournalEntry::Note { content, .. } => format!("[{}] note: {}", ts, content),
        JournalEntry::IntentSet { id, what, .. } => format!("[{}] intent_set {}: {}", ts, id, what),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

async fn resolve_reactor(inner: &Arc<HubInner>) -> Option<Arc<dyn ReactorHandle>> {
    let guard = inner.reactor.lock().await;
    guard.as_ref().cloned()
}

// ---------------------------------------------------------------------------
// Shim subprocess — re-exec'd hi-agent in a tiny relay mode
// ---------------------------------------------------------------------------

mod shim {
    use std::env;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use super::{ENV_SOCK, ENV_TAG};

    /// Entry point for `hi-agent mcp-shim`. Reads JSON-RPC lines from stdin,
    /// forwards to the hub's Unix socket, relays responses back to stdout.
    pub async fn run_shim_from_stdio() -> anyhow::Result<()> {
        let sock = env::var(ENV_SOCK)
            .map_err(|_| anyhow::anyhow!("{} not set", ENV_SOCK))?;
        let tag = env::var(ENV_TAG)
            .map_err(|_| anyhow::anyhow!("{} not set", ENV_TAG))?;

        let stream = UnixStream::connect(&sock).await?;
        let (sock_read, mut sock_write) = stream.into_split();

        // Handshake: tell the hub which session we're for.
        let handshake = format!("{{\"tag\":\"{}\"}}\n", tag);
        sock_write.write_all(handshake.as_bytes()).await?;
        sock_write.flush().await?;

        // stdin -> socket
        let stdin_to_sock = tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut line = String::new();
            loop {
                line.clear();
                let n = match reader.read_line(&mut line).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                if sock_write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if sock_write.flush().await.is_err() {
                    break;
                }
            }
        });

        // socket -> stdout
        let sock_to_stdout = tokio::spawn(async move {
            let mut reader = BufReader::new(sock_read);
            let mut stdout = tokio::io::stdout();
            let mut line = String::new();
            loop {
                line.clear();
                let n = match reader.read_line(&mut line).await {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                if stdout.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdout.flush().await.is_err() {
                    break;
                }
            }
        });

        // Either direction closing tears down the shim.
        let _ = tokio::try_join!(stdin_to_sock, sock_to_stdout);
        Ok(())
    }
}
