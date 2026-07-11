//! ACP subprocess lifecycle.
//!
//! Owns one child process (e.g. `claude-agent-acp`) and the long-lived JSON-RPC
//! connection to it, hosting exactly **one** ACP session. [`spawn`](AcpProcess::spawn)
//! returns the process plus the single stream of [`SessionUpdate`]s, and
//! [`open_session`](AcpProcess::open_session) opens that one session. There is no
//! `session_id` demux — every notification on the connection belongs to the one
//! session, so updates flow straight to that stream.
//!
//! `session/request_permission` is auto-allowed: without an explicit user
//! gate (we removed the `/approval` bridge), the agent's native tools would
//! hang waiting for a decision the harness never delivers. The fix here picks
//! the first `AllowOnce` (or `AllowAlways`) option from the request and
//! responds immediately. If the agent ever needs a human gate, re-wire a
//! bridge here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol as acp;
use acp::schema::ProtocolVersion;
use acp::schema::v1::{
    InitializeRequest, McpServer, NewSessionRequest, RequestPermissionRequest,
    RequestPermissionResponse, RequestPermissionOutcome, SelectedPermissionOutcome, SessionId,
    SessionNotification,
};
use anyhow::{Context, anyhow};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::foundation::acp::session::SessionUpdate;
use crate::foundation::acp::tap::{AcpTap, Dir};

/// Allocates the per-connection id the tap uses to group one session's frames
/// (one subprocess hosts one session). Process-global and monotonic.
static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Options for opening a new ACP session.
#[derive(Debug, Default, Clone)]
pub struct SessionOpts {
    /// System prompt the session should be primed with. ACP has no dedicated
    /// system-prompt slot on `session/new`; callers compose this into their
    /// initial `PromptRequest`.
    pub system_prompt: Option<String>,

    /// Working directory for the session. Defaults to the current process's
    /// cwd when not set. ACP requires this to be absolute.
    pub cwd: Option<PathBuf>,
}

/// One child-process-hosted ACP connection, hosting a single session.
pub struct AcpProcess {
    connection: acp::ConnectionTo<acp::Agent>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

/// Tracks every live ACP subprocess's driver task, so the host can reap them all
/// on shutdown rather than leaking orphaned `node`/`claude` children.
///
/// Each [`AcpProcess::spawn`] registers its driver here under the per-connection
/// id; the driver task removes its own entry when it exits (its session handle
/// was dropped, which signals shutdown), so the map only ever holds *live*
/// processes. [`shutdown`](Self::shutdown) aborts whatever remains — dropping a
/// driver future drops the ACP crate's `ChildGuard`, which `kill()`s the child.
#[derive(Clone, Default)]
pub struct ProcessRegistry {
    inner: Arc<Mutex<HashMap<u64, JoinHandle<anyhow::Result<()>>>>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, id: u64, driver: JoinHandle<anyhow::Result<()>>) {
        self.inner.lock().expect("process registry mutex").insert(id, driver);
    }

    /// Drop a driver handle from the live map. Called by the driver's own guard
    /// when it exits; removing a finished task's handle is harmless.
    fn remove(&self, id: u64) {
        let _ = self.inner.lock().expect("process registry mutex").remove(&id);
    }

    /// Reap every live ACP subprocess. Aborting a driver future drops its
    /// `ChildGuard` (which `kill()`s the child); awaiting the aborted handle
    /// confirms the kill ran before we return. The caller should bound this with
    /// a timeout so a wedged child can't hang process exit.
    pub async fn shutdown(&self) {
        let drivers: Vec<JoinHandle<anyhow::Result<()>>> = {
            let mut map = self.inner.lock().expect("process registry mutex");
            map.drain().map(|(_, driver)| driver).collect()
        };
        if drivers.is_empty() {
            tracing::info!("no live ACP subprocesses to reap");
            return;
        }
        let n = drivers.len();
        tracing::info!(sessions = n, "reaping ACP subprocesses");
        for driver in drivers {
            driver.abort();
            let _ = driver.await;
        }
        tracing::info!(sessions = n, "ACP subprocesses reaped");
    }
}

