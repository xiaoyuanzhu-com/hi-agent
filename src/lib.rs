//! hi-agent — reference implementation of the human-interface spec.

use std::path::PathBuf;

use tokio::net::TcpListener;

pub mod acp;
pub mod agent;
pub mod appearance;
pub mod capabilities;
pub mod channel_log;
pub mod config;
pub mod llm_proxy;
pub mod memory;
pub mod observatory;
pub mod runtime;
pub mod reactor;
pub mod segment;
pub mod server;
pub mod types;
pub mod vendors;
pub mod views;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    pub agent: config::AgentConfig,
}

/// Build the axum app, spawn the ACP subprocess + reactor, bind, and serve
/// until the process is terminated.
pub async fn run(config: Config) -> anyhow::Result<()> {
    tracing::debug!(?config, "starting hi-agent");

    let memory = memory::Memory::open(&config.data_dir).await?;
    tracing::info!(data_dir = %config.data_dir.display(), "memory opened");

    // Structured visibility into the ACP session lifecycle. The agent layer,
    // reactor, workers and heartbeat feed it; `GET /api/sessions` reads the live
    // mirror and `GET /api/sessions/events` streams the history over SSE.
    let observatory = observatory::Observatory::new(
        Some(config.data_dir.join("sessions.jsonl")),
        reactor::swap_budget_chars(),
    );

    // Resolve all capabilities from the environment. Unconfigured capabilities
    // are fine; gates affect /audio (STT) and the speak path (TTS) only.
    capabilities::init_from_env()?;
    tracing::info!(
        stt = capabilities::stt::available(),
        tts = capabilities::tts::available(),
        "capabilities resolved"
    );

    let (router, seams) =
        server::build(memory.clone(), config.data_dir.clone(), observatory.clone());

    // Resolve the runtime: prefer system tools on PATH, else install on first run.
    let runtime = runtime::ensure().await?;
    tracing::info!(origin = runtime.origin, "runtime resolved");

    // Start the local LLM proxy; the adapter talks to it instead of the upstream.
    let proxy = llm_proxy::LlmProxy::start(
        config.agent.upstream_base_url.clone(),
        config.agent.upstream_key.clone(),
    )
    .await?;

    // Render the managed settings.json into a hi-agent-owned config dir.
    let claude_config_dir = config.data_dir.join("claude-config");
    config.agent.render_settings_json(&claude_config_dir)?;

    // Spawn config for the agent session layer. The subprocess itself is spawned
    // lazily, one per scene, on that scene's first session (Chrome-style isolation);
    // the pinned runtime, managed env, and local LLM proxy are shared by all.
    let child_env = config.agent.child_env(
        proxy.port(),
        config.port,
        &claude_config_dir,
        runtime.node_bin_dir(),
        &runtime.claude_bin,
    );
    let adapter_entry = runtime
        .adapter_entry
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("adapter path not UTF-8"))?
        .to_string();
    let agent = agent::AgentLayer::new(
        agent::SpawnConfig {
            program: runtime.node_bin.clone(),
            args: vec![adapter_entry],
            env: child_env,
        },
        observatory.clone(),
    );
    tracing::info!("agent session layer ready (per-scene processes spawn on first contact)");

    // Keep the proxy alive for the life of the process.
    let _proxy = proxy;

    let soul = reactor::load_soul(&config.data_dir);
    let _reactor = reactor::start(
        memory,
        agent,
        soul,
        seams.inbound_rx,
        seams.warm_rx,
        seams.out_tx,
        observatory,
    );
    tracing::info!("reactor started");

    let addr = ("0.0.0.0", config.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("hi-agent listening on http://0.0.0.0:{}", config.port);

    axum::serve(listener, router).await?;
    Ok(())
}
