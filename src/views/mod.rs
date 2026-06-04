//! View compiler — turns agent-authored JSX/TSX into an ESM module the browser
//! imports same-origin.
//!
//! The agent emits a view as component source. We run it through esbuild's
//! single-file *transform* (not a bundle): JSX/TS → ESM, with every bare import
//! (`react`, `react/jsx-runtime`, `@hi/core`, `motion/react`) left untouched so
//! the page's import map resolves them to the host's shared instances. Output is
//! content-addressed under `data_dir/generated/views/<hash>.mjs` and served by
//! `GET /generated/views/<hash>.mjs`; identical source compiles at most once.
//!
//! esbuild ships as a native binary in the `@esbuild/<os>-<arch>` package, which
//! the managed runtime installs alongside the ACP adapter (see
//! `src/runtime/package.json`). We exec that binary directly — no Node wrapper.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::runtime::ResolvedRuntime;

/// Compiles agent view source to a served ESM module URL. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ViewCompiler {
    /// The esbuild native binary (`@esbuild/<os>-<arch>/bin/esbuild`).
    esbuild_bin: PathBuf,
    /// Where compiled modules are written (`data_dir/generated/views`).
    generated_dir: PathBuf,
}

impl ViewCompiler {
    /// Build from the resolved runtime. esbuild lives in the same `node_modules`
    /// the ACP adapter was installed into; compiled modules go under `data_dir`.
    pub fn new(runtime: &ResolvedRuntime, data_dir: &Path) -> Self {
        Self::with_paths(
            esbuild_binary(&runtime.adapter_entry),
            data_dir.join("generated").join("views"),
        )
    }

    fn with_paths(esbuild_bin: PathBuf, generated_dir: PathBuf) -> Self {
        Self { esbuild_bin, generated_dir }
    }

    /// Compile `source` to an ESM module and return its served URL
    /// (`/generated/views/<hash>.mjs`). Content-addressed: identical source
    /// yields the same URL and is compiled at most once (a cache hit never
    /// spawns esbuild).
    pub async fn compile(&self, source: &str) -> anyhow::Result<String> {
        let (hash, url) = module_ref(source);
        let out_path = self.generated_dir.join(format!("{hash}.mjs"));
        if out_path.exists() {
            return Ok(url);
        }
        if !self.esbuild_bin.exists() {
            bail!(
                "esbuild not found at {} — the managed runtime installs it via \
                 `npm ci`; a system runtime must provide it on the adapter",
                self.esbuild_bin.display()
            );
        }

        let js = self.transform(source).await?;

        tokio::fs::create_dir_all(&self.generated_dir)
            .await
            .with_context(|| format!("creating {}", self.generated_dir.display()))?;
        // Atomic publish: write a temp then rename, so a concurrent import never
        // observes a half-written module.
        let tmp = self
            .generated_dir
            .join(format!("{hash}.mjs.tmp.{}", std::process::id()));
        tokio::fs::write(&tmp, js.as_bytes())
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &out_path)
            .await
            .with_context(|| format!("publishing {}", out_path.display()))?;
        Ok(url)
    }

    /// Run esbuild as a single-file transform: source on stdin, ESM on stdout.
    /// No `--bundle`, so bare imports survive for the import map to resolve.
    async fn transform(&self, source: &str) -> anyhow::Result<String> {
        let mut child = Command::new(&self.esbuild_bin)
            .arg("--loader=tsx")
            .arg("--format=esm")
            .arg("--jsx=automatic")
            .arg("--jsx-import-source=react")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning esbuild at {}", self.esbuild_bin.display()))?;

        // Write stdin from a task so a large stdout can't deadlock us against a
        // full pipe while we're still feeding the source. Dropping the handle
        // closes stdin, signalling EOF.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let src = source.as_bytes().to_vec();
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&src).await;
        });

        let out = child.wait_with_output().await.context("waiting for esbuild")?;
        let _ = writer.await;

        if !out.status.success() {
            bail!(
                "esbuild rejected the view source:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        String::from_utf8(out.stdout).context("esbuild output was not UTF-8")
    }
}

