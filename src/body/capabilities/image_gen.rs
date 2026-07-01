//! Image-generation capability — text prompt → still image(s).
//!
//! Synchronous: one request, one response with the picture(s).
//!
//! The capability is a module of free functions over a process-global,
//! once-initialized config: [`init`] reads `IMAGE_GEN_PROVIDER`,
//! [`available`] reports whether a provider is configured, and [`generate`]
//! dispatches to it. The config never appears in a signature.
//!
//! **No caller wires this in yet.** The module is built and unit-tested
//! standalone so a later *emission* path can call it as a purely additive
//! change.

use std::sync::OnceLock;

use crate::foundation::vendors::doubao_image_gen;

/// How the vendor should return a generated image: a hosted `url` (the default;
/// cheaper on the wire but expires upstream, so a caller persists it promptly)
/// or inline base64 (`b64_json`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageFormat {
    #[default]
    Url,
    B64Json,
}

/// A request for one or more still images. Only `prompt` is required; every
/// other field is `None` → "let the model decide", so the vendor's own defaults
/// apply (size, watermark, …) rather than us hard-coding them.
#[derive(Debug, Clone, Default)]
pub struct ImageRequest {
    pub prompt: String,
    /// e.g. `"1024x1024"`, `"2K"`, or `"adaptive"`; vendor-specific.
    pub size: Option<String>,
    pub seed: Option<i64>,
    pub watermark: Option<bool>,
    pub response_format: ImageFormat,
}

impl ImageRequest {
    /// A bare prompt with all knobs left at the vendor default.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self { prompt: prompt.into(), ..Default::default() }
    }
}

/// One generated image. Exactly one of `url` / `b64_json` is populated,
/// matching the requested [`ImageFormat`].
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub url: Option<String>,
    pub b64_json: Option<String>,
    pub size: Option<String>,
}

enum Backend {
    Disabled,
    Doubao(doubao_image_gen::Config),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

const ENV_PROVIDER: &str = "IMAGE_GEN_PROVIDER";

/// The default wire when the store selects none — the only image-gen impl today.
const DEFAULT_WIRE: &str = "doubao";

/// Resolve the image-gen backend into the process-global config. With a BYOK key
/// the configured `wire` selects the impl (`None` → [`DEFAULT_WIRE`]); otherwise
/// `IMAGE_GEN_PROVIDER` decides (unset/`none` disables). An unknown wire or provider
/// name is an error. Adding a vendor is a new `Backend` variant plus a match arm
/// here. Idempotent — the first init wins.
pub fn init(
    store_key: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    wire: Option<&str>,
) -> anyhow::Result<()> {
    let backend = if store_key.map(|k| !k.trim().is_empty()).unwrap_or(false) {
        match wire.unwrap_or(DEFAULT_WIRE) {
            "doubao" => Backend::Doubao(doubao_image_gen::Config::from_env_with(store_key, base_url, model)?),
            other => anyhow::bail!("unknown image-gen wire: {other}"),
        }
    } else {
        match std::env::var(ENV_PROVIDER).unwrap_or_default().as_str() {
            "" | "none" => Backend::Disabled,
            "doubao" => Backend::Doubao(doubao_image_gen::Config::from_env()?),
            other => anyhow::bail!("unknown {ENV_PROVIDER}: {other}"),
        }
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

/// Whether a provider is configured.
pub fn available() -> bool {
    matches!(BACKEND.get(), Some(Backend::Doubao(_)))
}

/// Generate image(s) for `req` and return them. Synchronous: the future
/// resolves once the picture(s) are ready.
pub async fn generate(req: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
    match BACKEND.get() {
        Some(Backend::Doubao(cfg)) => doubao_image_gen::generate(cfg, req).await,
        _ => anyhow::bail!("image generation not configured (set {ENV_PROVIDER})"),
    }
}
