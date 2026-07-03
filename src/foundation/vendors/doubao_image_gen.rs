//! Volcengine Ark (Doubao) image generation.
//!
//! Endpoint (per <https://www.volcengine.com/docs/82379/2375486>):
//!
//!   POST {api_base}/images/generations            (synchronous)
//!   Authorization: Bearer <api_key>
//!
//! `api_base` defaults to the **plan** endpoint
//! `https://ark.cn-beijing.volces.com/api/plan/v3` — deliberately *not* the
//! plain `/api/v3`, which the docs warn bills as extra. Override only if you are
//! on a different region or billing arrangement.
//!
//! Generation rides the OpenAI-compatible `images/generations` shape
//! (`prompt` / `size` / `response_format` / `seed` / `watermark`, response is a
//! `data` array of `url` or `b64_json`).

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::body::capabilities::image_gen::{GeneratedImage, ImageFormat, ImageRequest};

/// The plan endpoint. The bare `/api/v3` variant bills as extra (per the docs),
/// so it is intentionally not the default.
const DEFAULT_API_BASE: &str = "https://ark.cn-beijing.volces.com/api/plan/v3";
const DEFAULT_IMAGE_MODEL: &str = "doubao-seedream-5.0-lite";
/// Image synthesis is slow (tens of seconds); budget generously.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

impl ImageFormat {
    /// The wire token Ark expects in `response_format`.
    fn as_wire(self) -> &'static str {
        match self {
            ImageFormat::Url => "url",
            ImageFormat::B64Json => "b64_json",
        }
    }
}

pub struct Config {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
    model: String,
}

impl Config {
    /// Resolve config from the credential store. `key` is the vendor API key
    /// (required — the caller builds a config only when a key is present); `base_url`,
    /// when set, is the gateway's **full** image-generations endpoint (songguo, whose
    /// path differs from the vendor's native one) and is used verbatim; with no
    /// `base_url` (BYOK) the vendor's own endpoint is used. `model` overrides the
    /// seedream default. No env.
    pub fn from_store(
        key: Option<&str>,
        base_url: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<Self> {
        let api_key = key
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow::anyhow!("image generation (doubao) requires an API key"))?
            .to_string();
        let endpoint = match base_url.map(str::trim).filter(|b| !b.is_empty()) {
            Some(base) => base.trim_end_matches('/').to_string(),
            None => format!("{}/images/generations", DEFAULT_API_BASE),
        };
        let model = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_IMAGE_MODEL)
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building doubao image-gen HTTP client")?;
        Ok(Self { client, api_key, endpoint, model })
    }
}

/// Build the `images/generations` body. Pure (no I/O) so the wire shape is
/// unit-testable without a network call. Optional knobs are omitted when
/// unset so the model applies its own defaults.
fn build_request(cfg: &Config, req: &ImageRequest) -> Value {
    let mut body = json!({
        "model": cfg.model,
        "prompt": req.prompt,
        "response_format": req.response_format.as_wire(),
    });
    let obj = body.as_object_mut().expect("json object");
    if let Some(size) = &req.size {
        obj.insert("size".into(), json!(size));
    }
    if let Some(seed) = req.seed {
        obj.insert("seed".into(), json!(seed));
    }
    if let Some(watermark) = req.watermark {
        obj.insert("watermark".into(), json!(watermark));
    }
    body
}

pub async fn generate(cfg: &Config, req: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
    let body = build_request(cfg, req);
    let url = cfg.endpoint.clone();

    let resp = cfg
        .client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .context("doubao image-gen request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("reading doubao image-gen response")?;
    if !status.is_success() {
        anyhow::bail!("doubao image-gen HTTP {status}: {text}");
    }

    let parsed: ImageResponse = serde_json::from_str(&text)
        .with_context(|| format!("parsing doubao image-gen response: {text}"))?;
    if parsed.data.is_empty() {
        anyhow::bail!("doubao image-gen returned no images");
    }
    Ok(parsed.data.into_iter().map(Into::into).collect())
}

#[derive(Debug, Deserialize)]
struct ImageResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
}

#[derive(Debug, Deserialize)]
struct ImageDatum {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

impl From<ImageDatum> for GeneratedImage {
    fn from(d: ImageDatum) -> Self {
        GeneratedImage { url: d.url, b64_json: d.b64_json, size: d.size }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_gen() -> Config {
        Config {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            endpoint: format!("{}/images/generations", DEFAULT_API_BASE),
            model: DEFAULT_IMAGE_MODEL.to_string(),
        }
    }

    #[test]
    fn default_base_is_the_plan_endpoint_not_plain_v3() {
        // The docs warn that /api/v3 bills as extra; the default must be plan/v3.
        assert!(DEFAULT_API_BASE.contains("/api/plan/v3"));
        assert!(!DEFAULT_API_BASE.contains("/api/v3/"));
    }

    #[test]
    fn image_request_omits_unset_knobs() {
        let body = build_request(&image_gen(), &ImageRequest::new("a red bicycle"));
        assert_eq!(body["model"], DEFAULT_IMAGE_MODEL);
        assert_eq!(body["prompt"], "a red bicycle");
        assert_eq!(body["response_format"], "url");
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("size"));
        assert!(!obj.contains_key("seed"));
        assert!(!obj.contains_key("watermark"));
    }

    #[test]
    fn image_request_includes_set_knobs() {
        let req = ImageRequest {
            prompt: "a sunset".to_string(),
            size: Some("2K".to_string()),
            seed: Some(42),
            watermark: Some(false),
            response_format: ImageFormat::B64Json,
        };
        let body = build_request(&image_gen(), &req);
        assert_eq!(body["response_format"], "b64_json");
        assert_eq!(body["size"], "2K");
        assert_eq!(body["seed"], 42);
        assert_eq!(body["watermark"], false);
    }

    #[test]
    fn parses_image_response_url_and_b64() {
        let raw = r#"{
            "data": [
                { "url": "https://example.com/a.png", "size": "1024x1024" },
                { "b64_json": "AAAA" }
            ]
        }"#;
        let parsed: ImageResponse = serde_json::from_str(raw).unwrap();
        let images: Vec<GeneratedImage> = parsed.data.into_iter().map(Into::into).collect();
        assert_eq!(images[0].url.as_deref(), Some("https://example.com/a.png"));
        assert_eq!(images[0].size.as_deref(), Some("1024x1024"));
        assert_eq!(images[1].b64_json.as_deref(), Some("AAAA"));
        assert!(images[1].url.is_none());
    }
}
