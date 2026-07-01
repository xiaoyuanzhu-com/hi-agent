//! Text-to-speech capability — text in, streamed audio out.
//!
//! Synthesis is a *streaming session*, not a one-shot call: [`start`] opens a
//! session, the caller pushes text incrementally as the agent produces it, and
//! audio frames stream back as they are synthesized. One session spans a whole
//! turn, so the audio is one continuous stream rather than a sequence of
//! per-sentence clips — the brain consolidates, the client just plays.
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init`] resolves the vendor from the config store,
//! [`available`] reports whether a provider is configured, and [`start`]
//! dispatches to it. The config never appears in a signature.

use std::sync::OnceLock;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::foundation::vendors::volcengine_tts;

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

/// The default wire when the store selects none — the only TTS impl today.
const DEFAULT_WIRE: &str = "volcengine";

/// Resolve the TTS backend into the process-global config from the credential
/// store. A non-empty `store_key` (BYOK or broker-managed) enables the capability
/// on the configured `wire` (`None` → [`DEFAULT_WIRE`]); no key → disabled. An
/// unknown wire is an error. Adding a vendor is a new `Backend` variant plus a
/// match arm here. Idempotent — the first init wins.
pub fn init(store_key: Option<&str>, base_url: Option<&str>, wire: Option<&str>) -> anyhow::Result<()> {
    let backend = if store_key.map(|k| !k.trim().is_empty()).unwrap_or(false) {
        match wire.unwrap_or(DEFAULT_WIRE) {
            "volcengine" => Backend::Volcengine(volcengine_tts::Config::from_store(store_key, base_url)?),
            other => anyhow::bail!("unknown TTS wire: {other}"),
        }
    } else {
        Backend::Disabled
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
        _ => anyhow::bail!("TTS not configured (set a TTS key in Settings)"),
    }
}
