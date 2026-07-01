//! Credential store: the user's BYOK keys, or (xiaoyuanzhu) the broker-issued
//! account tokens plus the configs the broker hands back. Persisted under the
//! data dir as `config.db` (SQLite; see the [`db`] submodule), resolved at
//! startup, refreshed by the broker client. When a managed key is unset the agent
//! falls back to `.env` (`AI_API_KEY`, `VOLCENGINE_*`, `DOUBAO_*`, …) so dev /
//! journey-test flows keep working. A vendor key in effect also implies that
//! vendor is the provider for its capability.
//!
//! Both modes' configs are stored side by side (one row per `(mode, feature)`),
//! so switching mode in Settings surfaces whatever was last entered for it. The
//! in-memory [`Credentials`] shape is unchanged — only persistence moved from a
//! JSON file to SQLite; a legacy `credentials.json` is imported once on first
//! load (see [`db::load`]).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// File under the data dir holding the credential store (SQLite).
const FILE: &str = "config.db";

/// Absolute path to the credential store for `data_dir`.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// Env flag that overrides the stored credential mode — handy for flipping
/// xiaoyuanzhu/byok in testing without the Settings UI or editing the file. When
/// set it wins over the stored mode; unset → the stored mode (default xiaoyuanzhu).
const ENV_MODE: &str = "HI_AGENT_MODE";

/// Parse a mode string, case-insensitive (`byok` | `xiaoyuanzhu`). The legacy
/// values `free`/`login` map to `xiaoyuanzhu` (the mode that absorbed both).
/// Unknown → None.
fn parse_mode(s: &str) -> Option<Mode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "byok" => Some(Mode::Byok),
        "xiaoyuanzhu" | "free" | "login" => Some(Mode::Xiaoyuanzhu),
        _ => None,
    }
}

/// The mode forced by `HI_AGENT_MODE`, if set to a recognized value.
fn mode_override() -> Option<Mode> {
    let v = std::env::var(ENV_MODE).ok()?;
    let m = parse_mode(&v);
    if m.is_none() && !v.trim().is_empty() {
        tracing::warn!(value = %v, "ignoring unknown HI_AGENT_MODE (expected byok|xiaoyuanzhu)");
    }
    m
}

/// How the agent obtains its credentials.
/// - `xiaoyuanzhu`: a broker account (`hi.xiaoyuanzhu.com`) — the default, so a
///   fresh install works with no setup. Anonymous device bootstrap yields the
///   `free` tier; a signed-in account.xiaoyuanzhu.com session yields `sub`.
/// - `byok`: the user's own keys (the flat fields below).
///
/// `xiaoyuanzhu` goes through a one-time **bootstrap** that yields account
/// [`Tokens`]; the access token then authenticates the configs + energy fetches.
///
/// The legacy `free`/`login` values deserialize to `Xiaoyuanzhu` (they were split
/// modes that collapsed into it), so an older `credentials.json` loads unchanged.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Byok,
    #[default]
    #[serde(alias = "free", alias = "login")]
    Xiaoyuanzhu,
}

/// The user's credentials (BYOK) plus, for xiaoyuanzhu, the broker account tokens
/// and the configs/energy the broker minted. [`Credentials::effective`] picks
/// which credential set is live for the current mode.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// Which credential source is live. Default `xiaoyuanzhu`.
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
    /// Broker-issued account tokens (xiaoyuanzhu). The unified credential after
    /// bootstrap: the access token authenticates configs + energy; the refresh
    /// token mints a new access when it expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// Last configs the broker minted (xiaoyuanzhu) — the vendor settings applied.
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

/// A single-vendor config. In BYOK only `api_key` is set (other params stay on
/// env defaults); in managed mode the broker also fills `base_url` (songguo) and
/// may fill `model`, and the vendor host-rebases its native endpoint onto songguo.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VendorKey {
    /// Gateway base; empty → the vendor's own default endpoint.
    pub base_url: String,
    pub api_key: String,
    /// Model override; None → the vendor's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl VendorKey {
    /// The trimmed key if non-empty, else `None` — the "use my key / fall back to
    /// env" signal threaded into each capability's init.
    pub fn key_opt(&self) -> Option<&str> {
        let k = self.api_key.trim();
        if k.is_empty() { None } else { Some(k) }
    }

    /// The managed gateway base if set, else `None` (use the vendor's default).
    pub fn base_url_opt(&self) -> Option<&str> {
        let b = self.base_url.trim();
        if b.is_empty() { None } else { Some(b) }
    }

    /// The managed model override if set, else `None`.
    pub fn model_opt(&self) -> Option<&str> {
        self.model.as_deref().map(str::trim).filter(|m| !m.is_empty())
    }
}

