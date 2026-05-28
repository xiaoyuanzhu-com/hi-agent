//! Reactor — per-peer queues, worker registry, dispatch.
//!
//! The reactor is the always-responsive Rust core: it owns one mpsc per peer
//! and one task per peer that runs routing turns serially against an
//! ephemeral ACP session. Cognition is delegated to those sessions; the
//! central dispatch never awaits on ACP. See `docs/impl.md` § "Routing layer"
//! and § "Aliveness — Cognition contract".
//!
//! Public surface (Step 3):
//! - [`Reactor`] handle returned from [`start`].
//! - [`Reactor::snapshot_workers`] / [`Reactor::list_workers_for_peer`] for
//!   downstream Step 5 once the registry is populated.
//! - [`Reactor::inject_synthetic_signal`] used by Step 8's heartbeat to push
//!   `/intent` signals through the same per-peer routing path.
//!
//! Interruption policy (impl.md): a new POST for a peer whose routing turn is
//! still running causes `session/cancel` on the in-flight ACP session. The
//! per-peer task observes the cancel via `SessionRun::next_update` returning
//! `None` or yielding an error, drains every signal already queued, and
//! re-prompts with the merged batch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::SessionId;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::acp::{AcpProcess, AcpSession, SessionOpts, SessionUpdate};
use crate::mcp::{McpHub, ReactorHandle, SessionTag};
use crate::memory::{Memory, WorkerSummary, build_for_peer};
use crate::server::approval::{ApprovalDecision, ApprovalEvent};
use crate::types::{ApprovalId, Channel, JournalEntry, PeerId, Signal, WorkerId};

/// Router system prompt. Step 4 instructs the model to dispatch through the
/// MCP toolbelt rather than reply inline. Inline text continues to be
/// fan-routed to /thought as a v0 fallback (see `emit_thought`) so a misbehaving
/// model still produces visible output; v1 will remove the fallback.
const ROUTER_SYSTEM_PROMPT: &str = "You are the router for a human-interface agent. \
You do not perform tasks — you dispatch. \
You have these tools: speak, spawn_worker, cancel_worker, list_workers, set_intent, recall, note. \
For a normal text reply to the peer, call `speak` with channel=\"thought\". \
For deferred reminders, call `set_intent`. \
For research or multi-step work, call `spawn_worker`. \
Use `recall` to search memory, `note` to write a journal entry without speaking, \
and `list_workers` / `cancel_worker` to manage running work. \
Do not write text outside of tool calls.";

/// Worker system prompt. Workers do one concern, may take seconds to hours,
/// and have a reduced toolbelt: speak/set_intent/recall/note. They cannot
/// spawn other workers — that is a routing-layer concern.
const WORKER_SYSTEM_PROMPT: &str = "You are a worker for a human-interface agent. \
You were spawned to do one concern; the brief follows in the user message. \
You have file access, code execution, and these emission tools: \
speak, set_intent, recall, note. Use `speak` with channel=\"thought\" to emit text to the peer. \
Work as long as needed to complete the concern. When done, return. \
If you need permission for a sensitive action, request it via ACP request_permission. \
You cannot spawn other workers.";

/// How long `cancel_worker` waits for the pump task to clean up gracefully
/// before force-removing the entry from the registry.
const WORKER_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Buffer size for each peer's inbound mpsc. Small on purpose: backpressure
/// here surfaces as central-dispatch lag, which is the right place to notice.
const PEER_QUEUE_CAPACITY: usize = 64;

/// Synthetic-signal sender identity for self-injected signals (heartbeat,
/// step-8 intent firings). Mirrors the `from: "self@..."` convention in
/// impl.md § Aliveness — Heartbeat.
const SELF_SENDER: &str = "self@agent";

/// Sender identity used when emitting on a channel as the agent. Distinct
/// from `SELF_SENDER` because outbound emissions are "from the agent" to a
/// peer, while synthetic inputs are "from self" to the router.
const AGENT_SENDER: &str = "agent@self";

/// Approval timeout per impl.md § "Approval": "v0 uses 5 minutes. After that,
/// the request is journaled as expired and the requesting session is told to
/// abort."
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Outcome handed back from the approval bridge to a parked ACP
/// request-permission handler.
#[derive(Debug, Clone)]
pub enum ApprovalOutcome {
    Decision { allow: bool, reason: Option<String> },
    Expired,
}

/// Handle for the reactor. Cheap to clone — internal state is `Arc`-shared.
#[derive(Clone)]
pub struct Reactor {
    inner: Arc<ReactorInner>,
}

