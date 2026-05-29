//! Node + ACP adapter + claude CLI runtime, installed into an OS cache dir on
//! first run and reused thereafter (keyed by the build-stamped `bundle_id`).
//!
//! Rather than embedding a ~200 MB archive in the hi-agent binary, the only
//! things baked in are the two small *pin* files (`runtime/package.json` +
//! `runtime/package-lock.json`) and the manifest version stamps. On first run
//! we download the pinned Node release and `npm ci` the adapter into
//! `<cache>/hi-agent/<bundle_id>/`, then resolve the node/adapter/claude paths.
//! Subsequent runs reuse that directory via a `.complete` marker, so the cost is
//! paid once per pinned version.
//!
//! The pins still come from `runtime/manifest.toml` + the committed lockfile, so
//! cognition stays reproducible — we just fetch at first run instead of at build
//! time. Bumping any pin changes `bundle_id`, which transparently triggers a
//! fresh install into a new cache dir.
//!
//! Prototype scope: macOS + Linux on x86_64/aarch64, extraction via the system
//! `tar`, and no SHA-256 verification of the Node download yet (the manifest's
//! checksums are placeholders). The `HI_AGENT_DEV_*` env vars point at an
//! external runtime for local debugging without any download.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use tokio::process::Command;

/// Build-stamped identity of the pinned runtime (`bundle_version` + node +
/// adapter). Doubles as the cache subdirectory name and the `--version` tag.
pub const BUNDLE_ID: &str = env!("HI_AGENT_BUNDLE_ID");

/// Pinned Node version (no leading `v`), stamped from `runtime/manifest.toml`.
const NODE_VERSION: &str = env!("HI_AGENT_NODE_VERSION");

/// The committed pin files, embedded so `npm ci` reproduces the exact tree
/// without needing the repo on disk. Tiny (text), unlike the runtime itself.
const PACKAGE_JSON: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/runtime/package.json"));
const PACKAGE_LOCK: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/runtime/package-lock.json"));

/// Path of the adapter entry relative to the install dir, after `npm ci`.
const ADAPTER_REL: &str = "adapter/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js";

/// Absolute paths to the installed runtime components.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pub node_bin: PathBuf,
    pub adapter_entry: PathBuf,
    pub claude_bin: PathBuf,
}

