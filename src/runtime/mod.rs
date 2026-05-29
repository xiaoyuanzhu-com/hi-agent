//! Embedded Node + ACP adapter + claude CLI runtime, extracted to an OS cache
//! dir on first run and reused thereafter (keyed by build-stamped bundle_id).

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

/// The compressed runtime archive embedded at build time. `build.rs` writes
/// either the real bundle or a zero-byte placeholder to this path.
const EMBEDDED_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.tar.zst"));

/// Build-stamped identity of the embedded archive.
pub const BUNDLE_ID: &str = env!("HI_AGENT_BUNDLE_ID");

/// Relative paths inside the archive, recorded by the bundle script.
#[derive(Debug, Deserialize)]
struct RuntimeManifest {
    node: String,
    adapter: String,
    claude: String,
}

/// Absolute paths to the extracted runtime components.
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

/// Resolve the embedded runtime: extract on first run, reuse thereafter.
///
/// Errors if the embedded archive is empty (a dev build produced without
/// `make bundle`); set the dev escape-hatch env vars in that case (see
/// `resolve_dev_override`).
pub fn ensure() -> anyhow::Result<ResolvedRuntime> {
    if let Some(dev) = resolve_dev_override() {
        tracing::warn!("using HI_AGENT_DEV_* runtime override (unsupported, debug only)");
        return Ok(dev);
    }
    if EMBEDDED_ARCHIVE.is_empty() {
        return Err(anyhow!(
            "no runtime bundled (empty embedded archive); run `make bundle` or set \
             HI_AGENT_DEV_NODE / HI_AGENT_DEV_ADAPTER / HI_AGENT_DEV_CLAUDE"
        ));
    }
    let cache_root = cache_root()?;
    extract_bundle(EMBEDDED_ARCHIVE, BUNDLE_ID, &cache_root)
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

/// Extract `archive` (a .tar.zst) for `bundle_id` under `cache_root`, atomically
/// and idempotently. Reuses an existing complete extraction.
pub fn extract_bundle(
    archive: &[u8],
    bundle_id: &str,
    cache_root: &Path,
) -> anyhow::Result<ResolvedRuntime> {
    let target = cache_root.join(bundle_id);
    let marker = target.join(".complete");

    if marker.exists() {
        return resolve(&target);
    }

    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("creating cache root {}", cache_root.display()))?;

    // Extract into a unique temp dir, then rename into place.
    let tmp = cache_root.join(format!(".{bundle_id}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;

    let decoder = zstd::Decoder::new(archive).context("opening zstd decoder")?;
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(&tmp).context("unpacking runtime archive")?;

    std::fs::write(tmp.join(".complete"), b"")?;

    // Atomic publish. If another process won the race, drop ours and reuse.
    match std::fs::rename(&tmp, &target) {
        Ok(()) => {}
        Err(_) if marker.exists() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(anyhow!("publishing runtime to {}: {e}", target.display()));
        }
    }

    // GC stale sibling bundles (best effort).
    if let Ok(entries) = std::fs::read_dir(cache_root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name != bundle_id && !name.starts_with('.') {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    resolve(&target)
}

/// Read `runtime.json` from an extracted dir and build absolute paths.
fn resolve(target: &Path) -> anyhow::Result<ResolvedRuntime> {
    let manifest_path = target.join("runtime.json");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let m: RuntimeManifest = serde_json::from_str(&text).context("parsing runtime.json")?;
    Ok(ResolvedRuntime {
        node_bin: target.join(m.node),
        adapter_entry: target.join(m.adapter),
        claude_bin: target.join(m.claude),
    })
}

/// Dev escape hatch (debug only): point at an external runtime via env so
/// `cargo run` works before the bundle pipeline exists. Returns `Some` only if
/// all three vars are set.
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
