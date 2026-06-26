//! BYOK settings routes — read/write the vendor credential store.
//!
//! `GET /api/settings/credentials` reports the configured state **without ever
//! returning the key** (only a last-4 hint), plus whether `.env` provides a
//! fallback. `POST` writes the store via [`Credentials::save`]. Persist-only: the
//! running agent baked its env at startup, so the response flags
//! `restart_required` — a restart re-resolves the store. These routes are
//! browser-facing, so they sit behind the OIDC login gate when auth is enabled
//! (only the owner should set keys).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::foundation::credentials::Credentials;
use crate::foundation::server::AppState;

/// Last 4 chars of a key, UTF-8-safe (keys are ASCII, but don't assume it).
fn last4(s: &str) -> String {
    let mut tail: Vec<char> = s.chars().rev().take(4).collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// Report the credential state for the Settings UI. Never returns the raw key.
pub async fn get_credentials(State(state): State<Arc<AppState>>) -> Json<Value> {
    let creds = Credentials::load(&state.data_dir);
    let key = creds.llm.api_key.trim();
    let configured = !key.is_empty();
    let env_fallback = std::env::var("AI_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    Json(json!({
        "llm": {
            "base_url": creds.llm.base_url,
            "model": creds.llm.model,
            "configured": configured,
            "key_hint": if configured { format!("••••{}", last4(key)) } else { String::new() },
            // When the store has no key but `.env` does, the agent runs on the env
            // key — surface that so the UI doesn't look "unconfigured" misleadingly.
            "env_fallback": env_fallback,
        }
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

#[derive(Deserialize)]
pub struct CredentialsUpdate {
    llm: LlmUpdate,
}

/// Persist the credential store. Always 200; the body's `ok` flag reports success
/// (mirrors the reflex route's convention). `restart_required` is always true —
/// the change takes effect on the next start.
pub async fn post_credentials(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CredentialsUpdate>,
) -> Json<Value> {
    let mut creds = Credentials::load(&state.data_dir);
    creds.llm.base_url = body.llm.base_url.trim().to_string();
    creds.llm.model = body
        .llm
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    if let Some(k) = body.llm.api_key {
        creds.llm.api_key = k.trim().to_string();
    }
    match creds.save(&state.data_dir) {
        Ok(()) => Json(json!({
            "ok": true,
            "restart_required": true,
            "configured": !creds.llm.api_key.trim().is_empty(),
        })),
        Err(e) => {
            tracing::warn!(error = %e, "failed to save credentials");
            Json(json!({ "ok": false, "error": e.to_string() }))
        }
    }
}