struct ReactorInner {
    memory: Memory,
    acp: Arc<AcpProcess>,
    mcp_hub: Arc<McpHub>,
    thought_out: broadcast::Sender<Signal>,
    approval_out: broadcast::Sender<ApprovalEvent>,
    /// Outstanding approval requests, keyed by id. Each entry holds a oneshot
    /// sender the bridge resolves on decision or timeout. See Step 7.
    pending_approvals: Mutex<HashMap<ApprovalId, PendingApproval>>,
    /// ACP session_id → peer mapping. Populated when a routing session is
    /// opened so the `session/request_permission` handler can address the
    /// approval event to the right peer.
    session_peers: Mutex<HashMap<SessionId, PeerId>>,
    peers: Mutex<HashMap<PeerId, PeerHandle>>,
    /// Worker registry. Populated by Step 5 via [`spawn_worker`]; read by
    /// `snapshot_workers_for_peer` to surface running workers to the router.
    #[allow(dead_code)]
    workers: Mutex<HashMap<WorkerId, WorkerEntry>>,
}

struct PendingApproval {
    peer: PeerId,
    requested: DateTime<Utc>,
    /// Where to deliver the outcome. Taken (`Option::take`) so a decision and
    /// the timeout sweeper cannot both fire — whichever arrives first wins.
    respond_to: Option<oneshot::Sender<ApprovalOutcome>>,
}

struct PeerHandle {
    inbound: mpsc::Sender<Signal>,
    /// The "router running?" flag, represented as a clone of the in-flight
    /// ACP session. `None` means idle. The per-peer task writes
    /// `Some(session)` before issuing the prompt and clears it on completion.
    /// `Arc<AcpSession>` so the dispatcher can call `.cancel()` without
    /// blocking on the per-peer task's mutex guard lifetime.
    in_flight: Arc<Mutex<Option<Arc<AcpSession>>>>,
}

/// Registry entry for a running worker. The pump task owns the streaming
/// loop; this struct keeps enough state for `cancel_worker`, `list_workers`,
/// and `snapshot_workers` to operate without touching the pump.
#[allow(dead_code)]
pub(crate) struct WorkerEntry {
    pub id: WorkerId,
    pub peer: PeerId,
    pub brief: String,
    pub started: chrono::DateTime<Utc>,
    /// Shared with the pump task. `cancel_worker` clones and calls `cancel()`
    /// on it; the pump task observes the cancel through `next_update`.
    pub session: Arc<AcpSession>,
    /// MCP session tag registered for this worker. Held so cleanup can
    /// unregister the tag from the hub when the pump exits.
    pub tag: SessionTag,
    /// JoinHandle for the pump task. Held so a hung session can be aborted
    /// from `cancel_worker`'s grace-period fallback.
    pub pump_handle: tokio::task::JoinHandle<()>,
}

/// Spawn the central dispatch task and return the [`Reactor`] handle.
///
/// The dispatch loop pulls every inbound signal off `inbound_rx`, looks up
/// (or creates) the per-peer queue, applies the interruption policy
/// (cancel-in-flight on a new arrival), and forwards the signal to the
/// peer's task. It never awaits on ACP.
pub fn start(
    memory: Memory,
    acp: Arc<AcpProcess>,
    mcp_hub: Arc<McpHub>,
    mut inbound_rx: mpsc::Receiver<Signal>,
    thought_out: broadcast::Sender<Signal>,
    approval_out: broadcast::Sender<ApprovalEvent>,
    mut approval_decisions_rx: mpsc::Receiver<ApprovalDecision>,
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            acp,
            mcp_hub,
            thought_out,
            approval_out,
            pending_approvals: Mutex::new(HashMap::new()),
            session_peers: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
        }),
    };
    let dispatch_reactor = reactor.clone();

    tokio::spawn(async move {
        while let Some(signal) = inbound_rx.recv().await {
            // Inbound signals are always FROM a peer (or "self" for Step 8
            // synthetics). The peer's routing layer is keyed on `from`.
            let peer = signal.from.clone();
            dispatch_reactor.deliver_to_peer(peer, signal).await;
        }
        tracing::warn!("reactor inbound channel closed; dispatch loop exiting");
    });

    // Approval decisions loop. POST /approval pushes here; we look up the
    // pending entry, journal the decision, and resolve the parked oneshot.
    let decisions_inner = reactor.inner.clone();
    tokio::spawn(async move {
        while let Some(decision) = approval_decisions_rx.recv().await {
            decisions_inner.handle_decision(decision).await;
        }
        tracing::warn!("reactor approval-decisions channel closed");
    });

    reactor
}

