//! Capabilities — the stable interface layer.
//!
//! Each capability (STT, TTS, vision, image generation, video generation) is an
//! **independent module of free functions** over a process-global,
//! once-initialized config. A capability function reads its global, picks the
//! configured vendor, and dispatches; the config is transparent to the caller
//! and never appears in a signature. The vendor impls live under
//! [`crate::vendors`].
//!
//! The capabilities are deliberately independent: no shared-vendor umbrella, no
//! cross-capability references. A vendor that happens to back several
//! capabilities is configured separately for each.
//!
//! [`init_from_env`] is the composition root for the env-configured capabilities
//! — it sequences each one's own `init_from_env`, so a misconfigured provider
//! (unknown name, missing key) fails fast at startup rather than as an error at
//! first use. The two recognition capabilities (voiceprint, face) are not
//! env-configured — they run pinned local ONNX models that [`init_recognition`]
//! auto-provisions on first run (see [`crate::models`]), so they have no provider
//! toggle and nothing for the operator to set.
//!
//! [`desktop_context`], [`input`], and [`screencast`] are the exceptions to the
//! env-config pattern: their vendor is the operating system, selected at compile
//! time, so they have no `init_from_env` and do not appear in the composition
//! root.

use crate::models;

pub mod desktop_context;
pub mod face;
pub mod image_gen;
pub mod input;
pub mod screencast;
pub mod stt;
pub mod tts;
pub mod video_gen;
pub mod vision;
pub mod voiceprint;

/// Initialize the env-configured capabilities. Fails fast if any configured
/// provider is missing a required credential or names an unknown provider. The
/// recognition capabilities are provisioned separately by [`init_recognition`].
pub fn init_from_env() -> anyhow::Result<()> {
    stt::init_from_env()?;
    tts::init_from_env()?;
    vision::init_from_env()?;
    image_gen::init_from_env()?;
    video_gen::init_from_env()?;
    Ok(())
}

/// Provision and load the local recognition models (voiceprint + face) — pinned
/// ONNX fetched into the OS cache on first run, reused thereafter. **Best-effort
/// and never fatal**: if a model can't be provisioned or loaded (offline first
/// run, mirror down, a bad pin), that capability stays disabled for this launch
/// and the agent runs without it — the same as any unconfigured capability. The
/// failure is logged server-side; there is nothing for the operator to fix.
///
/// The two models the face capability needs are fetched concurrently; voiceprint
/// runs alongside. Already-cached runs are effectively instant.
pub async fn init_recognition() {
    let (voice, scrfd, arcface) = tokio::join!(
        models::ensure(&models::CAMPLUS),
        models::ensure(&models::SCRFD),
        models::ensure(&models::ARCFACE),
    );

    match voice {
        Ok(path) => {
            if let Err(err) = voiceprint::init(path).await {
                tracing::error!(error = %err, "voiceprint model loaded but failed to init; capability disabled");
            }
        }
        Err(err) => tracing::warn!(error = %err, "voiceprint model unavailable; capability disabled"),
    }

    match (scrfd, arcface) {
        (Ok(s), Ok(a)) => {
            if let Err(err) = face::init(s, a).await {
                tracing::error!(error = %err, "face models loaded but failed to init; capability disabled");
            }
        }
        (s, a) => {
            let err = s.err().or(a.err()).map(|e| e.to_string()).unwrap_or_default();
            tracing::warn!(error = %err, "face models unavailable; capability disabled");
        }
    }
}
