//! Volcengine Ark (Doubao) vision — image/video understanding over the
//! OpenAI-compatible Chat Completions API.
//!
//! Endpoint (per <https://www.volcengine.com/docs/82379>):
//!
//!   POST {api_base}/chat/completions      (api_base default .../api/v3)
//!   Authorization: Bearer <api_key>
//!
//! Multimodal input rides the standard OpenAI content-part array — a `user`
//! message whose `content` is `[<media part>, {type:text}]`:
//!
//!   image: {"type":"image_url","image_url":{"url": <url|data-url>, "detail": "high"}}
//!   video: {"type":"video_url","video_url":{"url": <url|data-url>, "fps": 1}}
//!
//! Three **presets** pick the model + reasoning behaviour in one knob; each field
//! is independently overridable by env (see [`Config::from_env`]):
//!
//!   preset    model                  thinking   effort
//!   fast      doubao-seed-2.0-mini   disabled   minimal
//!   balanced  doubao-seed-2.0-lite   auto       medium     (default)
//!   accurate  doubao-seed-2.0-pro    enabled    high
//!
//! `thinking` is Doubao's deep-think toggle (`{"type": enabled|disabled|auto}`).
//! `effort` is sent as the OpenAI-compatible top-level `reasoning_effort`
//! (`minimal|low|medium|high`) which bounds how much the model deliberates;
//! adjust if a given Ark model rejects it.

use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::body::capabilities::vision::{MediaSource, VisualMedia};

const DEFAULT_API_BASE: &str = "https://ark.cn-beijing.volces.com/api/v3";
const DEFAULT_PRESET: &str = "balanced";
/// Image fidelity tier. Doubao accepts `low | high | xhigh`.
const IMAGE_DETAIL: &str = "high";
/// Frames-per-second sampled from a video. Higher = more sensitive to motion but
/// more tokens. Doubao's accepted range is 0.2–5.
const VIDEO_FPS: f32 = 1.0;
/// Vision — especially video with thinking enabled — is slow; budget generously.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

const ENV_API_KEY: &str = "DOUBAO_VISION_API_KEY";
const ENV_API_BASE: &str = "DOUBAO_VISION_API_BASE";
const ENV_PRESET: &str = "DOUBAO_VISION_PRESET";
const ENV_MODEL: &str = "DOUBAO_VISION_MODEL";
const ENV_THINKING: &str = "DOUBAO_VISION_THINKING";
const ENV_EFFORT: &str = "DOUBAO_VISION_EFFORT";

/// A fast/balanced/accurate bundle of model + reasoning settings.
struct Preset {
    model: &'static str,
    thinking: &'static str,
    effort: &'static str,
}

/// Map a preset name to its bundle. Unknown names are an error so a typo in
/// `DOUBAO_VISION_PRESET` fails at startup rather than silently picking a tier.
fn resolve_preset(name: &str) -> anyhow::Result<Preset> {
    Ok(match name {
        "fast" => Preset { model: "doubao-seed-2.0-mini", thinking: "disabled", effort: "minimal" },
        "balanced" => Preset { model: "doubao-seed-2.0-lite", thinking: "auto", effort: "medium" },
        "accurate" => Preset { model: "doubao-seed-2.0-pro", thinking: "enabled", effort: "high" },
        other => anyhow::bail!(
            "unknown {ENV_PRESET}: {other} (expected fast|balanced|accurate)"
        ),
    })
}

pub struct Config {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    model: String,
    thinking: String,
    effort: String,
}

impl Config {
    /// Resolve config from the environment: the selected preset provides the
    /// defaults for model/thinking/effort, and each is individually overridable.
    /// `DOUBAO_VISION_API_KEY` is required.
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var(ENV_API_KEY).map_err(|_| {
            anyhow::anyhow!("{ENV_API_KEY} is required when VISION_PROVIDER=doubao")
        })?;
        let api_base =
            std::env::var(ENV_API_BASE).unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        let preset_name =
            std::env::var(ENV_PRESET).unwrap_or_else(|_| DEFAULT_PRESET.to_string());
        let preset = resolve_preset(&preset_name)?;

        let model = std::env::var(ENV_MODEL).unwrap_or_else(|_| preset.model.to_string());
        let thinking =
            std::env::var(ENV_THINKING).unwrap_or_else(|_| preset.thinking.to_string());
        let effort = std::env::var(ENV_EFFORT).unwrap_or_else(|_| preset.effort.to_string());

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building doubao vision HTTP client")?;

        Ok(Self { client, api_key, api_base, model, thinking, effort })
    }
}