impl Reactor {
    /// Full worker registry as a snapshot, for diagnostics or tool surfaces.
    pub fn snapshot_workers(&self) -> Vec<WorkerSummary> {
        // Lock is sync-only safe via try_lock; we use a blocking borrow here
        // because callers (HTTP handlers, MCP tools) may be on async paths
        // but a registry read is cheap. Step 5 may revisit the API shape.
        let guard = match self.inner.workers.try_lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::debug!("workers registry contended; returning empty snapshot");
                return Vec::new();
            }
        };
        guard
            .values()
            .map(|w| WorkerSummary {
                id: w.id,
                brief: w.brief.clone(),
                started: w.started,
            })
            .collect()
    }

    /// Workers belonging to a particular peer. Used by snapshot building.
    pub async fn list_workers_for_peer(&self, peer: &PeerId) -> Vec<WorkerSummary> {
        let guard = self.inner.workers.lock().await;
        guard
            .values()
            .filter(|w| w.peer == *peer)
            .map(|w| WorkerSummary {
                id: w.id,
                brief: w.brief.clone(),
                started: w.started,
            })
            .collect()
    }

    /// Push a synthetic signal into a peer's routing path. Used by Step 8's
    /// heartbeat to inject fired intents. Journals a `SignalIn` before
    /// dispatching so the routing snapshot sees it.
    pub async fn inject_synthetic_signal(
        &self,
        peer: PeerId,
        channel: Channel,
        body: String,
    ) -> anyhow::Result<()> {
        let ts = Utc::now();
        let signal = Signal {
            channel,
            from: PeerId(SELF_SENDER.to_string()),
            to: Some(peer.clone()),
            body: body.clone(),
            ts,
        };

        // Journal-before-dispatch, same invariant as the HTTP path.
        let entry = JournalEntry::SignalIn {
            ts,
            channel,
            from: signal.from.clone(),
            body,
        };
        if let Err(err) = self.inner.memory.journal.append(entry).await {
            tracing::error!(error = %err, "journal append failed for synthetic signal");
        }

        // Synthetic signals route to the target peer's queue (the addressee),
        // not the synthetic sender's. Override `peer` rather than reading
        // `signal.from`.
        self.deliver_to_peer(peer, signal).await;
        Ok(())
    }

    /// Spawn a worker: open a long-lived ACP session prompted with `brief`,
    /// register it under the originating peer, and run a pump task that
    /// forwards every `Text` update to /thought stamped for that peer.
    ///
    /// Returns the worker id immediately; the session runs in parallel with
    /// the caller and with other workers. Cancellation: see [`cancel_worker`].
    pub async fn spawn_worker(
        &self,
        brief: String,
        peer: PeerId,
        _channel: Channel,
    ) -> anyhow::Result<WorkerId> {
        spawn_worker_impl(self.inner.clone(), brief, peer).await
    }

    /// Cancel a running worker. Idempotent — a missing id resolves to `Ok`.
    /// Issues `session/cancel` on the worker's ACP session and journals
    /// `WorkerCancel`. The pump task observes the cancel via `next_update`,
    /// journals `WorkerComplete`, and clears its registry slot. If the pump
    /// does not exit within [`WORKER_CANCEL_GRACE`], the registry entry is
    /// force-removed and the pump task is aborted.
    pub async fn cancel_worker(&self, id: WorkerId) -> anyhow::Result<()> {
        cancel_worker_impl(self.inner.clone(), id).await
    }

    /// Strong handle the MCP hub stores so it can dispatch tool calls back
    /// into the reactor. The hub holds this through a `Mutex<Option<...>>`
    /// so it can be cleared at shutdown (the strong-cycle caveat in
    /// `HubInner.reactor` documents the tradeoff).
    pub fn as_handle(&self) -> Arc<dyn ReactorHandle> {
        self.inner.clone()
    }

    /// Strong handle for the ACP approval bridge. Attached to [`AcpProcess`]
    /// after the reactor exists; the `session/request_permission` handler
    /// dispatches through this to journal, broadcast on `/approval`, and
    /// await the decision.
    pub fn as_approval_bridge(&self) -> Arc<dyn crate::acp::ApprovalBridge> {
        Arc::new(self.clone())
    }

    /// Record the peer an ACP session is acting for. The
    /// `session/request_permission` handler in `acp/process.rs` looks this up
    /// to address approval events to the right peer. Called when a routing
    /// session is opened; mirror-cleared via `unregister_session_peer`.
    pub async fn register_session_peer(&self, session_id: SessionId, peer: PeerId) {
        let mut g = self.inner.session_peers.lock().await;
        g.insert(session_id, peer);
    }

    /// Drop a session→peer mapping. Idempotent.
    pub async fn unregister_session_peer(&self, session_id: &SessionId) {
        let mut g = self.inner.session_peers.lock().await;
        g.remove(session_id);
    }

    /// Resolve the peer an ACP session is acting for, if known.
    pub async fn peer_for_session(&self, session_id: &SessionId) -> Option<PeerId> {
        let g = self.inner.session_peers.lock().await;
        g.get(session_id).cloned()
    }

    /// Submit an approval request originating from an ACP session. Journals
    /// the request, broadcasts an `ApprovalEvent` for the deciding peer's
    /// `/approval` long-poll, parks a oneshot, arms a 5-minute timeout, and
    /// awaits the outcome.
    ///
    /// Called from the `session/request_permission` handler in
    /// `src/acp/process.rs`. Returns the user's decision or `Expired` if the
    /// timeout elapses first.
    pub async fn submit_approval_request(
        &self,
        peer: PeerId,
        action: String,
        summary: String,
        details: serde_json::Value,
    ) -> anyhow::Result<ApprovalOutcome> {
        let id = ApprovalId::new();
        let requested = Utc::now();

        // Journal before broadcasting so a crash mid-broadcast still leaves
        // the request recorded.
        let entry = JournalEntry::ApprovalRequest {
            ts: requested,
            id,
            peer: peer.clone(),
            action: action.clone(),
            summary: summary.clone(),
            details: details.clone(),
        };
        if let Err(err) = self.inner.memory.journal.append(entry).await {
            tracing::error!(error = %err, "journal append failed for approval request");
        }

        let (tx, rx) = oneshot::channel::<ApprovalOutcome>();
        {
            let mut g = self.inner.pending_approvals.lock().await;
            g.insert(
                id,
                PendingApproval {
                    peer: peer.clone(),
                    requested,
                    respond_to: Some(tx),
                },
            );
        }

        let event = ApprovalEvent {
            id,
            peer: peer.clone(),
            action,
            summary,
            details,
            requested,
        };
        if let Err(err) = self.inner.approval_out.send(event) {
            // No subscribers is normal — the decider's long-poll may not be
            // open at this instant. The event is still parked in
            // `pending_approvals` and a later POST can resolve it (within
            // the timeout window).
            tracing::debug!(peer = %peer, error = %err, "no approval subscribers for broadcast");
        }

        // Arm the timeout sweeper. Whichever fires first (decision or
        // timeout) wins; the loser finds `respond_to` already `None`.
        let timeout_inner = self.inner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(APPROVAL_TIMEOUT).await;
            timeout_inner.handle_expiry(id).await;
        });

        match rx.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => {
                // Sender dropped without sending — treat as expired.
                tracing::warn!(id = %id, "approval oneshot dropped without outcome");
                Ok(ApprovalOutcome::Expired)
            }
        }
    }

    /// Route one signal to its peer queue, creating the queue + task if
    /// absent. Applies the interruption policy.
    async fn deliver_to_peer(&self, peer: PeerId, signal: Signal) {
        let (sender, in_flight) = self.get_or_create_peer(peer.clone()).await;

        // Interruption policy: if a routing turn is in flight, cancel it.
        // Clone the session Arc and release the mutex before awaiting cancel
        // so the per-peer task can still progress (its prompt-await also
        // briefly contends for this mutex).
        let in_flight_session: Option<Arc<AcpSession>> = {
            let guard = in_flight.lock().await;
            guard.as_ref().cloned()
        };
        if let Some(session) = in_flight_session {
            if let Err(err) = session.cancel().await {
                tracing::warn!(peer = %peer, error = %err, "session/cancel failed during interruption");
            } else {
                tracing::debug!(peer = %peer, "interrupting in-flight routing turn");
            }
        }

        if let Err(err) = sender.send(signal).await {
            tracing::error!(peer = %peer, error = %err, "peer inbound channel closed; dropping signal");
        }
    }

    /// Look up an existing peer handle or spawn a new per-peer routing task.
    async fn get_or_create_peer(
        &self,
        peer: PeerId,
    ) -> (mpsc::Sender<Signal>, Arc<Mutex<Option<Arc<AcpSession>>>>) {
        let mut peers = self.inner.peers.lock().await;
        if let Some(handle) = peers.get(&peer) {
            return (handle.inbound.clone(), handle.in_flight.clone());
        }

        let (tx, rx) = mpsc::channel::<Signal>(PEER_QUEUE_CAPACITY);
        let in_flight: Arc<Mutex<Option<Arc<AcpSession>>>> = Arc::new(Mutex::new(None));
        let handle = PeerHandle {
            inbound: tx.clone(),
            in_flight: in_flight.clone(),
        };
        peers.insert(peer.clone(), handle);
        drop(peers);

        let task_reactor = self.clone();
        let task_peer = peer.clone();
        let task_in_flight = in_flight.clone();
        tokio::spawn(async move {
            per_peer_loop(task_reactor, task_peer, rx, task_in_flight).await;
        });

        (tx, in_flight)
    }
}

