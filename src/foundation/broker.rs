//! Broker client — fetch the managed credential bundle from the broker
//! (hi.xiaoyuanzhu.com) in login/free mode and cache it in the credential store.
//!
//! The broker (`POST /api/agent/credentials`) exchanges an identity — an
//! Authentik bearer (login) or an anonymous device id (free) — for the whole
//! credential bundle the agent stores, plus an account/credits snapshot. This is
//! the client half: it mints a stable device id on first need, posts to the
//! broker, and folds the response into [`Managed`].

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::foundation::credentials::{Credentials, LlmCredentials, Managed, Mode, VendorKey};

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

/// The broker's response (mirrors hi.xiaoyuanzhu.com `server/credentials.go`). The
/// credential field names match the store types, so [`LlmCredentials`] /
/// [`VendorKey`] deserialize directly (a vendor's optional `base_url` is ignored
/// for now — the agent keeps that vendor's default endpoint).
#[derive(Deserialize, Default)]
struct BrokerResponse {
    #[serde(default)]
    credentials: BundleDto,
    #[serde(default)]
    account: AccountDto,
    #[serde(default)]
    expires_at: String,
}

#[derive(Deserialize, Default)]
struct BundleDto {
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
struct AccountDto {
    #[serde(default)]
    plan: String,
    #[serde(default)]
    credits: CreditsDto,
}

#[derive(Deserialize, Default)]
struct CreditsDto {
    #[serde(default)]
    remaining: i64,
    #[serde(default)]
    limit: i64,
    #[serde(default)]
    resets_at: String,
}

/// POST the broker for a fresh bundle. `mode` must be login or free; `bearer` is
/// the Authentik token for login (None for free).
async fn fetch(mode: Mode, device_id: &str, bearer: Option<&str>) -> anyhow::Result<Managed> {
    let mode_str = match mode {
        Mode::Login => "login",
        Mode::Free => "free",
        Mode::Byok => anyhow::bail!("broker fetch is only for login/free mode"),
    };
    let url = format!("{}/api/agent/credentials", base_url());
    // Bounded so a slow/unreachable broker can't hang startup (the fetch runs on
    // the boot path in the default free mode).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .context("building broker http client")?;
    let mut req = client
        .post(&url)
        .json(&serde_json::json!({ "mode": mode_str, "device_id": device_id }));
    if let Some(b) = bearer {
        req = req.bearer_auth(b);
    }
    let resp = req.send().await.with_context(|| format!("calling broker {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("broker {url} returned {status}: {body}");
    }
    let r: BrokerResponse = resp.json().await.context("parsing broker response")?;
    Ok(Managed {
        llm: r.credentials.llm,
        stt: r.credentials.stt,
        tts: r.credentials.tts,
        vision: r.credentials.vision,
        image: r.credentials.image,
        video: r.credentials.video,
        plan: r.account.plan,
        credits_remaining: r.account.credits.remaining,
        credits_limit: r.account.credits.limit,
        credits_resets_at: r.account.credits.resets_at,
        expires_at: r.expires_at,
    })
}

/// In login/free mode, fetch a fresh bundle from the broker and persist it into
/// the store. Best-effort: a failure logs and leaves any cached bundle in place.
/// Mints a stable `device_id` on first need. A no-op in BYOK mode.
///
/// v1 refetches on each startup; an expiry-driven mid-session refresh is future
/// work. Login currently has no bearer source wired, so a login fetch will 401 at
/// the broker until the Authentik access token is forwarded (see the TODO).
pub async fn refresh(data_dir: &Path) {
    let mut store = Credentials::load(data_dir);
    if store.mode == Mode::Byok {
        return;
    }

    let mut dirty = false;
    if store.device_id.trim().is_empty() {
        store.device_id = uuid::Uuid::now_v7().to_string();
        dirty = true;
    }

    // TODO(login): forward the signed-in user's Authentik access token here. Until
    // that is plumbed from the auth session, a login-mode fetch reaches the broker
    // without a bearer and is rejected (401) — logged below, BYOK still works.
    let bearer: Option<&str> = None;

    match fetch(store.mode, &store.device_id, bearer).await {
        Ok(managed) => {
            tracing::info!(
                mode = ?store.mode, plan = %managed.plan,
                credits_remaining = managed.credits_remaining,
                "refreshed managed credentials from broker"
            );
            store.managed = Some(managed);
            dirty = true;
        }
        Err(e) => {
            tracing::warn!(error = %e, mode = ?store.mode, "broker credential fetch failed; using cached bundle if any");
        }
    }

    if dirty {
        if let Err(e) = store.save(data_dir) {
            tracing::warn!(error = %e, "failed to persist credential store after broker refresh");
        }
    }
}
