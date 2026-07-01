//! BYOK settings routes — read/write the vendor credential store.
//!
//! `GET /api/settings/credentials` reports the configured state of every keyed
//! vendor **without ever returning the key** (only a last-4 hint), plus whether
//! `.env` provides a fallback. `POST` writes the store via [`Credentials::save`].
//! Persist-only: the running agent resolved its keys at startup, so the response
//! flags `restart_required` — a restart re-resolves the store. These routes are
//! browser-facing, so they sit behind the OIDC login gate when auth is enabled
//! (only the owner should set keys).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::foundation::credentials::{self, Credentials};
use crate::foundation::server::AppState;

/// Last 4 chars of a key, UTF-8-safe (keys are ASCII, but don't assume it).
fn last4(s: &str) -> String {
    let mut tail: Vec<char> = s.chars().rev().take(4).collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// Whether an env var is set to a non-empty value.
fn env_set(var: &str) -> bool {
    std::env::var(var).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// The redacted view for a vendor: configured state + key hint, plus the
/// non-secret `base_url` / `model` overrides so the UI can show and edit them.
fn vendor_view(vk: &credentials::VendorKey, env_var: &str) -> Value {
    let key = vk.api_key.trim();
    let configured = !key.is_empty();
    json!({
        "configured": configured,
        "key_hint": if configured { format!("••••{}", last4(key)) } else { String::new() },
        "env_fallback": env_set(env_var),
        "base_url": vk.base_url,
        "model": vk.model,
    })
}

/// Report the credential state for the Settings UI. Never returns a raw key.
pub async fn get_credentials(State(state): State<Arc<AppState>>) -> Json<Value> {
    let creds = Credentials::load(&state.data_dir);
    let key = creds.llm.api_key.trim();
    let llm_configured = !key.is_empty();
    // In xiaoyuanzhu mode, the broker's energy snapshot — for the UI to show the
    // tier + remaining/total energy. Absent until energy has been fetched.
    let account = creds.energy.as_ref().map(|e| {
        json!({
            "tier": e.tier,
            "energy_remaining": e.remaining,
            "energy_total": e.total,
            "resets_at": e.resets_at,
        })
    });
    Json(json!({
        "mode": creds.mode,
        "account": account,
        // The BYOK sections — always reported so the user can see/edit their own
        // keys (used when mode is byok; ignored while a managed bundle is live).
        "llm": {
            "base_url": creds.llm.base_url,
            "model": creds.llm.model,
            "configured": llm_configured,
            "key_hint": if llm_configured { format!("••••{}", last4(key)) } else { String::new() },
            // When the store has no key but `.env` does, the agent runs on the env
            // key — surface that so the UI doesn't look "unconfigured" misleadingly.
            "env_fallback": env_set("AI_API_KEY"),
        },
        "stt": vendor_view(&creds.stt, "VOLCENGINE_STT_API_KEY"),
        "tts": vendor_view(&creds.tts, "VOLCENGINE_TTS_API_KEY"),
        "vision": vendor_view(&creds.vision, "DOUBAO_VISION_API_KEY"),
        "image": vendor_view(&creds.image, "DOUBAO_IMAGE_API_KEY"),
        "video": vendor_view(&creds.video, "DOUBAO_VIDEO_API_KEY"),
    }))
}

/// The LLM section of a settings update. `api_key` is tri-state: omitted → keep the
/// stored key; empty string → clear it (back to `.env` / unconfigured); a value →
/// replace it. So editing the base URL alone never wipes the key.
#[derive(Deserialize)]
pub struct LlmUpdate {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

/// A vendor update. `api_key` is tri-state like the LLM's (absent keeps, "" clears,
/// a value sets). `base_url` / `model` are non-secret overrides: absent keeps the
/// stored value; a value (including "") sets it (empty clears back to the default).
#[derive(Deserialize)]
pub struct VendorUpdate {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// A settings update. Every section is optional — the UI may send only the ones
/// it changed. `mode` switches the credential source (byok / xiaoyuanzhu).
#[derive(Deserialize)]
pub struct CredentialsUpdate {
    #[serde(default)]
    mode: Option<credentials::Mode>,
    #[serde(default)]
    llm: Option<LlmUpdate>,
    #[serde(default)]
    stt: Option<VendorUpdate>,
    #[serde(default)]
    tts: Option<VendorUpdate>,
    #[serde(default)]
    vision: Option<VendorUpdate>,
    #[serde(default)]
    image: Option<VendorUpdate>,
    #[serde(default)]
    video: Option<VendorUpdate>,
}

/// Apply a vendor update in place. Each field is independent and absent-keeps:
/// `api_key` (secret) is set when present; `base_url` / `model` are set when
/// present, with an empty string clearing the override back to the code default.
fn apply_vendor(vk: &mut credentials::VendorKey, upd: Option<VendorUpdate>) {
    let Some(u) = upd else { return };
    if let Some(k) = u.api_key {
        vk.api_key = k.trim().to_string();
    }
    if let Some(b) = u.base_url {
        vk.base_url = b.trim().to_string();
    }
    if let Some(m) = u.model {
        let m = m.trim();
        vk.model = if m.is_empty() { None } else { Some(m.to_string()) };
    }
}

/// Persist the credential store. Always 200; the body's `ok` flag reports success
/// (mirrors the reflex route's convention). `restart_required` is always true —
/// the change takes effect on the next start. Selecting xiaoyuanzhu triggers a
/// broker fetch now (so the UI can show the account) but the running agent still
/// applies the new credentials on restart.
pub async fn post_credentials(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CredentialsUpdate>,
) -> Json<Value> {
    let mut creds = Credentials::load(&state.data_dir);
    let mode_selected = body.mode;
    if let Some(m) = mode_selected {
        creds.mode = m;
    }
    if let Some(llm) = body.llm {
        creds.llm.base_url = llm.base_url.trim().to_string();
        creds.llm.model = llm.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        if let Some(k) = llm.api_key {
            creds.llm.api_key = k.trim().to_string();
        }
    }
    apply_vendor(&mut creds.stt, body.stt);
    apply_vendor(&mut creds.tts, body.tts);
    apply_vendor(&mut creds.vision, body.vision);
    apply_vendor(&mut creds.image, body.image);
    apply_vendor(&mut creds.video, body.video);
    if let Err(e) = creds.save(&state.data_dir) {
        tracing::warn!(error = %e, "failed to save credentials");
        return Json(json!({ "ok": false, "error": e.to_string() }));
    }
    // If the user just selected xiaoyuanzhu, fetch the bundle now so the account
    // shows immediately and is cached for the next restart. Forward the signed-in
    // user's Authentik session token when present (this may be a logged-in browser
    // request) so the broker can mint a `sub`-tier account once that's wired;
    // absent, it authenticates by device id (anonymous `free` tier).
    if matches!(mode_selected, Some(credentials::Mode::Xiaoyuanzhu)) {
        let bearer = state.auth.as_ref().and_then(|a| a.session_bearer(&headers));
        crate::foundation::broker::refresh(&state.data_dir, bearer.as_deref()).await;
    }
    Json(json!({ "ok": true, "restart_required": true }))
}