/// Broker-minted configs (xiaoyuanzhu): the same credential fields as BYOK. The
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

/// The user-facing balance from `/energy` (xiaoyuanzhu). Cached for display; the
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
    /// in xiaoyuanzhu. `None` in xiaoyuanzhu before configs have been fetched —
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
            Mode::Xiaoyuanzhu => self.managed.as_ref().map(|m| Effective {
                llm: &m.llm,
                stt: &m.stt,
                tts: &m.tts,
                vision: &m.vision,
                image: &m.image,
                video: &m.video,
            }),
        }
    }

    /// Load from `<data_dir>/config.db`. A missing DB yields defaults; any read
    /// error logs a warning and also yields defaults, so a corrupt store can't
    /// brick boot — the user re-saves from Settings. On first load a legacy
    /// `credentials.json` is imported into the DB (see [`db::load`]).
    pub fn load(data_dir: &Path) -> Self {
        let mut c = db::load(data_dir).unwrap_or_else(|e| {
            tracing::warn!(
                path = %path(data_dir).display(), error = %e,
                "config store unreadable; using defaults (re-save from Settings)"
            );
            Self::default()
        });
        // An explicit HI_AGENT_MODE wins over the stored mode (testing override).
        if let Some(m) = mode_override() {
            c.mode = m;
        }
        c
    }

    /// Persist to `<data_dir>/config.db`, owner-only (`0600` on unix). Writes both
    /// modes' rows so a later mode switch surfaces the stored config for it.
    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        db::save(data_dir, self)
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
        f.debug_struct("VendorKey")
            .field("base_url", &self.base_url)
            .field("api_key", &redact(&self.api_key))
            .field("model", &self.model)
            .finish()
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

/// SQLite persistence for the credential store. The on-disk shape is normalized:
/// scalar flags in `app_settings`, one `credential` row per `(mode, feature)` (so
/// both modes coexist), and a single-row `account` for the broker tokens + energy.
/// The mapping to/from the in-memory [`Credentials`] lives entirely here; the rest
/// of the tree only sees [`Credentials::load`] / [`Credentials::save`].
mod db {
    use super::*;
    use rusqlite::{Connection, OptionalExtension, params};

    /// The broker account/energy row is a singleton (`id = 1`).
    const ACCOUNT_ID: i64 = 1;

    const SCHEMA: &str = "
        CREATE TABLE IF NOT EXISTS app_settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS credential (
            mode     TEXT NOT NULL,
            feature  TEXT NOT NULL,
            base_url TEXT NOT NULL DEFAULT '',
            api_key  TEXT NOT NULL DEFAULT '',
            model    TEXT,
            PRIMARY KEY (mode, feature)
        );
        CREATE TABLE IF NOT EXISTS account (
            id                INTEGER PRIMARY KEY CHECK (id = 1),
            access_token      TEXT,
            refresh_token     TEXT,
            access_expires_at TEXT,
            energy_remaining  INTEGER,
            energy_total      INTEGER,
            energy_resets_at  TEXT,
            energy_tier       TEXT
        );
    ";