/// Removes one driver's entry from the [`ProcessRegistry`] when the driver task
/// exits — by clean shutdown, error, or abort. Lives inside the driver task's
/// future, so it fires however that future ends.
struct RegistryGuard {
    registry: ProcessRegistry,
    id: u64,
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        self.registry.remove(self.id);
    }
}

impl AcpProcess {
    pub async fn spawn(
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
        tap: AcpTap,
        scene: String,
        registry: &ProcessRegistry,
    ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<SessionUpdate>)> {
        let program_str = program
            .to_str()
            .ok_or_else(|| anyhow!("program path is not valid UTF-8: {}", program.display()))?
            .to_string();
        let argv = build_argv(&program_str, &args, &env);

        let agent = acp::AcpAgent::from_args(argv.iter())
            .with_context(|| format!("constructing AcpAgent for {}", program.display()))?;

        // One id per subprocess (= per session), so the tap can group this
        // connection's frames — including its pre-`sessionId` handshake.
        let conn = CONN_SEQ.fetch_add(1, Ordering::Relaxed);

        let agent = agent.with_debug(move |line: &str, direction: acp::LineDirection| {
            // Mirror every frame to the raw ACP tap (the inspector's window),
            // tagged with this connection + scene, before the existing tracing.
            tap.record(
                conn,
                &scene,
                match direction {
                    acp::LineDirection::Stdin => Dir::Send,
                    acp::LineDirection::Stdout => Dir::Recv,
                    acp::LineDirection::Stderr => Dir::Stderr,
                },
                line,
            );
            match direction {
                acp::LineDirection::Stdin => tracing::trace!(target: "acp::send", "{line}"),
                acp::LineDirection::Stdout => tracing::trace!(target: "acp::recv", "{line}"),
                // The ACP adapter logs `Unexpected case: {...}` to stderr (via its
                // `unreachable` fallback) for any stream message subtype it lacks an
                // explicit case for. We capture every adapter stderr line and surface
                // it as a WARN — but `thinking_tokens` is a benign, informational token
                // estimate from a newer CLI than the adapter; treating it as "known,
                // no-op" here keeps the warning channel meaningful for real issues.
                acp::LineDirection::Stderr
                    if line.contains("Unexpected case") && line.contains("thinking_tokens") =>
                {
                    tracing::trace!(target: "acp::stderr", "{line}")
                }
                acp::LineDirection::Stderr => tracing::warn!(target: "acp::stderr", "{line}"),
            }
        });

        // The single session's update stream. Created before the connection
        // handler is wired so it can forward into it; notifications only arrive
        // after `open_session` + a prompt, so the early channel sees nothing yet.
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let update_tx_for_handler = update_tx.clone();
        let (conn_tx, conn_rx) = oneshot::channel::<acp::ConnectionTo<acp::Agent>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // The driver self-removes from the registry when it exits, so the live
        // map never accumulates finished sessions. Moved into the task below.
        let registry_guard = RegistryGuard { registry: registry.clone(), id: conn };

        let driver: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            let _registry_guard = registry_guard;
            let result: acp::Result<()> = acp::Client
                .builder()
                .on_receive_notification(
                    move |notification: SessionNotification,
                          _cx: acp::ConnectionTo<acp::Agent>| {
                        let tx = update_tx_for_handler.clone();
                        async move {
                            dispatch_session_update(&tx, notification);
                            Ok(())
                        }
                    },
                    acp::on_receive_notification!(),
                )
                .on_receive_request(
                    move |request: RequestPermissionRequest,
                          responder: acp::Responder<RequestPermissionResponse>,
                          _cx: acp::ConnectionTo<acp::Agent>| {
                        async move {
                            // Do not block the ACP event loop: detach.
                            tokio::spawn(async move {
                                auto_allow(request, responder);
                            });
                            Ok(())
                        }
                    },
                    acp::on_receive_request!(),
                )
                .connect_with(agent, |connection: acp::ConnectionTo<acp::Agent>| async move {
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    tracing::info!(agent_info = ?init.agent_info, "ACP connection initialised");

                    if conn_tx.send(connection).is_err() {
                        tracing::warn!("AcpProcess owner dropped before init completed");
                        return Ok(());
                    }

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
                let err = driver.await.unwrap_or_else(|join_err| {
                    Err(anyhow!("ACP driver task panicked: {join_err}"))
                });
                return Err(err.err().unwrap_or_else(|| {
                    anyhow!("ACP driver ended unexpectedly during initialize")
                }));
            }
        };

        // Hand the live driver to the registry so host shutdown can reap this
        // subprocess; the guard inside the task removes it when the session
        // handle drops and signals shutdown.
        registry.insert(conn, driver);

        Ok((
            Self {
                connection,
                shutdown_tx: Some(shutdown_tx),
            },
            update_rx,
        ))
    }

