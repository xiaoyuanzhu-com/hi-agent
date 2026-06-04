//! Node + ACP adapter + claude CLI runtime resolution.
//!
//! We **prefer what the system already offers**: if `node`, the ACP adapter
//! (`claude-agent-acp`), and the `claude` CLI are all on `PATH`, we use them
//! directly and download nothing. Having those tools on `PATH` is also how you
//! point hi-agent at your own runtime for local development.
//!
//! Only when the system *doesn't* offer the full set do we fall back to a
//! self-contained install: download the pinned Node release and `npm ci` the
//! adapter (which carries the `claude` binary as a platform dep) into a single
//! fixed directory ([`runtime_dir`]), then resolve the node/adapter/claude paths.
//! Subsequent runs reuse that directory via a `.complete` marker, so the install
//! cost is paid once.
//!
//! The pins come from `src/runtime/manifest.toml` + the committed lockfile, so a
//! managed install stays reproducible — we just fetch at first run instead of at
//! build time. The install dir is fixed (not version-keyed): bumping a pin does
//! *not* auto-reinstall today; delete the dir to force a fresh install. Deliberate
//! version management (auto-update) is a later concern.
//!
//! Detection is all-or-nothing: a partial system set (e.g. `node` but no
//! `claude`) falls back to the managed install so we never mix a system tool with
//! a managed one. Prototype scope for the install path: macOS + Linux on
//! x86_64/aarch64, extraction via the system `tar`, and no SHA-256 verification of
//! the Node download yet.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use tokio::process::Command;

/// Pinned Node version (no leading `v`), stamped from `src/runtime/manifest.toml`.
const NODE_VERSION: &str = env!("HI_AGENT_NODE_VERSION");

/// The committed pin files, embedded so `npm ci` reproduces the exact tree
/// without needing the repo on disk. Tiny (text), unlike the runtime itself.
const PACKAGE_JSON: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime/package.json"));
const PACKAGE_LOCK: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/runtime/package-lock.json"));

/// Path of the adapter entry relative to the install dir, after `npm ci`.
const ADAPTER_REL: &str = "adapter/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js";

/// Absolute paths to the resolved runtime components.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pub node_bin: PathBuf,
    pub adapter_entry: PathBuf,
    pub claude_bin: PathBuf,
    /// Where these came from — `"system"` (found on `PATH`) or `"managed"`
    /// (downloaded/installed into the cache). For logging only.
    pub origin: &'static str,
}

