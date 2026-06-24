//! Static `ffmpeg` binary provisioning for the bundled macOS app.
//!
//! Unlike the recognition models, `ffmpeg` has never been *managed*: the still-
//! frame/clip helpers ([`super::ffmpeg_frame`]) shell out to whatever `ffmpeg` is
//! on `PATH` (or `FFMPEG_BIN`). That is fine on a dev box or in Docker (apt has
//! it), but a shipped `.app` cannot assume the user installed ffmpeg. So for the
//! hermetic bundle we ship a pinned static `ffmpeg` under
//! `Contents/Resources/ffmpeg/ffmpeg`, provisioned at package time and resolved
//! first at runtime ([`bundled_bin`]).
//!
//! The pin is a single static macOS-arm64 binary from `eugeneware/ffmpeg-static`
//! (immutable per-tag GitHub release asset), verified by SHA-256 + size exactly
//! like a [`super::super::models::ModelSpec`]. We only ever *decode* (H.264 / HEVC /
//! VP8 / VP9 are native ffmpeg decoders) and encode `mjpeg` stills + `pcm_s16le`
//! clips — all built in — so the stock build covers our use.
//!
//! **Licensing:** these are GPL builds. hi-agent invokes `ffmpeg` as a *separate
//! process* (no linking), so the GPL does not reach our Rust code; to honor the
//! source-availability obligation when *distributing* the binary we carry its
//! upstream `LICENSE` next to it (fetched best-effort by [`provision_into`]) and
//! pin the exact upstream tag below.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// A pinned static ffmpeg build for one host target: where to fetch it and what a
/// correct copy is (same shape as a model pin — SHA-256 is integrity + identity).
struct FfmpegPin {
    url: &'static str,
    sha256: &'static str,
    size: u64,
    /// Upstream release asset for the build's license text, carried beside the
    /// binary for distribution compliance. Not integrity-pinned (informational).
    license_url: &'static str,
}

/// Upstream release tag we pin (eugeneware/ffmpeg-static). Bump deliberately and
/// re-verify the SHA below.
const RELEASE_TAG: &str = "b6.1.1";

/// The pin for the current host, or `None` on a target we don't ship a static
/// build for. Only **macOS arm64** is wired up today — that is the only shape the
/// `.app` targets; every other target keeps using `PATH`/`FFMPEG_BIN` ffmpeg.
fn pin() -> Option<FfmpegPin> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some(FfmpegPin {
            url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/ffmpeg-darwin-arm64",
            sha256: "a90e3db6a3fd35f6074b013f948b1aa45b31c6375489d39e572bea3f18336584",
            size: 45_568_216,
            license_url: "https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/darwin-arm64.LICENSE",
        }),
        _ => None,
    }
}

/// The bundled static ffmpeg inside a packaged `.app`, or `None` when not running
/// from a bundle or it isn't present. `Contents/Resources/ffmpeg/ffmpeg`.
pub fn bundled_bin() -> Option<PathBuf> {
    let p = crate::bundle::resources_dir()?.join("ffmpeg").join("ffmpeg");
    p.is_file().then_some(p)
}

/// Provision the pinned static ffmpeg into `<dir>/ffmpeg` (binary named `ffmpeg`,
/// made executable), plus its `LICENSE` beside it. Used at package time to fill a
/// `.app`'s `Contents/Resources/ffmpeg`. Verifies size + SHA-256 before
/// publishing via temp-then-rename, so an interrupted run never leaves a partial
/// binary. Errors if the host target has no pin (so packaging fails loudly rather
/// than silently shipping a model-less, ffmpeg-less app).
pub async fn provision_into(dir: &Path) -> anyhow::Result<()> {
    let pin = pin().ok_or_else(|| {
        anyhow!(
            "no pinned static ffmpeg for {}-{} (tag {RELEASE_TAG}); only macOS arm64 is wired up",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })?;

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;

    let bin = dir.join("ffmpeg");
    if let Ok(meta) = tokio::fs::metadata(&bin).await {
        if meta.len() == pin.size {
            return Ok(()); // already provisioned (size matches the pin)
        }
        let _ = tokio::fs::remove_file(&bin).await;
    }

    hint(&format!("downloading static ffmpeg {RELEASE_TAG} (~{} MB)…", pin.size / 1_000_000));

    let tmp = dir.join(format!(".ffmpeg.tmp.{}", std::process::id()));
    let _ = tokio::fs::remove_file(&tmp).await;
    if let Err(e) = download_verify(&pin, &tmp).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    make_executable(&tmp).await?;

    tokio::fs::rename(&tmp, &bin)
        .await
        .with_context(|| format!("publishing ffmpeg to {}", bin.display()))?;

    // License text beside the binary — best-effort (distribution hygiene, not a
    // gate): a missing/changed asset must not fail the bundle.
    if let Ok(resp) = reqwest::get(pin.license_url).await {
        if let Ok(resp) = resp.error_for_status() {
            if let Ok(bytes) = resp.bytes().await {
                let _ = tokio::fs::write(dir.join("LICENSE"), &bytes).await;
            }
        }
    }

    tracing::info!(path = %bin.display(), "static ffmpeg ready");
    Ok(())
}

/// Stream `pin.url` to `tmp`, hashing as we go; fail if the final length or digest
/// disagrees with the pin so `tmp` is never trusted on a mismatch.
async fn download_verify(pin: &FfmpegPin, tmp: &Path) -> anyhow::Result<()> {
    let resp = reqwest::get(pin.url)
        .await
        .with_context(|| format!("requesting {}", pin.url))?
        .error_for_status()
        .with_context(|| format!("downloading {}", pin.url))?;

    let mut file = tokio::fs::File::create(tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut len: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading ffmpeg download body")?;
        hasher.update(&chunk);
        len += chunk.len() as u64;
        file.write_all(&chunk).await.context("writing ffmpeg chunk")?;
    }
    file.flush().await.context("flushing ffmpeg file")?;

    if len != pin.size {
        bail!("ffmpeg: downloaded {len} bytes, expected {} (truncated?)", pin.size);
    }
    let digest = hex_lower(&hasher.finalize());
    if digest != pin.sha256 {
        bail!("ffmpeg: sha256 {digest}, expected {} (wrong or corrupt file)", pin.sha256);
    }
    Ok(())
}

/// Set the owner-execute bit (and group/other read+execute) on a freshly
/// downloaded binary so it can be spawned.
#[cfg(unix)]
async fn make_executable(p: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    tokio::fs::set_permissions(p, perms)
        .await
        .with_context(|| format!("chmod +x {}", p.display()))
}

#[cfg(not(unix))]
async fn make_executable(_p: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// First-run user-facing hint — straight to stderr (not `tracing`) so it shows
/// regardless of `RUST_LOG`, mirroring the runtime/model provisioners.
fn hint(msg: &str) {
    eprintln!("hi-agent: {msg}");
}

/// Lowercase hex of a byte slice (for comparing a computed digest to the pin).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_well_formed_when_present() {
        if let Some(p) = pin() {
            assert_eq!(p.sha256.len(), 64);
            assert!(p.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(p.url.starts_with("https://"));
            assert!(p.url.contains(RELEASE_TAG));
            assert!(p.size > 0);
        }
    }
}
