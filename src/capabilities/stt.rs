//! Speech-to-text capability — audio bytes in, text out.
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init_from_env`] reads `STT_PROVIDER` and the
//! selected vendor's credentials into the global, [`available`] reports whether
//! a provider is configured, and [`transcribe`] / [`transcribe_streaming`]
//! dispatch to it. The config never appears in a signature — it is transparent
//! to the caller. An uninitialized global means "not configured", the same
//! state as `STT_PROVIDER` unset.

use std::sync::OnceLock;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::vendors::volcengine_stt;

/// One transcript update from a streaming recognition.
///
/// The upstream is two-pass: it emits a fast, rolling preliminary text
/// (`is_final = false`) that keeps changing as you speak, then a polished,
/// punctuated/ITN-corrected final (`is_final = true`) for the utterance.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub is_final: bool,
}

enum Backend {
    Disabled,
    Volcengine(volcengine_stt::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "STT_PROVIDER";

/// Resolve the provider from `STT_PROVIDER` into the process-global config.
/// Unset or `none` disables the capability; an unknown name is an error so a
/// typo fails at startup rather than as a 501 at request time. Idempotent —
/// the first init wins.
pub fn init_from_env() -> anyhow::Result<()> {
    let backend = match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
        "" | "none" => Backend::Disabled,
        "volcengine" => Backend::Volcengine(volcengine_stt::Config::from_env()?),
        other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured. Callers gate on this and respond with
/// "capability unavailable" (e.g. 501) when it is false.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::Volcengine(_)))
}

/// Transcribe `audio` (raw bytes) labeled with the given IANA mime type
/// (e.g. `audio/wav`, `audio/mpeg`). The vendor handles any format-specific
/// framing.
pub async fn transcribe(audio: Bytes, mime: &str) -> anyhow::Result<String> {
    match BACKEND.get() {
        Some(Backend::Volcengine(cfg)) => volcengine_stt::transcribe(cfg, audio, mime).await,
        _ => anyhow::bail!("STT not configured (set {ENV_PROVIDER})"),
    }
}

/// Streaming transcription. Consumes 16 kHz mono 16-bit little-endian PCM
/// chunks from `audio_rx` until it closes, emitting incremental and final
/// [`Transcript`]s on `out` as the upstream produces them. Returns the final
/// transcript text. `out` send errors (receiver gone) are non-fatal;
/// recognition continues so the final can still be returned for journaling.
pub async fn transcribe_streaming(
    audio_rx: mpsc::Receiver<Bytes>,
    out: mpsc::Sender<Transcript>,
) -> anyhow::Result<String> {
    match BACKEND.get() {
        Some(Backend::Volcengine(cfg)) => {
            volcengine_stt::transcribe_streaming(cfg, audio_rx, out).await
        }
        _ => anyhow::bail!("STT not configured (set {ENV_PROVIDER})"),
    }
}
