//! hi-agent — reference implementation of the human-interface spec.
//!
//! See `docs/impl.md` for architecture. This crate exposes:
//! - `types` — spec primitives (PeerId, Channel, Signal, JournalEntry, ...).
//! - `server` — the axum HTTP front.
//! - `memory` — durable substrate (filled in by Step 6).

use std::path::PathBuf;

use tokio::net::TcpListener;

pub mod acp;
pub mod appearance;
pub mod heartbeat;
pub mod mcp;
pub mod memory;
pub mod reactor;
pub mod server;
pub mod types;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
}

/// Build the axum app, bind, and serve until the process is terminated.
///
/// Step 3 wires the reactor: spawn the `claude-code` ACP subprocess, then
/// hand its handle plus the server seams (inbound mpsc, outbound broadcasts)
/// to [`reactor::start`]. The returned `Reactor` is held for the lifetime
/// of `run` so the central dispatch task and per-peer tasks keep running.
pub async fn run(config: Config) -> anyhow::Result<()> {
    tracing::debug!(?config, "starting hi-agent");

    let memory = memory::Memory::open(&config.data_dir).await?;
    tracing::info!(data_dir = %config.data_dir.display(), "memory opened");

    let (router, seams) = server::build(memory.clone());

    // ACP subprocess: held for the lifetime of hi-agent (impl.md § "Four
    // primitives → ACP").
    let acp = std::sync::Arc::new(
        acp::AcpProcess::spawn("claude-agent-acp".into(), Vec::new()).await?,
    );
    tracing::info!("ACP subprocess up");

    // === MCP hub ===
    // Built BEFORE the reactor so we can pass it in; the reactor injects
    // itself back via `attach_reactor` once it exists. `current_exe()` is
    // what the shim subprocess re-execs (`hi-agent mcp-shim`); override via
    // `HI_AGENT_SHIM_BIN` for split-binary or Docker layouts.
    let sock_path = std::env::var("HI_AGENT_MCP_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config.data_dir.join("mcp.sock"));
    let shim_program = match std::env::var("HI_AGENT_SHIM_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => std::env::current_exe()?,
    };
    let mcp_hub = mcp::start(memory.clone(), sock_path, shim_program).await?;
    tracing::info!("mcp hub started");
    // === /MCP hub ===

    let reactor = reactor::start(
        memory.clone(),
        acp.clone(),
        mcp_hub.clone(),
        seams.inbound_rx,
        seams.thought_out,
        seams.approval_out,
        seams.approval_decisions_rx,
    );
    tracing::info!("reactor started");

    // Inject the reactor back into the hub so tool handlers can dispatch.
    mcp_hub.attach_reactor(reactor.as_handle()).await;

    // Inject the reactor into the ACP process so the
    // `session/request_permission` handler can bridge through `/approval`.
    acp.attach_bridge(reactor.as_approval_bridge()).await;

    // === heartbeat ===
    heartbeat::start(memory, reactor.clone());
    tracing::info!("heartbeat started");
    // === /heartbeat ===

    let addr = ("0.0.0.0", config.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("hi-agent listening on http://0.0.0.0:{}", config.port);

    axum::serve(listener, router).await?;
    Ok(())
}
