//! Broker client — bootstrap a xiaoyuanzhu account and fetch configs + energy from
//! the broker (hi.xiaoyuanzhu.com).
//!
//! Xiaoyuanzhu mode: the `device_id` seeds a one-time **bootstrap** that
//! auto-creates the account at the broker and returns OAuth tokens; thereafter the
//! access token authenticates `/configs` (rare) and `/energy` (frequent), refreshed
//! via the refresh token. After bootstrap the broker only ever sees one identity —
//! the account token — so the anonymous (`free` tier) and signed-in (`sub` tier)
//! accounts share one path. Signed-in bootstrap (Authentik-authenticated, seeded by
//! `bearer`) is future work; today an anonymous device account is always minted.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::foundation::credentials::{Credentials, Energy, Identity, LlmCredentials, Managed, Mode, Tokens, VendorKey};

/// Env override for the broker base URL (default [`DEFAULT_BROKER_URL`]).
const ENV_BROKER_URL: &str = "HI_AGENT_BROKER_URL";
const DEFAULT_BROKER_URL: &str = "https://hi.xiaoyuanzhu.com";

/// `app_settings` keys recording the outcome of the last broker sync, so the
/// Settings page can show a real state (connecting / connected / problem) instead
/// of a perpetual "connecting". Written on every refresh + energy poll; read by
/// the public `/api/account` status endpoint. Not secrets.
pub const KEY_BROKER_STATE: &str = "broker_state"; // "ok" | "error"
pub const KEY_BROKER_ERROR: &str = "broker_error"; // last error text (cleared on ok)
pub const KEY_BROKER_CHECKED_AT: &str = "broker_checked_at"; // rfc3339 of last attempt

fn base_url() -> String {
    std::env::var(ENV_BROKER_URL)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BROKER_URL.to_string())
}

/// The broker base URL (env override or default) — public so the account-link
/// handler can build the site URL it sends the browser to.
pub fn public_base_url() -> String {
    base_url()
}

/// Bounded HTTP client so a slow/unreachable broker can't hang the boot path.
fn http() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("building broker http client")
}

