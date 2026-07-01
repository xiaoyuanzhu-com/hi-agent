//! Capabilities — the stable interface layer.
//!
//! Each capability (STT, TTS, vision, image generation, video generation) is an
//! **independent module of free functions** over a process-global,
//! once-initialized config. A capability function reads its global, picks the
//! configured vendor, and dispatches; the config is transparent to the caller
//! and never appears in a signature. The vendor impls live under
//! [`crate::foundation::vendors`].
//!
//! The capabilities are deliberately independent: no shared-vendor umbrella, no
//! cross-capability references. A vendor that happens to back several
//! capabilities is configured separately for each.
//!
//! [`init`] is the composition root for the keyed capabilities — it sequences
//! each one's own `init`, threading that vendor's BYOK key (from the credential
//! store, else its `.env` key) in. A misconfigured provider (unknown name, a key
//! that won't build) fails fast at startup rather than as an error at first use.
//! The two recognition capabilities (voiceprint, face) are not env-configured — they run pinned local ONNX models that [`init_recognition`]
//! auto-provisions on first run (see [`crate::foundation::models`]), so they have no provider
//! toggle and nothing for the operator to set.
//!
//! [`accessibility`], [`audio_capture`], [`desktop_context`], [`hotkey`],
//! [`input`], [`screencast`], and [`tray`] are the exceptions to the env-config
//! pattern: their vendor is the operating system, selected at compile time, so they
//! have no `init` and do not appear in the composition root.

use crate::foundation::models;

pub mod accessibility;
pub mod audio_capture;
pub mod bundle;
pub mod desktop_context;
pub mod face;
pub mod hotkey;
pub mod image_gen;
pub mod input;
pub mod screencast;
pub mod stt;
pub mod tray;
pub mod tts;
pub mod video_gen;
pub mod vision;
pub mod voiceprint;

/// Initialize the keyed capabilities (STT, TTS, vision, image/video gen) from the
/// credentials in effect for the current mode — the user's BYOK keys, or the
/// broker-minted bundle (xiaoyuanzhu) — falling back to `.env` per vendor. Fails
/// fast if a configured provider is missing its key or names an unknown provider.
/// The recognition capabilities are provisioned separately by [`init_recognition`].
pub fn init(creds: &crate::foundation::credentials::Credentials) -> anyhow::Result<()> {
    let eff = creds.effective();
    stt::init(
        eff.as_ref().and_then(|e| e.stt.key_opt()),
        eff.as_ref().and_then(|e| e.stt.base_url_opt()),
        eff.as_ref().and_then(|e| e.stt.model_opt()),
        eff.as_ref().and_then(|e| e.stt.wire_opt()),
    )?;
    tts::init(
        eff.as_ref().and_then(|e| e.tts.key_opt()),
        eff.as_ref().and_then(|e| e.tts.base_url_opt()),
        eff.as_ref().and_then(|e| e.tts.wire_opt()),
    )?;
    vision::init(
        eff.as_ref().and_then(|e| e.vision.key_opt()),
        eff.as_ref().and_then(|e| e.vision.base_url_opt()),
        eff.as_ref().and_then(|e| e.vision.model_opt()),
        eff.as_ref().and_then(|e| e.vision.wire_opt()),
    )?;
    image_gen::init(
        eff.as_ref().and_then(|e| e.image.key_opt()),
        eff.as_ref().and_then(|e| e.image.base_url_opt()),
        eff.as_ref().and_then(|e| e.image.model_opt()),
        eff.as_ref().and_then(|e| e.image.wire_opt()),
    )?;
    video_gen::init(
        eff.as_ref().and_then(|e| e.video.key_opt()),
        eff.as_ref().and_then(|e| e.video.base_url_opt()),
        eff.as_ref().and_then(|e| e.video.model_opt()),
        eff.as_ref().and_then(|e| e.video.wire_opt()),
    )?;
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
