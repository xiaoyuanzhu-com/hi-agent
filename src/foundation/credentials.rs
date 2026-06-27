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

/// How the agent obtains its credentials.
/// - `byok`: the user's own keys (the flat fields below), the default.
/// - `login` / `free`: a bundle fetched from the broker (hi.xiaoyuanzhu.com),
///   cached in [`Credentials::managed`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Byok,
    Login,
    Free,
}

/// The user's credentials (BYOK) plus, for login/free, the broker-minted bundle.
/// The flat `llm`/vendor fields are the BYOK keys; `managed` caches the broker
/// bundle. [`Credentials::effective`] picks which set is live for the current mode.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// Which credential source is live. Default `byok`.
    pub mode: Mode,
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
    /// Stable per-install id sent to the broker as the free-mode allowance key.
    /// Minted on first need (see the broker client); not a secret.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    /// The last bundle the broker minted (login/free). Refreshed before `expires_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<Managed>,
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

/// A broker-minted bundle (login/free): the same credential fields as BYOK plus
/// the account snapshot. Cached in the store; refreshed from the broker.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Managed {
    pub llm: LlmCredentials,
    pub stt: VendorKey,
    pub tts: VendorKey,
    pub vision: VendorKey,
    pub image: VendorKey,
    pub video: VendorKey,
    /// Plan name from the broker (e.g. "free", "pro").
    pub plan: String,
    pub credits_remaining: i64,
    pub credits_limit: i64,
    pub credits_resets_at: String,
    /// RFC3339; after this the bundle should be refreshed (refresh timer is future
    /// work — v1 refetches on each startup).
    pub expires_at: String,
}

impl std::fmt::Debug for Managed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Managed")
            .field("llm", &self.llm)
            .field("plan", &self.plan)
            .field("credits_remaining", &self.credits_remaining)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// The credentials in effect for the current mode — borrows from either the BYOK
/// fields or the managed bundle.
pub struct Effective<'a> {
    pub llm: &'a LlmCredentials,
    pub stt: &'a VendorKey,
    pub tts: &'a VendorKey,
    pub vision: &'a VendorKey,
    pub image: &'a VendorKey,
    pub video: &'a VendorKey,
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
            .field("mode", &self.mode)
            .field("llm", &self.llm)
            .field("stt", &self.stt)
            .field("tts", &self.tts)
            .field("vision", &self.vision)
            .field("image", &self.image)
            .field("video", &self.video)
            .field("device_id", &self.device_id)
            .field("managed", &self.managed)
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
    /// The credentials in effect: the BYOK fields in `byok` mode, the managed
    /// bundle in login/free. `None` in login/free before a bundle has been
    /// fetched — callers then fall back to `.env` (resolve) or leave the
    /// capability off.
    pub fn effective(&self) -> Option<Effective<'_>> {
        match self.mode {
            Mode::Byok => Some(Effective {
                llm: &self.llm,
                stt: &self.stt,
                tts: &self.tts,
                vision: &self.vision,
                image: &self.image,
                video: &self.video,
            }),
            Mode::Login | Mode::Free => self.managed.as_ref().map(|m| Effective {
                llm: &m.llm,
                stt: &m.stt,
                tts: &m.tts,
                vision: &m.vision,
                image: &m.image,
                video: &m.video,
            }),
        }
    }

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

    #[test]
    fn effective_picks_byok_or_managed() {
        let mut c = Credentials::default();
        assert_eq!(c.mode, Mode::Byok);
        c.llm.api_key = "byok-key".into();
        assert_eq!(c.effective().unwrap().llm.api_key, "byok-key");

        // login/free with no bundle → nothing in effect (callers fall back to env).
        c.mode = Mode::Free;
        assert!(c.effective().is_none());

        // a bundle takes over.
        c.managed = Some(Managed {
            llm: LlmCredentials {
                base_url: "https://songguo.xiaoyuanzhu.com".into(),
                api_key: "managed-key".into(),
                model: None,
            },
            stt: VendorKey { api_key: "managed-stt".into() },
            plan: "free".into(),
            ..Default::default()
        });
        let e = c.effective().unwrap();
        assert_eq!(e.llm.api_key, "managed-key");
        assert_eq!(e.llm.base_url, "https://songguo.xiaoyuanzhu.com");
        assert_eq!(e.stt.key_opt(), Some("managed-stt"));
        // BYOK key is ignored while a managed bundle is live.
        assert_ne!(e.llm.api_key, "byok-key");
    }

    #[test]
    fn mode_and_device_id_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials { mode: Mode::Login, device_id: "dev-1".into(), ..Default::default() };
        c.save(dir.path()).unwrap();
        let back = Credentials::load(dir.path());
        assert_eq!(back.mode, Mode::Login);
        assert_eq!(back.device_id, "dev-1");
    }

    #[test]
    fn managed_debug_redacts_keys() {
        let m = Managed {
            llm: LlmCredentials { base_url: "x".into(), api_key: "managed-super-secret".into(), model: None },
            plan: "pro".into(),
            ..Default::default()
        };
        let rendered = format!("{m:?}");
        assert!(!rendered.contains("managed-super-secret"), "key leaked: {rendered}");
    }
}