/// Coarse, non-identifying device telemetry sent on bootstrap (sanity-check only).
fn device_info() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "app_version": env!("CARGO_PKG_VERSION"),
        "install_shape": std::env::var("HI_AGENT_INSTALL_SHAPE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

#[derive(Deserialize, Default)]
struct TokenDto {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// One model the broker offers for a task, with editorial 0–100 scores. Today we
/// select purely on `quality`; `speed`/`price` are parsed so the shape round-trips
/// and a smarter policy can weigh them later.
#[derive(Deserialize, Default, Clone)]
struct ModelDto {
    #[serde(default)]
    model: String,
    /// Optional cheaper/faster companion for the background "haiku" slot. Empty →
    /// the client reuses `model` for that slot. Only meaningful on the LLM task.
    #[serde(default)]
    small: String,
    #[serde(default)]
    quality: i64,
    // Parsed for round-trip + a future weighted policy; selection uses quality today.
    #[serde(default)]
    #[allow(dead_code)]
    speed: i64,
    #[serde(default)]
    #[allow(dead_code)]
    price: i64,
}

/// One wire's endpoint: the full songguo URL for that protocol, the shared token,
/// and the models served over it.
#[derive(Deserialize, Default, Clone)]
struct WireDto {
    #[serde(default)]
    url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    models: Vec<ModelDto>,
}

/// GET /api/agent/configs — a three-layer menu: HF task name → wire name →
/// endpoint. Collapsed into the internal per-slot [`Managed`] by [`managed_from`].
type ConfigsDto = std::collections::HashMap<String, std::collections::HashMap<String, WireDto>>;

/// The `scheme://host[:port]` origin of a full URL, dropping the path. The LLM CLI
/// (`ANTHROPIC_BASE_URL`) and the vendor adapters re-append their own paths, so the
/// internal `base_url` stays a bare origin — matching what the broker sent before
/// it moved to full per-wire URLs. Falls back to the input if it isn't URL-shaped.
fn origin_of(url: &str) -> String {
    let u = url.trim();
    if let Some(after) = u.find("://").map(|i| i + 3) {
        if let Some(slash) = u[after..].find('/') {
            return u[..after + slash].to_string();
        }
    }
    u.to_string()
}

/// Reduce one task's `wire → endpoint` map to (wire, full url, api_key, model):
/// the single wire (one per task today; lexically-first for determinism) and the
/// highest-`quality` model, plus that model's optional `small` companion. Callers
/// keep the full URL or reduce it to its origin.
fn pick_wire(
    wires: &std::collections::HashMap<String, WireDto>,
) -> Option<(String, String, String, Option<String>, Option<String>)> {
    let (wire, w) = wires.iter().min_by(|a, b| a.0.cmp(b.0))?;
    let best = w.models.iter().max_by_key(|m| m.quality);
    let model = best
        .map(|m| m.model.trim().to_string())
        .filter(|s| !s.is_empty());
    let small = best
        .map(|m| m.small.trim().to_string())
        .filter(|s| !s.is_empty());
    Some((wire.clone(), w.url.trim().to_string(), w.api_key.clone(), model, small))
}

/// Collapse the broker menu into the internal per-slot [`Managed`], selecting the
/// best-quality model per task.
///
/// Our code uses the broker's **full URL** verbatim for every capability — that's
/// the single source of truth for each endpoint. The one exception is the LLM: its
/// URL is handed to the Claude CLI via `ANTHROPIC_BASE_URL`, and the CLI appends
/// `/v1/messages` itself, so the LLM slot takes just the **origin**. Every slot
/// keeps `wire` empty, so each capability uses its single default adapter.
fn managed_from(c: &ConfigsDto) -> Managed {
    fn resolve(
        c: &ConfigsDto,
        task: &str,
        full: bool,
    ) -> Option<(String, String, Option<String>, Option<String>)> {
        c.get(task).and_then(|w| pick_wire(w)).map(|(_wire, url, api_key, model, small)| {
            (if full { url } else { origin_of(&url) }, api_key, model, small)
        })
    }
    let vendor = |task: &str| -> VendorKey {
        resolve(c, task, true)
            .map(|(base_url, api_key, model, _small)| VendorKey { wire: String::new(), base_url, api_key, model })
            .unwrap_or_default()
    };
    let llm = resolve(c, "text-generation", false)
        .map(|(base_url, api_key, model, small)| LlmCredentials {
            wire: String::new(),
            base_url,
            api_key,
            model,
            small,
        })
        .unwrap_or_default();
    Managed {
        llm,
        stt: vendor("automatic-speech-recognition"),
        tts: vendor("text-to-speech"),
        vision: vendor("image-text-to-text"),
        image: vendor("text-to-image"),
        video: vendor("text-to-video"),
    }
}

#[derive(Deserialize, Default)]
struct EnergyDto {
    #[serde(default)]
    remaining: i64,
    #[serde(default)]
    total: i64,
    #[serde(default)]
    resets_at: String,
    #[serde(default)]
    tier: String,
}

fn tokens_from(t: TokenDto) -> Tokens {
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(t.expires_in.max(0))).to_rfc3339();
    Tokens {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        access_expires_at: expires_at,
    }
}

/// POST /api/agent/bootstrap — free device → account tokens (auto-creates the
/// account at the broker on first contact).
async fn bootstrap(device_id: &str) -> anyhow::Result<Tokens> {
    let url = format!("{}/api/agent/bootstrap", base_url());
    let resp = http()?
        .post(&url)
        .json(&serde_json::json!({
            "mode": "free",
            "device_id": device_id,
            "device_info": device_info(),
        }))
        .send()
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("bootstrap {url} returned {status}: {}", resp.text().await.unwrap_or_default());
    }
    Ok(tokens_from(resp.json().await.context("parsing bootstrap response")?))
}

/// POST /api/agent/token — refresh_token grant (the broker rotates the refresh
/// token, so the returned pair must be persisted).
async fn refresh_access(refresh_token: &str) -> anyhow::Result<Tokens> {
    let url = format!("{}/api/agent/token", base_url());
    let resp = http()?
        .post(&url)
        .json(&serde_json::json!({ "grant_type": "refresh_token", "refresh_token": refresh_token }))
        .send()
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("token {url} returned {status}: {}", resp.text().await.unwrap_or_default());
    }
    Ok(tokens_from(resp.json().await.context("parsing token response")?))
}

