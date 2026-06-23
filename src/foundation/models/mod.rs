//! ONNX model auto-provisioning for the local recognition capabilities.
//!
//! The voiceprint (CAM++) and face (InsightFace `buffalo_l`) capabilities run
//! local ONNX models. Rather than make the operator hand-place model files and
//! point env vars at them, we **fetch the pinned models on first run** into the
//! OS cache — the same managed philosophy as [`crate::runtime`] (which downloads
//! Node + the ACP adapter): a bundled app needs no separate model install, and
//! `make dev` just works with recognition on.
//!
//! Each model is pinned by URL + SHA-256 + byte length. The cache file is
//! **content-addressed** by the digest, so bumping a pin downloads fresh beside
//! the old file instead of reusing a stale one. A download is verified (size +
//! SHA-256) before it is trusted, and published via temp-then-rename, so a
//! concurrent or interrupted start never observes a partial or corrupt file.
//!
//! Provisioning is **best-effort, not fatal**: if a fetch fails (offline first
//! run, mirror down), the capability simply stays disabled for this launch and
//! the agent runs without it — exactly as an unconfigured capability would. A
//! later launch heals it, and once cached the cost is paid once.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// A pinned model: where to fetch it and what a correct copy is. The SHA-256 is
/// both the integrity check and the cache key; `size` is a cheap first-line
/// check and drives the "~N MB" first-run hint.
pub struct ModelSpec {
    /// Stable stem for logs and the cache filename (e.g. `campplus`).
    pub name: &'static str,
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the exact bytes served at `url`.
    pub sha256: &'static str,
    /// Expected length in bytes.
    pub size: u64,
}

/// CAM++ speaker-embedding model — 3D-Speaker `zh-cn 16k common` (80-dim fbank
/// in, 192-d voiceprint out), the model the voiceprint vendor's front-end
/// (`knf-rs` fbank + CMN) was written against. Served as a single file by
/// sherpa-onnx's speaker-recognition release (note the upstream tag's spelling).
pub const CAMPLUS: ModelSpec = ModelSpec {
    name: "campplus_sv_zh-cn_16k-common",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
    sha256: "f682b514c05d947ee3fa91cd6ec6c5c7543479a128373fa29b1faedccd21fd11",
    size: 28_281_138,
};

/// InsightFace `buffalo_l` SCRFD detector (`det_10g`): RGB image in, per-stride
/// (8/16/32) score/bbox/keypoint maps out. Byte-identical to the official
/// release zip; served as a single file by the `public-data/insightface` mirror.
pub const SCRFD: ModelSpec = ModelSpec {
    name: "buffalo_l_det_10g",
    url: "https://huggingface.co/public-data/insightface/resolve/main/models/buffalo_l/det_10g.onnx",
    sha256: "5838f7fe053675b1c7a08b633df49e7af5495cee0493c7dcf6697200b85b5b91",
    size: 16_923_827,
};

/// InsightFace `buffalo_l` ArcFace recognizer (`w600k_r50`): aligned 112×112
/// crop in, L2-normalized 512-d embedding out. Same mirror as [`SCRFD`].
pub const ARCFACE: ModelSpec = ModelSpec {
    name: "buffalo_l_w600k_r50",
    url: "https://huggingface.co/public-data/insightface/resolve/main/models/buffalo_l/w600k_r50.onnx",
    sha256: "4c06341c33c2ca1f86781dab0e829f88ad5b64be9fba56e56bc9ebdefc619e43",
    size: 174_383_860,
};

/// Cache dir for provisioned models. Override with `HI_AGENT_MODELS_DIR` (used
/// verbatim — a dev escape hatch); otherwise a `models/` subdir under the OS
/// cache dir, alongside the managed runtime's cache.
fn models_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("HI_AGENT_MODELS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("dev", "human-interface", "hi-agent")
        .ok_or_else(|| anyhow!("cannot determine OS cache dir"))?;
    Ok(dirs.cache_dir().join("models"))
}

/// Ensure `spec`'s model is present in the cache and return its path. A cached
/// file of the right size at the digest-addressed path is reused (its digest was
/// verified before it was published); otherwise the model is downloaded, its
/// size + SHA-256 verified, and atomically published.
pub async fn ensure(spec: &ModelSpec) -> anyhow::Result<PathBuf> {
    let dir = models_dir()?;
    // Address by a digest prefix: a pin bump changes the name, so a new model
    // lands beside the old rather than colliding with a stale file.
    let path = dir.join(format!("{}-{}.onnx", spec.name, &spec.sha256[..16]));

    if let Ok(meta) = tokio::fs::metadata(&path).await {
        if meta.len() == spec.size {
            return Ok(path);
        }
        // A truncated/corrupt leftover at the addressed path — drop and re-fetch.
        let _ = tokio::fs::remove_file(&path).await;
    }

    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;

    hint(&format!(
        "first run — downloading recognition model {} (~{} MB)…",
        spec.name,
        spec.size / 1_000_000
    ));

    // Stream to a sibling temp, hashing as we go, then publish atomically.
    let tmp = dir.join(format!(".{}.tmp.{}", spec.name, std::process::id()));
    let _ = tokio::fs::remove_file(&tmp).await;
    if let Err(e) = download_verify(spec, &tmp).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    match tokio::fs::rename(&tmp, &path).await {
        Ok(()) => {}
        // A racing start already published the same file; keep theirs, drop ours.
        Err(_) if tokio::fs::try_exists(&path).await.unwrap_or(false) => {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(anyhow!("publishing model to {}: {e}", path.display()));
        }
    }
    tracing::info!(model = spec.name, path = %path.display(), "recognition model ready");
    Ok(path)
}

/// Download `spec` to `tmp`, streaming to disk while computing the SHA-256, and
/// fail if the final length or digest disagrees with the pin (so `tmp` is never
/// trusted on a mismatch — the caller removes it).
async fn download_verify(spec: &ModelSpec, tmp: &Path) -> anyhow::Result<()> {
    let resp = reqwest::get(spec.url)
        .await
        .with_context(|| format!("requesting {}", spec.url))?
        .error_for_status()
        .with_context(|| format!("downloading {}", spec.url))?;

    let mut file = tokio::fs::File::create(tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut len: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {} download body", spec.name))?;
        hasher.update(&chunk);
        len += chunk.len() as u64;
        file.write_all(&chunk).await.context("writing model chunk")?;
    }
    file.flush().await.context("flushing model file")?;

    if len != spec.size {
        bail!("{}: downloaded {len} bytes, expected {} (truncated?)", spec.name, spec.size);
    }
    let digest = hex_lower(&hasher.finalize());
    if digest != spec.sha256 {
        bail!("{}: sha256 {digest}, expected {} (wrong or corrupt file)", spec.name, spec.sha256);
    }
    Ok(())
}

/// First-run user-facing hint — straight to stderr (not `tracing`) so it shows
/// regardless of `RUST_LOG`, mirroring the managed runtime's install messages.
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
    fn hex_lower_is_zero_padded_lowercase() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff, 0xa0]), "000fffa0");
    }

    #[test]
    fn pins_are_well_formed() {
        for spec in [&CAMPLUS, &SCRFD, &ARCFACE] {
            assert_eq!(spec.sha256.len(), 64, "{}: sha256 is 64 hex chars", spec.name);
            assert!(spec.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(spec.url.starts_with("https://"), "{}: https url", spec.name);
            assert!(spec.size > 0);
        }
    }
}
