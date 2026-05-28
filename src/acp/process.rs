//! ACP subprocess lifecycle.
//!
//! Owns one child process (e.g. `claude-code`) and the long-lived JSON-RPC
//! connection to it. Hands out [`AcpSession`] handles for higher layers to
//! drive prompts on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol as acp;
use acp::schema::{
    EnvVariable, InitializeRequest, McpServer, McpServerStdio, NewSessionRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification,
};
use anyhow::{Context, anyhow};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::acp::session::{AcpSession, SessionUpdate};

/// MCP server descriptor passed to `session/new`.
///
/// Step 4 fills this in: the in-process MCP hub generates one of these per
/// routing session. The fields are deliberately a thin shadow of
/// [`McpServerStdio`] — only stdio transport is mandatory across ACP agents,
/// and the hub spawns a tiny shim process that proxies stdio to a Unix socket.
#[derive(Debug, Clone, Default)]
pub struct McpServerCfg {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpServerCfg {
    fn to_acp(&self) -> Option<McpServer> {
        if self.command.as_os_str().is_empty() {
            return None;
        }
        let env: Vec<EnvVariable> = self
            .env
            .iter()
            .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
            .collect();
        let stdio = McpServerStdio::new(self.name.clone(), self.command.clone())
            .args(self.args.clone())
            .env(env);
        Some(McpServer::Stdio(stdio))
    }
}

/// Options for opening a new ACP session.
#[derive(Debug, Default, Clone)]
pub struct SessionOpts {
    /// System prompt the session should be primed with. Plumbed in Step 3 via
    /// the first prompt; ACP does not have a `system_prompt` field on
    /// `session/new` in the protocol's current shape, so callers compose this
    /// into their initial `PromptRequest`.
    pub system_prompt: Option<String>,

    /// MCP servers to attach to this session. Empty for Step 2.
    pub mcp_servers: Vec<McpServerCfg>,

    /// Working directory for the session. Defaults to the current process's
    /// cwd when not set. ACP requires this to be absolute.
    pub cwd: Option<PathBuf>,
}

/// Shared routing table: per-session sender of [`SessionUpdate`]s.
///
/// Populated by [`AcpProcess::new_session`] and drained by the notification
/// handler installed in the connection builder. Sessions register themselves
/// before the first prompt so that updates arriving immediately after
/// `session/new` aren't lost.
pub(crate) type RoutingTable =
    Arc<Mutex<HashMap<SessionId, mpsc::UnboundedSender<SessionUpdate>>>>;

// ---------------------------------------------------------------------------
// Approval bridge — Step 7
// ---------------------------------------------------------------------------

/// Outcome handed back from the reactor's approval bridge to this module.
/// Mirrors `reactor::ApprovalOutcome` but kept here so `acp/process.rs` does
/// not depend on the reactor module directly.
#[derive(Debug, Clone)]
pub enum ApprovalOutcome {
    /// User decision arrived within the timeout.
    Decision { allow: bool, reason: Option<String> },
    /// Timed out (5 minutes per impl.md § Approval) or the requester dropped.
    Expired,
}

/// Subset of reactor functionality the ACP request-permission handler needs.
/// Implemented by the reactor; attached via [`AcpProcess::attach_bridge`]
/// after both exist.
#[async_trait::async_trait]
pub trait ApprovalBridge: Send + Sync {
    /// Resolve the peer the given ACP session is acting for, if any.
    async fn peer_for_session(&self, session_id: &SessionId) -> Option<crate::types::PeerId>;

