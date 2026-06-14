//! Agent session layer — one ACP subprocess per session.
//!
//! Exposes each ACP session as an independent [`AcpSession`] handle. Callers
//! (the reactor) never see subprocesses or the JSON-RPC connection — those stay
//! internal to [`AcpProcess`], which the returned handle owns.
//!
//! **Granularity: one subprocess per session.** Each [`session`](AgentLayer::session)
//! call spawns its own subprocess (Chrome-style isolation taken to the session
//! level), opens that process's single session, and hands back a handle that
//! owns the process — dropping the handle tears the process down. One session's
//! crash or OOM cannot touch another, and there is no `session_id` demux. The
//! cost is a fresh subprocess spawn + ACP `initialize` + MCP `tools/list`
//! round-trip per session.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::{HttpHeader, McpServer, McpServerHttp};

use crate::acp::{AcpProcess, AcpSession, AcpTap, SessionOpts};
use crate::config::{HEADER_ROLE, HEADER_SCENE, HEADER_WORKER_ID};
use crate::types::Scene;

/// Which tool surface a session gets, carried as `X-HI-Role` on its MCP attach so
/// the `/mcp` server exposes the right tools (see [`crate::mcp`]). A reactor
/// session drives output and delegation; a worker can only raise a question; a
/// reflection session ("sleep") only reads/writes derived memory (episodes,
/// facets) and has no voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionRole {
    #[default]
    Reactor,
    Worker,
    Reflection,
}

impl SessionRole {
    fn as_str(self) -> &'static str {
        match self {
            SessionRole::Reactor => "reactor",
            SessionRole::Worker => "worker",
            SessionRole::Reflection => "reflection",
        }
    }
}

/// How to spawn one ACP subprocess. Cloned per session: the same pinned runtime
/// and managed env back every session's process (they share one local LLM proxy
/// and one rendered config dir, resolved once at startup).
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The per-session subprocess spawner. Cloneable handle; clones share one config.
#[derive(Clone)]
pub struct AgentLayer {
    inner: Arc<Inner>,
}

struct Inner {
    spawn: SpawnConfig,
    /// hi-agent's own HTTP base URL (e.g. `http://127.0.0.1:8080`), used to build
    /// each session's MCP attach URL (`<base>/mcp`). The same value the child gets
    /// as `HI_AGENT_BASE_URL`.
    server_base_url: String,
    /// Raw JSON-RPC wire tap — every session's subprocess records its frames here
    /// for the raw ACP inspector. Handed to each [`AcpProcess`] at spawn.
    tap: AcpTap,
}

impl AgentLayer {
    pub fn new(spawn: SpawnConfig, tap: AcpTap, server_base_url: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                server_base_url,
                tap,
            }),
        }
    }

    /// Spawn a dedicated subprocess and open its single session for `scene`.
    /// `role` selects the tool surface the session gets; `worker_id` (workers
    /// only) names which working session a tool call comes from. The session
    /// connects to hi-agent's own `/mcp` endpoint, tagged with these via headers
    /// so the server can route its tool calls. The returned handle owns the
    /// subprocess — the caller drives prompts on it, and dropping it tears the
    /// process down.
    pub async fn session(
        &self,
        scene: &Scene,
        role: SessionRole,
        worker_id: Option<u64>,
        opts: SessionOpts,
    ) -> anyhow::Result<AcpSession> {
        let mut headers = vec![
            HttpHeader::new(HEADER_SCENE, scene.0.clone()),
            HttpHeader::new(HEADER_ROLE, role.as_str()),
        ];
        if let Some(id) = worker_id {
            headers.push(HttpHeader::new(HEADER_WORKER_ID, id.to_string()));
        }
        let mcp = McpServer::Http(
            McpServerHttp::new("hi-agent", format!("{}/mcp", self.inner.server_base_url))
                .headers(headers),
        );

        let SessionOpts { system_prompt, cwd } = opts;

        let spawn = &self.inner.spawn;
        tracing::info!(scene = %scene, role = role.as_str(), "spawning ACP subprocess for session");
        let (process, rx) = AcpProcess::spawn(
            spawn.program.clone(),
            spawn.args.clone(),
            spawn.env.clone(),
            self.inner.tap.clone(),
            scene.0.clone(),
        )
        .await?;
        let id = process
            .open_session(SessionOpts { system_prompt: None, cwd }, vec![mcp])
            .await?;

        Ok(AcpSession::new(id, process, rx, system_prompt))
    }
}
