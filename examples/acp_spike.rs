//! Step 0/Step 2 spike: spawn one ACP child process, open three concurrent
//! sessions, prompt each, stream updates, then shut down cleanly.
//!
//!   CLAUDE_CODE_BIN=claude-code cargo run --example acp_spike
//!
//! The child binary defaults to `claude-code`. Set `RUST_LOG=hi_agent=info` to
//! see the per-session log lines; `RUST_LOG=hi_agent=trace` includes the raw
//! JSON-RPC traffic.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use hi_agent::acp::{AcpProcess, SessionOpts, SessionUpdate};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("hi_agent=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let binary = env::var_os("CLAUDE_CODE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude-code"));
    let args: Vec<String> = env::args().skip(1).collect();

    tracing::info!(binary = %binary.display(), ?args, "spawning ACP child");
    let process = Arc::new(AcpProcess::spawn(binary, args).await?);

    let prompts = [
        ("session-1", "Count to 3."),
        ("session-2", "Name a fruit."),
        ("session-3", "Say hello."),
    ];

    let mut tasks = Vec::new();
    for (label, prompt) in prompts {
        let proc = process.clone();
        let label = label.to_string();
        let prompt = prompt.to_string();
        tasks.push(tokio::spawn(async move {
            let session = proc.new_session(SessionOpts::default()).await?;
            tracing::info!(label = %label, session_id = %session.id().0, "session opened");

            let mut run = session.prompt(prompt).await?;
            while let Some(update) = run.next_update().await {
                match update {
                    SessionUpdate::Text(t) => tracing::info!(label = %label, "{}", t.trim_end()),
                    SessionUpdate::Thought(t) => {
                        tracing::debug!(label = %label, "(thought) {}", t.trim_end())
                    }
                    SessionUpdate::ToolCall(stub) => {
                        tracing::info!(label = %label, "(tool) {}", stub.raw_variant)
                    }
                    SessionUpdate::Other(v) => {
                        tracing::debug!(label = %label, "(other) {}", v)
                    }
                }
            }
            let result = run.wait().await?;
            tracing::info!(
                label = %label,
                stop = ?result.stop_reason,
                len = result.text.len(),
                "session run finished"
            );
            session.close().await?;
            anyhow::Ok(())
        }));
    }

    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "session task failed"),
            Err(join_err) => tracing::error!(error = %join_err, "session task panicked"),
        }
    }

    tracing::info!("shutting down ACP process");
    match Arc::try_unwrap(process) {
        Ok(p) => p.shutdown().await?,
        Err(_) => tracing::warn!("AcpProcess still has outstanding references at shutdown"),
    }

    Ok(())
}