// `ReactorInner` implements the toolbelt-facing trait. The hub holds it as
// `Arc<dyn ReactorHandle>` and never needs to know about `Reactor` itself.
impl ReactorInner {
    /// Resolve a decision from POST /approval: remove the pending entry,
    /// journal the decision, and forward to the parked oneshot. Acks the
    /// POST handler with `true` if a pending entry matched, `false` if not
    /// (so the HTTP layer can return 404).
    async fn handle_decision(&self, decision: ApprovalDecision) {
        let ts = Utc::now();
        let respond_to = {
            let mut g = self.pending_approvals.lock().await;
            g.remove(&decision.id).and_then(|mut pa| pa.respond_to.take())
        };

        match respond_to {
            Some(tx) => {
                let entry = JournalEntry::ApprovalDecision {
                    ts,
                    id: decision.id,
                    allow: decision.allow,
                    reason: decision.reason.clone(),
                };
                if let Err(err) = self.memory.journal.append(entry).await {
                    tracing::error!(error = %err, "journal append failed for approval decision");
                }
                let outcome = ApprovalOutcome::Decision {
                    allow: decision.allow,
                    reason: decision.reason,
                };
                if tx.send(outcome).is_err() {
                    tracing::warn!(id = %decision.id, "approval requester dropped before decision");
                }
                let _ = decision.ack.send(true);
            }
            None => {
                tracing::info!(
                    id = %decision.id,
                    decided_by = %decision.decided_by,
                    "POST /approval for unknown id (expired or already decided)"
                );
                let _ = decision.ack.send(false);
            }
        }
    }

