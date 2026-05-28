use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "hi-agent", about = "Reference implementation of the human-interface spec")]
struct Cli {
    /// HTTP port to bind on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Where journal.jsonl and intents.jsonl live.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Re-exec branch: when claude-code spawns the MCP server, it runs us as
    // `hi-agent mcp-shim`. The shim is a tiny stdio↔Unix-socket relay (see
    // `mcp.rs` § "Transport"). We short-circuit before any HTTP / ACP setup.
    if std::env::args().nth(1).as_deref() == Some(hi_agent::mcp::SHIM_FLAG) {
        // Minimal logging — anything on stdout would corrupt the JSON-RPC
        // stream. Errors go to stderr via the default subscriber.
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
            )
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
        return hi_agent::mcp::run_shim_from_stdio().await;
    }

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let config = hi_agent::Config {
        port: cli.port,
        data_dir: cli.data_dir,
    };

    hi_agent::run(config).await
}
