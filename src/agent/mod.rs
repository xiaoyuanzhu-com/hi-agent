//! Agent session layer — the per-scene process pool.
//!
//! Exposes each ACP session as an independent [`AcpSession`] handle. Callers
//! (the reactor) never see subprocesses, the routing table, or `session_id`
//! demux — those stay internal to [`AcpProcess`].
//!
//! **Pool granularity: one subprocess per scene** (Chrome-style site-isolation,
//! where the *scene* is the isolation unit). All of a scene's sessions — its
//! persistent reactor session and any ephemeral working sessions — multiplex
//! inside that scene's single subprocess; different scenes get different
//! subprocesses, so one scene's crash or OOM cannot touch another. The process
//! is spawned lazily on a scene's first session and kept warm thereafter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::{HttpHeader, McpServer, McpServerHttp};
use tokio::sync::{Mutex, OnceCell};

use crate::acp::{AcpProcess, AcpSession, AcpTap, SessionOpts};
use crate::config::{HEADER_ROLE, HEADER_SCENE, HEADER_WORKER_ID};
use crate::observatory::{EventKind, Observatory};
use crate::types::Scene;

/// Which tool surface a session gets, carried as `X-HI-Role` on its MCP attach so
/// the `/mcp` server exposes the right tools (see [`crate::mcp`]). A reactor
/// session drives output and delegation; a worker can only raise a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionRole {
    #[default]
    Reactor,
    Worker,
}

impl SessionRole {
    fn as_str(self) -> &'static str {
        match self {
            SessionRole::Reactor => "reactor",
            SessionRole::Worker => "worker",
        }
    }
}

/// How to spawn one ACP subprocess. Cloned per scene: the same pinned runtime and
/// managed env back every scene's process (they share one local LLM proxy and
/// one rendered config dir, resolved once at startup).
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The per-scene process pool. Cloneable handle; clones share one pool.
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
    /// Structured visibility — process spawn/restart events feed it.
    observatory: Observatory,
    /// Raw JSON-RPC wire tap — every scene's subprocess records its frames here
    /// for the raw ACP inspector. Handed to each [`AcpProcess`] at spawn.
    tap: AcpTap,
    /// One lazily-initialised process per scene. The `OnceCell` makes concurrent
    /// first-contacts for the *same* scene wait on a single spawn, while leaving
    /// other scenes free to proceed (the map lock is held only to fetch the cell,
    /// never across the spawn itself).
    scenes: Mutex<HashMap<Scene, Arc<OnceCell<Arc<AcpProcess>>>>>,
}

impl AgentLayer {
    pub fn new(
        spawn: SpawnConfig,
        observatory: Observatory,
        tap: AcpTap,
        server_base_url: String,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                server_base_url,
                observatory,
                tap,
                scenes: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Open a new session for `scene`, spawning that scene's subprocess on first
    /// use and reusing it thereafter. `role` selects the tool surface the session
    /// gets; `worker_id` (workers only) names which working session a tool call
    /// comes from. The session connects to hi-agent's own `/mcp` endpoint, tagged
    /// with these via headers so the server can route its tool calls. The returned
    /// handle is independent — the caller drives prompts on it with no knowledge of
    /// the pool.
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

        let process = self.process_for(scene).await?;
        process.new_session(opts, vec![mcp]).await
    }

    /// Kill and forget a scene's process. The next [`session`](Self::session)
    /// call cold-starts a fresh one. Any outstanding sessions on the old process
    /// are invalidated — the reactor rebuilds from the journal (this is the
    /// within-scene shared-fate recovery path: a worker OOM that takes the scene's
    /// process down is a recoverable hiccup, not data loss).
    pub async fn restart(&self, scene: &Scene) {
        let removed = {
            let mut scenes = self.inner.scenes.lock().await;
            scenes.remove(scene)
        };
        // Dropping the cell drops its `Arc<AcpProcess>`; the process tears down
        // once the last handle is gone (its `Drop` signals shutdown).
        drop(removed);
        self.inner
            .observatory
            .record(scene, EventKind::ProcessRestarted)
            .await;
        tracing::info!(scene = %scene, "scene ACP subprocess dropped; will cold-start on next session");
    }

    async fn process_for(&self, scene: &Scene) -> anyhow::Result<Arc<AcpProcess>> {
        let cell = {
            let mut scenes = self.inner.scenes.lock().await;
            scenes
                .entry(scene.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let spawn = &self.inner.spawn;
        let observatory = &self.inner.observatory;
        let tap = &self.inner.tap;
        let process = cell
            .get_or_try_init(|| async {
                tracing::info!(scene = %scene, "spawning ACP subprocess for scene");
                let proc = AcpProcess::spawn(
                    spawn.program.clone(),
                    spawn.args.clone(),
                    spawn.env.clone(),
                    tap.clone(),
                    scene.0.clone(),
                )
                .await?;
                observatory.record(scene, EventKind::ProcessSpawned).await;
                Ok::<_, anyhow::Error>(Arc::new(proc))
            })
            .await?;

        Ok(process.clone())
    }
}