    /// Handle the 5-minute timeout for an outstanding approval. If the entry
    /// is still pending, journal `ApprovalExpired` and send
    /// `ApprovalOutcome::Expired`. No-op if a decision already arrived.
    async fn handle_expiry(&self, id: ApprovalId) {
        let respond_to = {
            let mut g = self.pending_approvals.lock().await;
            g.remove(&id).and_then(|mut pa| {
                let took = pa.respond_to.take();
                if took.is_some() {
                    tracing::info!(id = %id, peer = %pa.peer, requested = %pa.requested, "approval expired");
                }
                took
            })
        };
        let Some(tx) = respond_to else {
            return;
        };
        let entry = JournalEntry::ApprovalExpired { ts: Utc::now(), id };
        if let Err(err) = self.memory.journal.append(entry).await {
            tracing::error!(error = %err, "journal append failed for approval expiry");
        }
        let _ = tx.send(ApprovalOutcome::Expired);
    }
}

#[async_trait::async_trait]
impl ReactorHandle for ReactorInner {
    async fn list_workers_for_peer(&self, peer: &PeerId) -> Vec<WorkerSummary> {
        let guard = self.workers.lock().await;
        guard
            .values()
            .filter(|w| w.peer == *peer)
            .map(|w| WorkerSummary {
                id: w.id,
                brief: w.brief.clone(),
                started: w.started,
            })
            .collect()
    }
    async fn spawn_worker(
        self: Arc<Self>,
        brief: String,
        peer: PeerId,
        _channel: Channel,
    ) -> anyhow::Result<WorkerId> {
        spawn_worker_impl(self, brief, peer).await
    }
    async fn cancel_worker(self: Arc<Self>, id: WorkerId) -> anyhow::Result<()> {
        cancel_worker_impl(self, id).await
    }
    async fn emit_thought(&self, peer: &PeerId, body: String) {
        let ts = Utc::now();
        let signal = Signal {
            channel: Channel::Thought,
            from: PeerId(AGENT_SENDER.to_string()),
            to: Some(peer.clone()),
            body: body.clone(),
            ts,
        };
        let entry = JournalEntry::SignalOut {
            ts,
            channel: Channel::Thought,
            to: peer.clone(),
            body,
        };
        if let Err(err) = self.memory.journal.append(entry).await {
            tracing::error!(peer = %peer, error = %err, "journal append failed for emit_thought (mcp)");
        }
        if let Err(err) = self.thought_out.send(signal) {
            tracing::debug!(peer = %peer, error = %err, "no thought subscribers for mcp emission");
        }
    }
}

