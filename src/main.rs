use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "hi-agent", about = "Reference implementation of the human-interface spec")]
struct Cli {
    /// HTTP port to bind on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Where journal.jsonl lives.
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load `.env` if present so `cargo run` picks up STT_PROVIDER and friends
    // without a separate `set -a; source .env` dance. Existing process env
    // takes precedence (dotenvy::dotenv never overwrites), so production
    // deployments that inject env via systemd/k8s/compose are unaffected.
    let _ = dotenvy::dotenv();

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
