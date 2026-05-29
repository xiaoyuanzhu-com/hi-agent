//! hi-agent — reference implementation of the human-interface spec.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;

pub mod acp;
pub mod appearance;
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
}

/// Build the axum app, spawn the ACP subprocess + reactor, bind, and serve
/// until the process is terminated.
pub async fn run(config: Config) -> anyhow::Result<()> {
    tracing::debug!(?config, "starting hi-agent");

    let memory = memory::Memory::open(&config.data_dir).await?;
    tracing::info!(data_dir = %config.data_dir.display(), "memory opened");

    // Voice capabilities. `None` is fine; gates affect /audio only.
    let stt = voice::build_stt()?;
    let _tts = voice::build_tts()?;
    tracing::info!(
        stt = stt.is_some(),
        tts = _tts.is_some(),
        "voice capabilities resolved"
    );

    let (router, seams) = server::build(memory.clone(), config.data_dir.clone(), stt);

    let acp = Arc::new(
        acp::AcpProcess::spawn("claude-agent-acp".into(), Vec::new()).await?,
    );
    tracing::info!("ACP subprocess up");

    let _reactor = reactor::start(memory, acp, seams.inbound_rx, seams.thought_bus);
    tracing::info!("reactor started");

    // Hold the audio_out broadcast sender so subscribers see Lagged not Closed
    // while there is no producer.
    let _audio_out = seams.audio_out;

    let addr = ("0.0.0.0", config.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("hi-agent listening on http://0.0.0.0:{}", config.port);

    axum::serve(listener, router).await?;
    Ok(())
}
