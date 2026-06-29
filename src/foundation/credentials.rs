//! Credential store: the user's BYOK keys, or (free/login) the broker-issued
//! account tokens plus the configs the broker hands back. Persisted under the
//! data dir as `credentials.json`, resolved at startup, refreshed by the broker
//! client. When a managed key is unset the agent falls back to `.env`
//! (`AI_API_KEY`, `VOLCENGINE_*`, `DOUBAO_*`, …) so dev / journey-test flows keep
//! working. A vendor key in effect also implies that vendor is the provider for
//! its capability.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// File under the data dir holding the credential store.
const FILE: &str = "credentials.json";

/// Absolute path to the credential store for `data_dir`.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// Env flag that overrides the stored credential mode — handy for flipping
/// free/byok in testing without the Settings UI or editing the file. When set it
/// wins over the stored mode; unset → the stored mode (default free).
const ENV_MODE: &str = "HI_AGENT_MODE";

/// Parse a mode string, case-insensitive (`byok` | `login` | `free`). Unknown → None.
fn parse_mode(s: &str) -> Option<Mode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "byok" => Some(Mode::Byok),
        "login" => Some(Mode::Login),
        "free" => Some(Mode::Free),
        _ => None,
    }
}

/// The mode forced by `HI_AGENT_MODE`, if set to a recognized value.
fn mode_override() -> Option<Mode> {
    let v = std::env::var(ENV_MODE).ok()?;
    let m = parse_mode(&v);
    if m.is_none() && !v.trim().is_empty() {
        tracing::warn!(value = %v, "ignoring unknown HI_AGENT_MODE (expected byok|login|free)");
    }
    m
}

/// How the agent obtains its credentials.
/// - `free`: an anonymous device account, auto-created at the broker — the
///   default, so a fresh install works with no setup.
/// - `login`: an account.xiaoyuanzhu.com user (where a subscription lives).
/// - `byok`: the user's own keys (the flat fields below).
///
/// `free`/`login` go through a one-time **bootstrap** that yields account
/// [`Tokens`]; the access token then authenticates the configs + energy fetches.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Byok,
    Login,
    #[default]
    Free,
}

/// The user's credentials (BYOK) plus, for free/login, the broker account tokens
/// and the configs/energy the broker minted. [`Credentials::effective`] picks
/// which credential set is live for the current mode.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// Which credential source is live. Default `free`.
    pub mode: Mode,
    pub llm: LlmCredentials,
    pub stt: VendorKey,
    pub tts: VendorKey,
    pub vision: VendorKey,
    pub image: VendorKey,
    pub video: VendorKey,
    /// Stable per-install id — the seed for the free bootstrap (not a secret).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    /// Broker-issued account tokens (free/login). The unified credential after
    /// bootstrap: the access token authenticates configs + energy; the refresh
    /// token mints a new access when it expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// Last configs the broker minted (free/login) — the vendor settings applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed: Option<Managed>,
    /// Last energy snapshot, for the Settings bar. Polled on its own cadence,
    /// separate from configs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy: Option<Energy>,
}

/// Broker-issued account tokens. The access token is a short-lived bearer for
/// configs/energy; the refresh token mints new access tokens (and is rotated each
/// refresh, so the newest must always be persisted).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// RFC3339 access-token expiry; refresh at or before this.
    pub access_expires_at: String,
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

/// A single-vendor secret — just the API key (the BYOK essential).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VendorKey {
    pub api_key: String,
}

impl VendorKey {
    /// The trimmed key if non-empty, else `None` — the "use my key / fall back to
    /// env" signal threaded into each capability's init.
    pub fn key_opt(&self) -> Option<&str> {
        let k = self.api_key.trim();
        if k.is_empty() { None } else { Some(k) }
    }
}

/// Broker-minted configs (free/login): the same credential fields as BYOK. The
/// account/energy snapshot is separate ([`Energy`]) so it can be polled often
/// without re-fetching configs.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Managed {
    pub llm: LlmCredentials,
    pub stt: VendorKey,
    pub tts: VendorKey,
    pub vision: VendorKey,
    pub image: VendorKey,
    pub video: VendorKey,
}

/// The user-facing balance from `/energy` (free/login). Cached for display; the
/// live value is metered at the gateway. `unit` is always "energy".
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Energy {
    pub remaining: i64,
    pub total: i64,
    pub resets_at: String,
    /// Tier the broker reports: "free" or "sub".
    pub tier: String,
}

/// The credentials in effect for the current mode — borrows from either the BYOK
/// fields or the managed configs.
pub struct Effective<'a> {
    pub llm: &'a LlmCredentials,
    pub stt: &'a VendorKey,
    pub tts: &'a VendorKey,
    pub vision: &'a VendorKey,
    pub image: &'a VendorKey,
    pub video: &'a VendorKey,
}

