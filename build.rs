fn main() {
    println!("cargo:rerun-if-changed=src/appearance/web/dist");
    println!("cargo:rerun-if-changed=src/runtime/manifest.toml");
    // The pin files are embedded by src/runtime (include_str!); rebuild on change.
    println!("cargo:rerun-if-changed=src/runtime/package.json");
    println!("cargo:rerun-if-changed=src/runtime/package-lock.json");

    // Pinned-version stamps surfaced by `--version`. NODE_VERSION also drives the
    // download URL. The runtime is fetched on first run (see src/runtime), not
    // embedded.
    let manifest = read_manifest_versions();
    println!("cargo:rustc-env=HI_AGENT_NODE_VERSION={}", manifest.node_version);
    println!("cargo:rustc-env=HI_AGENT_ADAPTER_VERSION={}", manifest.adapter_version);
    println!("cargo:rustc-env=HI_AGENT_CLAUDE_VERSION={}", manifest.claude_version);

    // macOS only: compile + link the native SwiftUI Settings window (the Phase-1 shell
    // client of the config API — see src/foundation/vendors/macos_swift_settings.rs).
    // Gated on the target OS so the Linux/Docker and Windows builds never invoke swiftc.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_swift_settings();
    }
}

/// Compile `swift/HiSettings.swift` into a static lib and emit the link directives that
/// pull it (plus AppKit/SwiftUI/Foundation and the OS Swift runtime) into the binary.
///
/// UNBUILT-caveat: written without a macOS toolchain to test against. The static Swift
/// link is the most likely fix-forward point — if the linker can't resolve the Swift
/// runtime symbols, options are (a) add the toolchain's lib path from
/// `swiftc -print-target-info`, or (b) switch to a dynamic library + an @rpath and
/// bundle the dylib. SwiftUI itself is an OS framework (never statically linked).
fn build_swift_settings() {
    let swift = "src/foundation/vendors/swift/HiSettings.swift";
    println!("cargo:rerun-if-changed={swift}");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let lib = format!("{out_dir}/libHiSettings.a");

    // Resolve the macOS SDK so swiftc links against the right frameworks.
    let sdk = std::process::Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun --show-sdk-path (is the Xcode command-line toolchain installed?)");
    let sdk_path = String::from_utf8_lossy(&sdk.stdout).trim().to_string();

    // Emit a static archive of the module. `-parse-as-library` keeps top-level items
    // (the @_cdecl entry) from being treated as `main`.
    let status = std::process::Command::new("xcrun")
        .args([
            "--sdk",
            "macosx",
            "swiftc",
            "-O",
            "-parse-as-library",
            "-static",
            "-emit-library",
            "-module-name",
            "HiSettings",
            "-sdk",
            &sdk_path,
            "-o",
            &lib,
            swift,
        ])
        .status()
        .expect("run swiftc");
    assert!(status.success(), "swiftc failed to build HiSettings.swift");

    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static=HiSettings");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=SwiftUI");
    println!("cargo:rustc-link-lib=framework=Foundation");
    // Swift-in-the-OS runtime: the static archive carries autolink hints for swiftCore
    // et al.; hand the linker the OS Swift lib dir so it can resolve them.
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-L/usr/lib/swift");
}

struct ManifestVersions {
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
    let adapter_version = get("adapter_version").unwrap_or_else(|| "dev".to_string());
    let claude_version = get("claude_version").unwrap_or_else(|| "dev".to_string());

    ManifestVersions { node_version, adapter_version, claude_version }
}
