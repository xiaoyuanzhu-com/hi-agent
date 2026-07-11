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

use crate::foundation::acp::{AcpProcess, AcpSession, AcpTap, ProcessRegistry, SessionOpts};
use crate::foundation::config::{AgentConfig, HEADER_ROLE, HEADER_SCENE, HEADER_WORKER_ID};
use crate::types::Scene;

/// Which tool surface a session gets, carried as `X-HI-Role` on its MCP attach so
/// the `/mcp` server exposes the right tools (see [`crate::foundation::mcp`]). A reactor
/// session drives output and delegation; a worker can only raise a question; a
/// reflection session ("sleep") only reads/writes derived memory (episodes,
/// facets) and has no voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionRole {
    #[default]
    Reactor,
    /// The fast, **tools-off** reactor voice (reactor/cognition split): a single
    /// generation with no `/mcp` attach at all, so there is no tool loop. It speaks
    /// via its plain message text (`agent_message_chunk`), not a `say` tool.
    ReactorVoice,
    Worker,
    Reflection,
}

impl SessionRole {
    fn as_str(self) -> &'static str {
        match self {
            SessionRole::Reactor => "reactor",
            SessionRole::ReactorVoice => "reactor-voice",
            SessionRole::Worker => "worker",
            SessionRole::Reflection => "reflection",
        }
    }
}

/// How to spawn one ACP subprocess. Cloned per session: the pinned runtime, args,
/// and **static** env (config dir, server URL, PATH — resolved once at startup).
/// The volatile upstream credential vars (`ANTHROPIC_*`) are NOT frozen here — they
/// are re-resolved from the credential store at each [`session`](AgentLayer::session)
/// spawn and merged onto this env, so a fresh child never carries a stale key.
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
    /// Data dir, so each spawn can re-resolve the upstream credential from the
    /// store ([`AgentConfig::resolve`]) rather than freeze a boot-time key. Cheap
    /// SQLite read, dwarfed by the subprocess spawn + ACP `initialize` it precedes.
    data_dir: PathBuf,
    /// hi-agent's own HTTP base URL (e.g. `http://127.0.0.1:12358`), used to build
    /// each session's MCP attach URL (`<base>/mcp`). The same value the child gets
    /// as `HI_AGENT_BASE_URL`.
    server_base_url: String,
    /// Raw JSON-RPC wire tap — every session's subprocess records its frames here
    /// for the raw ACP inspector. Handed to each [`AcpProcess`] at spawn.
    tap: AcpTap,
    /// Every spawned subprocess registers its driver here, so the host can reap
    /// them all on shutdown instead of leaking orphaned children. See
    /// [`AgentLayer::shutdown`].
    registry: ProcessRegistry,
}

impl AgentLayer {
    pub fn new(spawn: SpawnConfig, data_dir: PathBuf, tap: AcpTap, server_base_url: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                data_dir,
                server_base_url,
                tap,
                registry: ProcessRegistry::new(),
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
        // The reactor VOICE gets no `/mcp` attach at all — a single tools-free
        // generation, which is what makes it a fast, loop-free turn. Every other role
        // attaches hi-agent's tool surface, routed server-side by the X-HI-Role header.
        let mcp_servers = if matches!(role, SessionRole::ReactorVoice) {
            Vec::new()
        } else {
            let mut headers = vec![
                HttpHeader::new(HEADER_SCENE, scene.0.clone()),
                HttpHeader::new(HEADER_ROLE, role.as_str()),
            ];
            if let Some(id) = worker_id {
                headers.push(HttpHeader::new(HEADER_WORKER_ID, id.to_string()));
            }
            vec![McpServer::Http(
                McpServerHttp::new("hi-agent", format!("{}/mcp", self.inner.server_base_url))
                    .headers(headers),
            )]
        };

        let SessionOpts { system_prompt, cwd } = opts;
        // Never let a session root at the process cwd. `session/new` requires a cwd,
        // and an unset one falls through to `std::env::current_dir()` (acp/process.rs)
        // — which for a Finder-launched `.app` is `/` and in dev is often `~`. The
        // agent (Claude Code) reads its project tree on startup, so rooting it there
        // walks into `~/Pictures`, `~/Music`, `~/Documents`, … and fires a burst of
        // TCC "wants to access your Photos/Music/…" prompts at first launch. Default
        // instead to the data dir (under Application Support — not a TCC-gated
        // location), the agent's own world. Workers still override with `views_dir`.
        let cwd = cwd.or_else(|| Some(self.inner.data_dir.clone()));

        let spawn = &self.inner.spawn;
        // Merge the current upstream credential onto the static env at spawn time,
        // so this child always carries the freshest key from the store (broker
        // re-mint, Settings edit, mode switch) — never a stale boot-time snapshot.
        let mut env = spawn.env.clone();
        env.extend(AgentConfig::resolve(&self.inner.data_dir).auth_child_env());
        tracing::info!(scene = %scene, role = role.as_str(), cwd = ?cwd, "spawning ACP subprocess for session");
        let (process, rx) = AcpProcess::spawn(
            spawn.program.clone(),
            spawn.args.clone(),
            env,
            self.inner.tap.clone(),
            scene.0.clone(),
            &self.inner.registry,
        )
        .await?;
        let id = process
            .open_session(SessionOpts { system_prompt: None, cwd }, mcp_servers)
            .await?;

        Ok(AcpSession::new(id, process, rx, system_prompt))
    }

    /// Reap every live ACP subprocess this layer has spawned (reactor, worker and
    /// reflection sessions all flow through [`session`](Self::session)). Used on
    /// host shutdown so no `node`/`claude` children are orphaned. Bound the call
    /// with a timeout — a wedged child should not hang process exit.
    pub async fn shutdown(&self) {
        self.inner.registry.shutdown().await;
    }
}