impl Credentials {
    /// The credentials in effect: BYOK fields in `byok` mode, the managed configs
    /// in free/login. `None` in free/login before configs have been fetched —
    /// callers then fall back to `.env` (resolve) or leave the capability off.
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
    /// corrupt one logs a warning and also yields defaults, so a bad hand-edit
    /// can't brick boot — the user re-saves from Settings.
    pub fn load(data_dir: &Path) -> Self {
        let p = path(data_dir);
        let mut c = match std::fs::read(&p) {
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
        };
        // An explicit HI_AGENT_MODE wins over the stored mode (testing override).
        if let Some(m) = mode_override() {
            c.mode = m;
        }
        c
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

fn redact(s: &str) -> &'static str {
    if s.trim().is_empty() { "<unset>" } else { "<redacted>" }
}

// Hand-written Debug impls so a stray trace never prints a secret.
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

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &redact(&self.access_token))
            .field("refresh_token", &redact(&self.refresh_token))
            .field("access_expires_at", &self.access_expires_at)
            .finish()
    }
}

impl std::fmt::Debug for Managed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Managed").field("llm", &self.llm).finish_non_exhaustive()
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
            .field("tokens", &self.tokens)
            .field("managed", &self.managed)
            .field("energy", &self.energy)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_is_case_insensitive() {
        assert_eq!(parse_mode("byok"), Some(Mode::Byok));
        assert_eq!(parse_mode("FREE"), Some(Mode::Free));
        assert_eq!(parse_mode(" login "), Some(Mode::Login));
        assert_eq!(parse_mode("nope"), None);
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials::load(dir.path());
        assert_eq!(c.mode, Mode::Free);
        assert!(c.llm.api_key.is_empty());
        assert!(c.tokens.is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials {
            mode: Mode::Free,
            device_id: "dev-1".into(),
            tokens: Some(Tokens {
                access_token: "acc".into(),
                refresh_token: "ref".into(),
                access_expires_at: "2026-06-29T00:00:00Z".into(),
            }),
            managed: Some(Managed {
                llm: LlmCredentials {
                    base_url: "https://songguo.xiaoyuanzhu.com".into(),
                    api_key: "sg-secret".into(),
                    model: None,
                },
                stt: VendorKey { api_key: "sg-secret".into() },
                ..Default::default()
            }),
            energy: Some(Energy { remaining: 70, total: 100, resets_at: "x".into(), tier: "free".into() }),
            ..Default::default()
        };
        c.save(dir.path()).unwrap();
        let back = Credentials::load(dir.path());
        assert_eq!(back.device_id, "dev-1");
        assert_eq!(back.tokens.as_ref().unwrap().access_token, "acc");
        assert_eq!(back.managed.as_ref().unwrap().llm.base_url, "https://songguo.xiaoyuanzhu.com");
        assert_eq!(back.energy.as_ref().unwrap().remaining, 70);
    }

    #[test]
    fn effective_picks_byok_or_managed() {
        let mut c = Credentials::default();
        assert_eq!(c.mode, Mode::Free); // free is the default

        c.mode = Mode::Byok;
        c.llm.api_key = "byok-key".into();
        assert_eq!(c.effective().unwrap().llm.api_key, "byok-key");

        // free with no configs → nothing in effect (callers fall back to env).
        c.mode = Mode::Free;
        assert!(c.effective().is_none());

        c.managed = Some(Managed {
            llm: LlmCredentials {
                base_url: "https://songguo.xiaoyuanzhu.com".into(),
                api_key: "managed-key".into(),
                model: None,
            },
            stt: VendorKey { api_key: "managed-stt".into() },
            ..Default::default()
        });
        let e = c.effective().unwrap();
        assert_eq!(e.llm.api_key, "managed-key");
        assert_eq!(e.stt.key_opt(), Some("managed-stt"));
        assert_ne!(e.llm.api_key, "byok-key"); // BYOK ignored while managed is live
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = Credentials {
            llm: LlmCredentials { base_url: "https://x".into(), api_key: "sk-super-secret".into(), model: None },
            vision: VendorKey { api_key: "vision-super-secret".into() },
            tokens: Some(Tokens {
                access_token: "access-super-secret".into(),
                refresh_token: "refresh-super-secret".into(),
                access_expires_at: "x".into(),
            }),
            ..Default::default()
        };
        let rendered = format!("{c:?}");
        for leak in ["sk-super-secret", "vision-super-secret", "access-super-secret", "refresh-super-secret"] {
            assert!(!rendered.contains(leak), "leaked {leak}: {rendered}");
        }
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
