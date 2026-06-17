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
//! [`init_from_env`] is the composition root — it sequences each capability's
//! own `init_from_env`, so a misconfigured provider (unknown name, missing key)
//! fails fast at startup rather than as an error at first use.
//!
//! [`desktop_context`] is the one exception to the env-config pattern: its
//! vendor is the operating system, selected at compile time, so it has no
//! `init_from_env` and does not appear in the composition root.

pub mod desktop_context;
pub mod face;
pub mod image_gen;
pub mod stt;
pub mod tts;
pub mod video_gen;
pub mod vision;
pub mod voiceprint;

/// Initialize every capability from the environment. Fails fast if any
/// configured provider is missing a required credential or names an unknown
/// provider.
pub fn init_from_env() -> anyhow::Result<()> {
    stt::init_from_env()?;
    tts::init_from_env()?;
    vision::init_from_env()?;
    image_gen::init_from_env()?;
    video_gen::init_from_env()?;
    voiceprint::init_from_env()?;
    face::init_from_env()?;
    Ok(())
}
