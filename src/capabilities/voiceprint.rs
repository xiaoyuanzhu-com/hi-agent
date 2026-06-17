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
//! process-global, once-initialized config: [`init_from_env`] reads
//! `VOICEPRINT_PROVIDER`, [`available`] reports whether a provider is
//! configured, and [`embed`] dispatches to it. The config never appears in a
//! signature. Unlike the API-backed capabilities the vendor is a local ONNX
//! model, so inference is CPU-bound and runs on a blocking thread.
//!
//! **No caller wires this in yet.** This is the capability a future perception
//! path (e.g. embedding each speaker-turn the STT returns) will call; wiring it
//! in later is purely additive.

use std::sync::OnceLock;

use anyhow::Context;

use crate::vendors::campplus;

enum Backend {
    Disabled,
    CamPlusPlus(campplus::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "VOICEPRINT_PROVIDER";

/// Resolve the provider from `VOICEPRINT_PROVIDER` into the process-global
/// config. Unset or `none` disables the capability; an unknown name is an error
/// so a typo fails at startup rather than at first use. Idempotent — the first
/// init wins.
pub fn init_from_env() -> anyhow::Result<()> {
    let backend = match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
        "" | "none" => Backend::Disabled,
        "campplus" => Backend::CamPlusPlus(campplus::Config::from_env()?),
        other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured.
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
        _ => anyhow::bail!("voiceprint not configured (set {ENV_PROVIDER})"),
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