    /// Submit an approval request for the given peer. Journals the request,
    /// broadcasts the event, parks a oneshot, and awaits the user's decision
    /// (or the 5-minute timeout). Returns the outcome.
    async fn submit_approval_request(
        &self,
        peer: crate::types::PeerId,
        action: String,
        summary: String,
        details: serde_json::Value,
    ) -> anyhow::Result<ApprovalOutcome>;
}

/// Lazily-attached bridge handle. The reactor is constructed after
/// `AcpProcess`, so the handler closure captures this slot and reads it on
/// each request rather than holding the bridge directly.
pub(crate) type BridgeSlot = Arc<Mutex<Option<Arc<dyn ApprovalBridge>>>>;

/// One child-process-hosted ACP connection.
///
/// Cheap to share via `Arc<AcpProcess>` — internally the connection handle is
/// already `Clone`. Sessions outlive `&self` borrows; nothing here serialises
/// per-session work behind a global lock.
pub struct AcpProcess {
    connection: acp::ConnectionTo<acp::Agent>,
    routing: RoutingTable,
    /// Slot for the approval bridge. Filled by `attach_bridge` after the
    /// reactor exists. The request-permission handler reads through this slot
    /// so that handler registration can happen before the reactor does.
    bridge: BridgeSlot,
    shutdown_tx: Option<oneshot::Sender<()>>,
    driver: Option<JoinHandle<anyhow::Result<()>>>,
}

impl AcpProcess {
    /// Spawn an ACP agent as a child process and complete the `initialize`
    /// handshake. `program` is the binary to run; `args` are additional
    /// arguments. Stderr from the child is forwarded to `tracing::warn!`.
    pub async fn spawn(program: PathBuf, args: Vec<String>) -> anyhow::Result<Self> {
        let program_str = program
            .to_str()
            .ok_or_else(|| anyhow!("program path is not valid UTF-8: {}", program.display()))?
            .to_string();
        let mut argv: Vec<String> = Vec::with_capacity(1 + args.len());
        argv.push(program_str);
        argv.extend(args);

        let agent = acp::AcpAgent::from_args(argv.iter())
            .with_context(|| format!("constructing AcpAgent for {}", program.display()))?;

        // Surface every JSON-RPC line at trace level so reactor authors can
        // debug the wire. Stderr from the child is also routed through this
        // hook — bumped to `warn!` so transport-level failures are visible
        // without enabling trace logging.
        let agent = agent.with_debug(|line: &str, direction: acp::LineDirection| {
            match direction {
                acp::LineDirection::Stdin => tracing::trace!(target: "acp::send", "{line}"),
                acp::LineDirection::Stdout => tracing::trace!(target: "acp::recv", "{line}"),
                acp::LineDirection::Stderr => tracing::warn!(target: "acp::stderr", "{line}"),
            }
        });

        let routing: RoutingTable = Arc::new(Mutex::new(HashMap::new()));
        let bridge: BridgeSlot = Arc::new(Mutex::new(None));

        // The driver task runs the connection's event loop forever. Inside the
        // `connect_with` closure we (a) initialize, (b) hand a clone of the
        // ConnectionTo back to this constructor, then (c) park on a shutdown
        // signal so the loop keeps processing notifications.
        let routing_for_handler = routing.clone();
        let bridge_for_handler = bridge.clone();
        let (conn_tx, conn_rx) = oneshot::channel::<acp::ConnectionTo<acp::Agent>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let driver: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            let result: acp::Result<()> = acp::Client
                .builder()
                .on_receive_notification(
                    move |notification: SessionNotification,
                          _cx: acp::ConnectionTo<acp::Agent>| {
                        // Clone the routing Arc per invocation so the handler
                        // can fire many times — the outer closure is FnMut and
                        // each future owns its own Arc.
                        let routing = routing_for_handler.clone();
                        async move {
                            dispatch_session_update(routing, notification).await;
                            Ok(())
                        }
                    },
                    acp::on_receive_notification!(),
                )
                .on_receive_request(
                    move |request: RequestPermissionRequest,
                          responder: acp::Responder<RequestPermissionResponse>,
                          _cx: acp::ConnectionTo<acp::Agent>| {
                        // Step 7 bridge: do not block the ACP event loop.
                        // Spawn a task that resolves the approval through the
                        // reactor and replies asynchronously.
                        let bridge_slot = bridge_for_handler.clone();
                        async move {
                            tokio::spawn(async move {
                                dispatch_request_permission(bridge_slot, request, responder).await;
                            });
                            Ok(())
                        }
                    },
                    acp::on_receive_request!(),
                )
                .connect_with(agent, |connection: acp::ConnectionTo<acp::Agent>| async move {
                    // Handshake. We send V1; the response carries the version
                    // the agent agreed on.
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    tracing::info!(
                        agent_info = ?init.agent_info,
                        "ACP connection initialised"
                    );

                    // Surface the connection to the constructor. If the
                    // receiver is gone the caller dropped out — bail.
                    if conn_tx.send(connection).is_err() {
                        tracing::warn!("AcpProcess owner dropped before init completed");
                        return Ok(());
                    }

                    // Park until shutdown. The connect_with closure must hold
                    // the event loop open; returning here would tear the
                    // transport down before the rest of hi-agent can use it.
                    let _ = shutdown_rx.await;
                    tracing::info!("ACP driver received shutdown signal");
                    Ok(())
                })
                .await;

            match &result {
                Ok(()) => tracing::info!("ACP driver exited cleanly"),
                Err(e) => tracing::warn!(error = %e, "ACP driver exited with error"),
            }
            result.map_err(|e| anyhow!("ACP driver: {e}"))
        });