/// GET /api/agent/configs — the vendor settings to apply (rare fetch).
async fn fetch_configs(access: &str) -> anyhow::Result<Managed> {
    let url = format!("{}/api/agent/configs", base_url());
    let resp = http()?
        .get(&url)
        .bearer_auth(access)
        .send()
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("configs {url} returned {status}: {}", resp.text().await.unwrap_or_default());
    }
    let c: ConfigsDto = resp.json().await.context("parsing configs")?;
    Ok(managed_from(&c))
}

/// GET /api/agent/energy — the user-facing balance (frequent poll).
pub async fn fetch_energy(access: &str) -> anyhow::Result<Energy> {
    let url = format!("{}/api/agent/energy", base_url());
    let resp = http()?
        .get(&url)
        .bearer_auth(access)
        .send()
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("energy {url} returned {status}: {}", resp.text().await.unwrap_or_default());
    }
    let e: EnergyDto = resp.json().await.context("parsing energy")?;
    Ok(Energy { remaining: e.remaining, total: e.total, resets_at: e.resets_at, tier: e.tier })
}

#[derive(Deserialize, Default)]
struct WebTicketDto {
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    path: String,
}

/// Mint a one-time web-handoff ticket and return the browser URL that lands the
/// user on the site **already signed in as this device account**
/// (`<broker><path>?ticket=…`). The tray's "Subscribe" opens this (default landing
/// page); the out-of-energy hint passes `prefer_path = Some("/account")`. Xiaoyuanzhu
/// mode only — it needs the bootstrapped access token; errors if bootstrap hasn't
/// produced one yet (the caller surfaces that, e.g. "try again in a moment"). The
/// ticket is a URL-safe JWT (base64url + dots), so no query-encoding is needed.
pub async fn subscribe_url(data_dir: &Path, prefer_path: Option<&str>) -> anyhow::Result<String> {
    let store = Credentials::load(data_dir);
    let access = store
        .tokens
        .as_ref()
        .map(|t| t.access_token.trim().to_string())
        .unwrap_or_default();
    if access.is_empty() {
        anyhow::bail!("no broker access token yet (account bootstrap hasn't completed)");
    }
    let url = format!("{}/api/agent/web-ticket", base_url());
    let resp = http()?
        .post(&url)
        .bearer_auth(&access)
        .send()
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("web-ticket {url} returned {status}: {}", resp.text().await.unwrap_or_default());
    }
    let dto: WebTicketDto = resp.json().await.context("parsing web-ticket response")?;
    if dto.ticket.trim().is_empty() {
        anyhow::bail!("broker returned an empty web ticket");
    }
    // The caller's preferred landing page wins (the hint wants `/account`); else the
    // broker's suggested path, else the account page. The ticket is a login handoff,
    // valid for any page on the domain, so overriding the path is safe.
    let prefer = prefer_path.map(|p| p.trim()).filter(|p| !p.is_empty());
    let broker_path = dto.path.trim();
    let path = prefer.unwrap_or(if broker_path.is_empty() { "/account" } else { broker_path });
    Ok(format!("{}{}?ticket={}", base_url(), path, dto.ticket.trim()))
}

/// Get a usable access token: refresh if we hold a refresh token, else bootstrap.
/// Re-bootstraps on a failed refresh (device_id makes that idempotent).
async fn ensure_tokens(store: &Credentials) -> anyhow::Result<Tokens> {
    if let Some(t) = &store.tokens {
        if !t.refresh_token.trim().is_empty() {
            match refresh_access(&t.refresh_token).await {
                Ok(nt) => return Ok(nt),
                // Expected, self-healing path: the broker rotates/expires refresh
                // tokens, and a stale one (prior run, broker DB reset) is idempotently
                // recovered by re-bootstrapping on the stable device id. Not fatal.
                Err(e) => tracing::warn!(error = %e, "broker refresh token stale; re-bootstrapping a fresh account (self-heals)"),
            }
        }
    }
    bootstrap(&store.device_id).await
}

/// The signed-in account's identity in a claim response.
#[derive(Deserialize, Default)]
struct ClaimIdentityDto {
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tier: String,
}