/// Locate the esbuild native binary in the same `node_modules` the adapter was
/// installed into. Returns a non-existent sentinel if either the layout is
/// unfamiliar or the host platform is unsupported — `compile` reports it clearly.
fn esbuild_binary(adapter_entry: &Path) -> PathBuf {
    let platform = match crate::runtime::node_target() {
        Ok((os, arch)) => format!("{os}-{arch}"),
        Err(_) => return PathBuf::from("esbuild-unsupported-platform"),
    };
    match node_modules_ancestor(adapter_entry) {
        Some(nm) => nm.join("@esbuild").join(platform).join("bin").join("esbuild"),
        None => PathBuf::from("esbuild-unresolved"),
    }
}

/// The nearest ancestor of `p` that is a `node_modules` directory.
fn node_modules_ancestor(p: &Path) -> Option<PathBuf> {
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if parent.file_name().is_some_and(|n| n == "node_modules") {
            return Some(parent.to_path_buf());
        }
        cur = parent;
    }
    None
}

/// Deterministic content hash + served URL for `source`. A cache key, not a
/// security boundary: a 64-bit hash is ample for de-duping a few authored views.
fn module_ref(source: &str) -> (String, String) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    let hash = format!("{:016x}", h.finish());
    let url = format!("/generated/views/{hash}.mjs");
    (hash, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_ref_is_deterministic_and_hex() {
        let (h1, u1) = module_ref("export default () => null");
        let (h2, u2) = module_ref("export default () => null");
        assert_eq!(h1, h2);
        assert_eq!(u1, u2);
        assert_eq!(u1, format!("/generated/views/{h1}.mjs"));
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
        let (h3, _) = module_ref("a different view");
        assert_ne!(h1, h3, "different source must hash differently");
    }

    #[test]
    fn finds_node_modules_ancestor() {
        let entry = Path::new("/r/adapter/node_modules/@scope/pkg/dist/index.js");
        assert_eq!(
            node_modules_ancestor(entry),
            Some(PathBuf::from("/r/adapter/node_modules"))
        );
        assert_eq!(node_modules_ancestor(Path::new("/usr/local/bin/tool")), None);
    }

    /// Locate an esbuild native binary if one is installed on this host (managed
    /// runtime, or a dev checkout that has run `npm ci`). Returns `None` to skip
    /// the spawning tests where esbuild isn't provisioned — mirroring how the
    /// runtime tests avoid requiring a managed install.
    fn esbuild_probe() -> Option<PathBuf> {
        let (os, arch) = crate::runtime::node_target().ok()?;
        let candidates = [
            // Managed runtime under the OS cache dir.
            directories::ProjectDirs::from("dev", "human-interface", "hi-agent").map(|d| {
                d.cache_dir()
                    .join("runtime/adapter/node_modules/@esbuild")
                    .join(format!("{os}-{arch}"))
                    .join("bin/esbuild")
            }),
        ];
        candidates
            .into_iter()
            .flatten()
            .find(|p| p.exists())
    }

    #[tokio::test]
    async fn compiles_jsx_to_esm_and_preserves_bare_imports() {
        let Some(esbuild_bin) = esbuild_probe() else {
            eprintln!("skipping: esbuild not provisioned on this host");
            return;
        };
        let tmp = std::env::temp_dir().join(format!("hi-views-test-{}", std::process::id()));
        let compiler = ViewCompiler::with_paths(esbuild_bin, tmp.clone());

        let source = r#"
            import { motion } from "motion/react";
            import { useSpeech } from "@hi/core";
            export default function V() {
              const s = useSpeech();
              return <motion.div layoutId="x">{s.length}</motion.div>;
            }
        "#;
        let url = compiler.compile(source).await.expect("compile succeeds");
        assert!(url.starts_with("/generated/views/") && url.ends_with(".mjs"));

        let file = tmp.join(url.rsplit('/').next().unwrap());
        let js = std::fs::read_to_string(&file).expect("module written");
        assert!(js.contains(r#"from "react/jsx-runtime""#), "jsx runtime import emitted");
        assert!(js.contains(r#"from "motion/react""#), "bare motion import preserved");
        assert!(js.contains(r#"from "@hi/core""#), "bare @hi/core import preserved");
        assert!(!js.contains("<motion.div"), "JSX transformed away");

        // Second compile of identical source is a cache hit (same URL).
        let url2 = compiler.compile(source).await.expect("cache hit");
        assert_eq!(url, url2);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
