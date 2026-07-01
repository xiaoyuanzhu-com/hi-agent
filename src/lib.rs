//! hi-agent — reference implementation of the human-interface spec.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::Notify;

pub mod appearance;
pub mod body;
pub mod bundle;
pub mod foundation;
pub mod identity;
pub mod mind;
pub mod net;
pub mod runtime;
pub mod types;


#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    pub agent: foundation::config::AgentConfig,
    pub auth: foundation::auth::AuthConfig,
}

/// Absolutize `dir` against the current working directory (if relative) and
/// lexically strip `.`/`..` components so it reads as a clean absolute path.
/// Purely lexical — it does not touch the filesystem or resolve symlinks.
fn normalize_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(dir)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Public entry: serve until SIGINT/SIGTERM. Thin wrapper over
/// [`run_with_shutdown`] with a trigger that never fires, so the only shutdown
/// sources are the OS signals — byte-for-byte the historical behavior. The Linux,
/// Docker, and headless-macOS paths all enter here.
pub async fn run(config: Config) -> anyhow::Result<()> {
    run_with_shutdown(config, Arc::new(Notify::new())).await
}

/// Build the axum app, spawn the ACP subprocess + reactor, bind, and serve until
/// the process is terminated by an OS signal **or** `shutdown` is notified. The
/// notify is the macOS tray's "Quit" path ([`run_with_tray`]); everywhere else it
/// is a no-op trigger handed in by [`run`].
async fn run_with_shutdown(config: Config, shutdown: Arc<Notify>) -> anyhow::Result<()> {
    // Normalize the data dir once, up front: absolutize it (it rides to child
    // processes via env, which may run with a different cwd) and strip `.`/`..`
    // components so the paths we hand the mind read as clean absolutes —
    // `.../hi-agent/data/prompts/core.md`, not `.../hi-agent/./data/prompts/core.md`.
    // Every downstream consumer (load_soul, prompts_dir, views_dir, …) inherits this.
    let mut config = config;
    config.data_dir = normalize_dir(&config.data_dir)
        .context("resolving cwd to absolutize data dir")?;
    tracing::debug!(?config, "starting hi-agent");

    // Xiaoyuanzhu: refresh the managed credential bundle from the broker, then
    // re-resolve the LLM credential from the (possibly updated) store —
    // `build_config` resolved it before this point, and BYOK mode is a no-op so
    // this changes nothing there. Best-effort: a broker failure leaves the cached
    // bundle (or boots unconfigured). No request context here, so no Authentik
    // token — a signed-in `sub` upgrade only happens on mode-select in Settings,
    // where the session token is available; startup mints/keeps the device account.
    foundation::broker::refresh(&config.data_dir, None).await;
    config.agent = foundation::config::AgentConfig::resolve(&config.data_dir);

    // Keep managed credentials fresh while running: re-fetch configs on a slow
    // cadence (rotating the access token) and poll energy on a fast one. No-op in
    // BYOK. New sessions pick up a rotated token; long-running ones on respawn.
    foundation::broker::spawn_refresh_loop(config.data_dir.clone());

    let memory = mind::memory::Memory::open(&config.data_dir).await?;
    tracing::info!(data_dir = %config.data_dir.display(), "memory opened");

    // Materialise the bundled prompts under <data_dir>/prompts/ so the mind's
    // system prompt (core.md + speaking.md + meaning.md) and the view-builder's guides
    // (appearance.md + aesthetic.md, opened as files by build sub-agents) are on
    // disk, composed with any `*.local.md` operator overrides. Absolutize the dir:
    // it rides to the child as HI_AGENT_PROMPTS_DIR, and the child may run with a
    // different cwd than us.
    identity::install_prompts(&config.data_dir).context("installing bundled prompts")?;
    let prompts_dir = {
        let d = config.data_dir.join("prompts");
        if d.is_absolute() {
            d
        } else {
            std::env::current_dir().context("resolving cwd to absolutize prompts dir")?.join(d)
        }
    };

    // The agent's view workshop — the disposable tree where views are built. It's
    // every worker's cwd (so a build sub-agent works in a real project dir) and where
    // it writes view source (`<project>/<name>.jsx`). Absolutized as above; also the
    // root the server serves at `/views/*` (compiled modules land in `_compiled`).
    let views_dir = {
        let d = config.data_dir.join("views");
        if d.is_absolute() {
            d
        } else {
            std::env::current_dir().context("resolving cwd to absolutize views dir")?.join(d)
        }
    };
    std::fs::create_dir_all(&views_dir).context("creating views dir")?;

    // Seed the bundled built-in views (the file-upload entry) into the tree so the
    // agent can show them by ref like any view. Overwritten each boot — the tree is
    // disposable, so a binary update reseeds the latest.
    mind::views::install_builtin_views(&config.data_dir).context("installing built-in views")?;

    // The agent's precious drive — where it files artifacts worth keeping (a user's
    // handed-over documents, its own kept work). Created here so it always exists;
    // filling it is the agent's job. (Verbatim annex of memory; see data-dir-layout.)
    std::fs::create_dir_all(config.data_dir.join("drive")).context("creating drive dir")?;

    // Structured visibility into the ACP session lifecycle. The agent layer,
    // reactor, workers and heartbeat feed it; `GET /api/sessions` reads the live
    // mirror and `GET /api/sessions/events` streams the history over SSE.
    let observatory = foundation::observatory::Observatory::new(
        Some(config.data_dir.join("sessions.jsonl")),
        body::reactor::swap_budget_chars(),
    );

    // Raw ACP wire tap — every JSON-RPC frame, business-logic agnostic. The agent
    // layer hands it to each scene's subprocess; `GET /api/acp/frames/events`
    // streams it to the raw session inspector.
    let acp_tap = foundation::acp::AcpTap::new();

    // Resolve all keyed capabilities BYOK-first: each vendor's key from the
    // credential store (`<data_dir>/credentials.json`) wins, else its `.env` key.
    // Unconfigured capabilities are fine; gates affect /audio (STT) and the speak
    // path (TTS) only.
    let creds = foundation::credentials::Credentials::load(&config.data_dir);
    body::capabilities::init(&creds)?;
    // Voice/face recognition need no env config — provision their pinned local
    // ONNX models on first run (cached thereafter) and load them. Best-effort:
    // a failed provision leaves the capability disabled, never blocks startup.
    body::capabilities::init_recognition().await;
    tracing::info!(
        stt = body::capabilities::stt::available(),
        tts = body::capabilities::tts::available(),
        voiceprint = body::capabilities::voiceprint::available(),
        face = body::capabilities::face::available(),
        "capabilities resolved"
    );

    // Scene→tool-sink table shared between the HTTP front's `/mcp` handler and the
    // reactor that registers each scene's sink. The mind drives output and
    // side-effects by calling tools on `/mcp`; they route here.
    let tool_registry = body::reactor::ToolRegistry::new();
    // Scene→barge-in table, shared the same way: the server's STT relay reports
    // recognized speech, the reactor stamps voice spans and folds the inferred
    // "what went unheard" note into the next prompt. No cancel, no endpoint.
    let interrupts = body::reactor::InterruptRegistry::new();
    // Scene→live-subscriber counts, shared the same way: the server's out-channel
    // handlers hold a guard per connection, the reactor renders the counts into
    // each turn as human-model facts ("no screen is attached").
    let presence = body::presence::Presence::new();

    // Build the auth gate (None when HI_AGENT_AUTH is off — the historical
    // wide-open server). Fallible: it generates/reads the cookie key under
    // <data_dir>/auth/, so a bad key file surfaces here rather than mid-request.
    let auth_state = foundation::auth::AuthState::from_config(config.auth.clone(), &config.data_dir)
        .context("initializing auth gate")?;

    let (router, seams) = foundation::server::build(
        memory.clone(),
        config.data_dir.clone(),
        observatory.clone(),
        acp_tap.clone(),
        tool_registry.clone(),
        interrupts.clone(),
        presence.clone(),
        auth_state,
    );

    // Resolve the runtime: prefer system tools on PATH, else install on first run.
    let runtime = runtime::ensure().await?;
    tracing::info!(origin = runtime.origin, "runtime resolved");

    // Render the managed settings.json into a hi-agent-owned config dir.
    // Absolutize it: it's handed to the child as `CLAUDE_CONFIG_DIR`, and the
    // child may run with a different cwd than us — a relative path would make
    // claude read a *different* dir than the one we seed the approval into.
    let claude_config_dir = {
        let dir = config.data_dir.join("claude-config");
        if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir()
                .context("resolving cwd to absolutize claude config dir")?
                .join(dir)
        }
    };
    if !config.agent.is_configured() {
        tracing::warn!(
            "no LLM credentials configured — the broker (xiaoyuanzhu) should mint them \
             automatically; otherwise set a key in Settings (BYOK). The agent boots but \
             prompts fail until a key is set"
        );
    }
    config.agent.render_settings_json(&claude_config_dir)?;
    // Pre-approve the upstream key, else Claude Code rejects the env-supplied
    // `ANTHROPIC_API_KEY` ("Please run /login") and prompts fail with -32000.
    config.agent.approve_api_key(&claude_config_dir)?;

    // Spawn config for the agent session layer. The subprocess itself is spawned
    // lazily, one per scene, on that scene's first session (Chrome-style isolation);
    // the pinned runtime and managed env are shared by all. The child reaches the
    // upstream LLM directly (no local proxy).
    let mut child_env = config.agent.child_env(
        config.port,
        &claude_config_dir,
        runtime.node_bin_dir(),
        &runtime.claude_bin,
    );
    // The view-builder sub-agent opens <prompts>/appearance.md and aesthetic.md as
    // files; hand it the absolute dir the same way workers already get
    // HI_AGENT_BASE_URL.
    child_env.push(("HI_AGENT_PROMPTS_DIR".to_string(), prompts_dir.display().to_string()));
    // Diagnostic: surface exactly what differs between launchers (terminal vs.
    // cmux etc.) — cwd, the resolved runtime binaries, the config dir claude
    // will read, and the upstream key's fingerprint vs. what we seeded.
    {
        let get = |k: &str| {
            child_env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("<unset>")
        };
        let key = get("ANTHROPIC_API_KEY");
        let fp = &key[key.len().saturating_sub(20)..];
        tracing::info!(
            cwd = ?std::env::current_dir().ok(),
            config_dir = %claude_config_dir.display(),
            config_dir_abs = ?std::fs::canonicalize(&claude_config_dir).ok(),
            runtime_origin = runtime.origin,
            claude_bin = %runtime.claude_bin.display(),
            node_bin = %runtime.node_bin.display(),
            anthropic_base_url = get("ANTHROPIC_BASE_URL"),
            claude_config_dir_env = get("CLAUDE_CONFIG_DIR"),
            claude_code_executable = get("CLAUDE_CODE_EXECUTABLE"),
            api_key_fp = fp,
            path_head = child_env.iter().find(|(n,_)| n == "PATH").map(|(_,v)| v.split(':').next().unwrap_or("")).unwrap_or(""),
            "child auth/runtime env resolved"
        );
    }

    let adapter_entry = runtime
        .adapter_entry
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("adapter path not UTF-8"))?
        .to_string();
    let agent = foundation::agent::AgentLayer::new(
        foundation::agent::SpawnConfig {
            program: runtime.node_bin.clone(),
            args: vec![adapter_entry],
            env: child_env,
        },
        acp_tap,
        format!("http://127.0.0.1:{}", config.port),
    );
    tracing::info!("agent session layer ready (one subprocess spawns per session)");
    // A handle for shutdown: the reactor takes ownership of `agent` below, but on
    // termination we still need to reap every subprocess it spawned. The clone
    // shares the same process registry.
    let agent_for_shutdown = agent.clone();


    let soul = identity::load_soul(&config.data_dir);
    // The reactor compiles view source to ESM via esbuild; modules land under
    // data_dir/views/_compiled. esbuild is hi-agent's own tool (not the
    // adapter's) — `ensure_view_esbuild` guarantees one whether the runtime came
    // from PATH or the managed install, so views aren't silently broken in dev.
    let esbuild_bin = runtime::ensure_view_esbuild(&runtime)
        .await
        .context("resolving esbuild for the view compiler")?;
    let view_compiler = mind::views::ViewCompiler::new(esbuild_bin, &config.data_dir);
    let _reactor = body::reactor::start(
        memory,
        agent,
        soul,
        seams.inbound_rx,
        seams.warm_rx,
        seams.out_tx,
        observatory,
        view_compiler,
        tool_registry,
        interrupts,
        presence,
        views_dir,
    );
    tracing::info!("reactor started");

    // Arm the "come and see this" gesture: a double-tap of Command hands the agent
    // a screenshot of the current screen as a file (macOS only, best-effort — needs
    // the Accessibility + Screen Recording grants, else it stays inert). One
    // desktop, one person showing one agent, so it lands in a single fixed scene.
    body::gesture::install(seams.state, crate::types::Scene("desktop".to_string()));

    let addr = ("0.0.0.0", config.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("hi-agent listening on http://0.0.0.0:{}", config.port);

    // Serve until SIGINT/SIGTERM or the tray's Quit. `with_graceful_shutdown`
    // stops accepting new connections and lets in-flight requests finish. We run
    // it in a task so we can also watch the same trigger ourselves and *bound* the
    // drain: the SSE and long-poll endpoints hold a connection open indefinitely,
    // so an unbounded graceful wait would never return.
    let server_shutdown = shutdown.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_requested(server_shutdown))
            .await
    });

    tokio::select! {
        joined = &mut server => match joined {
            Ok(Ok(())) => tracing::info!("HTTP server stopped"),
            Ok(Err(e)) => tracing::error!(error = %e, "HTTP server error"),
            Err(e) => tracing::error!(error = %e, "HTTP server task panicked"),
        },
        _ = shutdown_requested(shutdown.clone()) => {
            tracing::info!(grace = ?SHUTDOWN_GRACE, "shutdown requested; draining in-flight requests");
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                Ok(Ok(Ok(()))) => tracing::info!("HTTP server drained cleanly"),
                Ok(Ok(Err(e))) => tracing::error!(error = %e, "HTTP server error during drain"),
                Ok(Err(e)) => tracing::error!(error = %e, "HTTP server task panicked during drain"),
                Err(_) => {
                    tracing::warn!(grace = ?SHUTDOWN_GRACE, "drain grace elapsed; aborting in-flight connections");
                    server.abort();
                }
            }
        }
    }

    // Reap every ACP subprocess (one `node` + `claude` per live session) so none
    // are orphaned. Bounded so a stuck child can't hang exit.
    if tokio::time::timeout(SHUTDOWN_GRACE, agent_for_shutdown.shutdown()).await.is_err() {
        tracing::warn!("ACP subprocess reaping timed out");
    }

    tracing::info!("hi-agent shut down");
    Ok(())
}