/// POST /api/agent/claim success body: fresh account tokens + the adopted identity.
#[derive(Deserialize, Default)]
struct ClaimDto {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    identity: ClaimIdentityDto,
}

/// A broker error body (`{error, message}`) — read to surface the 409 conflict code.
#[derive(Deserialize, Default)]
struct ErrorDto {
    #[serde(default)]
    error: String,
}

/// The result of a web→device claim.
pub enum ClaimOutcome {
    /// The device adopted the signed-in account; `email` is who it now is.
    Adopted { email: String },
    /// The broker declined to switch automatically — `code` is the machine reason
    /// (`keep_current` = a recoverable account is signed in here; `chooser_required`
    /// = both accounts are bound). The caller explains it to the user.
    Conflict { code: String },
}

/// Redeem a device-ticket to adopt the account the browser is signed in as (the
/// web→device link). Authenticates with this device's current access token so the
/// broker can apply its adoption policy (A = this device, B = the ticket's account);
/// on success it relinks the device server-side and returns fresh tokens for B,
/// which we swap into the store along with B's identity, then refresh configs +
/// energy under the new tokens. A 409 comes back as [`ClaimOutcome::Conflict`] for
/// the caller to explain rather than an error. Xiaoyuanzhu mode only.
pub async fn claim_device(data_dir: &Path, ticket: &str) -> anyhow::Result<ClaimOutcome> {
    let mut store = Credentials::load(data_dir);
    if store.mode == Mode::Byok {
        anyhow::bail!("account linking is only available in xiaoyuanzhu mode");
    }
    // The claim is authed as the device's current account — make sure we hold a
    // usable access token (bootstrap on first run), and persist it before the call.
    let tokens = ensure_tokens(&store).await.context("ensuring a device token before claim")?;
    store.tokens = Some(tokens.clone());
    let _ = store.save(data_dir);

    let url = format!("{}/api/agent/claim", base_url());
    let resp = http()?
        .post(&url)
        .bearer_auth(&tokens.access_token)
        .json(&serde_json::json!({ "ticket": ticket }))
        .send()
        .await
        .with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    if status.as_u16() == 409 {
        let code = resp.json::<ErrorDto>().await.map(|e| e.error).unwrap_or_default();
        return Ok(ClaimOutcome::Conflict { code });
    }
    if !status.is_success() {
        anyhow::bail!("claim {url} returned {status}: {}", resp.text().await.unwrap_or_default());
    }
    let dto: ClaimDto = resp.json().await.context("parsing claim response")?;

    // Swap in the adopted account's tokens + identity.
    store.tokens = Some(tokens_from(TokenDto {
        access_token: dto.access_token,
        refresh_token: dto.refresh_token,
        expires_in: dto.expires_in,
    }));
    let email = dto.identity.email.clone();
    store.identity = if email.trim().is_empty() {
        None
    } else {
        Some(Identity { email: email.clone(), name: dto.identity.name, tier: dto.identity.tier })
    };
    store.save(data_dir).context("saving claimed account")?;

    // Pull the adopted account's configs + energy under its new tokens. `refresh`
    // reloads the store (keeping the identity we just wrote) and persists again.
    refresh(data_dir, None).await;
    Ok(ClaimOutcome::Adopted { email })
}


    let now = chrono::Utc::now().to_rfc3339();
    let _ = set_setting(data_dir, KEY_BROKER_STATE, if ok { "ok" } else { "error" });
    let _ = set_setting(data_dir, KEY_BROKER_ERROR, if ok { "" } else { error });
    let _ = set_setting(data_dir, KEY_BROKER_CHECKED_AT, &now);
}

