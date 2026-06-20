use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "hi-agent", about = "Reference implementation of the human-interface spec")]
#[command(version = version_string())]
struct Cli {
    /// HTTP port to bind on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Root for memory (`memory/raw/…`), the soul, and runtime state.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,

    /// Delete every person's voice gallery (voice.f32 + voice/ previews) and exit.
    /// One-shot maintenance to clear voiceprint clusters contaminated before the
    /// per-speaker span-slicing fix; face data, names, and prose facets are kept.
    #[arg(long)]
    purge_voice_galleries: bool,
}

/// Version line including the pinned runtime component versions.
fn version_string() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (node ", env!("HI_AGENT_NODE_VERSION"),
        "; adapter ", env!("HI_AGENT_ADAPTER_VERSION"),
        "; claude ", env!("HI_AGENT_CLAUDE_VERSION"), ")"
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    if cli.purge_voice_galleries {
        let removed = hi_agent::memory::people_vectors::purge_voice(&cli.data_dir).await?;
        tracing::info!(removed, data_dir = %cli.data_dir.display(), "purged voice galleries");
        return Ok(());
    }

    let agent = hi_agent::config::AgentConfig::load()?;
    let config = hi_agent::Config {
        port: cli.port,
        data_dir: cli.data_dir,
        agent,
    };

    hi_agent::run(config).await
}
