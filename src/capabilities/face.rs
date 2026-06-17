//! Face capability — detect faces in an image and embed each one.
//!
//! Each face's embedding is the point: it turns "this face" into a comparable
//! vector, so the agent's judgment layer can weigh it as *soft evidence*
//! alongside voiceprint, topic, and context — never as a hard identity verdict
//! (twins are a feature, not a bug, when ambiguity is allowed). Enrollment,
//! matching, and identity belief live downstream; this capability only produces
//! the per-face vectors.
//!
//! It is bundled: a single capability whose one vendor owns the whole
//! detect→align→embed pipeline, so there is no cross-capability reference (the
//! same way `vision` is one capability). Like the others it is a module of free
//! functions over a process-global, once-initialized config: [`init_from_env`]
//! reads `FACE_PROVIDER`, [`available`] reports configuration, and
//! [`detect_and_embed`] dispatches. The vendor is a local ONNX pair (InsightFace
//! SCRFD + ArcFace, the `buffalo_l` models Immich uses), so inference is
//! CPU-bound and runs on a blocking thread.
//!
//! **No caller wires this in yet.** A future perception path (e.g. sampling
//! video frames into faces) is the caller; wiring it in later is purely additive.

use std::sync::OnceLock;

use anyhow::Context;
use bytes::Bytes;

use crate::vendors::insightface_face;

/// One detected face: bounding box and 5 landmarks in **original-image pixels**,
/// the detector's confidence, and an L2-normalized 512-d identity embedding.
#[derive(Debug, Clone)]
pub struct DetectedFace {
    /// `[x1, y1, x2, y2]`.
    pub bbox: [f32; 4],
    /// Left eye, right eye, nose, left mouth, right mouth — each `[x, y]`.
    pub landmarks: [[f32; 2]; 5],
    /// Detection confidence in `[0, 1]`.
    pub score: f32,
    /// L2-normalized embedding; cosine similarity is a plain dot product.
    pub embedding: Vec<f32>,
}

enum Backend {
    Disabled,
    Insightface(insightface_face::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "FACE_PROVIDER";

/// Resolve the provider from `FACE_PROVIDER` into the process-global config.
/// Unset or `none` disables the capability; an unknown name is an error so a
/// typo fails at startup rather than at first use. Idempotent — first init wins.
pub fn init_from_env() -> anyhow::Result<()> {
    let backend = match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
        "" | "none" => Backend::Disabled,
        "insightface" => Backend::Insightface(insightface_face::Config::from_env()?),
        other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::Insightface(_)))
}

/// Detect and embed every face in `image` (raw encoded bytes — JPEG/PNG/etc.,
/// auto-detected). Returns one [`DetectedFace`] per kept detection, or an empty
/// vector if none are found. The synchronous ONNX pipeline runs on a blocking
/// thread so the async runtime is never stalled.
pub async fn detect_and_embed(image: Bytes) -> anyhow::Result<Vec<DetectedFace>> {
    tokio::task::spawn_blocking(move || match BACKEND.get() {
        Some(Backend::Insightface(cfg)) => insightface_face::detect_and_embed(cfg, &image),
        _ => anyhow::bail!("face not configured (set {ENV_PROVIDER})"),
    })
    .await
    .context("face detect_and_embed task panicked")?
}

/// Cosine similarity of two face embeddings. For the L2-normalized vectors
/// [`detect_and_embed`] returns this is their dot product, in `[-1, 1]`; higher
/// means more likely the same person. The threshold/decision is the caller's.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_is_one_for_identical_and_zero_for_orthogonal() {
        let a = [0.6_f32, 0.8];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }
}
