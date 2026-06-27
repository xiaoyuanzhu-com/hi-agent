//! BYOK credential store: the user's own vendor keys, persisted under the data
//! dir as `credentials.json` and resolved at startup. When a key is unset, the
//! agent falls back to `.env` (`AI_API_KEY`, `VOLCENGINE_*`, `DOUBAO_*`, …) so
//! dev / journey-test flows keep working. A vendor key in the store also implies
//! that vendor is the selected provider for its capability.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// File under the data dir holding the user's vendor credentials.
const FILE: &str = "credentials.json";

/// Absolute path to the credential store for `data_dir`.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// The user's own vendor credentials (BYOK). `llm` is the upstream Claude
/// adapter; the rest are the keyed capability vendors (each defaults to the env
/// when its key is unset).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub llm: LlmCredentials,
    /// Speech-to-text (Volcengine).
    pub stt: VendorKey,
    /// Text-to-speech (Volcengine).
    pub tts: VendorKey,
    /// Image + video understanding (Doubao).
    pub vision: VendorKey,
    /// Image generation (Doubao).
    pub image: VendorKey,
    /// Video generation (Doubao).
    pub video: VendorKey,
}

/// Upstream LLM credentials (the bundled Claude adapter's `ANTHROPIC_*`).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmCredentials {
    /// Upstream base URL; empty → the bundled Anthropic default.
    pub base_url: String,
    /// Upstream API key; empty → not configured (falls back to `.env`).
    pub api_key: String,
    /// Model override (`ANTHROPIC_MODEL`); `None` → the adapter's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// A single-vendor secret — just the API key (the BYOK essential). Non-secret
/// params (endpoints, resource ids, models, voices) stay on their env defaults.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VendorKey {
    /// API key; empty → not configured (falls back to that vendor's env key).
    pub api_key: String,
}

impl VendorKey {
    /// The trimmed key if non-empty, else `None` — the "use my key / fall back to
    /// env" signal threaded into each capability's init.
    pub fn key_opt(&self) -> Option<&str> {
        let k = self.api_key.trim();
        if k.is_empty() {
            None
        } else {
            Some(k)
        }
    }
}

// Hand-written so a stray trace never prints the key.
impl std::fmt::Debug for LlmCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCredentials")
            .field("base_url", &self.base_url)
            .field("api_key", &redact(&self.api_key))
            .field("model", &self.model)
            .finish()
    }
}

impl std::fmt::Debug for VendorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VendorKey").field("api_key", &redact(&self.api_key)).finish()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("llm", &self.llm)
            .field("stt", &self.stt)
            .field("tts", &self.tts)
            .field("vision", &self.vision)
            .field("image", &self.image)
            .field("video", &self.video)
            .finish()
    }
}

fn redact(s: &str) -> &'static str {
    if s.trim().is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

impl Credentials {
    /// Load from `<data_dir>/credentials.json`. A missing file yields defaults; a
    /// corrupt one logs a warning and *also* yields defaults, so a bad hand-edit
    /// can't brick boot — the user re-saves from Settings.
    pub fn load(data_dir: &Path) -> Self {
        let p = path(data_dir);
        match std::fs::read(&p) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(
                    path = %p.display(), error = %e,
                    "credentials.json is unreadable; ignoring (re-save from Settings)"
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "could not read credentials.json; ignoring");
                Self::default()
            }
        }
    }

    /// Persist to `<data_dir>/credentials.json`, owner-only (`0600` on unix).
    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let p = path(data_dir);
        let body = serde_json::to_vec_pretty(self).context("serializing credentials")?;
        std::fs::write(&p, &body).with_context(|| format!("writing {}", p.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 {}", p.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials::load(dir.path());
        assert!(c.llm.api_key.is_empty());
        assert!(c.llm.base_url.is_empty());
        assert!(c.llm.model.is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials {
            llm: LlmCredentials {
                base_url: "https://gw.example/v1".into(),
                api_key: "sk-secret".into(),
                model: Some("claude-opus-4-8".into()),
            },
            stt: VendorKey { api_key: "stt-key".into() },
            ..Default::default()
        };
        c.save(dir.path()).unwrap();
        let back = Credentials::load(dir.path());
        assert_eq!(back.llm.base_url, "https://gw.example/v1");
        assert_eq!(back.llm.api_key, "sk-secret");
        assert_eq!(back.llm.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(back.stt.api_key, "stt-key");
        assert_eq!(back.stt.key_opt(), Some("stt-key"));
        assert_eq!(back.tts.key_opt(), None);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), b"not json{{").unwrap();
        let c = Credentials::load(dir.path());
        assert!(c.llm.api_key.is_empty());
    }

    #[test]
    fn debug_redacts_the_key() {
        let c = Credentials {
            llm: LlmCredentials {
                base_url: "https://x".into(),
                api_key: "sk-super-secret".into(),
                model: None,
            },
            vision: VendorKey { api_key: "vision-super-secret".into() },
            ..Default::default()
        };
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("sk-super-secret"), "key leaked: {rendered}");
        assert!(!rendered.contains("vision-super-secret"), "vendor key leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Credentials::default().save(dir.path()).unwrap();
        let mode = std::fs::metadata(path(dir.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