impl ResolvedRuntime {
    /// Directory containing the `node` binary (for PATH prefixing).
    pub fn node_bin_dir(&self) -> &Path {
        self.node_bin.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// Resolve the runtime: install on first run, reuse thereafter.
pub async fn ensure() -> anyhow::Result<ResolvedRuntime> {
    if let Some(dev) = resolve_dev_override() {
        tracing::warn!("using HI_AGENT_DEV_* runtime override (unsupported, debug only)");
        return Ok(dev);
    }

    let cache_root = cache_root()?;
    let target = cache_root.join(BUNDLE_ID);

    // Reuse a complete install from a previous run.
    if target.join(".complete").exists() {
        tracing::debug!(bundle_id = BUNDLE_ID, "runtime already installed");
        return resolve(&target);
    }

    install(&cache_root, &target).await
}

/// Base cache dir, overridable by `HI_AGENT_RUNTIME_DIR`.
fn cache_root() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("HI_AGENT_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("dev", "human-interface", "hi-agent")
        .ok_or_else(|| anyhow!("cannot determine OS cache dir"))?;
    Ok(dirs.cache_dir().join("runtime"))
}

/// Install the pinned Node + adapter into `<cache_root>/<BUNDLE_ID>`. Builds in a
/// unique temp dir and atomically renames into place, so concurrent or
/// interrupted starts never observe a half-installed runtime.
async fn install(cache_root: &Path, target: &Path) -> anyhow::Result<ResolvedRuntime> {
    tokio::fs::create_dir_all(cache_root)
        .await
        .with_context(|| format!("creating cache root {}", cache_root.display()))?;

    let tmp = cache_root.join(format!(".{BUNDLE_ID}.tmp.{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    tokio::fs::create_dir_all(&tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;

    // 1. Node — download + extract into <tmp>/node.
    let node_bin = fetch_node(&tmp).await.context("installing the Node runtime")?;
    // 2. Adapter + claude — npm ci against the committed lockfile into <tmp>/adapter.
    npm_ci(&node_bin, &tmp).await.context("installing the ACP adapter")?;

    // Fail loudly if the install didn't produce the paths we expect, before we
    // publish anything (a corrupt cache dir is worse than a clear error).
    let staged = resolve(&tmp)?;
    for (label, p) in [
        ("node", &staged.node_bin),
        ("adapter", &staged.adapter_entry),
        ("claude", &staged.claude_bin),
    ] {
        if !p.exists() {
            bail!(
                "runtime installed but the {label} entry is missing at {} \
                 (the pinned package layout may have changed)",
                p.display()
            );
        }
    }

    tokio::fs::write(tmp.join(".complete"), b"")
        .await
        .context("writing the completion marker")?;

    // Atomic publish. If another process won the race, drop ours and reuse.
    match tokio::fs::rename(&tmp, target).await {
        Ok(()) => {}
        Err(_) if target.join(".complete").exists() => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(anyhow!("publishing runtime to {}: {e}", target.display()));
        }
    }

    gc_stale(cache_root);
    hint("runtime ready.");
    resolve(target)
}

/// Build absolute paths from an installed (or reused) target dir.
fn resolve(target: &Path) -> anyhow::Result<ResolvedRuntime> {
    // The `claude` CLI ships as a native binary inside a platform-specific
    // package `@anthropic-ai/claude-agent-sdk-<os>-<arch>` (an optional dep of
    // the SDK; npm installs only the one matching this host). Its <os>-<arch>
    // suffix is exactly the Node target mapping.
    let (os, arch) = node_target()?;
    let claude_rel =
        format!("adapter/node_modules/@anthropic-ai/claude-agent-sdk-{os}-{arch}/claude");
    Ok(ResolvedRuntime {
        node_bin: target.join("node").join("bin").join("node"),
        adapter_entry: target.join(ADAPTER_REL),
        claude_bin: target.join(claude_rel),
    })
}

/// Download the pinned Node release and extract it into `<dir>/node`, returning
/// the path to its `node` binary. Uses the system `tar` (handles strip,
/// symlinks, hardlinks, and permissions correctly).
async fn fetch_node(dir: &Path) -> anyhow::Result<PathBuf> {
    let (os, arch) = node_target()?;
    let stem = format!("node-v{NODE_VERSION}-{os}-{arch}");
    let url = format!("https://nodejs.org/dist/v{NODE_VERSION}/{stem}.tar.gz");

    let node_dir = dir.join("node");
    tokio::fs::create_dir_all(&node_dir)
        .await
        .with_context(|| format!("creating {}", node_dir.display()))?;

    hint(&format!("first run — downloading Node {NODE_VERSION} (~30 MB)…"));
    tracing::debug!(%url, "downloading Node");
    let bytes = reqwest::get(url.as_str())
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading Node from {url}"))?
        .bytes()
        .await
        .context("reading the Node download body")?;

    let tarball = dir.join("node.tar.gz");
    tokio::fs::write(&tarball, &bytes)
        .await
        .with_context(|| format!("writing {}", tarball.display()))?;

    // Strip the leading `node-v.../` component so paths land directly in node/.
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&node_dir)
        .arg("--strip-components=1")
        .status()
        .await
        .context("running `tar` to extract Node (is `tar` installed?)")?;
    if !status.success() {
        bail!("`tar` failed to extract {}", tarball.display());
    }
    let _ = tokio::fs::remove_file(&tarball).await;

    let node_bin = node_dir.join("bin").join("node");
    if !is_executable(&node_bin) {
        bail!("Node extracted but `{}` is missing or not executable", node_bin.display());
    }
    Ok(node_bin)
}

/// `npm ci --omit=dev` the committed lockfile into `<dir>/adapter`, driving npm
/// via `node <npm-cli.js>` so the freshly-downloaded Node (not on PATH) runs it.
async fn npm_ci(node_bin: &Path, dir: &Path) -> anyhow::Result<()> {
    let adapter = dir.join("adapter");
    tokio::fs::create_dir_all(&adapter)
        .await
        .with_context(|| format!("creating {}", adapter.display()))?;
    tokio::fs::write(adapter.join("package.json"), PACKAGE_JSON)
        .await
        .context("writing package.json")?;
    tokio::fs::write(adapter.join("package-lock.json"), PACKAGE_LOCK)
        .await
        .context("writing package-lock.json")?;

    let npm_cli = npm_cli_for(node_bin)
        .with_context(|| format!("locating npm bundled with {}", node_bin.display()))?;

    hint("first run — installing the ACP adapter (this can take a minute)…");
    let mut cmd = Command::new(node_bin);
    cmd.arg(&npm_cli)
        .arg("ci")
        .arg("--omit=dev")
        .current_dir(&adapter)
        .env("npm_config_fund", "false")
        .env("npm_config_audit", "false")
        .env("npm_config_update_notifier", "false");

    let out = run_with_heartbeat(cmd, "…still installing")
        .await
        .context("running npm ci")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("npm ci failed:\n{}", stderr.trim());
    }
    Ok(())
}

/// Locate `npm-cli.js` bundled alongside a `node` binary at `<prefix>/bin/node`.
fn npm_cli_for(node_bin: &Path) -> Option<PathBuf> {
    let prefix = node_bin.parent()?.parent()?; // <prefix>/bin/node -> <prefix>
    let cli = prefix.join("lib/node_modules/npm/bin/npm-cli.js");
    cli.exists().then_some(cli)
}

/// Run a command to completion, capturing output, and print `slow_hint` every
/// 15s so a long install doesn't look hung.
async fn run_with_heartbeat(
    mut cmd: Command,
    slow_hint: &str,
) -> anyhow::Result<std::process::Output> {
    let fut = cmd.output();
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    ticker.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            res = &mut fut => return Ok(res?),
            _ = ticker.tick() => hint(slow_hint),
        }
    }
}