/// Approval bridge — `Reactor` forwards to its inherent methods so the trait
/// object can drive the journal, broadcast, and pending map through the same
/// path as the rest of the reactor.
#[async_trait::async_trait]
impl crate::acp::ApprovalBridge for Reactor {
    async fn peer_for_session(&self, session_id: &SessionId) -> Option<PeerId> {
        Reactor::peer_for_session(self, session_id).await
    }

    async fn submit_approval_request(
        &self,
        peer: PeerId,
        action: String,
        summary: String,
        details: serde_json::Value,
    ) -> anyhow::Result<crate::acp::AcpApprovalOutcome> {
        let outcome = Reactor::submit_approval_request(self, peer, action, summary, details).await?;
        Ok(match outcome {
            ApprovalOutcome::Decision { allow, reason } => {
                crate::acp::AcpApprovalOutcome::Decision { allow, reason }
            }
            ApprovalOutcome::Expired => crate::acp::AcpApprovalOutcome::Expired,
        })
    }
}

/// Per-peer routing task body. One instance per peer; lives for the lifetime
/// of the process (or until the inbound mpsc closes).
async fn per_peer_loop(
    reactor: Reactor,
    peer: PeerId,
    mut inbound: mpsc::Receiver<Signal>,
    in_flight: Arc<Mutex<Option<Arc<AcpSession>>>>,
) {
    loop {
        // Block for the first signal. Subsequent signals are drained
        // non-blocking so the interruption-merge case collapses into a
        // single routing turn.
        let first = match inbound.recv().await {
            Some(s) => s,
            None => {
                tracing::info!(peer = %peer, "per-peer inbound closed; exiting loop");
                return;
            }
        };
        let mut batch: Vec<Signal> = vec![first];
        while let Ok(extra) = inbound.try_recv() {
            batch.push(extra);
        }

        if let Err(err) = run_routing_turn(&reactor, &peer, &batch, &in_flight).await {
            tracing::warn!(peer = %peer, error = %err, "routing turn failed");
            // Clear in_flight in case the failure path left it set.
            let mut guard = in_flight.lock().await;
            *guard = None;
        }
    }
}

/// Execute one routing turn: build snapshot, open ephemeral ACP session,
/// stream updates, emit text on `/thought`, release the session.
async fn run_routing_turn(
    reactor: &Reactor,
    peer: &PeerId,
    batch: &[Signal],
    in_flight: &Arc<Mutex<Option<Arc<AcpSession>>>>,
) -> anyhow::Result<()> {
    let workers = reactor.list_workers_for_peer(peer).await;
    let snap = build_for_peer(&reactor.inner.memory, peer, &workers).await?;
    let prompt_text = format!(
        "{}\n\n## New signals\n{}",
        snap.render_for_prompt(),
        render_batch(batch),
    );

    // Register a session_tag → peer mapping so the in-process MCP tools know
    // which peer to operate for. The tag travels to claude-code through the
    // McpServerCfg's env, then back through the shim's handshake.
    let tag = SessionTag::new_random();
    reactor
        .inner
        .mcp_hub
        .register_session(tag.clone(), peer.clone())
        .await;

    let mcp_cfg = reactor.inner.mcp_hub.router_mcp_server_cfg(&tag);

    let session = Arc::new(
        reactor
            .inner
            .acp
            .new_session(SessionOpts {
                system_prompt: Some(ROUTER_SYSTEM_PROMPT.to_string()),
                mcp_servers: vec![mcp_cfg],
                cwd: None,
            })
            .await?,
    );

    // Register the ACP session_id → peer mapping so the
    // `session/request_permission` handler can address approval events to
    // the right peer.
    reactor
        .register_session_peer(session.id().clone(), peer.clone())
        .await;

    // Park a clone of the session in `in_flight` so the central dispatch can
    // cancel it on a fresh arrival. Release the mutex immediately; we hold
    // our own clone for prompt/stream work.
    {
        let mut guard = in_flight.lock().await;
        *guard = Some(session.clone());
    }

    // Best-effort cleanup wrapper so an early `?` from `prompt` or `next_update`
    // still clears the session→peer mapping and the in-flight slot.
    let outcome: anyhow::Result<()> = async {
        let mut run = session.prompt(prompt_text).await?;

        loop {
            let update = run.next_update().await;
            match update {
                Some(SessionUpdate::Text(text)) => {
                    emit_thought(reactor, peer, text).await;
                }
                Some(SessionUpdate::Thought(_)) => {
                    // Internal reasoning. v0 does not surface to peers.
                }
                Some(SessionUpdate::ToolCall(stub)) => {
                    tracing::debug!(peer = %peer, variant = stub.raw_variant, "router tool call (Step 4 will handle)");
                }
                Some(SessionUpdate::Other(name)) => {
                    tracing::trace!(peer = %peer, variant = %name, "ignored ACP update variant");
                }
                None => break,
            }
        }

        // Drain the stream / collect the final response. Errors here include
        // interruption-driven cancellation; both paths land back at the loop
        // entry in `per_peer_loop`, which re-prompts with any queued signals.
        match run.wait().await {
            Ok(result) => {
                tracing::debug!(peer = %peer, stop = ?result.stop_reason, "routing turn finished");
            }
            Err(err) => {
                tracing::debug!(peer = %peer, error = %err, "routing run ended with error (likely cancel)");
            }
        }

        Ok(())
    }
    .await;

    // Clear the slot. The session's routing entry is reclaimed when the
    // last `Arc<AcpSession>` drops via the `AcpSession::Drop` impl, so we
    // do not need to call `close()` explicitly.
    {
        let mut guard = in_flight.lock().await;
        *guard = None;
    }
    reactor.unregister_session_peer(session.id()).await;
    drop(session);

    // Drop the session_tag mapping; subsequent tool calls keyed on this tag
    // would (correctly) fail to resolve a peer.
    reactor.inner.mcp_hub.unregister_session(&tag).await;

    outcome
}

