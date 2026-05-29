//! Extraction is atomic, idempotent, and resolves paths from runtime.json.

use std::io::Write;

use hi_agent::runtime::{extract_bundle, ResolvedRuntime};

/// Build a tiny .tar.zst in memory with a runtime.json and stub files.
fn synthetic_archive() -> Vec<u8> {
    let mut tar_buf = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut tar_buf);
        let manifest = br#"{"node":"node/bin/node","adapter":"adapter/index.js","claude":"adapter/claude"}"#;
        let add = |path: &str, data: &[u8], tar: &mut tar::Builder<&mut Vec<u8>>| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, data).unwrap();
        };
        add("runtime.json", manifest, &mut tar);
        add("node/bin/node", b"#!stub", &mut tar);
        add("adapter/index.js", b"// stub", &mut tar);
        add("adapter/claude", b"#!stub", &mut tar);
        tar.finish().unwrap();
    }
    let mut zst = Vec::new();
    let mut enc = zstd::Encoder::new(&mut zst, 1).unwrap();
    enc.write_all(&tar_buf).unwrap();
    enc.finish().unwrap();
    zst
}

#[test]
fn extracts_then_reuses() {
    let cache = tempfile::tempdir().unwrap();
    let archive = synthetic_archive();

    let r1: ResolvedRuntime =
        extract_bundle(&archive, "bundleA", cache.path()).unwrap();
    assert!(r1.node_bin.ends_with("node/bin/node"));
    assert!(r1.adapter_entry.ends_with("adapter/index.js"));
    assert!(r1.claude_bin.ends_with("adapter/claude"));
    assert!(r1.node_bin.exists());

    // Second call reuses (COMPLETE marker present) and returns the same paths.
    let r2 = extract_bundle(&archive, "bundleA", cache.path()).unwrap();
    assert_eq!(r1.node_bin, r2.node_bin);

    // Different bundle_id extracts into a different dir.
    let r3 = extract_bundle(&archive, "bundleB", cache.path()).unwrap();
    assert_ne!(r1.node_bin, r3.node_bin);
}
