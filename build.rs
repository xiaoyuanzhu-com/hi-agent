use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/appearance/web/dist");
    println!("cargo:rerun-if-changed=runtime/embed");
    println!("cargo:rerun-if-env-changed=HI_AGENT_TARGET");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("runtime.tar.zst");

    // Pick the archive for the active (or overridden) target triple.
    let target = std::env::var("HI_AGENT_TARGET")
        .or_else(|_| std::env::var("TARGET"))
        .unwrap_or_default();
    let candidate = PathBuf::from("runtime/embed").join(format!("{target}.tar.zst"));

    if candidate.exists() {
        std::fs::copy(&candidate, &dest).expect("copying runtime archive to OUT_DIR");
        println!("cargo:warning=embedding runtime bundle: {}", candidate.display());
    } else {
        // Dev build without a bundle: embed a zero-byte placeholder so the
        // binary still compiles and runs (with the dev runtime override).
        std::fs::write(&dest, b"").expect("writing placeholder runtime archive");
        println!(
            "cargo:warning=no runtime bundle for target '{target}' — embedding empty placeholder \
             (run `make bundle`)"
        );
    }

    // Version stamps surfaced by `--version` and used as the cache key.
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
/// back to "dev" placeholders when the manifest or bundle is absent.
fn read_manifest_versions() -> ManifestVersions {
    let text = std::fs::read_to_string("runtime/manifest.toml").unwrap_or_default();
    let get = |key: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let l = l.trim();
            let prefix = format!("{key} =");
            l.strip_prefix(&prefix)
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    };
    let node_version = get("node_version").unwrap_or_else(|| "dev".to_string());
    let bundle_version = get("bundle_version").unwrap_or_else(|| "dev".to_string());
    let adapter_version = get("adapter_version").unwrap_or_else(|| "dev".to_string());
    let claude_version = get("claude_version").unwrap_or_else(|| "dev".to_string());

    // The archive's own bytes are the real key; here we approximate with the
    // pinned versions. The bundle script writes claude_version into the
    // manifest after `npm ci` resolves it.
    let bundle_id = format!("{bundle_version}-node{node_version}-adapter{adapter_version}");

    ManifestVersions { bundle_id, node_version, adapter_version, claude_version }
}