/// GC sibling cache dirs whose name ≠ the current `bundle_id` (best effort).
fn gc_stale(cache_root: &Path) {
    if let Ok(entries) = std::fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name != BUNDLE_ID && !name.starts_with('.') {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Map the host to Node's release naming. `Err` on platforms we don't auto-install.
fn node_target() -> anyhow::Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!(
            "runtime auto-install supports macOS and Linux only (OS `{other}`). \
             Set HI_AGENT_DEV_NODE / HI_AGENT_DEV_ADAPTER / HI_AGENT_DEV_CLAUDE to \
             point at an external runtime."
        ),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => bail!(
            "runtime auto-install supports x86_64 and aarch64 only (arch `{other}`). \
             Set HI_AGENT_DEV_NODE / HI_AGENT_DEV_ADAPTER / HI_AGENT_DEV_CLAUDE."
        ),
    };
    Ok((os, arch))
}

/// True if `p` is a regular file with any execute bit set.
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Dev escape hatch (debug only): point at an external runtime via env so
/// `cargo run` works without any download. Returns `Some` only if all three
/// vars are set.
fn resolve_dev_override() -> Option<ResolvedRuntime> {
    let node = std::env::var("HI_AGENT_DEV_NODE").ok()?;
    let adapter = std::env::var("HI_AGENT_DEV_ADAPTER").ok()?;
    let claude = std::env::var("HI_AGENT_DEV_CLAUDE").ok()?;
    Some(ResolvedRuntime {
        node_bin: PathBuf::from(node),
        adapter_entry: PathBuf::from(adapter),
        claude_bin: PathBuf::from(claude),
    })
}

/// First-run user-facing hint. Goes straight to stderr (not `tracing`) so it is
/// visible regardless of `RUST_LOG`.
fn hint(msg: &str) {
    eprintln!("hi-agent: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_target_maps_known_hosts() {
        // Whatever host runs the test must be a supported target.
        let (os, arch) = node_target().expect("test host should be a supported target");
        assert!(matches!(os, "darwin" | "linux"));
        assert!(matches!(arch, "x64" | "arm64"));
    }

    #[test]
    fn resolve_builds_expected_paths() {
        let r = resolve(Path::new("/cache/bundleX")).unwrap();
        assert_eq!(r.node_bin, Path::new("/cache/bundleX/node/bin/node"));
        assert!(r.adapter_entry.ends_with("claude-agent-acp/dist/index.js"));
        assert!(r.claude_bin.ends_with("claude"));
        assert!(
            r.claude_bin
                .to_string_lossy()
                .contains("@anthropic-ai/claude-agent-sdk-"),
            "claude path should point at a platform package: {}",
            r.claude_bin.display()
        );
        assert_eq!(r.node_bin_dir(), Path::new("/cache/bundleX/node/bin"));
    }

    #[test]
    fn dev_override_needs_all_three() {
        // Not asserting env mutation here (process-global); just the shape: with
        // none set, the override is absent.
        if std::env::var_os("HI_AGENT_DEV_NODE").is_none() {
            assert!(resolve_dev_override().is_none());
        }
    }
}
