//! ACP subprocess lifecycle.
//!
//! Owns one child process (e.g. `claude-agent-acp`) and the long-lived
//! JSON-RPC connection to it. Hands out [`AcpSession`] handles for higher
//! layers to drive prompts on.
//!
//! `session/request_permission` is auto-allowed: without an explicit user
//! gate (we removed the `/approval` bridge), the agent's native tools would
//! hang waiting for a decision the harness never delivers. The fix here picks
//! the first `AllowOnce` (or `AllowAlways`) option from the request and
//! responds immediately. If the agent ever needs a human gate, re-wire a
//! bridge here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol as acp;
use acp::schema::{
    InitializeRequest, McpServer, NewSessionRequest, ProtocolVersion, RequestPermissionRequest,
    RequestPermissionResponse, RequestPermissionOutcome, SelectedPermissionOutcome, SessionId,
    SessionNotification,
};
use anyhow::{Context, anyhow};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::acp::session::{AcpSession, SessionUpdate};

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

/// Shared routing table: per-session sender of [`SessionUpdate`]s.
pub(crate) type RoutingTable =
    Arc<Mutex<HashMap<SessionId, mpsc::UnboundedSender<SessionUpdate>>>>;

/// One child-process-hosted ACP connection.
pub struct AcpProcess {
    connection: acp::ConnectionTo<acp::Agent>,
    routing: RoutingTable,
    shutdown_tx: Option<oneshot::Sender<()>>,
    driver: Option<JoinHandle<anyhow::Result<()>>>,
}

impl AcpProcess {
    pub async fn spawn(
        program: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<Self> {
        let program_str = program
            .to_str()
            .ok_or_else(|| anyhow!("program path is not valid UTF-8: {}", program.display()))?
            .to_string();
        let argv = build_argv(&program_str, &args, &env);

        let agent = acp::AcpAgent::from_args(argv.iter())
            .with_context(|| format!("constructing AcpAgent for {}", program.display()))?;

        let agent = agent.with_debug(|line: &str, direction: acp::LineDirection| {
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

        let routing: RoutingTable = Arc::new(Mutex::new(HashMap::new()));
        let routing_for_handler = routing.clone();
        let (conn_tx, conn_rx) = oneshot::channel::<acp::ConnectionTo<acp::Agent>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let driver: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            let result: acp::Result<()> = acp::Client
                .builder()
                .on_receive_notification(
                    move |notification: SessionNotification,
                          _cx: acp::ConnectionTo<acp::Agent>| {
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

        Ok(Self {
            connection,
            routing,
            shutdown_tx: Some(shutdown_tx),
            driver: Some(driver),
        })
    }

    pub async fn new_session(
        &self,
        opts: SessionOpts,
        mcp_servers: Vec<McpServer>,
    ) -> anyhow::Result<AcpSession> {
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

async fn dispatch_session_update(routing: RoutingTable, n: SessionNotification) {
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

/// Auto-allow every `session/request_permission`: pick the first AllowOnce (or
/// AllowAlways) option from the request. We removed `/approval` per the
/// human-interface direction — sensitive actions ought to be policy choices,
/// not modal prompts.
fn auto_allow(
    request: RequestPermissionRequest,
    responder: acp::Responder<RequestPermissionResponse>,
) {
    use acp::schema::PermissionOptionKind as K;
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