/// Emit one text chunk on the `/thought` broadcast and journal a `SignalOut`.
async fn emit_thought(reactor: &Reactor, peer: &PeerId, body: String) {
    let ts = Utc::now();
    let signal = Signal {
        channel: Channel::Thought,
        from: PeerId(AGENT_SENDER.to_string()),
        to: Some(peer.clone()),
        body: body.clone(),
        ts,
    };

    let entry = JournalEntry::SignalOut {
        ts,
        channel: Channel::Thought,
        to: peer.clone(),
        body,
    };
    if let Err(err) = reactor.inner.memory.journal.append(entry).await {
        tracing::error!(peer = %peer, error = %err, "journal append failed for outbound thought");
    }

    if let Err(err) = reactor.inner.thought_out.send(signal) {
        // No subscribers is normal (the peer's long-poll may not be open at
        // this instant). Log at debug so it does not look like an error.
        tracing::debug!(peer = %peer, error = %err, "no thought subscribers for emission");
    }
}

/// Format the batch of new signals for inclusion in the router prompt.
/// Mirrors the snapshot's `render_entry` shape so the router sees a
/// consistent view of "what's just arrived" vs "what came before".
fn render_batch(batch: &[Signal]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for sig in batch {
        let ts = sig.ts.format("%H:%M:%S");
        let _ = writeln!(
            s,
            "[{}] {} on /{}: \"{}\"",
            ts, sig.from, sig.channel, sig.body
        );
    }
    s
}

// ---------------------------------------------------------------------------
// Worker layer (Step 5)
// ---------------------------------------------------------------------------

/// Open a worker session and register it. The spawn returns as soon as the
/// pump task is dispatched — the worker runs in parallel with the caller.
async fn spawn_worker_impl(
    inner: Arc<ReactorInner>,
    brief: String,
    peer: PeerId,
) -> anyhow::Result<WorkerId> {
    let id = WorkerId::new();
    let started = Utc::now();

    // Register the worker tag with the MCP hub so the worker's MCP shim
    // connections resolve to the originating peer with worker-flavored tools.
    let tag = SessionTag::new_random();
    inner
        .mcp_hub
        .register_session_worker(tag.clone(), peer.clone())
        .await;

    let mcp_cfg = inner.mcp_hub.worker_mcp_server_cfg(&tag);

    let session = match inner
        .acp
        .new_session(SessionOpts {
            system_prompt: Some(WORKER_SYSTEM_PROMPT.to_string()),
            mcp_servers: vec![mcp_cfg],
            cwd: None,
        })
        .await
    {
        Ok(s) => Arc::new(s),
        Err(err) => {
            // Roll back the hub registration so the tag does not leak.
            inner.mcp_hub.unregister_session(&tag).await;
            return Err(err);
        }
    };

    // session_id → peer mapping is registered before issuing the prompt so
    // any `session/request_permission` from this worker routes to the right
    // peer immediately.
    {
        let mut g = inner.session_peers.lock().await;
        g.insert(session.id().clone(), peer.clone());
    }

    // Journal the spawn before kicking off the prompt so a crash mid-spawn
    // still leaves a trace.
    let entry = JournalEntry::WorkerSpawn {
        ts: started,
        id,
        peer: peer.clone(),
        brief: brief.clone(),
    };
    if let Err(err) = inner.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed for worker spawn");
    }

    // Pump task — drives the worker's prompt to completion and forwards
    // every text update to /thought stamped for the originating peer.
    let pump_inner = inner.clone();
    let pump_session = session.clone();
    let pump_peer = peer.clone();
    let pump_tag = tag.clone();
    let pump_brief = brief.clone();
    let pump_handle = tokio::spawn(async move {
        run_worker_pump(pump_inner, id, pump_peer, pump_brief, pump_session, pump_tag).await;
    });

    // Insert into the registry.
    {
        let mut workers = inner.workers.lock().await;
        workers.insert(
            id,
            WorkerEntry {
                id,
                peer,
                brief,
                started,
                session,
                tag,
                pump_handle,
            },
        );
    }

    Ok(id)
}

