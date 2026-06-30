//! Volcengine Ark (Doubao) video generation.
//!
//! Endpoints (per <https://www.volcengine.com/docs/82379/2375486>):
//!
//!   submit POST {api_base}/contents/generations/tasks     (submit → task id)
//!   poll   GET  {api_base}/contents/generations/tasks/{id} (poll → status)
//!   Authorization: Bearer <api_key>
//!
//! `api_base` defaults to the **plan** endpoint
//! `https://ark.cn-beijing.volces.com/api/plan/v3` — deliberately *not* the
//! plain `/api/v3`, which the docs warn bills as extra. Override only if you are
//! on a different region or billing arrangement.
//!
//! Generation is Ark's async task API: the create body is `{model, content:[…]}`
//! with generation knobs (`resolution` / `ratio` / `duration` / …) as sibling
//! fields — the Seedance 2.0 convention — and the poll response carries `status`
//! plus, on success, `content.video_url` (note: that URL expires ~24h after
//! success, so a caller should download it promptly).

use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::body::capabilities::video_gen::{ImageRef, VideoRequest, VideoStatus, VideoTask};

/// The plan endpoint. The bare `/api/v3` variant bills as extra (per the docs),
/// so it is intentionally not the default.
const DEFAULT_API_BASE: &str = "https://ark.cn-beijing.volces.com/api/plan/v3";
const DEFAULT_VIDEO_MODEL: &str = "doubao-seedance-2.0";
/// Submit/poll calls are quick, but one generous timeout covers the slow path.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

const ENV_VIDEO_API_KEY: &str = "DOUBAO_VIDEO_API_KEY";
const ENV_VIDEO_API_BASE: &str = "DOUBAO_VIDEO_API_BASE";
const ENV_VIDEO_MODEL: &str = "DOUBAO_VIDEO_MODEL";

impl ImageRef {
    /// Resolve to a URL the API accepts: a passthrough URL, or raw bytes encoded
    /// as a base64 `data:` URL.
    fn to_url(&self) -> String {
        match self {
            ImageRef::Url(url) => url.clone(),
            ImageRef::Bytes { bytes, mime } => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                format!("data:{mime};base64,{b64}")
            }
        }
    }
}

pub struct Config {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    model: String,
}

impl Config {
    /// Resolve config from the environment. `DOUBAO_VIDEO_API_KEY` is required;
    /// base URL and model fall back to the plan endpoint and seedance default.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_env_with(None, None, None)
    }

    /// Back-compat: BYOK key override only.
    pub fn from_env_with_key(key_override: Option<&str>) -> anyhow::Result<Self> {
        Self::from_env_with(key_override, None, None)
    }

    /// Resolve config, taking managed overrides when present: key, api-base host
    /// (rebased onto the gateway), and model.
    pub fn from_env_with(
        key_override: Option<&str>,
        base_url_override: Option<&str>,
        model_override: Option<&str>,
    ) -> anyhow::Result<Self> {
        let api_key = match key_override {
            Some(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => std::env::var(ENV_VIDEO_API_KEY).map_err(|_| {
                anyhow::anyhow!("{ENV_VIDEO_API_KEY} is required when VIDEO_GEN_PROVIDER=doubao")
            })?,
        };
        let mut api_base =
            std::env::var(ENV_VIDEO_API_BASE).unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        if let Some(base) = base_url_override {
            api_base = super::rebase_host(&api_base, base);
        }
        let model = match model_override {
            Some(m) if !m.trim().is_empty() => m.trim().to_string(),
            _ => std::env::var(ENV_VIDEO_MODEL).unwrap_or_else(|_| DEFAULT_VIDEO_MODEL.to_string()),
        };
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building doubao video-gen HTTP client")?;
        Ok(Self { client, api_key, api_base, model })
    }
}

