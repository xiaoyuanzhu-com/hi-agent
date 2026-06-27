//! Speech-to-text capability — audio bytes in, text out.
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init`] reads `STT_PROVIDER` and the
//! selected vendor's credentials into the global, [`available`] reports whether
//! a provider is configured, and [`transcribe`] / [`transcribe_streaming`]
//! dispatch to it. The config never appears in a signature — it is transparent
//! to the caller. An uninitialized global means "not configured", the same
//! state as `STT_PROVIDER` unset.

use std::sync::OnceLock;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::foundation::vendors::volcengine_stt;

/// One transcript update from a streaming recognition.
///
/// The upstream is two-pass: it emits a fast, rolling preliminary text
/// (`is_final = false`) that keeps changing as you speak, then a polished,
/// punctuated/ITN-corrected final (`is_final = true`) for the utterance.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub is_final: bool,
    /// Vendor speaker-cluster label for a finalized utterance when diarization is
    /// on (two-pass mode) — e.g. `"0"`, `"1"`. `None` on rolling partials and when
    /// speaker info is off. It is a within-session cluster id, not a persistent
    /// identity; the caller resolves identity by voiceprinting the utterance audio.
    pub speaker_id: Option<String>,
    /// Diarized utterance spans carried on a final, one per speaker turn that
    /// finalized in this update (empty on partials and when diarization is off).
    /// Each gives a speaker label and the utterance's `[start_ms, end_ms]` from the
    /// stream start, so the caller can slice that speaker's *own* audio out of the
    /// live buffer and voiceprint it — instead of attributing a whole multi-speaker
    /// stretch to one label. Distinct from [`Self::speaker_id`], which names only
    /// the single turn the dispatched sentence belongs to.
    pub segments: Vec<DiarizedSpan>,
}

/// A finalized utterance's speaker and audio span (milliseconds from stream start),
/// the unit the voiceprint path slices and embeds per speaker.
#[derive(Debug, Clone)]
pub struct DiarizedSpan {
    pub speaker_id: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

enum Backend {
    Disabled,
    Volcengine(volcengine_stt::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "STT_PROVIDER";

/// Resolve the STT backend into the process-global config. BYOK-first: a
/// non-empty `store_key` (the user's key from `credentials.json`) implies the
/// provider (Volcengine is the only STT impl) and overrides the env key. With no
/// store key, fall back to `STT_PROVIDER` — unset/`none` disables, an unknown
/// name is an error so a typo fails at startup rather than as a 501 at request
/// time. Idempotent — the first init wins.
pub fn init(store_key: Option<&str>) -> anyhow::Result<()> {
    let backend = if store_key.map(|k| !k.trim().is_empty()).unwrap_or(false) {
        Backend::Volcengine(volcengine_stt::Config::from_env_with_key(store_key)?)
    } else {
        match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
            "" | "none" => Backend::Disabled,
            "volcengine" => Backend::Volcengine(volcengine_stt::Config::from_env()?),
            other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
        }
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