impl ResolvedRuntime {
    /// Directory containing the `node` binary (for PATH prefixing).
    pub fn node_bin_dir(&self) -> &Path {
        self.node_bin.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// Resolve the runtime: use the system tools if all are present, otherwise
/// install on first run and reuse thereafter.
pub async fn ensure() -> anyhow::Result<ResolvedRuntime> {
    // Prefer what the system already offers — no download when the user has
    // node + the ACP adapter + claude on PATH.
    if let Some(system) = resolve_system() {
        return Ok(system);
    }

    let target = runtime_dir()?;

    // Reuse a complete install from a previous run.
    if target.join(".complete").exists() {
        tracing::debug!(path = %target.display(), "runtime already installed");
        return resolve(&target);
    }

    install(&target).await
}

/// The single directory the managed runtime installs into. Override with
/// `HI_AGENT_RUNTIME_DIR`; otherwise a fixed spot in the OS cache dir.
fn runtime_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("HI_AGENT_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("dev", "human-interface", "hi-agent")
        .ok_or_else(|| anyhow!("cannot determine OS cache dir"))?;
    Ok(dirs.cache_dir().join("runtime"))
}

/// Install the pinned Node + adapter into `target`. Builds in a sibling temp dir
/// and atomically renames into place, so concurrent or interrupted starts never
/// observe a half-installed runtime.
async fn install(target: &Path) -> anyhow::Result<ResolvedRuntime> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("runtime dir {} has no parent", target.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;

    let tmp = parent.join(format!(".runtime.tmp.{}", std::process::id()));
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

    // Clear any leftover partial install at the fixed path, then atomically
    // rename ours into place. If another process won the race (a complete
    // install already sits there), drop ours and reuse it.
    let _ = tokio::fs::remove_dir_all(target).await;
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
        origin: "managed",
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

/// Map the host to Node's release naming. `Err` on platforms we don't auto-install.
/// Also names the esbuild platform package (`@esbuild/<os>-<arch>`), which uses
/// the same `<os>-<arch>` convention — see `crate::views`.
pub(crate) fn node_target() -> anyhow::Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!(
            "runtime auto-install supports macOS and Linux only (OS `{other}`). \
             Install node, claude-agent-acp, and claude on your PATH to use the \
             system runtime instead."
        ),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => bail!(
            "runtime auto-install supports x86_64 and aarch64 only (arch `{other}`). \
             Install node, claude-agent-acp, and claude on your PATH to use the \
             system runtime instead."
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

/// Use the system's tools when it offers the full set: `node`, the ACP adapter
/// (`claude-agent-acp`), and the `claude` CLI all on `PATH`. All-or-nothing —
/// returns `None` if any is missing, so we never pair a system tool with a
/// managed one. The adapter bin is a JS entry (it has a `node` shebang), so we
/// keep running it as `node <entry>` exactly like the managed adapter.
fn resolve_system() -> Option<ResolvedRuntime> {
    let node_bin = find_on_path("node")?;
    let adapter_entry = find_on_path("claude-agent-acp")?;
    // Resolve `claude` deliberately rather than by raw PATH order: GUI launchers
    // (cmux, some IDEs) prepend their own `claude` *shim* to PATH that only
    // authenticates inside that app's sandbox — running it standalone yields
    // "Please run /login". `resolve_claude_bin` skips those. If it finds nothing
    // usable we return None, dropping to the managed runtime (a real bundled
    // claude) rather than pairing the system tools with a broken claude.
    let claude_bin = resolve_claude_bin()?;
    tracing::debug!(
        node = %node_bin.display(),
        adapter = %adapter_entry.display(),
        claude = %claude_bin.display(),
        "using system runtime from PATH",
    );
    Some(ResolvedRuntime {
        node_bin,
        adapter_entry,
        claude_bin,
        origin: "system",
    })
}

/// Find an executable named `name` on `PATH`, returning the first match.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Locate a *usable* `claude` CLI, resisting launcher shims.
///
/// Priority: `HI_AGENT_CLAUDE_BIN` override → first non-shim `claude` on PATH →
/// canonical install locations (in case the launcher's PATH omits them). A
/// "shim" is any candidate inside a macOS `.app` bundle — the pattern used by
/// GUI launchers like cmux, whose `claude` only works in their own auth
/// sandbox. Returns `None` when only a shim exists, so the caller falls back to
/// the managed runtime instead of a `claude` that will fail at prompt time.
fn resolve_claude_bin() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("HI_AGENT_CLAUDE_BIN") {
        let p = PathBuf::from(raw);
        if is_executable(&p) {
            return Some(p);
        }
        tracing::warn!(path = %p.display(), "HI_AGENT_CLAUDE_BIN is not executable; ignoring");
    }

    let mut shim: Option<PathBuf> = None;
    if let Some(path) = std::env::var_os("PATH") {
        for cand in std::env::split_paths(&path).map(|dir| dir.join("claude")) {
            if !is_executable(&cand) {
                continue;
            }
            if is_app_bundle_path(&cand) {
                shim.get_or_insert(cand); // remember, but keep looking for a real one
                continue;
            }
            return Some(cand);
        }
    }

    // PATH yielded only a shim (or nothing). Try canonical install locations the
    // launcher's PATH may have dropped, before giving up.
    for cand in canonical_claude_paths() {
        if is_executable(&cand) && !is_app_bundle_path(&cand) {
            return Some(cand);
        }
    }

    if let Some(shim) = &shim {
        tracing::warn!(
            path = %shim.display(),
            "the only `claude` on PATH is an app-bundle shim (e.g. a GUI launcher's); \
             falling back to the managed runtime — set HI_AGENT_CLAUDE_BIN to override",
        );
    }
    None
}

/// Standard places the official installer / package managers put `claude`.
fn canonical_claude_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(&home).join(".local/bin/claude"));
    }
    out.push(PathBuf::from("/opt/homebrew/bin/claude"));
    out.push(PathBuf::from("/usr/local/bin/claude"));
    out
}

/// True if any path component is a macOS application bundle (`*.app`).
fn is_app_bundle_path(p: &Path) -> bool {
    p.components()
        .any(|c| c.as_os_str().to_string_lossy().ends_with(".app"))
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
        let r = resolve(Path::new("/cache/runtimeX")).unwrap();
        assert_eq!(r.node_bin, Path::new("/cache/runtimeX/node/bin/node"));
        assert!(r.adapter_entry.ends_with("claude-agent-acp/dist/index.js"));
        assert!(r.claude_bin.ends_with("claude"));
        assert!(
            r.claude_bin
                .to_string_lossy()
                .contains("@anthropic-ai/claude-agent-sdk-"),
            "claude path should point at a platform package: {}",
            r.claude_bin.display()
        );
        assert_eq!(r.node_bin_dir(), Path::new("/cache/runtimeX/node/bin"));
        assert_eq!(r.origin, "managed");
    }

    #[test]
    fn app_bundle_paths_are_recognized_as_shims() {
        assert!(is_app_bundle_path(Path::new(
            "/Applications/cmux.app/Contents/Resources/bin/claude"
        )));
        assert!(!is_app_bundle_path(Path::new("/Users/me/.local/bin/claude")));
        assert!(!is_app_bundle_path(Path::new("/opt/homebrew/bin/claude")));
    }

    #[test]
    fn find_on_path_locates_a_known_executable() {
        // `tar` is required by the install path, so it's a safe always-present
        // executable to probe for on any supported host.
        assert!(find_on_path("tar").is_some());
        assert!(find_on_path("definitely-not-a-real-binary-xyz").is_none());
    }
}
