//! Volcengine BigModel STT — record-file recognition (大模型录音文件识别).
//!
//! Wire protocol per <https://www.volcengine.com/docs/6561/1354869>:
//!
//! 1. `POST https://openspeech.bytedance.com/api/v1/auc_bigmodel/submit`
//!    with `Authorization: Bearer; <access_token>` and a JSON body carrying
//!    `app.appid`, `app.token`, `app.cluster`, `audio.data` (base64) or
//!    `audio.url`, plus a `request.model_name` selecting the bigmodel.
//!    Response: `{"resp": {"id": "<task-id>", ...}}`.
//! 2. Poll `POST /api/v1/auc_bigmodel/query` with the task id until the
//!    response includes `text` (or an error code).
//!
//! v0 keeps the client minimal: synchronous polling, 1 s interval, 60 s
//! ceiling. Streaming and callback variants are deferred.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::stt::Stt;

const SUBMIT_URL: &str = "https://openspeech.bytedance.com/api/v1/auc_bigmodel/submit";
const QUERY_URL: &str = "https://openspeech.bytedance.com/api/v1/auc_bigmodel/query";
const POLL_INTERVAL: Duration = Duration::from_millis(1000);
const POLL_TIMEOUT: Duration = Duration::from_secs(60);

const ENV_APPID: &str = "VOLCENGINE_STT_APPID";
const ENV_TOKEN: &str = "VOLCENGINE_STT_ACCESS_TOKEN";
const ENV_CLUSTER: &str = "VOLCENGINE_STT_CLUSTER";
const ENV_MODEL: &str = "VOLCENGINE_STT_MODEL";

const DEFAULT_CLUSTER: &str = "volcengine_input_common";
const DEFAULT_MODEL: &str = "bigmodel";

pub struct VolcengineStt {
    appid: String,
    access_token: String,
    cluster: String,
    model: String,
    http: reqwest::Client,
}

impl VolcengineStt {
    pub fn from_env() -> anyhow::Result<Self> {
        let appid = std::env::var(ENV_APPID)
            .map_err(|_| anyhow::anyhow!("{ENV_APPID} is required when STT_PROVIDER=volcengine"))?;
        let access_token = std::env::var(ENV_TOKEN).map_err(|_| {
            anyhow::anyhow!("{ENV_TOKEN} is required when STT_PROVIDER=volcengine")
        })?;
        let cluster = std::env::var(ENV_CLUSTER).unwrap_or_else(|_| DEFAULT_CLUSTER.to_string());
        let model = std::env::var(ENV_MODEL).unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(Self {
            appid,
            access_token,
            cluster,
            model,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    fn auth_header(&self) -> String {
        // Volcengine speech APIs use `Bearer; <token>` (note the semicolon).
        format!("Bearer; {}", self.access_token)
    }
}

#[async_trait]
impl Stt for VolcengineStt {
    async fn transcribe(&self, audio: Bytes, mime: &str) -> anyhow::Result<String> {
        let format = mime_to_format(mime);
        let audio_b64 = base64::engine::general_purpose::STANDARD.encode(&audio);

        let submit_body = json!({
            "app": {
                "appid": self.appid,
                "token": self.access_token,
                "cluster": self.cluster,
            },
            "user": { "uid": "hi-agent" },
            "audio": {
                "format": format,
                "data": audio_b64,
            },
            "request": {
                "model_name": self.model,
                "enable_itn": true,
                "enable_punc": true,
            },
        });

        let submit: SubmitResponse = self
            .http
            .post(SUBMIT_URL)
            .header("Authorization", self.auth_header())
            .json(&submit_body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let task_id = submit
            .resp
            .as_ref()
            .and_then(|r| r.id.clone())
            .ok_or_else(|| anyhow::anyhow!("volcengine STT submit returned no task id: {submit:?}"))?;

        let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            let query_body = json!({
                "appid": self.appid,
                "token": self.access_token,
                "cluster": self.cluster,
                "id": task_id,
            });

            let query: QueryResponse = self
                .http
                .post(QUERY_URL)
                .header("Authorization", self.auth_header())
                .json(&query_body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let resp = query
                .resp
                .ok_or_else(|| anyhow::anyhow!("volcengine STT query missing resp field"))?;

            // Volcengine convention: code 1000 = completed, 1001/1002 = running.
            // Codes outside that set are failures.
            match resp.code {
                Some(1000) => {
                    let text = resp.text.unwrap_or_default();
                    if text.is_empty() {
                        anyhow::bail!("volcengine STT returned empty transcript");
                    }
                    return Ok(text);
                }
                Some(1001) | Some(1002) => {
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!("volcengine STT polling timed out after {POLL_TIMEOUT:?}");
                    }
                }
                Some(code) => {
                    anyhow::bail!(
                        "volcengine STT failed (code={code}, message={:?})",
                        resp.message
                    );
                }
                None => {
                    anyhow::bail!("volcengine STT query response missing code: {:?}", resp);
                }
            }
        }
    }
}

fn mime_to_format(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" => "flac",
        "audio/aac" | "audio/x-aac" => "aac",
        "audio/m4a" | "audio/x-m4a" | "audio/mp4" => "m4a",
        _ => "wav",
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SubmitResponse {
    #[serde(default)]
    resp: Option<SubmitResp>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SubmitResp {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct QueryResponse {
    #[serde(default)]
    resp: Option<QueryResp>,
}

#[derive(Debug, Deserialize, Serialize)]
struct QueryResp {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    text: Option<String>,
}
