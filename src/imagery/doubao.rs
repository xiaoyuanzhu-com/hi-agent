//! Volcengine Ark (Doubao) image and video generation.
//!
//! Endpoints (per <https://www.volcengine.com/docs/82379/2375486>):
//!
//!   image  POST {api_base}/images/generations            (synchronous)
//!   video  POST {api_base}/contents/generations/tasks     (submit → task id)
//!          GET  {api_base}/contents/generations/tasks/{id} (poll → status)
//!   Authorization: Bearer <api_key>
//!
//! `api_base` defaults to the **plan** endpoint
//! `https://ark.cn-beijing.volces.com/api/plan/v3` — deliberately *not* the
//! plain `/api/v3`, which the docs warn bills as extra. Override only if you are
//! on a different region or billing arrangement.
//!
//! Image generation rides the OpenAI-compatible `images/generations` shape
//! (`prompt` / `size` / `response_format` / `seed` / `watermark`, response is a
//! `data` array of `url` or `b64_json`). Video generation is Ark's async task
//! API: the create body is `{model, content:[…]}` with generation knobs
//! (`resolution` / `ratio` / `duration` / …) as sibling fields — the Seedance
//! 2.0 convention — and the poll response carries `status` plus, on success,
//! `content.video_url` (note: that URL expires ~24h after success, so a caller
//! should download it promptly).

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    GeneratedImage, ImageFormat, ImageGen, ImageRef, ImageRequest, VideoGen, VideoRequest,
    VideoStatus, VideoTask,
};

/// The plan endpoint. The bare `/api/v3` variant bills as extra (per the docs),
/// so it is intentionally not the default.
const DEFAULT_API_BASE: &str = "https://ark.cn-beijing.volces.com/api/plan/v3";
const DEFAULT_IMAGE_MODEL: &str = "doubao-seedream-5.0-lite";
const DEFAULT_VIDEO_MODEL: &str = "doubao-seedance-2.0";
/// Image synthesis is slow (tens of seconds); video submit/poll calls are quick
/// but share the budget. One generous timeout covers both.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

const ENV_IMAGE_API_KEY: &str = "DOUBAO_IMAGE_API_KEY";
const ENV_IMAGE_API_BASE: &str = "DOUBAO_IMAGE_API_BASE";
const ENV_IMAGE_MODEL: &str = "DOUBAO_IMAGE_MODEL";

const ENV_VIDEO_API_KEY: &str = "DOUBAO_VIDEO_API_KEY";
const ENV_VIDEO_API_BASE: &str = "DOUBAO_VIDEO_API_BASE";
const ENV_VIDEO_MODEL: &str = "DOUBAO_VIDEO_MODEL";

impl ImageFormat {
    /// The wire token Ark expects in `response_format`.
    fn as_wire(self) -> &'static str {
        match self {
            ImageFormat::Url => "url",
            ImageFormat::B64Json => "b64_json",
        }
    }
}

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

// --- Image -----------------------------------------------------------------

pub struct DoubaoImageGen {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    model: String,
}

impl DoubaoImageGen {
    /// Resolve config from the environment. `DOUBAO_IMAGE_API_KEY` is required;
    /// base URL and model fall back to the plan endpoint and seedream default.
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var(ENV_IMAGE_API_KEY).map_err(|_| {
            anyhow::anyhow!("{ENV_IMAGE_API_KEY} is required when IMAGE_GEN_PROVIDER=doubao")
        })?;
        let api_base =
            std::env::var(ENV_IMAGE_API_BASE).unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        let model =
            std::env::var(ENV_IMAGE_MODEL).unwrap_or_else(|_| DEFAULT_IMAGE_MODEL.to_string());
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building doubao image-gen HTTP client")?;
        Ok(Self { client, api_key, api_base, model })
    }

    /// Build the `images/generations` body. Pure (no I/O) so the wire shape is
    /// unit-testable without a network call. Optional knobs are omitted when
    /// unset so the model applies its own defaults.
    fn build_request(&self, req: &ImageRequest) -> Value {
        let mut body = json!({
            "model": self.model,
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
}

#[async_trait]
impl ImageGen for DoubaoImageGen {
    async fn generate(&self, req: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let body = self.build_request(req);
        let url = format!("{}/images/generations", self.api_base.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
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

// --- Video -----------------------------------------------------------------

pub struct DoubaoVideoGen {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    model: String,
}

impl DoubaoVideoGen {
    /// Resolve config from the environment. `DOUBAO_VIDEO_API_KEY` is required;
    /// base URL and model fall back to the plan endpoint and seedance default.
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var(ENV_VIDEO_API_KEY).map_err(|_| {
            anyhow::anyhow!("{ENV_VIDEO_API_KEY} is required when VIDEO_GEN_PROVIDER=doubao")
        })?;
        let api_base =
            std::env::var(ENV_VIDEO_API_BASE).unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        let model =
            std::env::var(ENV_VIDEO_MODEL).unwrap_or_else(|_| DEFAULT_VIDEO_MODEL.to_string());
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building doubao video-gen HTTP client")?;
        Ok(Self { client, api_key, api_base, model })
    }

    /// Build the create-task body. Pure (no I/O) so the wire shape is
    /// unit-testable. The prompt is the first `content` part; a first-frame
    /// reference (image-to-video) is appended as an `image_url` part. Generation
    /// knobs ride as sibling fields, omitted when unset.
    fn build_create_request(&self, req: &VideoRequest) -> Value {
        let mut content = vec![json!({ "type": "text", "text": req.prompt })];
        if let Some(frame) = &req.first_frame {
            content.push(json!({
                "type": "image_url",
                "image_url": { "url": frame.to_url() },
                "role": "first_frame",
            }));
        }

        let mut body = json!({ "model": self.model, "content": content });
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

    fn tasks_url(&self) -> String {
        format!("{}/contents/generations/tasks", self.api_base.trim_end_matches('/'))
    }
}

#[async_trait]
impl VideoGen for DoubaoVideoGen {
    async fn submit(&self, req: &VideoRequest) -> anyhow::Result<String> {
        let body = self.build_create_request(req);

        let resp = self
            .client
            .post(self.tasks_url())
            .bearer_auth(&self.api_key)
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

    async fn poll(&self, task_id: &str) -> anyhow::Result<VideoTask> {
        let url = format!("{}/{task_id}", self.tasks_url());

        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
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

    fn image_gen() -> DoubaoImageGen {
        DoubaoImageGen {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            model: DEFAULT_IMAGE_MODEL.to_string(),
        }
    }

    fn video_gen() -> DoubaoVideoGen {
        DoubaoVideoGen {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            api_base: DEFAULT_API_BASE.to_string(),
            model: DEFAULT_VIDEO_MODEL.to_string(),
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
        let body = image_gen().build_request(&ImageRequest::new("a red bicycle"));
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
        let body = image_gen().build_request(&req);
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

    #[test]
    fn video_create_text_to_video_omits_unset_knobs() {
        let body = video_gen().build_create_request(&VideoRequest::new("a cat running"));
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
        let body = video_gen().build_create_request(&req);

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
        let body = video_gen().build_create_request(&req);
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
