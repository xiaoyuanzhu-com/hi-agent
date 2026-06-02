//! Text-to-speech capability — text in, streamed audio out.
//!
//! Synthesis is a *streaming session*, not a one-shot call: [`start`] opens a
//! session, the caller pushes text incrementally as the agent produces it, and
//! audio frames stream back as they are synthesized. One session spans a whole
//! turn, so the audio is one continuous stream rather than a sequence of
//! per-sentence clips — the brain consolidates, the client just plays.
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init_from_env`] reads `TTS_PROVIDER`, [`available`]
//! reports whether a provider is configured, and [`start`] dispatches to it. The
//! config never appears in a signature.

use std::sync::OnceLock;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::vendors::volcengine_tts;

/// A live synthesis session. Feed text via [`text`](Self::text) as it becomes
/// available; drop the sender to signal end-of-input (the provider flushes any
/// trailing audio and closes). Drain [`frames`](Self::frames) for the audio
/// bytes; the receiver closes when synthesis ends or the session errors.
///
/// Every frame shares the same [`mime`](Self::mime) — it is fixed for the life
/// of the session and known the moment the session opens, so the HTTP layer can
/// set `Content-Type` before the first byte.
pub struct TtsStream {
    /// IANA mime type for every frame in this stream, e.g. `audio/mpeg`.
    pub mime: String,
    /// Push text to be spoken. Send each chunk as it arrives; dropping the
    /// sender signals that no more text is coming.
    pub text: mpsc::Sender<String>,
    /// Synthesized audio frames, in order. Closes when synthesis completes.
    pub frames: mpsc::Receiver<Bytes>,
}

enum Backend {
    Disabled,
    Volcengine(volcengine_tts::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "TTS_PROVIDER";

/// Resolve the provider from `TTS_PROVIDER` into the process-global config.
/// Unset or `none` disables the capability; an unknown name is an error.
/// Idempotent — the first init wins.
pub fn init_from_env() -> anyhow::Result<()> {
    let backend = match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
        "" | "none" => Backend::Disabled,
        "volcengine" => Backend::Volcengine(volcengine_tts::Config::from_env()?),
        other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::Volcengine(_)))
}

/// Open a streaming synthesis session. Returns once the session is ready to
/// accept text; synthesis is driven by pushing text and draining frames.
pub async fn start() -> anyhow::Result<TtsStream> {
    match BACKEND.get() {
        Some(Backend::Volcengine(cfg)) => volcengine_tts::start(cfg).await,
        _ => anyhow::bail!("TTS not configured (set {ENV_PROVIDER})"),
    }
}