/// Build the create-task body. Pure (no I/O) so the wire shape is
/// unit-testable. The prompt is the first `content` part; a first-frame
/// reference (image-to-video) is appended as an `image_url` part. Generation
/// knobs ride as sibling fields, omitted when unset.
fn build_create_request(cfg: &Config, req: &VideoRequest) -> Value {
    let mut content = vec![json!({ "type": "text", "text": req.prompt })];
    if let Some(frame) = &req.first_frame {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": frame.to_url() },
            "role": "first_frame",
        }));
    }

    let mut body = json!({ "model": cfg.model, "content": content });
    let obj = body.as_object_mut().expect("json object");
    if let Some(resolution) = &req.resolution {
        obj.insert("resolution".into(), json!(resolution));
    }
    if let Some(ratio) = &req.ratio {
        obj.insert("ratio".into(), json!(ratio));
    }
    if let Some(duration) = req.duration {
        obj.insert("duration".into(), json!(duration));
    }
    if let Some(watermark) = req.watermark {
        obj.insert("watermark".into(), json!(watermark));
    }
    if let Some(seed) = req.seed {
        obj.insert("seed".into(), json!(seed));
    }
    body
}

fn tasks_url(cfg: &Config) -> String {
    format!("{}/contents/generations/tasks", cfg.api_base.trim_end_matches('/'))
}

pub async fn submit(cfg: &Config, req: &VideoRequest) -> anyhow::Result<String> {
    let body = build_create_request(cfg, req);

    let resp = cfg
        .client
        .post(tasks_url(cfg))
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .context("doubao video-gen submit failed")?;

    let status = resp.status();
    let text = resp.text().await.context("reading doubao video-gen submit response")?;
    if !status.is_success() {
        anyhow::bail!("doubao video-gen submit HTTP {status}: {text}");
    }

    let parsed: SubmitResponse = serde_json::from_str(&text)
        .with_context(|| format!("parsing doubao video-gen submit response: {text}"))?;
    if parsed.id.is_empty() {
        anyhow::bail!("doubao video-gen submit returned no task id: {text}");
    }
    Ok(parsed.id)
}

pub async fn poll(cfg: &Config, task_id: &str) -> anyhow::Result<VideoTask> {
    let url = format!("{}/{task_id}", tasks_url(cfg));

    let resp = cfg
        .client
        .get(&url)
        .bearer_auth(&cfg.api_key)
        .send()
        .await
        .context("doubao video-gen poll failed")?;

    let status = resp.status();
    let text = resp.text().await.context("reading doubao video-gen poll response")?;
    if !status.is_success() {
        anyhow::bail!("doubao video-gen poll HTTP {status}: {text}");
    }

    let parsed: TaskResponse = serde_json::from_str(&text)
        .with_context(|| format!("parsing doubao video-gen poll response: {text}"))?;
    parsed.into_task()
}

#[derive(Debug, Deserialize)]
struct SubmitResponse {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Deserialize)]
struct TaskResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    content: Option<TaskContent>,
    #[serde(default)]
    error: Option<TaskError>,
}

