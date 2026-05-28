//! Volcengine BigModel TTS — synchronous HTTP TTS (语音合成 大模型).
//!
//! Wire protocol per <https://www.volcengine.com/docs/6561/1329505>:
//!
//! - `POST https://openspeech.bytedance.com/api/v1/tts` with
//!   `Authorization: Bearer; <access_token>` and a JSON body shaped as
//!   `{ app, user, audio, request }`. `audio.encoding` selects the container
//!   (`mp3` | `wav` | `ogg_opus`); `audio.voice_type` selects the timbre.
//! - Response is JSON; the synthesized audio is base64-encoded in the `data`
//!   field. We decode and hand back as a typed `AudioBlob`.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::tts::{AudioBlob, Tts};

const TTS_URL: &str = "https://openspeech.bytedance.com/api/v1/tts";

const ENV_APPID: &str = "VOLCENGINE_TTS_APPID";
const ENV_TOKEN: &str = "VOLCENGINE_TTS_ACCESS_TOKEN";
const ENV_CLUSTER: &str = "VOLCENGINE_TTS_CLUSTER";
const ENV_VOICE: &str = "VOLCENGINE_TTS_VOICE";
const ENV_ENCODING: &str = "VOLCENGINE_TTS_ENCODING";

const DEFAULT_CLUSTER: &str = "volcano_tts";
const DEFAULT_VOICE: &str = "zh_female_qingxin";
const DEFAULT_ENCODING: &str = "mp3";

pub struct VolcengineTts {
    appid: String,
    access_token: String,
    cluster: String,
    voice_type: String,
    encoding: String,
    http: reqwest::Client,
}

impl VolcengineTts {
    pub fn from_env() -> anyhow::Result<Self> {
        let appid = std::env::var(ENV_APPID)
            .map_err(|_| anyhow::anyhow!("{ENV_APPID} is required when TTS_PROVIDER=volcengine"))?;
        let access_token = std::env::var(ENV_TOKEN).map_err(|_| {
            anyhow::anyhow!("{ENV_TOKEN} is required when TTS_PROVIDER=volcengine")
        })?;
        let cluster = std::env::var(ENV_CLUSTER).unwrap_or_else(|_| DEFAULT_CLUSTER.to_string());
        let voice_type = std::env::var(ENV_VOICE).unwrap_or_else(|_| DEFAULT_VOICE.to_string());
        let encoding =
            std::env::var(ENV_ENCODING).unwrap_or_else(|_| DEFAULT_ENCODING.to_string());
        Ok(Self {
            appid,
            access_token,
            cluster,
            voice_type,
            encoding,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    fn auth_header(&self) -> String {
        format!("Bearer; {}", self.access_token)
    }
}

#[async_trait]
impl Tts for VolcengineTts {
    async fn synthesize(&self, text: &str) -> anyhow::Result<AudioBlob> {
        let req_id = Uuid::now_v7().to_string();
        let body = json!({
            "app": {
                "appid": self.appid,
                "token": self.access_token,
                "cluster": self.cluster,
            },
            "user": { "uid": "hi-agent" },
            "audio": {
                "voice_type": self.voice_type,
                "encoding": self.encoding,
                "speed_ratio": 1.0,
                "volume_ratio": 1.0,
                "pitch_ratio": 1.0,
            },
            "request": {
                "reqid": req_id,
                "text": text,
                "operation": "query",
            },
        });

        let resp: TtsResponse = self
            .http
            .post(TTS_URL)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if let Some(code) = resp.code {
            // Volcengine convention: 3000 = success on TTS bigmodel.
            if code != 3000 {
                anyhow::bail!(
                    "volcengine TTS failed (code={code}, message={:?})",
                    resp.message
                );
            }
        }
        let data_b64 = resp
            .data
            .ok_or_else(|| anyhow::anyhow!("volcengine TTS response missing data field"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data_b64)
            .map_err(|e| anyhow::anyhow!("decoding TTS audio: {e}"))?;
        Ok(AudioBlob {
            bytes: Bytes::from(bytes),
            mime: encoding_to_mime(&self.encoding).to_string(),
            ext: encoding_to_ext(&self.encoding),
        })
    }
}

fn encoding_to_mime(enc: &str) -> &'static str {
    match enc {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg_opus" | "ogg" => "audio/ogg",
        "pcm" => "audio/L16",
        _ => "application/octet-stream",
    }
}

fn encoding_to_ext(enc: &str) -> &'static str {
    match enc {
        "mp3" => "mp3",
        "wav" => "wav",
        "ogg_opus" | "ogg" => "ogg",
        "pcm" => "pcm",
        _ => "bin",
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct TtsResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    /// Base64-encoded audio payload.
    #[serde(default)]
    data: Option<String>,
}
