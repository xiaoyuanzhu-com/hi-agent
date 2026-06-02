fn main() {
    println!("cargo:rerun-if-changed=src/appearance/web/dist");
    println!("cargo:rerun-if-changed=src/runtime/manifest.toml");
    // The pin files are embedded by src/runtime (include_str!); rebuild on change.
    println!("cargo:rerun-if-changed=src/runtime/package.json");
    println!("cargo:rerun-if-changed=src/runtime/package-lock.json");

    // Version stamps surfaced by `--version` and used as the install cache key.
    // The runtime itself is fetched on first run (see src/runtime), not embedded.
    let manifest = read_manifest_versions();
    println!("cargo:rustc-env=HI_AGENT_BUNDLE_ID={}", manifest.bundle_id);
    println!("cargo:rustc-env=HI_AGENT_NODE_VERSION={}", manifest.node_version);
    println!("cargo:rustc-env=HI_AGENT_ADAPTER_VERSION={}", manifest.adapter_version);
    println!("cargo:rustc-env=HI_AGENT_CLAUDE_VERSION={}", manifest.claude_version);
}

struct ManifestVersions {
    bundle_id: String,
    node_version: String,
    adapter_version: String,
    claude_version: String,
}

/// Minimal manifest read. Avoids extra build-deps by scanning for keys; falls
/// back to "dev" placeholders when the manifest is absent.
fn read_manifest_versions() -> ManifestVersions {
    let text = std::fs::read_to_string("src/runtime/manifest.toml").unwrap_or_default();
    // Match `key = "value"` tolerating arbitrary whitespace around `=` (the
    // manifest column-aligns its keys, so a fixed-space prefix would miss them).
    let get = |key: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let rest = l.trim().strip_prefix(key)?;
            let rest = rest.trim_start().strip_prefix('=')?;
            Some(rest.trim().trim_matches('"').to_string())
        })
    };
    let node_version = get("node_version").unwrap_or_else(|| "dev".to_string());
    let bundle_version = get("bundle_version").unwrap_or_else(|| "dev".to_string());
    let adapter_version = get("adapter_version").unwrap_or_else(|| "dev".to_string());
    let claude_version = get("claude_version").unwrap_or_else(|| "dev".to_string());

    // bundle_id keys the per-version install cache dir; bump any pin to roll it.
    let bundle_id = format!("{bundle_version}-node{node_version}-adapter{adapter_version}");

    ManifestVersions { bundle_id, node_version, adapter_version, claude_version }
}
