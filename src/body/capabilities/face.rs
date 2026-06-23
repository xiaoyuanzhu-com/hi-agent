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
//! functions over a process-global, once-initialized config: [`init`] loads the
//! local ONNX pair (auto-provisioned by [`crate::foundation::models`]), [`available`] reports
//! whether it is loaded, and [`detect_and_embed`] dispatches. There is only one
//! implementation and no meaningful choice to expose, so it is built-in (on
//! whenever the models provision) rather than a provider toggle — there is
//! nothing to configure. The vendor is a local ONNX pair (InsightFace SCRFD +
//! ArcFace, the `buffalo_l` models Immich uses), so inference is CPU-bound and
//! runs on a blocking thread.
//!
//! Callers: posted stills and camera-stream keyframes are recognized in
//! [`crate::foundation::server::vision`], and reflection clusters faces into the people store
//! in [`crate::body::reactor::heartbeat`].

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;
use bytes::Bytes;

use crate::foundation::vendors::insightface_face;

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
    Insightface(insightface_face::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Turn the face capability on by loading the auto-provisioned InsightFace ONNX
/// pair at `scrfd_path` (detector) and `arcface_path` (recognizer). The sessions
/// are built on a blocking thread (parsing the models is synchronous CPU/IO).
/// Errors if either can't be loaded (a real pin/corruption bug — the caller
/// leaves the capability disabled). Idempotent — first init wins.
pub async fn init(scrfd_path: PathBuf, arcface_path: PathBuf) -> anyhow::Result<()> {
    let cfg =
        tokio::task::spawn_blocking(move || insightface_face::Config::load(&scrfd_path, &arcface_path))
            .await
            .context("InsightFace load task panicked")??;
    let _ = BACKEND.set(Backend::Insightface(cfg));
    Ok(())
}

/// Whether the capability is loaded and ready.
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
        None => anyhow::bail!("face capability not loaded"),
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

/// Crop one detected face out of `image` (the original encoded bytes) to a JPEG —
/// the previewable likeness of a [`DetectedFace`], so a cluster shows *whose* face
/// it is, not just a vector. `bbox` is `[x1, y1, x2, y2]` in original-image pixels;
/// `margin` pads it by that fraction of the box on each side (e.g. `0.3` for a bit
/// of head/shoulders), clamped to the image. Independent of detection/embedding so
/// the caller can keep a crop beside the gallery without re-running the model.
pub fn crop_to_jpeg(image: &[u8], bbox: [f32; 4], margin: f32) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(image).context("decoding image for face crop")?;
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let [x1, y1, x2, y2] = bbox;
    let (mw, mh) = ((x2 - x1) * margin, (y2 - y1) * margin);
    let x1 = (x1 - mw).max(0.0);
    let y1 = (y1 - mh).max(0.0);
    let x2 = (x2 + mw).min(iw);
    let y2 = (y2 + mh).min(ih);
    let (w, h) = (((x2 - x1) as u32).max(1), ((y2 - y1) as u32).max(1));
    let crop = img.crop_imm(x1 as u32, y1 as u32, w, h);
    let mut out = Vec::new();
    crop.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
        .context("encoding face crop as JPEG")?;
    Ok(out)
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
