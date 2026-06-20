//! hi-agent — reference implementation of the human-interface spec.

use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use tokio::net::TcpListener;

pub mod acp;
pub mod agent;
pub mod appearance;
pub mod capabilities;
pub mod channel_log;
pub mod config;
pub mod llm_proxy;
pub mod mcp;
pub mod memory;
pub mod observatory;
pub mod presence;
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

/// Build the axum app, spawn the ACP subprocess + reactor, bind, and serve
/// until the process is terminated.
pub async fn run(config: Config) -> anyhow::Result<()> {
    // Normalize the data dir once, up front: absolutize it (it rides to child
    // processes via env, which may run with a different cwd) and strip `.`/`..`
    // components so the paths we hand the mind read as clean absolutes —
    // `.../hi-agent/data/prompts/core.md`, not `.../hi-agent/./data/prompts/core.md`.
    // Every downstream consumer (load_soul, prompts_dir, views_dir, …) inherits this.
    let mut config = config;
    config.data_dir = normalize_dir(&config.data_dir)
        .context("resolving cwd to absolutize data dir")?;
    tracing::debug!(?config, "starting hi-agent");

    let memory = memory::Memory::open(&config.data_dir).await?;
    tracing::info!(data_dir = %config.data_dir.display(), "memory opened");

    // Materialise the bundled prompts under <data_dir>/prompts/ so the mind's
    // system prompt (core.md + speaking.md + meaning.md) and the view-builder's guides
    // (appearance.md + aesthetic.md, opened as files by build sub-agents) are on
    // disk, composed with any `*.local.md` operator overrides. Absolutize the dir:
    // it rides to the child as HI_AGENT_PROMPTS_DIR, and the child may run with a
    // different cwd than us.
    reactor::install_prompts(&config.data_dir).context("installing bundled prompts")?;
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
    views::install_builtin_views(&config.data_dir).context("installing built-in views")?;

    // The agent's precious drive — where it files artifacts worth keeping (a user's
    // handed-over documents, its own kept work). Created here so it always exists;
    // filling it is the agent's job. (Verbatim annex of memory; see data-dir-layout.)
    std::fs::create_dir_all(config.data_dir.join("drive")).context("creating drive dir")?;

    // Structured visibility into the ACP session lifecycle. The agent layer,
    // reactor, workers and heartbeat feed it; `GET /api/sessions` reads the live
    // mirror and `GET /api/sessions/events` streams the history over SSE.
    let observatory = observatory::Observatory::new(
        Some(config.data_dir.join("sessions.jsonl")),
        reactor::swap_budget_chars(),
    );

    // Raw ACP wire tap — every JSON-RPC frame, business-logic agnostic. The agent
    // layer hands it to each scene's subprocess; `GET /api/acp/frames/events`
    // streams it to the raw session inspector.
    let acp_tap = acp::AcpTap::new();

    // Resolve all capabilities from the environment. Unconfigured capabilities
    // are fine; gates affect /audio (STT) and the speak path (TTS) only.
    capabilities::init_from_env()?;
    tracing::info!(
        stt = capabilities::stt::available(),
        tts = capabilities::tts::available(),
        voiceprint = capabilities::voiceprint::available(),
        face = capabilities::face::available(),
        "capabilities resolved"
    );

    // Scene→tool-sink table shared between the HTTP front's `/mcp` handler and the
    // reactor that registers each scene's sink. The mind drives output and
    // side-effects by calling tools on `/mcp`; they route here.
    let tool_registry = reactor::ToolRegistry::new();
    // Scene→barge-in table, shared the same way: the server's STT relay reports
    // recognized speech, the reactor stamps voice spans and folds the inferred
    // "what went unheard" note into the next prompt. No cancel, no endpoint.
    let interrupts = reactor::InterruptRegistry::new();
    // Scene→live-subscriber counts, shared the same way: the server's out-channel
    // handlers hold a guard per connection, the reactor renders the counts into
    // each turn as human-model facts ("no screen is attached").
    let presence = presence::Presence::new();

    let (router, seams) = server::build(
        memory.clone(),
        config.data_dir.clone(),
        observatory.clone(),
        acp_tap.clone(),
        tool_registry.clone(),
        interrupts.clone(),
        presence.clone(),
    );

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
    config.agent.render_settings_json(&claude_config_dir)?;
    // Pre-approve the placeholder key, else Claude Code rejects the env-supplied
    // `ANTHROPIC_API_KEY` ("Please run /login") and prompts fail with -32000.
    config.agent.approve_placeholder_key(&claude_config_dir)?;

    // Spawn config for the agent session layer. The subprocess itself is spawned
    // lazily, one per scene, on that scene's first session (Chrome-style isolation);
    // the pinned runtime, managed env, and local LLM proxy are shared by all.
    let mut child_env = config.agent.child_env(
        proxy.port(),
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
    // will read, and the placeholder key's fingerprint vs. what we seeded.
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
            placeholder_fp = fp,
            path_head = child_env.iter().find(|(n,_)| n == "PATH").map(|(_,v)| v.split(':').next().unwrap_or("")).unwrap_or(""),
            "child auth/runtime env resolved"
        );
    }

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
        acp_tap,
        format!("http://127.0.0.1:{}", config.port),
    );
    tracing::info!("agent session layer ready (one subprocess spawns per session)");

    // Keep the proxy alive for the life of the process.
    let _proxy = proxy;

    let soul = reactor::load_soul(&config.data_dir);
    // The reactor compiles view source to ESM via esbuild; modules land under
    // data_dir/views/_compiled. esbuild is hi-agent's own tool (not the
    // adapter's) — `ensure_view_esbuild` guarantees one whether the runtime came
    // from PATH or the managed install, so views aren't silently broken in dev.
    let esbuild_bin = runtime::ensure_view_esbuild(&runtime)
        .await
        .context("resolving esbuild for the view compiler")?;
    let view_compiler = views::ViewCompiler::new(esbuild_bin, &config.data_dir);
    let _reactor = reactor::start(
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

    let addr = ("0.0.0.0", config.port);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("hi-agent listening on http://0.0.0.0:{}", config.port);

    axum::serve(listener, router).await?;
    Ok(())
}