        let connection = match conn_rx.await {
            Ok(c) => c,
            Err(_) => {
                // Driver task ended before sending the connection — most
                // likely the child failed to spawn or `initialize` failed.
                let err = driver.await.unwrap_or_else(|join_err| {
                    Err(anyhow!("ACP driver task panicked: {join_err}"))
                });
                return Err(err.err().unwrap_or_else(|| {
                    anyhow!("ACP driver ended unexpectedly during initialize")
                }));
            }
        };

        Ok(Self {
            connection,
            routing,
            bridge,
            shutdown_tx: Some(shutdown_tx),
            driver: Some(driver),
        })
    }

    /// Inject the approval bridge after the reactor is constructed. Called
    /// from `lib.rs` once both exist. Without a bridge, incoming
    /// `session/request_permission` requests are rejected (mapped to a
    /// `Cancelled` outcome).
    pub async fn attach_bridge(&self, bridge: Arc<dyn ApprovalBridge>) {
        let mut slot = self.bridge.lock().await;
        *slot = Some(bridge);
    }

    /// Open a new ACP session on the existing process.
    pub async fn new_session(&self, opts: SessionOpts) -> anyhow::Result<AcpSession> {
        let cwd = match opts.cwd {
            Some(p) => p,
            None => std::env::current_dir().context("reading current dir for new session")?,
        };

        let mut req = NewSessionRequest::new(cwd);
        let mcp: Vec<McpServer> = opts
            .mcp_servers
            .iter()
            .filter_map(|c| c.to_acp())
            .collect();
        if !mcp.is_empty() {
            req.mcp_servers = mcp;
        }

        let resp = self
            .connection
            .send_request(req)
            .block_task()
            .await
            .map_err(|e| anyhow!("session/new failed: {e}"))?;

        let session_id = resp.session_id;
        let (tx, rx) = mpsc::unbounded_channel::<SessionUpdate>();
        {
            let mut table = self.routing.lock().await;
            table.insert(session_id.clone(), tx);
        }

        tracing::info!(session_id = %session_id.0, "ACP session opened");

        Ok(AcpSession::new(
            session_id,
            self.connection.clone(),
            self.routing.clone(),
            rx,
            opts.system_prompt,
        ))
    }

    /// Trigger a graceful shutdown of the driver task. The child process is
    /// torn down by the ACP transport when the connection ends.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.signal_shutdown();
        if let Some(driver) = self.driver.take() {
            match driver.await {
                Ok(inner) => inner?,
                Err(join_err) => return Err(anyhow!("ACP driver join failed: {join_err}")),
            }
        }
        Ok(())
    }

    fn signal_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        // Best-effort: poke the driver so the child can exit. If shutdown was
        // already called, this is a no-op.
        self.signal_shutdown();
    }
}

async fn dispatch_session_update(routing: RoutingTable, n: SessionNotification) {
    // Translate the raw ACP `SessionUpdate` into our streaming variant and
    // forward it to the session's receiver. If the receiver is gone we drop
    // the update — the session has been closed or cancelled.
    let updates = SessionUpdate::from_acp(&n.update);
    if updates.is_empty() {
        return;
    }

    let table = routing.lock().await;
    if let Some(tx) = table.get(&n.session_id) {
        for u in updates {
            if tx.send(u).is_err() {
                tracing::debug!(
                    session_id = %n.session_id.0,
                    "session receiver dropped while update arrived"
                );
                break;
            }
        }
    } else {
        tracing::debug!(
            session_id = %n.session_id.0,
            "no receiver registered for session update"
        );
    }
}

