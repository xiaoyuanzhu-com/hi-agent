//! Broker client — bootstrap a free account and fetch configs + energy from the
//! broker (hi.xiaoyuanzhu.com).
//!
//! Free mode: the `device_id` seeds a one-time **bootstrap** that auto-creates
//! the account at the broker and returns OAuth tokens; thereafter the access
//! token authenticates `/configs` (rare) and `/energy` (frequent), refreshed via
//! the refresh token. After bootstrap the broker only ever sees one identity —
//! the account token — so free and (future) sub are unified. Login/sub bootstrap
//! (Authentik-authenticated) is future work.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::foundation::credentials::{Credentials, Energy, LlmCredentials, Managed, Mode, Tokens, VendorKey};

/// Env override for the broker base URL (default [`DEFAULT_BROKER_URL`]).
const ENV_BROKER_URL: &str = "HI_AGENT_BROKER_URL";
const DEFAULT_BROKER_URL: &str = "https://hi.xiaoyuanzhu.com";

fn base_url() -> String {
    std::env::var(ENV_BROKER_URL)
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BROKER_URL.to_string())
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

#[derive(Deserialize, Default)]
struct ConfigsDto {
    #[serde(default)]
    llm: LlmCredentials,
    #[serde(default)]
    stt: VendorKey,
    #[serde(default)]
    tts: VendorKey,
    #[serde(default)]
    vision: VendorKey,
    #[serde(default)]
    image: VendorKey,
    #[serde(default)]
    video: VendorKey,
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
    Ok(Managed { llm: c.llm, stt: c.stt, tts: c.tts, vision: c.vision, image: c.image, video: c.video })
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

/// Get a usable access token: refresh if we hold a refresh token, else bootstrap.
/// Re-bootstraps on a failed refresh (device_id makes that idempotent).
async fn ensure_tokens(store: &Credentials) -> anyhow::Result<Tokens> {
    if let Some(t) = &store.tokens {
        if !t.refresh_token.trim().is_empty() {
            match refresh_access(&t.refresh_token).await {
                Ok(nt) => return Ok(nt),
                Err(e) => tracing::warn!(error = %e, "token refresh failed; re-bootstrapping"),
            }
        }
    }
    bootstrap(&store.device_id).await
}

/// In free mode: ensure account tokens (bootstrap/refresh), fetch configs +
/// energy, and persist. Best-effort — failures log and keep any cached configs.
/// Mints a `device_id` on first need. No-op in BYOK. Login/sub bootstrap
/// (Authentik-authenticated, seeded by `bearer`) is future work. v1 runs at
/// startup and on mode-select; a periodic loop is wired in `lib.rs`.
pub async fn refresh(data_dir: &Path, bearer: Option<&str>) {
    let mut store = Credentials::load(data_dir);
    match store.mode {
        Mode::Byok => return,
        Mode::Login => {
            // The bearer (when present, from a Settings request) will seed the
            // login/sub account bootstrap once it's wired.
            tracing::debug!(
                has_bearer = bearer.is_some(),
                "login mode: account bootstrap not yet wired; keeping cached configs"
            );
            return;
        }
        Mode::Free => {}
    }

    let mut dirty = false;
    if store.device_id.trim().is_empty() {
        store.device_id = uuid::Uuid::now_v7().to_string();
        dirty = true;
    }

    let tokens = match ensure_tokens(&store).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "broker bootstrap/refresh failed; keeping cached configs");
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
            store.energy = Some(en);
        }
        Err(e) => tracing::warn!(error = %e, "energy fetch failed; keeping cached"),
    }

    if dirty {
        if let Err(e) = store.save(data_dir) {
            tracing::warn!(error = %e, "failed to persist credential store after broker refresh");
        }
    }
}

/// Lightweight energy poll: re-fetch the balance with the cached access token and
/// update the store. Best-effort — a failure leaves the last value in place.
async fn poll_energy(data_dir: &Path) {
    let mut store = Credentials::load(data_dir);
    if store.mode == Mode::Byok {
        return;
    }
    let Some(tokens) = store.tokens.clone() else {
        return;
    };
    match fetch_energy(&tokens.access_token).await {
        Ok(en) => {
            store.energy = Some(en);
            if let Err(e) = store.save(data_dir) {
                tracing::debug!(error = %e, "failed to persist energy poll");
            }
        }
        Err(e) => tracing::debug!(error = %e, "energy poll failed; keeping last value"),
    }
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