/// Build the Chat Completions request body. Pure (no I/O) so the wire shape
/// is unit-testable without a network call.
fn build_request(cfg: &Config, media: VisualMedia, prompt: &str) -> anyhow::Result<Value> {
    let media_part = match media {
        VisualMedia::Image(src) => json!({
            "type": "image_url",
            "image_url": { "url": src.into_url()?, "detail": IMAGE_DETAIL },
        }),
        VisualMedia::Video(src) => json!({
            "type": "video_url",
            "video_url": { "url": src.into_url()?, "fps": VIDEO_FPS },
        }),
    };

    Ok(json!({
        "model": cfg.model,
        "messages": [{
            "role": "user",
            "content": [
                media_part,
                { "type": "text", "text": prompt },
            ],
        }],
        "thinking": { "type": cfg.thinking },
        "reasoning_effort": cfg.effort,
    }))
}

pub async fn understand(cfg: &Config, media: VisualMedia, prompt: &str) -> anyhow::Result<String> {
    let body = build_request(cfg, media, prompt)?;
    let url = format!("{}/chat/completions", cfg.api_base.trim_end_matches('/'));

    let resp = cfg
        .client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .context("doubao vision request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("reading doubao vision response")?;
    if !status.is_success() {
        anyhow::bail!("doubao vision HTTP {status}: {text}");
    }

    let parsed: ChatResponse = serde_json::from_str(&text)
        .with_context(|| format!("parsing doubao vision response: {text}"))?;
    parsed
        .first_text()
        .ok_or_else(|| anyhow::anyhow!("doubao vision returned no content"))
}

impl MediaSource {
    /// Resolve to a URL the API accepts: a passthrough URL, or raw bytes encoded
    /// as a base64 `data:` URL.
    fn into_url(self) -> anyhow::Result<String> {
        match self {
            MediaSource::Url(url) => Ok(url),
            MediaSource::Bytes { bytes, mime } => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(format!("data:{mime};base64,{b64}"))
            }
        }
    }
}

/// Minimal view of the Chat Completions response — we only need the assistant's
/// text. Doubao puts any chain-of-thought in a separate `reasoning_content`
/// field which we deliberately ignore; the answer is in `content`.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

impl ChatResponse {
    fn first_text(&self) -> Option<String> {
        self.choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn config(preset: &str) -> Config {
        let p = resolve_preset(preset).unwrap();
        Config {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            model: p.model.to_string(),
            thinking: p.thinking.to_string(),
            effort: p.effort.to_string(),
        }
    }

    #[test]
    fn presets_map_to_expected_settings() {
        let fast = resolve_preset("fast").unwrap();
        assert_eq!(fast.model, "doubao-seed-2.0-mini");
        assert_eq!(fast.thinking, "disabled");
        assert_eq!(fast.effort, "minimal");

        let balanced = resolve_preset("balanced").unwrap();
        assert_eq!(balanced.model, "doubao-seed-2.0-lite");

        let accurate = resolve_preset("accurate").unwrap();
        assert_eq!(accurate.model, "doubao-seed-2.0-pro");
        assert_eq!(accurate.thinking, "enabled");
        assert_eq!(accurate.effort, "high");
    }

    #[test]
    fn unknown_preset_is_an_error() {
        assert!(resolve_preset("turbo").is_err());
    }

    #[test]
    fn image_bytes_become_a_base64_data_url_part() {
        let cfg = config("accurate");
        let media = VisualMedia::image_bytes(Bytes::from_static(b"\xff\xd8jpegbytes"), "image/jpeg");
        let body = build_request(&cfg, media, "What is this?").unwrap();

        assert_eq!(body["model"], "doubao-seed-2.0-pro");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");

        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "image_url");
        let url = content[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "got {url}");
        assert_eq!(content[0]["image_url"]["detail"], IMAGE_DETAIL);
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "What is this?");
    }

    #[test]
    fn video_url_passes_through_with_fps() {
        let cfg = config("balanced");
        let media = VisualMedia::video_url("https://example.com/clip.mp4");
        let body = build_request(&cfg, media, "Summarize the clip.").unwrap();

        let part = &body["messages"][0]["content"][0];
        assert_eq!(part["type"], "video_url");
        assert_eq!(part["video_url"]["url"], "https://example.com/clip.mp4");
        assert!(part["video_url"]["fps"].is_number());
    }

    #[test]
    fn parses_assistant_text_from_choices() {
        let raw = r#"{
            "choices": [
                { "message": { "role": "assistant", "reasoning_content": "...", "content": "  a red car  " } }
            ]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.first_text().as_deref(), Some("a red car"));
    }

    #[test]
    fn empty_content_yields_none() {
        let raw = r#"{ "choices": [ { "message": { "content": "" } } ] }"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.first_text().is_none());
    }
}