/// Resolve one `session/request_permission` through the approval bridge.
///
/// Spawned as a detached task by the request handler so the ACP event loop
/// stays unblocked. On any error path we respond with `Cancelled` — this is
/// the same outcome the user-cancel path produces, which the agent already
/// has to handle.
async fn dispatch_request_permission(
    bridge_slot: BridgeSlot,
    request: RequestPermissionRequest,
    responder: acp::Responder<RequestPermissionResponse>,
) {
    let bridge = {
        let g = bridge_slot.lock().await;
        g.clone()
    };
    let Some(bridge) = bridge else {
        tracing::warn!(
            session_id = %request.session_id.0,
            "request_permission arrived but bridge not attached; replying Cancelled"
        );
        respond_cancelled(responder);
        return;
    };

    let peer = match bridge.peer_for_session(&request.session_id).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                session_id = %request.session_id.0,
                "request_permission for session with no peer mapping; replying Cancelled"
            );
            respond_cancelled(responder);
            return;
        }
    };

    let action = request
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| "tool call".to_string());
    let summary = action.clone();
    // Serialize the full request as `details` so the deciding client can
    // render whatever it wants. `to_value` on a non-serializable input is
    // unreachable for ACP schema types; fall back to Null on the unlikely
    // error path to keep the handler robust.
    let details = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);

    let outcome = match bridge
        .submit_approval_request(peer, action, summary, details)
        .await
    {
        Ok(o) => o,
        Err(err) => {
            tracing::warn!(error = %err, "approval bridge errored; replying Cancelled");
            respond_cancelled(responder);
            return;
        }
    };

    let response = match outcome {
        ApprovalOutcome::Decision { allow, reason: _ } => {
            // Map allow/deny back onto one of the agent-provided options. We
            // prefer "once" variants — v0 does not surface "always" semantics
            // through `/approval` (the spec body has only `{allow, reason}`).
            let want_allow = allow;
            let option_id = pick_option(&request.options, want_allow);
            match option_id {
                Some(option_id) => RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ),
                None => {
                    // The agent didn't offer a matching option. Cancel so it
                    // can decide its own fallback rather than silently
                    // succeed-or-fail in the wrong direction.
                    tracing::warn!(
                        session_id = %request.session_id.0,
                        want_allow,
                        "no permission option matches decision; replying Cancelled"
                    );
                    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                }
            }
        }
        ApprovalOutcome::Expired => {
            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
        }
    };

    if let Err(err) = responder.respond(response) {
        tracing::warn!(error = %err, "responding to request_permission failed");
    }
}

fn respond_cancelled(responder: acp::Responder<RequestPermissionResponse>) {
    let resp = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
    if let Err(err) = responder.respond(resp) {
        tracing::warn!(error = %err, "respond_cancelled failed");
    }
}

/// Map a boolean decision onto one of the agent-offered `PermissionOption`s.
/// Prefers "once" variants over "always" since `/approval` carries a single
/// decision without "remember this" semantics.
fn pick_option(
    options: &[acp::schema::PermissionOption],
    want_allow: bool,
) -> Option<acp::schema::PermissionOptionId> {
    use acp::schema::PermissionOptionKind as K;
    let preferred = if want_allow { K::AllowOnce } else { K::RejectOnce };
    let fallback = if want_allow { K::AllowAlways } else { K::RejectAlways };

    options
        .iter()
        .find(|o| matches_kind(&o.kind, &preferred))
        .or_else(|| options.iter().find(|o| matches_kind(&o.kind, &fallback)))
        .map(|o| o.option_id.clone())
}

fn matches_kind(
    a: &acp::schema::PermissionOptionKind,
    b: &acp::schema::PermissionOptionKind,
) -> bool {
    use acp::schema::PermissionOptionKind as K;
    matches!(
        (a, b),
        (K::AllowOnce, K::AllowOnce)
            | (K::AllowAlways, K::AllowAlways)
            | (K::RejectOnce, K::RejectOnce)
            | (K::RejectAlways, K::RejectAlways)
    )
}
