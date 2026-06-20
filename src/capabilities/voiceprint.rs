//! Voiceprint capability — a speaker embedding (acoustic identity) from a voice
//! sample.
//!
//! The vector is the whole point: it turns "this audio" into a comparable point
//! in speaker space, so the agent's judgment layer can later weigh it as *soft
//! evidence* ("this voice is 0.82 similar to the person I call 老王") alongside
//! face, topic, and context — never as a hard verdict. Enrollment, matching, and
//! identity belief live downstream; this capability only produces the vector.
//!
//! Like the other capabilities it is a module of free functions over a
//! process-global, once-initialized config: [`init`] loads the local CAM++ ONNX
//! (auto-provisioned by [`crate::models`]), [`available`] reports whether it is
//! loaded, and [`embed`] dispatches to it. There is only one implementation and
//! no meaningful choice to expose, so it is built-in (on whenever the model
//! provisions) rather than a provider toggle — there is nothing to configure.
//! The vendor is a local ONNX model, so inference is CPU-bound and runs on a
//! blocking thread.
//!
//! Callers: the audio channel voiceprints posted clips and live-mic speaker
//! turns ([`crate::server::audio`]), and reflection clusters clip voices into the
//! people store ([`crate::reactor::heartbeat`]).

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;

use crate::vendors::campplus;

enum Backend {
    CamPlusPlus(campplus::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Turn the voiceprint capability on by loading the auto-provisioned CAM++ ONNX
/// at `model_path`. The ONNX session is built on a blocking thread (parsing a
/// model is synchronous CPU/IO). Errors if the model can't be loaded (a real
/// pin/corruption bug — the caller leaves the capability disabled). Idempotent —
/// the first init wins.
pub async fn init(model_path: PathBuf) -> anyhow::Result<()> {
    let cfg = tokio::task::spawn_blocking(move || campplus::Config::load(&model_path))
        .await
        .context("CAM++ load task panicked")??;
    let _ = BACKEND.set(Backend::CamPlusPlus(cfg));
    Ok(())
}

/// Whether the capability is loaded and ready.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::CamPlusPlus(_)))
}

/// Embed one utterance of **16 kHz mono 16-bit little-endian PCM** into an
/// L2-normalized speaker vector — the same audio contract as STT streaming. The
/// synchronous ONNX inference runs on a blocking thread so the async runtime is
/// never stalled.
pub async fn embed(pcm_16k_mono: Vec<i16>) -> anyhow::Result<Vec<f32>> {
    tokio::task::spawn_blocking(move || match BACKEND.get() {
        Some(Backend::CamPlusPlus(cfg)) => campplus::embed(cfg, &pcm_16k_mono),
        None => anyhow::bail!("voiceprint capability not loaded"),
    })
    .await
    .context("voiceprint embed task panicked")?
}

/// Cosine similarity of two embeddings. For the L2-normalized vectors [`embed`]
/// returns this is their dot product, in `[-1, 1]`; higher means more likely the
/// same speaker. The threshold/decision is the caller's, deliberately.
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
