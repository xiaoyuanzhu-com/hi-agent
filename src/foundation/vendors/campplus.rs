//! CAM++ speaker-embedding vendor — local ONNX inference, no network.
//!
//! 3D-Speaker's CAM++ maps a mono 16 kHz utterance to a fixed-length speaker
//! embedding (a voiceprint). The pipeline is:
//!
//!   i16 PCM → f32 → 80-dim kaldi fbank → ONNX → L2-normalized vector
//!
//! The fbank is the only correctness-critical step: it must match the front-end
//! CAM++ trained on. `knf-rs` (kaldi-native-fbank) hardcodes exactly that —
//! 16 kHz, 25/10 ms frames, no dither, 80 mel bins — and applies the same
//! per-utterance cepstral mean normalization, so we feed its output straight in.
//!
//! The vendor owns its [`ort::session::Session`]. Because `Session::run` takes
//! `&mut self`, the session sits behind a [`Mutex`]; inference is synchronous
//! CPU work that the capability layer runs on a blocking thread. The embedding
//! dimension (192 for the zh-cn model) is whatever the loaded model emits — we
//! don't hardcode it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use ort::session::Session;
use ort::value::Tensor;

pub struct Config {
    session: Mutex<Session>,
    /// Kept for diagnostics/logging; the loaded session is the live handle.
    #[allow(dead_code)]
    model_path: PathBuf,
}

impl Config {
    /// Load the CAM++ ONNX at `model_path` — the model auto-provisioned by
    /// [`crate::foundation::models`]. Fails if the file can't be opened as a model (a real
    /// pin/corruption bug), so it surfaces at startup, not at first embed.
    pub fn load(model_path: &Path) -> anyhow::Result<Self> {
        let session = Session::builder()
            .context("creating ORT session builder")?
            .commit_from_file(model_path)
            .with_context(|| format!("loading CAM++ model from {}", model_path.display()))?;
        Ok(Self { session: Mutex::new(session), model_path: model_path.to_path_buf() })
    }
}

/// Embed one utterance of 16 kHz mono 16-bit PCM into an L2-normalized speaker
/// vector. Synchronous CPU work.
pub fn embed(cfg: &Config, pcm_16k_mono: &[i16]) -> anyhow::Result<Vec<f32>> {
    let feats = fbank(pcm_16k_mono)?;
    let (frames, bins) = feats.dim();
    let data = feats.into_raw_vec_and_offset().0; // row-major [frames*bins]
    let input = Tensor::from_array((vec![1_i64, frames as i64, bins as i64], data))
        .context("building CAM++ input tensor")?;

    let mut session = cfg.session.lock().expect("CAM++ session mutex poisoned");
    let outputs = session.run(ort::inputs![input]).context("CAM++ inference")?;
    let (_shape, emb) = outputs[0]
        .try_extract_tensor::<f32>()
        .context("extracting CAM++ embedding")?;
    Ok(l2_normalize(emb))
}

/// 80-dim kaldi fbank with per-utterance CMN, matching CAM++'s training
/// front-end. `knf-rs` fixes the options and applies the mean subtraction.
fn fbank(pcm_16k_mono: &[i16]) -> anyhow::Result<ndarray::Array2<f32>> {
    let mut samples = vec![0.0_f32; pcm_16k_mono.len()];
    knf_rs::convert_integer_to_float_audio(pcm_16k_mono, &mut samples);
    knf_rs::compute_fbank(&samples).map_err(|e| anyhow::anyhow!("computing fbank: {e}"))
}

/// L2-normalize so downstream cosine similarity is a plain dot product. A
/// zero vector (silent/empty input) is returned unchanged rather than producing
/// NaNs.
fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_makes_a_unit_vector() {
        let out = l2_normalize(&[3.0, 4.0]);
        let norm = (out[0] * out[0] + out[1] * out[1]).sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm = {norm}");
        assert!((out[0] - 0.6).abs() < 1e-6 && (out[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_passes_through_a_zero_vector() {
        assert_eq!(l2_normalize(&[0.0, 0.0, 0.0]), vec![0.0, 0.0, 0.0]);
    }
}
