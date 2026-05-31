//! hi-agent — reference implementation of the human-interface spec.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;

pub mod acp;
pub mod appearance;
pub mod channel_log;
pub mod config;
pub mod llm_proxy;
pub mod memory;
pub mod runtime;
pub mod reactor;
pub mod server;
pub mod types;
pub mod voice;

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

    // Voice capabilities. `None` is fine; gates affect /audio only.
    let stt = voice::build_stt()?;
    let tts = voice::build_tts()?;
    tracing::info!(
        stt = stt.is_some(),
        tts = tts.is_some(),
        "voice capabilities resolved"
    );

    let (router, seams) = server::build(memory.clone(), config.data_dir.clone(), stt);

    // Resolve the runtime (download + install on first run, reuse thereafter).
    let runtime = runtime::ensure().await?;
    tracing::info!(bundle_id = runtime::BUNDLE_ID, "runtime resolved");

    // Start the local LLM proxy; the adapter talks to it instead of the upstream.
    let proxy = llm_proxy::LlmProxy::start(
        config.agent.upstream_base_url.clone(),
        config.agent.upstream_key.clone(),
    )
    .await?;

    // Render the managed settings.json into a hi-agent-owned config dir.
    let claude_config_dir = config.data_dir.join("claude-config");
    config.agent.render_settings_json(&claude_config_dir)?;

    // Spawn the adapter via the bundled node, with managed env.
    let child_env = config.agent.child_env(
        proxy.port(),
        &claude_config_dir,
        runtime.node_bin_dir(),
        &runtime.claude_bin,
    );
    let adapter_entry = runtime
        .adapter_entry
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("adapter path not UTF-8"))?
        .to_string();
    let acp = Arc::new(
        acp::AcpProcess::spawn(runtime.node_bin.clone(), vec![adapter_entry], child_env).await?,
    );
    tracing::info!("ACP subprocess up (bundled node + adapter)");

    // Keep the proxy alive for the life of the process.
    let _proxy = proxy;

    let _reactor = reactor::start(
        memory,
        acp,
        seams.inbound_rx,
        seams.thought_bus,
        tts,
        seams.audio_out.clone(),
        seams.surface_out.clone(),
    );
    tracing::info!("reactor started");

    // Hold clones of the broadcast senders so subscribers see Lagged not Closed
    // even between turns (the reactor holds the producing clones).
    let _audio_out = seams.audio_out;
    let _surface_out = seams.surface_out;

    let addr = ("0.0.0.0", config.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("hi-agent listening on http://0.0.0.0:{}", config.port);

    axum::serve(listener, router).await?;
    Ok(())
}