    /// The stable string a `Mode` is stored under (matches the serde/JSON name, so
    /// legacy imports and the wire API line up).
    fn mode_str(m: Mode) -> &'static str {
        match m {
            Mode::Byok => "byok",
            Mode::Xiaoyuanzhu => "xiaoyuanzhu",
        }
    }

    /// Open (creating if needed) the config DB, ensure the schema, and lock it down
    /// to owner-only. A short busy timeout lets the startup load, the settings
    /// writes, and the periodic broker poll serialize instead of erroring.
    fn open(data_dir: &Path) -> anyhow::Result<Connection> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let p = path(data_dir);
        let conn = Connection::open(&p).with_context(|| format!("opening {}", p.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA).context("initializing config schema")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        Ok(conn)
    }

    /// Load the credential store, importing a legacy `credentials.json` on first run.
    pub fn load(data_dir: &Path) -> anyhow::Result<Credentials> {
        let conn = open(data_dir)?;
        maybe_import_legacy(&conn, data_dir)?;
        read(&conn)
    }

    /// Persist the whole store atomically (both modes' rows + the account).
    pub fn save(data_dir: &Path, c: &Credentials) -> anyhow::Result<()> {
        let conn = open(data_dir)?;
        let tx = conn.unchecked_transaction()?;
        write_all(&tx, c)?;
        tx.commit()?;
        Ok(())
    }

    /// Reconstruct a [`Credentials`] from the tables. Absent rows read as empty /
    /// `None`, so a partially-populated store loads cleanly.
    fn read(conn: &Connection) -> anyhow::Result<Credentials> {
        let mode = get_setting(conn, "mode")?
            .and_then(|s| parse_mode(&s))
            .unwrap_or_default();
        let device_id = get_setting(conn, "device_id")?.unwrap_or_default();
        let (tokens, energy) = read_account(conn)?;
        Ok(Credentials {
            mode,
            device_id,
            llm: read_llm(conn, Mode::Byok, "llm")?,
            stt: read_vendor(conn, Mode::Byok, "stt")?,
            tts: read_vendor(conn, Mode::Byok, "tts")?,
            vision: read_vendor(conn, Mode::Byok, "vision")?,
            image: read_vendor(conn, Mode::Byok, "image")?,
            video: read_vendor(conn, Mode::Byok, "video")?,
            managed: read_managed(conn)?,
            tokens,
            energy,
        })
    }

    /// Write every field of `c` — the BYOK flat fields, the managed bundle (when
    /// present), the account, and the scalar flags. Upserts, so re-saving is idempotent.
    fn write_all(conn: &Connection, c: &Credentials) -> anyhow::Result<()> {
        set_setting(conn, "mode", mode_str(c.mode))?;
        set_setting(conn, "device_id", &c.device_id)?;
        write_llm(conn, Mode::Byok, "llm", &c.llm)?;
        write_vendor(conn, Mode::Byok, "stt", &c.stt)?;
        write_vendor(conn, Mode::Byok, "tts", &c.tts)?;
        write_vendor(conn, Mode::Byok, "vision", &c.vision)?;
        write_vendor(conn, Mode::Byok, "image", &c.image)?;
        write_vendor(conn, Mode::Byok, "video", &c.video)?;
        if let Some(m) = &c.managed {
            write_llm(conn, Mode::Xiaoyuanzhu, "llm", &m.llm)?;
            write_vendor(conn, Mode::Xiaoyuanzhu, "stt", &m.stt)?;
            write_vendor(conn, Mode::Xiaoyuanzhu, "tts", &m.tts)?;
            write_vendor(conn, Mode::Xiaoyuanzhu, "vision", &m.vision)?;
            write_vendor(conn, Mode::Xiaoyuanzhu, "image", &m.image)?;
            write_vendor(conn, Mode::Xiaoyuanzhu, "video", &m.video)?;
        }
        write_account(conn, c)?;
        Ok(())
    }

    fn get_setting(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
        Ok(conn
            .query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    fn set_setting(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
        conn.execute(
            "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// The `(base_url, api_key, model)` triple for one `(mode, feature)`, or `None`
    /// when no row exists.
    fn read_row(
        conn: &Connection,
        mode: Mode,
        feature: &str,
    ) -> anyhow::Result<Option<(String, String, Option<String>)>> {
        Ok(conn
            .query_row(
                "SELECT base_url, api_key, model FROM credential WHERE mode = ?1 AND feature = ?2",
                params![mode_str(mode), feature],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    fn read_vendor(conn: &Connection, mode: Mode, feature: &str) -> anyhow::Result<VendorKey> {
        Ok(read_row(conn, mode, feature)?
            .map(|(base_url, api_key, model)| VendorKey { base_url, api_key, model })
            .unwrap_or_default())
    }

    fn read_llm(conn: &Connection, mode: Mode, feature: &str) -> anyhow::Result<LlmCredentials> {
        Ok(read_row(conn, mode, feature)?
            .map(|(base_url, api_key, model)| LlmCredentials { base_url, api_key, model })
            .unwrap_or_default())
    }

    /// The managed bundle is `Some` iff at least one xiaoyuanzhu row was stored —
    /// mirrors the JSON store's `managed: Option<Managed>` (absent until fetched).
    fn read_managed(conn: &Connection) -> anyhow::Result<Option<Managed>> {
        let any: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM credential WHERE mode = ?1)",
            params![mode_str(Mode::Xiaoyuanzhu)],
            |r| r.get(0),
        )?;
        if !any {
            return Ok(None);
        }
        Ok(Some(Managed {
            llm: read_llm(conn, Mode::Xiaoyuanzhu, "llm")?,
            stt: read_vendor(conn, Mode::Xiaoyuanzhu, "stt")?,
            tts: read_vendor(conn, Mode::Xiaoyuanzhu, "tts")?,
            vision: read_vendor(conn, Mode::Xiaoyuanzhu, "vision")?,
            image: read_vendor(conn, Mode::Xiaoyuanzhu, "image")?,
            video: read_vendor(conn, Mode::Xiaoyuanzhu, "video")?,
        }))
    }

    fn write_vendor(conn: &Connection, mode: Mode, feature: &str, vk: &VendorKey) -> anyhow::Result<()> {
        write_row(conn, mode, feature, &vk.base_url, &vk.api_key, vk.model.as_deref())
    }

    fn write_llm(conn: &Connection, mode: Mode, feature: &str, llm: &LlmCredentials) -> anyhow::Result<()> {
        write_row(conn, mode, feature, &llm.base_url, &llm.api_key, llm.model.as_deref())
    }

    fn write_row(
        conn: &Connection,
        mode: Mode,
        feature: &str,
        base_url: &str,
        api_key: &str,
        model: Option<&str>,
    ) -> anyhow::Result<()> {
        conn.execute(
            "INSERT INTO credential (mode, feature, base_url, api_key, model) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(mode, feature) DO UPDATE SET
                 base_url = excluded.base_url, api_key = excluded.api_key, model = excluded.model",
            params![mode_str(mode), feature, base_url, api_key, model],
        )?;
        Ok(())
    }

    /// Read the singleton account row. Tokens are `Some` iff an expiry is stored;
    /// energy is `Some` iff a tier is stored — the two are written independently.
    fn read_account(conn: &Connection) -> anyhow::Result<(Option<Tokens>, Option<Energy>)> {
        let row = conn
            .query_row(
                "SELECT access_token, refresh_token, access_expires_at,
                        energy_remaining, energy_total, energy_resets_at, energy_tier
                 FROM account WHERE id = ?1",
                params![ACCOUNT_ID],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((at, rt, exp, remaining, total, resets_at, tier)) = row else {
            return Ok((None, None));
        };
        let tokens = exp.map(|access_expires_at| Tokens {
            access_token: at.unwrap_or_default(),
            refresh_token: rt.unwrap_or_default(),
            access_expires_at,
        });
        let energy = tier.map(|tier| Energy {
            remaining: remaining.unwrap_or_default(),
            total: total.unwrap_or_default(),
            resets_at: resets_at.unwrap_or_default(),
            tier,
        });
        Ok((tokens, energy))
    }

    fn write_account(conn: &Connection, c: &Credentials) -> anyhow::Result<()> {
        let (at, rt, exp) = match &c.tokens {
            Some(t) => (Some(&t.access_token), Some(&t.refresh_token), Some(&t.access_expires_at)),
            None => (None, None, None),
        };
        let (remaining, total, resets_at, tier) = match &c.energy {
            Some(e) => (Some(e.remaining), Some(e.total), Some(&e.resets_at), Some(&e.tier)),
            None => (None, None, None, None),
        };
        conn.execute(
            "INSERT INTO account (id, access_token, refresh_token, access_expires_at,
                                  energy_remaining, energy_total, energy_resets_at, energy_tier)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 access_token = excluded.access_token, refresh_token = excluded.refresh_token,
                 access_expires_at = excluded.access_expires_at,
                 energy_remaining = excluded.energy_remaining, energy_total = excluded.energy_total,
                 energy_resets_at = excluded.energy_resets_at, energy_tier = excluded.energy_tier",
            params![ACCOUNT_ID, at, rt, exp, remaining, total, resets_at, tier],
        )?;
        Ok(())
    }

    /// The pre-SQLite JSON store, imported once. Named for its legacy filename.
    const LEGACY_JSON: &str = "credentials.json";

    /// Import a legacy `credentials.json` into a never-written DB, then rename it to
    /// `.bak` so the import runs at most once. A malformed legacy file logs and is
    /// skipped (the user re-saves from Settings) rather than blocking boot.
    fn maybe_import_legacy(conn: &Connection, data_dir: &Path) -> anyhow::Result<()> {
        let legacy = data_dir.join(LEGACY_JSON);
        if !legacy.exists() {
            return Ok(());
        }
        let already_written: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM app_settings)", [], |r| r.get(0))?;
        if already_written {
            return Ok(());
        }
        let bytes = std::fs::read(&legacy).with_context(|| format!("reading {}", legacy.display()))?;
        match serde_json::from_slice::<Credentials>(&bytes) {
            Ok(c) => {
                let tx = conn.unchecked_transaction()?;
                write_all(&tx, &c)?;
                tx.commit()?;
                let bak = data_dir.join(format!("{LEGACY_JSON}.bak"));
                let _ = std::fs::rename(&legacy, &bak);
                tracing::info!(backup = %bak.display(), "imported legacy credentials.json into config.db");
            }
            Err(e) => tracing::warn!(error = %e, "legacy credentials.json unreadable; skipping import"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_is_case_insensitive() {
        assert_eq!(parse_mode("byok"), Some(Mode::Byok));
        assert_eq!(parse_mode("XIAOYUANZHU"), Some(Mode::Xiaoyuanzhu));
        // Legacy values fold into xiaoyuanzhu.
        assert_eq!(parse_mode("FREE"), Some(Mode::Xiaoyuanzhu));
        assert_eq!(parse_mode(" login "), Some(Mode::Xiaoyuanzhu));
        assert_eq!(parse_mode("nope"), None);
    }

    #[test]
    fn legacy_mode_values_deserialize_to_xiaoyuanzhu() {
        // An older credentials.json with `"mode": "free"` (or "login") must still
        // load — the serde aliases fold it into xiaoyuanzhu, not a parse failure.
        for legacy in [r#"{"mode":"free"}"#, r#"{"mode":"login"}"#] {
            let c: Credentials = serde_json::from_str(legacy).unwrap();
            assert_eq!(c.mode, Mode::Xiaoyuanzhu);
        }
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials::load(dir.path());
        assert_eq!(c.mode, Mode::Xiaoyuanzhu);
        assert!(c.llm.api_key.is_empty());
        assert!(c.tokens.is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials {
            mode: Mode::Xiaoyuanzhu,
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
                stt: VendorKey { api_key: "sg-secret".into(), ..Default::default() },
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
    fn both_modes_configs_coexist_across_a_switch() {
        // The user's BYOK keys and the broker's managed bundle are stored side by
        // side; flipping the active mode must not lose the other mode's config.
        let dir = tempfile::tempdir().unwrap();
        let c = Credentials {
            mode: Mode::Byok,
            llm: LlmCredentials { api_key: "byok-llm".into(), ..Default::default() },
            managed: Some(Managed {
                llm: LlmCredentials { api_key: "managed-llm".into(), ..Default::default() },
                ..Default::default()
            }),
            ..Default::default()
        };
        c.save(dir.path()).unwrap();

        // Switch to xiaoyuanzhu and re-save (as the settings handler would).
        let mut back = Credentials::load(dir.path());
        assert_eq!(back.llm.api_key, "byok-llm"); // BYOK still there while in byok
        back.mode = Mode::Xiaoyuanzhu;
        back.save(dir.path()).unwrap();

        // Both configs survive the round-trip; effective() follows the active mode.
        let after = Credentials::load(dir.path());
        assert_eq!(after.mode, Mode::Xiaoyuanzhu);
        assert_eq!(after.llm.api_key, "byok-llm", "BYOK config must persist across a switch");
        assert_eq!(after.managed.as_ref().unwrap().llm.api_key, "managed-llm");
        assert_eq!(after.effective().unwrap().llm.api_key, "managed-llm");
    }

    #[test]
    fn imports_legacy_credentials_json_once() {
        let dir = tempfile::tempdir().unwrap();
        // A pre-SQLite store with a legacy mode value and a BYOK key.
        let legacy = r#"{"mode":"free","llm":{"base_url":"","api_key":"old-key","model":null}}"#;
        std::fs::write(dir.path().join("credentials.json"), legacy).unwrap();

        let c = Credentials::load(dir.path());
        assert_eq!(c.mode, Mode::Xiaoyuanzhu, "legacy free → xiaoyuanzhu");
        assert_eq!(c.llm.api_key, "old-key", "legacy key imported");
        // The JSON is renamed so the import can't run twice.
        assert!(!dir.path().join("credentials.json").exists());
        assert!(dir.path().join("credentials.json.bak").exists());

        // A second load reads purely from the DB (no re-import) and is unchanged.
        let again = Credentials::load(dir.path());
        assert_eq!(again.llm.api_key, "old-key");
    }

    #[test]
    fn effective_picks_byok_or_managed() {
        let mut c = Credentials::default();
        assert_eq!(c.mode, Mode::Xiaoyuanzhu); // xiaoyuanzhu is the default

        c.mode = Mode::Byok;
        c.llm.api_key = "byok-key".into();
        assert_eq!(c.effective().unwrap().llm.api_key, "byok-key");

        // xiaoyuanzhu with no configs → nothing in effect (callers fall back to env).
        c.mode = Mode::Xiaoyuanzhu;
        assert!(c.effective().is_none());

        c.managed = Some(Managed {
            llm: LlmCredentials {
                base_url: "https://songguo.xiaoyuanzhu.com".into(),
                api_key: "managed-key".into(),
                model: None,
            },
            stt: VendorKey { api_key: "managed-stt".into(), ..Default::default() },
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
            vision: VendorKey { api_key: "vision-super-secret".into(), ..Default::default() },
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
