//! Agent session layer — the per-peer process pool.
//!
//! Exposes each ACP session as an independent [`AcpSession`] handle. Callers
//! (the reactor) never see subprocesses, the routing table, or `session_id`
//! demux — those stay internal to [`AcpProcess`].
//!
//! **Pool granularity: one subprocess per peer** (Chrome-style site-isolation,
//! where the *peer* is the isolation unit). All of a peer's sessions — its
//! persistent reactor session and any ephemeral working sessions — multiplex
//! inside that peer's single subprocess; different peers get different
//! subprocesses, so one peer's crash or OOM cannot touch another. The process
//! is spawned lazily on a peer's first session and kept warm thereafter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, OnceCell};

use crate::acp::{AcpProcess, AcpSession, SessionOpts};
use crate::types::PeerId;

/// How to spawn one ACP subprocess. Cloned per peer: the same pinned runtime and
/// managed env back every peer's process (they share one local LLM proxy and
/// one rendered config dir, resolved once at startup).
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The per-peer process pool. Cloneable handle; clones share one pool.
#[derive(Clone)]
pub struct AgentLayer {
    inner: Arc<Inner>,
}

struct Inner {
    spawn: SpawnConfig,
    /// One lazily-initialised process per peer. The `OnceCell` makes concurrent
    /// first-contacts for the *same* peer wait on a single spawn, while leaving
    /// other peers free to proceed (the map lock is held only to fetch the cell,
    /// never across the spawn itself).
    peers: Mutex<HashMap<PeerId, Arc<OnceCell<Arc<AcpProcess>>>>>,
}

impl AgentLayer {
    pub fn new(spawn: SpawnConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                peers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Open a new session for `peer`, spawning that peer's subprocess on first
    /// use and reusing it thereafter. The returned handle is independent — the
    /// caller drives prompts on it with no knowledge of the pool.
    pub async fn session(
        &self,
        peer: &PeerId,
        opts: SessionOpts,
    ) -> anyhow::Result<AcpSession> {
        let process = self.process_for(peer).await?;
        process.new_session(opts).await
    }

    /// Kill and forget a peer's process. The next [`session`](Self::session)
    /// call cold-starts a fresh one. Any outstanding sessions on the old process
    /// are invalidated — the reactor rebuilds from the journal (this is the
    /// within-peer shared-fate recovery path: a worker OOM that takes the peer's
    /// process down is a recoverable hiccup, not data loss).
    pub async fn restart(&self, peer: &PeerId) {
        let removed = {
            let mut peers = self.inner.peers.lock().await;
            peers.remove(peer)
        };
        // Dropping the cell drops its `Arc<AcpProcess>`; the process tears down
        // once the last handle is gone (its `Drop` signals shutdown).
        drop(removed);
        tracing::info!(peer = %peer, "peer ACP subprocess dropped; will cold-start on next session");
    }

    async fn process_for(&self, peer: &PeerId) -> anyhow::Result<Arc<AcpProcess>> {
        let cell = {
            let mut peers = self.inner.peers.lock().await;
            peers
                .entry(peer.clone())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        let spawn = &self.inner.spawn;
        let process = cell
            .get_or_try_init(|| async {
                tracing::info!(peer = %peer, "spawning ACP subprocess for peer");
                let proc = AcpProcess::spawn(
                    spawn.program.clone(),
                    spawn.args.clone(),
                    spawn.env.clone(),
                )
                .await?;
                Ok::<_, anyhow::Error>(Arc::new(proc))
            })
            .await?;

        Ok(process.clone())
    }
}