/// Cancel a worker. Issues `session/cancel` on its ACP session and journals
/// the cancel. If the pump task does not clean up within
/// [`WORKER_CANCEL_GRACE`], the registry entry is force-removed and the pump
/// task is aborted.
async fn cancel_worker_impl(inner: Arc<ReactorInner>, id: WorkerId) -> anyhow::Result<()> {
    // Clone the session Arc and release the lock before awaiting cancel so
    // the pump can keep making progress.
    let session = {
        let workers = inner.workers.lock().await;
        match workers.get(&id) {
            Some(entry) => entry.session.clone(),
            None => return Ok(()),
        }
    };

    if let Err(err) = session.cancel().await {
        tracing::warn!(worker_id = %id, error = %err, "session/cancel failed for worker");
    }

    // Journal the explicit cancel. The pump will additionally journal a
    // `WorkerComplete` once it exits, so the pair documents both intent and
    // observed end.
    let entry = JournalEntry::WorkerCancel { ts: Utc::now(), id };
    if let Err(err) = inner.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed for worker cancel");
    }

    // Fallback: if the pump does not remove the entry within the grace
    // period, force-remove it, abort the task, and tear down the session
    // and hub-tag mappings the pump would normally have cleaned up.
    let sweep_inner = inner.clone();
    tokio::spawn(async move {
        tokio::time::sleep(WORKER_CANCEL_GRACE).await;
        let removed = {
            let mut workers = sweep_inner.workers.lock().await;
            workers.remove(&id)
        };
        if let Some(entry) = removed {
            tracing::warn!(worker_id = %id, "worker pump did not exit within grace; force-removing");
            entry.pump_handle.abort();
            {
                let mut g = sweep_inner.session_peers.lock().await;
                g.remove(entry.session.id());
            }
            sweep_inner.mcp_hub.unregister_session(&entry.tag).await;
        }
    });

    Ok(())
}

/// Worker pump task body. Drives the session through one prompt and forwards
/// every text update to /thought addressed to `peer`. On completion (clean or
/// cancelled), journals `WorkerComplete`, unregisters the session_id → peer
/// mapping, drops the MCP tag, and removes the registry entry.
async fn run_worker_pump(
    inner: Arc<ReactorInner>,
    id: WorkerId,
    peer: PeerId,
    brief: String,
    session: Arc<AcpSession>,
    tag: SessionTag,
) {
    let session_id = session.id().clone();

    // Drive the prompt and stream updates. We always reach the cleanup block
    // below regardless of where in the loop we exit.
    match session.prompt(brief).await {
        Ok(mut run) => loop {
            let update = run.next_update().await;
            match update {
                Some(SessionUpdate::Text(text)) => {
                    inner.emit_thought(&peer, text).await;
                }
                Some(SessionUpdate::Thought(_)) => {
                    tracing::debug!(worker_id = %id, "worker internal reasoning");
                }
                Some(SessionUpdate::ToolCall(stub)) => {
                    tracing::debug!(
                        worker_id = %id,
                        variant = stub.raw_variant,
                        "worker tool call"
                    );
                }
                Some(SessionUpdate::Other(name)) => {
                    tracing::trace!(worker_id = %id, variant = %name, "ignored worker update");
                }
                None => break,
            }
        },
        Err(err) => {
            tracing::warn!(worker_id = %id, error = %err, "worker prompt failed to start");
        }
    }

    // Cleanup. Order: journal complete → drop session_id mapping → drop hub
    // session tag → remove registry entry → drop the Arc<AcpSession>.
    let entry = JournalEntry::WorkerComplete { ts: Utc::now(), id };
    if let Err(err) = inner.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed for worker complete");
    }

    {
        let mut g = inner.session_peers.lock().await;
        g.remove(&session_id);
    }
    inner.mcp_hub.unregister_session(&tag).await;
    {
        let mut workers = inner.workers.lock().await;
        workers.remove(&id);
    }
    drop(session);
}
