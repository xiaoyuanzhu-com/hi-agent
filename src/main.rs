use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "hi-agent", about = "Reference implementation of the human-interface spec")]
#[command(version = version_string())]
struct Cli {
    /// HTTP port to bind on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Root for memory (`memory/raw/…`), the soul, and runtime state. Unset: a
    /// packaged `.app` uses the OS data dir (`~/Library/Application Support/
    /// dev.human-interface.hi-agent`); a bare/dev binary uses `./data`.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Delete every person's voice gallery (voice.f32 + voice/ previews) and exit.
    /// One-shot maintenance to clear voiceprint clusters contaminated before the
    /// per-speaker span-slicing fix; face data, names, and prose facets are kept.
    #[arg(long)]
    purge_voice_galleries: bool,

    /// Package-time only: download + lay out the full managed runtime, recognition
    /// models, and static ffmpeg under <DIR> (a `.app`'s `Contents/Resources`),
    /// then exit. Hidden — driven by `make dmg`, not a normal run mode.
    #[arg(long, hide = true, value_name = "DIR")]
    provision_into: Option<PathBuf>,

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

/// The data dir to use when `--data-dir` is unset. A packaged `.app` gets the OS
/// data dir (writable, stable across launches); a bare/dev binary keeps the
/// historical cwd-relative `./data`.
fn default_data_dir() -> PathBuf {
    if hi_agent::bundle::resources_dir().is_some() {
        if let Some(dirs) = directories::ProjectDirs::from("dev", "human-interface", "hi-agent") {
            return dirs.data_dir().to_path_buf();
        }
    }
    PathBuf::from("./data")
}

/// Package-time: lay out the full managed runtime, the three recognition models,
/// and the static ffmpeg under `into` (a `.app`'s `Contents/Resources`), so the
/// shipped app runs hermetically. Each provisioner targets its own subdir, matching
/// where the runtime resolvers look at launch, and reuses a shared content-addressed
/// cache — so a repeat `make dmg` (or a prior `make dev`) downloads nothing.
async fn provision(into: PathBuf) -> anyhow::Result<()> {
    hi_agent::runtime::provision_into(&into.join("runtime"))
        .await
        .context("provisioning the managed runtime")?;
    for spec in [
        &hi_agent::foundation::models::CAMPLUS,
        &hi_agent::foundation::models::SCRFD,
        &hi_agent::foundation::models::ARCFACE,
    ] {
        hi_agent::foundation::models::provision_into(&into.join("models"), spec)
            .await
            .with_context(|| format!("provisioning model {}", spec.name))?;
    }
    hi_agent::foundation::vendors::ffmpeg::provision_into(&into.join("ffmpeg"))
        .await
        .context("provisioning static ffmpeg")?;
    tracing::info!(into = %into.display(), "bundle resources provisioned");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    // Package-time provisioning: fill a `.app`'s Resources with the managed
    // runtime + models + static ffmpeg, then exit. Forces the managed downloads
    // (never resolves a system runtime), so it stages a complete tree even on a
    // host that has node/claude/ffmpeg installed. Driven by `make dmg`.
    if let Some(into) = cli.provision_into {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(provision(into));
    }

    // Effective data dir: explicit flag wins; otherwise the OS data dir inside a
    // packaged `.app` (Finder launches with cwd `/`, so `./data` would write to
    // `/data`), or `./data` for a bare/dev binary.
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);

    if cli.purge_voice_galleries {
        let data_dir = data_dir.clone();
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async move {
            let removed = hi_agent::mind::memory::people_vectors::purge_voice(&data_dir).await?;
            tracing::info!(removed, data_dir = %data_dir.display(), "purged voice galleries");
            Ok(())
        });
    }

    let agent = hi_agent::foundation::config::AgentConfig::load()?;
    // Auth gate config (HI_AGENT_AUTH + OIDC/owner vars). Off by default; when
    // enabled, a missing OIDC var is a hard startup error (fail closed).
    let auth = hi_agent::foundation::auth::AuthConfig::from_env()?;
    // Read on every platform (so the flag is never dead code); only consulted on
    // macOS, where it selects the headless/server-owns-main-thread path.
    let no_tray = cli.no_tray;
    let config = hi_agent::Config {
        port: cli.port,
        data_dir,
        agent,
        auth,
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
