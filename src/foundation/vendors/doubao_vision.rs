//! Volcengine Ark (Doubao) vision — image/video understanding over the
//! OpenAI-compatible **Responses API** (`POST {api_base}/responses`).
//!
//! Endpoint (Volcengine Ark **Agent Plan**, per <https://www.volcengine.com/docs/82379/1569618>):
//!
//!   POST {api_base}/responses        (api_base default .../api/plan/v3)
//!   Authorization: Bearer <api_key>
//!
//! The Agent Plan exposes the OpenAI-compatible Responses API at `/api/plan/v3` and
//! takes the plan's **dedicated** API key (distinct from a regular platform key). A
//! pay-as-you-go platform key instead uses `/api/v3` — override via the vendor's
//! base URL in Settings.
//!
//! Multimodal input rides the Responses `input` array — a `user` message whose
//! `content` mixes the media part with the text prompt:
//!
//!   image: {"type":"input_image","image_url": <url|data-url>, "detail":"high"}
//!   video: {"type":"input_video","video_url": <url|data-url>, "fps": 1}
//!   text:  {"type":"input_text","text": <prompt>}
//!
//! The answer comes back in `output[]`: the assistant `message` item's `content[]`
//! carries `{"type":"output_text","text": …}` parts, which we concatenate.
//!
//! With no stored model override, the model is the balanced tier
//! (`doubao-seed-2.0-lite`); the other tiers are `fast`→`doubao-seed-2.0-mini` and
//! `accurate`→`doubao-seed-2.0-pro`. A stored model (Settings) overrides the tier
//! with any id verbatim.
//!
//! NOTE (verify against the live API): the Responses-API **video** content part —
//! the `input_video`/`video_url`/`fps` shape above — is modeled on the `input_image`
//! part and Doubao's Chat-Completions video extension; confirm it on first live run
//! and adjust [`build_request`] if the wire differs.

use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::body::capabilities::vision::{MediaSource, VisualMedia};

const DEFAULT_API_BASE: &str = "https://ark.cn-beijing.volces.com/api/plan/v3";
/// Image fidelity tier. Doubao accepts `low | high | xhigh`.
const IMAGE_DETAIL: &str = "high";
/// Frames-per-second sampled from a video. Higher = more sensitive to motion but
/// more tokens. Doubao's accepted range is 0.2–5.
const VIDEO_FPS: f32 = 1.0;
/// Vision — especially video — is slow; budget generously.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The model tier used when no explicit model override is stored.
const DEFAULT_PRESET: &str = "balanced";

/// Map a quality/speed preset to its Seed 2.0 model id: `fast` is cheapest,
/// `accurate` the most capable, `balanced` the middle. Unknown names error.
fn preset_model(name: &str) -> anyhow::Result<&'static str> {
    Ok(match name {
        "fast" => "doubao-seed-2.0-mini",
        "balanced" => "doubao-seed-2.0-lite",
        "accurate" => "doubao-seed-2.0-pro",
        other => anyhow::bail!("unknown vision preset: {other} (expected fast|balanced|accurate)"),
    })
}

pub struct Config {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    model: String,
}

impl Config {
    /// Resolve config from the credential store. `key` is the vendor API key
    /// (required — the caller builds a config only when a key is present); `base_url`
    /// host-rebases the api base onto the gateway (songguo) when the broker supplies
    /// one; `model` overrides the default. With no model, the balanced-preset model
    /// is used — no env.
    pub fn from_store(
        key: Option<&str>,
        base_url: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<Self> {
        let api_key = key
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow::anyhow!("vision (doubao) requires an API key"))?
            .to_string();
        let mut api_base = DEFAULT_API_BASE.to_string();
        if let Some(base) = base_url.map(str::trim).filter(|b| !b.is_empty()) {
            api_base = super::rebase_host(&api_base, base);
        }
        let model = match model.map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => m.to_string(),
            None => preset_model(DEFAULT_PRESET)?.to_string(),
        };

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building doubao vision HTTP client")?;

        Ok(Self { client, api_key, api_base, model })
    }
}