#[derive(Debug, Deserialize)]
struct TaskContent {
    #[serde(default)]
    video_url: Option<String>,
    #[serde(default)]
    last_frame_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskError {
    #[serde(default)]
    message: Option<String>,
}

impl TaskResponse {
    /// Map the wire `status` string + payload onto a typed [`VideoTask`].
    /// `succeeded` requires a `content.video_url`; its absence is an error
    /// rather than a silently-empty success.
    fn into_task(self) -> anyhow::Result<VideoTask> {
        let status = match self.status.as_str() {
            "queued" => VideoStatus::Queued,
            "running" => VideoStatus::Running,
            "succeeded" => {
                let content = self
                    .content
                    .ok_or_else(|| anyhow::anyhow!("succeeded video task has no content"))?;
                let video_url = content
                    .video_url
                    .ok_or_else(|| anyhow::anyhow!("succeeded video task has no video_url"))?;
                VideoStatus::Succeeded { video_url, last_frame_url: content.last_frame_url }
            }
            "failed" => VideoStatus::Failed {
                message: self
                    .error
                    .and_then(|e| e.message)
                    .unwrap_or_else(|| "unknown error".to_string()),
            },
            "cancelled" => VideoStatus::Cancelled,
            "expired" => VideoStatus::Expired,
            other => anyhow::bail!("unknown video task status: {other}"),
        };
        Ok(VideoTask { id: self.id, status })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn video_gen() -> Config {
        Config {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            model: DEFAULT_VIDEO_MODEL.to_string(),
        }
    }

    #[test]
    fn video_create_text_to_video_omits_unset_knobs() {
        let body = build_create_request(&video_gen(), &VideoRequest::new("a cat running"));
        assert_eq!(body["model"], DEFAULT_VIDEO_MODEL);
        let content = &body["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "a cat running");
        // text-to-video: no image part
        assert!(content.get(1).is_none());
        let obj = body.as_object().unwrap();
        assert!(!obj.contains_key("resolution"));
        assert!(!obj.contains_key("duration"));
    }

    #[test]
    fn video_create_image_to_video_appends_first_frame_and_knobs() {
        let req = VideoRequest {
            prompt: "zoom out slowly".to_string(),
            first_frame: Some(ImageRef::bytes(Bytes::from_static(b"\xff\xd8jpeg"), "image/jpeg")),
            resolution: Some("1080p".to_string()),
            ratio: Some("16:9".to_string()),
            duration: Some(5),
            watermark: Some(false),
            seed: Some(7),
        };
        let body = build_create_request(&video_gen(), &req);

        let frame = &body["content"][1];
        assert_eq!(frame["type"], "image_url");
        assert_eq!(frame["role"], "first_frame");
        let url = frame["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "got {url}");

        assert_eq!(body["resolution"], "1080p");
        assert_eq!(body["ratio"], "16:9");
        assert_eq!(body["duration"], 5);
        assert_eq!(body["watermark"], false);
        assert_eq!(body["seed"], 7);
    }

    #[test]
    fn video_first_frame_url_passes_through() {
        let req = VideoRequest {
            first_frame: Some(ImageRef::url("https://example.com/frame.png")),
            ..VideoRequest::new("pan left")
        };
        let body = build_create_request(&video_gen(), &req);
        assert_eq!(body["content"][1]["image_url"]["url"], "https://example.com/frame.png");
    }

    #[test]
    fn parses_running_then_succeeded() {
        let running: TaskResponse =
            serde_json::from_str(r#"{ "id": "cgt-1", "status": "running" }"#).unwrap();
        let task = running.into_task().unwrap();
        assert_eq!(task.id, "cgt-1");
        assert_eq!(task.status, VideoStatus::Running);
        assert!(!task.status.is_terminal());

        let done: TaskResponse = serde_json::from_str(
            r#"{ "id": "cgt-1", "status": "succeeded",
                 "content": { "video_url": "https://x/v.mp4", "last_frame_url": "https://x/f.png" } }"#,
        )
        .unwrap();
        let task = done.into_task().unwrap();
        assert!(task.status.is_terminal());
        match task.status {
            VideoStatus::Succeeded { video_url, last_frame_url } => {
                assert_eq!(video_url, "https://x/v.mp4");
                assert_eq!(last_frame_url.as_deref(), Some("https://x/f.png"));
            }
            other => panic!("expected succeeded, got {other:?}"),
        }
    }

    #[test]
    fn succeeded_without_video_url_is_an_error() {
        let resp: TaskResponse =
            serde_json::from_str(r#"{ "id": "cgt-1", "status": "succeeded", "content": {} }"#)
                .unwrap();
        assert!(resp.into_task().is_err());
    }

    #[test]
    fn parses_failed_with_message() {
        let resp: TaskResponse = serde_json::from_str(
            r#"{ "id": "cgt-1", "status": "failed", "error": { "message": "nsfw blocked" } }"#,
        )
        .unwrap();
        match resp.into_task().unwrap().status {
            VideoStatus::Failed { message } => assert_eq!(message, "nsfw blocked"),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn unknown_status_is_an_error() {
        let resp: TaskResponse =
            serde_json::from_str(r#"{ "id": "cgt-1", "status": "frobnicating" }"#).unwrap();
        assert!(resp.into_task().is_err());
    }
}
