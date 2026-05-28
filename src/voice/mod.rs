//! Voice capabilities — speech-to-text and text-to-speech.
//!
//! Each capability is an independent trait (`Stt`, `Tts`) with its own
//! provider impls under sibling modules. The two happen to share a vendor in
//! v0 (Volcengine), but configuration, credentials, and lifecycle are
//! per-capability — swapping STT or TTS is a one-file change.
//!
//! `build_stt` / `build_tts` read the `STT_PROVIDER` / `TTS_PROVIDER` env vars
//! and return `Some(handle)` for a configured provider or `None` if disabled.
//! Callers (`server::build`, `reactor::start`) treat `None` as "capability
//! unavailable" and respond accordingly: 501 on `POST /audio`, error string
//! from the `speak` MCP tool on `channel="audio"`.

use std::sync::Arc;

pub mod stt;
pub mod tts;
pub mod volcengine_stt;
pub mod volcengine_tts;

pub use stt::Stt;
pub use tts::{AudioBlob, Tts};

const ENV_STT_PROVIDER: &str = "STT_PROVIDER";
const ENV_TTS_PROVIDER: &str = "TTS_PROVIDER";

/// Construct the STT provider selected by `STT_PROVIDER`, or `None` if unset
/// or set to `none`. An unknown provider name is an error so misconfiguration
/// surfaces at startup rather than as a 501 at request time.
pub fn build_stt() -> anyhow::Result<Option<Arc<dyn Stt>>> {
    let provider = std::env::var(ENV_STT_PROVIDER).unwrap_or_default();
    match provider.as_str() {
        "" | "none" => Ok(None),
        "volcengine" => {
            let stt = volcengine_stt::VolcengineStt::from_env()?;
            Ok(Some(Arc::new(stt)))
        }
        other => anyhow::bail!("unknown STT_PROVIDER: {other}"),
    }
}

/// Construct the TTS provider selected by `TTS_PROVIDER`, or `None` if unset
/// or set to `none`.
pub fn build_tts() -> anyhow::Result<Option<Arc<dyn Tts>>> {
    let provider = std::env::var(ENV_TTS_PROVIDER).unwrap_or_default();
    match provider.as_str() {
        "" | "none" => Ok(None),
        "volcengine" => {
            let tts = volcengine_tts::VolcengineTts::from_env()?;
            Ok(Some(Arc::new(tts)))
        }
        other => anyhow::bail!("unknown TTS_PROVIDER: {other}"),
    }
}