/// In xiaoyuanzhu mode: ensure account tokens (bootstrap/refresh), fetch configs +
/// energy, and persist. Best-effort — failures log and keep any cached configs.
/// Mints a `device_id` on first need. No-op in BYOK. The `bearer` (a signed-in
/// Authentik session, when present) will seed the `sub`-tier bootstrap once that's
/// wired; today an anonymous device account is always minted. v1 runs at startup
/// and on mode-select; a periodic loop is wired in `lib.rs`.
pub async fn refresh(data_dir: &Path, bearer: Option<&str>) {
    let mut store = Credentials::load(data_dir);
    match store.mode {
        Mode::Byok => return,
        Mode::Xiaoyuanzhu => {}
    }
    let _ = bearer; // reserved for signed-in (`sub`-tier) bootstrap; unused today.

    let mut dirty = false;
    if store.device_id.trim().is_empty() {
        store.device_id = uuid::Uuid::now_v7().to_string();
        dirty = true;
    }

    let tokens = match ensure_tokens(&store).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "broker bootstrap/refresh failed; keeping cached configs");
            record_status(data_dir, false, &e.to_string());
            if dirty {
                if let Err(e) = store.save(data_dir) {
                    tracing::warn!(error = %e, "failed to persist credential store");
                }
            }
            return;
        }
    };
    store.tokens = Some(tokens.clone());
    dirty = true;

    match fetch_configs(&tokens.access_token).await {
        Ok(m) => {
            tracing::info!("fetched managed configs from broker");
            store.managed = Some(m);
        }
        Err(e) => tracing::warn!(error = %e, "configs fetch failed; keeping cached"),
    }
    match fetch_energy(&tokens.access_token).await {
        Ok(en) => {
            tracing::info!(tier = %en.tier, remaining = en.remaining, total = en.total, "energy refreshed");
            // Ground truth: raise / clear the out-of-energy hint from the balance. At
            // startup this catches an account that's already empty before any 402.
            crate::foundation::energy_state::reconcile(en.remaining, en.total);
            store.energy = Some(en);
        }
        Err(e) => tracing::warn!(error = %e, "energy fetch failed; keeping cached"),
    }

    // Tokens were obtained → the account exists and is healthy, even if a vendor
    // sub-fetch above degraded. Record success so the UI leaves "connecting".
    record_status(data_dir, true, "");

    if dirty {
        if let Err(e) = store.save(data_dir) {
            tracing::warn!(error = %e, "failed to persist credential store after broker refresh");
        }
    }
}

/// Lightweight energy poll that hands back the fresh balance: re-fetch with the
/// cached access token, persist it, and return it. `None` in BYOK, when no token
/// is cached yet, or when the fetch fails (the last cached value is left in
/// place). The reactor's out-of-energy poller uses the returned `remaining` to
/// detect a refill and resume, so this must return the value, not just store it.
pub async fn poll_energy_now(data_dir: &Path) -> Option<Energy> {
    let mut store = Credentials::load(data_dir);
    if store.mode == Mode::Byok {
        return None;
    }
    let tokens = store.tokens.clone()?;
    match fetch_energy(&tokens.access_token).await {
        Ok(en) => {
            // Ground truth: raise the hint when empty, clear it on refill. This is the
            // 60s periodic poll and the out-of-energy poller's own recovery check.
            crate::foundation::energy_state::reconcile(en.remaining, en.total);
            store.energy = Some(en.clone());
            if let Err(e) = store.save(data_dir) {
                tracing::debug!(error = %e, "failed to persist energy poll");
            }
            // Keeps the Settings page's "last checked" fresh between full refreshes.
            record_status(data_dir, true, "");
            Some(en)
        }
        Err(e) => {
            tracing::debug!(error = %e, "energy poll failed; keeping last value");
            None
        }
    }
}

/// Fire-and-forget energy poll for the periodic refresh loop.
async fn poll_energy(data_dir: &Path) {
    let _ = poll_energy_now(data_dir).await;
}

/// Spawn a detached loop that keeps managed credentials fresh while running: the
/// full configs refresh on a slow cadence (which also rotates the access token)
/// and an energy poll on a fast one. No-op in BYOK (each call returns early).
/// Best-effort; never panics.
pub fn spawn_refresh_loop(data_dir: std::path::PathBuf) {
    tokio::spawn(async move {
        let mut configs = tokio::time::interval(Duration::from_secs(3600));
        let mut energy = tokio::time::interval(Duration::from_secs(60));
        // Startup already refreshed once; drop the immediate first ticks.
        configs.tick().await;
        energy.tick().await;
        loop {
            tokio::select! {
                _ = configs.tick() => refresh(&data_dir, None).await,
                _ = energy.tick() => poll_energy(&data_dir).await,
            }
        }
    });
}