/// Build the Responses request body. Pure (no I/O) so the wire shape is
/// unit-testable without a network call.
fn build_request(cfg: &Config, media: VisualMedia, prompt: &str) -> anyhow::Result<Value> {
    let media_part = match media {
        VisualMedia::Image(src) => json!({
            "type": "input_image",
            "image_url": src.into_url()?,
            "detail": IMAGE_DETAIL,
        }),
        VisualMedia::Video(src) => json!({
            "type": "input_video",
            "video_url": src.into_url()?,
            "fps": VIDEO_FPS,
        }),
    };

    Ok(json!({
        "model": cfg.model,
        "input": [{
            "role": "user",
            "content": [
                media_part,
                { "type": "input_text", "text": prompt },
            ],
        }],
    }))
}

pub async fn understand(cfg: &Config, media: VisualMedia, prompt: &str) -> anyhow::Result<String> {
    let body = build_request(cfg, media, prompt)?;
    let url = format!("{}/responses", cfg.api_base.trim_end_matches('/'));

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

    let parsed: ResponsesReply = serde_json::from_str(&text)
        .with_context(|| format!("parsing doubao vision response: {text}"))?;
    parsed
        .first_text()
        .ok_or_else(|| anyhow::anyhow!("doubao vision returned no content"))
}

impl MediaSource {
    /// Resolve to a URL the API accepts: a passthrough URL, or raw bytes encoded as
    /// a base64 `data:` URL.
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

/// Minimal view of the Responses reply — we only need the assistant's text, which
/// lives in the `message` output item's `content[]` as `output_text` parts. Other
/// output items (e.g. `reasoning`) carry no `content` and are skipped.
#[derive(Debug, Deserialize)]
struct ResponsesReply {
    #[serde(default)]
    output: Vec<OutputItem>,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(default)]
    content: Vec<OutputContent>,
}

#[derive(Debug, Deserialize)]
struct OutputContent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

impl ResponsesReply {
    fn first_text(&self) -> Option<String> {
        let mut acc = String::new();
        for item in &self.output {
            for part in &item.content {
                if part.kind == "output_text"
                    && let Some(t) = &part.text
                {
                    acc.push_str(t);
                }
            }
        }
        let acc = acc.trim().to_string();
        if acc.is_empty() { None } else { Some(acc) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn config() -> Config {
        Config {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            model: "doubao-seed-2.0-lite".to_string(),
        }
    }

    #[test]
    fn presets_map_to_seed_2_0_tiers() {
        assert_eq!(preset_model("fast").unwrap(), "doubao-seed-2.0-mini");
        assert_eq!(preset_model("balanced").unwrap(), "doubao-seed-2.0-lite");
        assert_eq!(preset_model("accurate").unwrap(), "doubao-seed-2.0-pro");
        assert!(preset_model("turbo").is_err());
    }

    #[test]
    fn image_bytes_become_an_input_image_data_url() {
        let media = VisualMedia::image_bytes(Bytes::from_static(b"\xff\xd8jpegbytes"), "image/jpeg");
        let body = build_request(&config(), media, "What is this?").unwrap();

        assert_eq!(body["model"], "doubao-seed-2.0-lite");
        let content = &body["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_image");
        let url = content[0]["image_url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "got {url}");
        assert_eq!(content[0]["detail"], IMAGE_DETAIL);
        assert_eq!(content[1]["type"], "input_text");
        assert_eq!(content[1]["text"], "What is this?");
    }

    #[test]
    fn video_url_passes_through_as_input_video() {
        let media = VisualMedia::video_url("https://example.com/clip.mp4");
        let body = build_request(&config(), media, "Summarize the clip.").unwrap();

        let part = &body["input"][0]["content"][0];
        assert_eq!(part["type"], "input_video");
        assert_eq!(part["video_url"], "https://example.com/clip.mp4");
        assert!(part["fps"].is_number());
    }

    #[test]
    fn parses_output_text_from_responses() {
        let raw = r#"{
            "output": [
                { "type": "reasoning", "summary": [] },
                { "type": "message", "role": "assistant",
                  "content": [ { "type": "output_text", "text": "  a red car  " } ] }
            ]
        }"#;
        let parsed: ResponsesReply = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.first_text().as_deref(), Some("a red car"));
    }

    #[test]
    fn empty_output_yields_none() {
        let parsed: ResponsesReply = serde_json::from_str(r#"{ "output": [] }"#).unwrap();
        assert!(parsed.first_text().is_none());
    }
}