    /// Open this process's single ACP session and return its id. The id is used
    /// to address `session/prompt` and `session/cancel`; inbound notifications
    /// are not routed by it — they all flow to the stream returned by
    /// [`spawn`](Self::spawn).
    pub async fn open_session(
        &self,
        opts: SessionOpts,
        mcp_servers: Vec<McpServer>,
    ) -> anyhow::Result<SessionId> {
        let cwd = match opts.cwd {
            Some(p) => p,
            None => std::env::current_dir().context("reading current dir for new session")?,
        };

        let req = NewSessionRequest::new(cwd).mcp_servers(mcp_servers);

        let resp = self
            .connection
            .send_request(req)
            .block_task()
            .await
            .map_err(|e| anyhow!("session/new failed: {e}"))?;

        tracing::info!(session_id = %resp.session_id.0, "ACP session opened");
        Ok(resp.session_id)
    }

    /// The shared JSON-RPC connection, for the owning session to drive prompts on.
    pub(crate) fn connection(&self) -> &acp::ConnectionTo<acp::Agent> {
        &self.connection
    }

    fn signal_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

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

impl Drop for AcpProcess {
    fn drop(&mut self) {
        self.signal_shutdown();
    }
}

/// Forward one ACP notification's updates to the process's single session
/// stream. With one session per connection there is no `session_id` demux —
/// every notification belongs to that session.
fn dispatch_session_update(tx: &mpsc::UnboundedSender<SessionUpdate>, n: SessionNotification) {
    for u in SessionUpdate::from_acp(&n.update) {
        if tx.send(u).is_err() {
            tracing::debug!(
                session_id = %n.session_id.0,
                "session receiver dropped while update arrived"
            );
            break;
        }
    }
}

/// Auto-allow every `session/request_permission`: pick the first AllowOnce (or
/// AllowAlways) option from the request. We removed `/approval` per the
/// human-interface direction — sensitive actions ought to be policy choices,
/// not modal prompts.
fn auto_allow(
    request: RequestPermissionRequest,
    responder: acp::Responder<RequestPermissionResponse>,
) {
    use acp::schema::v1::PermissionOptionKind as K;
    let pick = request
        .options
        .iter()
        .find(|o| matches!(o.kind, K::AllowOnce))
        .or_else(|| request.options.iter().find(|o| matches!(o.kind, K::AllowAlways)))
        .map(|o| o.option_id.clone());

    let response = match pick {
        Some(option_id) => RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
        ),
        None => {
            tracing::warn!(
                session_id = %request.session_id.0,
                "no Allow option offered; replying Cancelled"
            );
            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
        }
    };

    if let Err(err) = responder.respond(response) {
        tracing::warn!(error = %err, "auto_allow respond failed");
    }
}

#[cfg(test)]
mod tests {
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