/// macOS entry: run the menu-bar status item on the **main thread** (AppKit's
/// `NSStatusItem` requires it) while the HTTP server + reactor run on a background
/// thread with their own runtime. This is the inversion the one-binary
/// distribution model accepted as the cost of a tray: elsewhere tokio owns the
/// main thread, here AppKit does.
///
/// The config is built on the server thread (via `build_config`) rather than before
/// this call, so a missing/invalid key keeps the app alive and visible instead of
/// aborting before the menu-bar item appears. On any startup failure the server
/// thread marks the icon, reveals `<data_dir>/.env` for editing, and waits for Quit
/// — the app stays up so the user can read the problem and fix it.
///
/// The tray's "Quit" notifies `shutdown`; the server thread observes it, runs the
/// normal graceful drain + ACP reap, then exits the process — which also tears
/// down the main-thread AppKit loop. If the status item can't be created (e.g. no
/// window-server session), the agent falls back to running headless rather than
/// failing.
#[cfg(target_os = "macos")]
pub fn run_with_tray(
    port: u16,
    data_dir: PathBuf,
    build_config: impl FnOnce() -> anyhow::Result<Config> + Send + 'static,
) -> anyhow::Result<()> {
    let url = format!("http://127.0.0.1:{port}/");
    let shutdown = Arc::new(Notify::new());

    let server_shutdown = shutdown.clone();
    let server = std::thread::Builder::new()
        .name("hi-agent-server".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "failed to build server runtime");
                    std::process::exit(1);
                }
            };
            // Build the config here, on the server thread, so a bad/missing config
            // doesn't abort before the menu-bar item is up. On failure keep the app
            // alive: mark the icon, put the `.env` to edit in front of the user, and
            // wait for Quit — rather than vanishing with no trace.
            let config = match build_config() {
                Ok(config) => config,
                Err(e) => {
                    tracing::error!(error = ?e, "configuration error — fix it and relaunch; the menu-bar app stays up");
                    body::capabilities::tray::set_text("⚠ needs setup");
                    // Reveal the file to edit (best-effort; needs a window-server
                    // session, so a no-op when headless).
                    let _ = std::process::Command::new("open")
                        .arg("-R")
                        .arg(data_dir.join(".env"))
                        .spawn();
                    rt.block_on(server_shutdown.notified());
                    std::process::exit(0);
                }
            };
            match rt.block_on(run_with_shutdown(config, server_shutdown.clone())) {
                // Graceful shutdown completed (drained + ACP subprocesses reaped).
                // Exit the process, which also stops the main-thread AppKit loop.
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    tracing::error!(error = %e, "hi-agent server exited with error; the menu-bar app stays up");
                    body::capabilities::tray::set_text("⚠ startup failed");
                    rt.block_on(server_shutdown.notified());
                    std::process::exit(0);
                }
            }
        })
        .context("spawning server thread")?;

    // Blocks on the AppKit run loop until the process exits via the server thread
    // above. Returns early only if the status item can't be created — in which
    // case fall back to running headless by joining the server.
    if let Err(e) = body::capabilities::tray::run(url, shutdown) {
        tracing::warn!(error = %e, "menu-bar item unavailable; running without it");
    }
    let _ = server.join();
    Ok(())
}

/// How long in-flight HTTP requests get to finish after a shutdown signal — and,
/// separately, the budget for reaping ACP subprocesses — before we stop waiting
/// and exit anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Resolves when shutdown is requested by an OS signal (SIGINT/SIGTERM) **or** by
/// the `extra` trigger (the tray's Quit). Takes the `Notify` by `Arc` so it can be
/// moved into the server task's graceful-shutdown future. See [`shutdown_signal`]
/// for the signal half.
async fn shutdown_requested(extra: Arc<Notify>) {
    tokio::select! {
        _ = shutdown_signal() => {}
        _ = extra.notified() => {}
    }
}

/// Resolves on the first SIGINT (Ctrl-C) or SIGTERM. Each call registers fresh
/// listeners, and tokio delivers the signal to all of them, so it is safe to
/// await in more than one place (the server's graceful-shutdown future and the
/// drain supervisor both use it). A failure to install a handler logs and then
/// parks forever, so it never spuriously triggers shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
