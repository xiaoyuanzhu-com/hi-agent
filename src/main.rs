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

    /// macOS only: run headless (no menu-bar icon), giving the HTTP server the main
    /// thread as on Linux/Docker. The tray is also auto-skipped under SSH (no window
    /// server). No effect on other platforms.
    #[arg(long)]
    no_tray: bool,
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

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    if cli.purge_voice_galleries {
        let data_dir = cli.data_dir.clone();
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async move {
            let removed = hi_agent::mind::memory::people_vectors::purge_voice(&data_dir).await?;
            tracing::info!(removed, data_dir = %data_dir.display(), "purged voice galleries");
            Ok(())
        });
    }

    let agent = hi_agent::foundation::config::AgentConfig::load()?;
    // Read on every platform (so the flag is never dead code); only consulted on
    // macOS, where it selects the headless/server-owns-main-thread path.
    let no_tray = cli.no_tray;
    let config = hi_agent::Config {
        port: cli.port,
        data_dir: cli.data_dir,
        agent,
    };

    // On macOS the default install shape is a desktop app: AppKit owns the main
    // thread and shows a menu-bar icon, while the HTTP server runs on a background
    // thread (see `hi_agent::run_with_tray`). Skip it — and keep today's behavior of
    // the server owning the main thread — when explicitly disabled (`--no-tray`) or
    // when there is no window server (running over SSH, where AppKit can't draw).
    #[cfg(target_os = "macos")]
    {
        let headless = no_tray || std::env::var_os("SSH_CONNECTION").is_some();
        if !headless {
            return hi_agent::run_with_tray(config);
        }
        tracing::info!("tray skipped (headless); serving without a menu-bar icon");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = no_tray;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(hi_agent::run(config))
}
