//! `/api/settings/*` — the config/energy/mode boundary the Settings UI (native or
//! web) drives as a thin client of the engine, so the engine stays the sole authority
//! over `config.db`. This reintroduces the HTTP config surface the tray refactor
//! removed (see [`super::AppState::auth`]), but **loopback-gated** and **secret-safe**:
//!
//! - Every handler rejects non-loopback peers ([`loopback_guard`]) — the server binds
//!   `0.0.0.0` and has no global auth, and these routes read/write credentials, so a
//!   LAN peer must never reach them (same stance as [`super::account::get_link_callback`]).
//! - The read surface returns `configured: bool` (+ non-secret `base_url`/`model`) per
//!   feature and **never the `api_key`** — the projected DTOs below are distinct types,
//!   not `Serialize` on [`Credentials`] (which holds keys inline).
//!
//! Design note: [docs/core-shell-config-api.md]. Scope is request/response config only;
//! the streaming perceive/act protocol is a separate (Phase 2) object.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::foundation::config::{
    self, KEY_GESTURES, KEY_LANGUAGE, KEY_THEME, LANGUAGES, THEMES,
};
use crate::foundation::credentials::{self, Credentials, Energy, Mode};
use crate::foundation::energy_state;
use crate::foundation::server::AppState;

/// The BYOK features, keyed by the stored credential-field name. Cross-platform (the
/// macOS `Feature` enum lives in a `vendors/macos_*` file we deliberately don't depend
/// on here). Face/voiceprint are local ONNX with no key, so they're absent.
const FEATURES: &[&str] = &["llm", "stt", "tts", "vision", "image", "video"];

/// The product's About link. Static; not user config.
const WEBSITE: &str = "https://hi.xiaoyuanzhu.com";

// --- projected, secret-free response DTOs -----------------------------------

#[derive(Serialize)]
struct SettingsSnapshot {
    appearance: AppearanceState,
    account: AccountState,
    about: AboutState,
}

#[derive(Serialize)]
struct AppearanceState {
    theme: ChoiceSetting,
    language: ChoiceSetting,
    gestures: FlagSetting,
}

/// A picker setting: current value + the offered options + whether a change applies
/// `live` or on `restart`, so the UI can tell the truth instead of guessing.
#[derive(Serialize)]
struct ChoiceSetting {
    value: String,
    options: Vec<Choice>,
    applies: &'static str,
}

#[derive(Serialize)]
struct Choice {
    value: String,
    label: String,
}

#[derive(Serialize)]
struct FlagSetting {
    value: bool,
    applies: &'static str,
}

#[derive(Serialize)]
struct AccountState {
    mode: Mode, // serializes lowercase ("byok" | "xiaoyuanzhu")
    identity: IdentityState,
    /// `None` in BYOK mode / before any energy snapshot exists.
    energy: Option<EnergySnapshot>,
    features: Vec<FeatureStatus>,
}

#[derive(Serialize)]
struct IdentityState {
    signed_in: bool,
    name: Option<String>,
    email: Option<String>,
}

#[derive(Serialize)]
struct EnergySnapshot {
    tier: String,
    remaining: i64,
    total: i64,
    resets_at: String,
    out_of_energy: bool,
}

/// A feature's BYOK config **without the key** — only whether one is set, plus the
/// non-secret base URL / model.
#[derive(Serialize)]
struct FeatureStatus {
    feature: String,
    configured: bool,
    base_url: Option<String>,
    model: Option<String>,
}

#[derive(Serialize)]
struct AboutState {
    version: String,
    website: String,
}

// --- request bodies ---------------------------------------------------------

/// Partial appearance write: any field omitted (`None`) is left unchanged.
#[derive(Deserialize, Default)]
pub(crate) struct AppearancePatch {
    theme: Option<String>,
    language: Option<String>,
    gestures: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct ModePatch {
    mode: Mode,
}

/// Partial per-feature write. `api_key`: a non-empty value replaces; an omitted or
/// **blank** value keeps the existing key (so the UI never has to re-enter it, and a
/// blank field can't wipe it). `base_url`/`model`: `None` keeps, `Some("")` clears.
#[derive(Deserialize, Default)]
pub(crate) struct FeaturePatch {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
}

// --- handlers ---------------------------------------------------------------

/// `GET /api/settings` — the whole snapshot the Settings window needs in one read.
pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(rejected) = loopback_guard(&peer) {
        return rejected;
    }
    Json(snapshot(&state.data_dir)).into_response()
}

/// `PUT /api/settings/appearance` — theme / language / gestures (partial).
pub(crate) async fn put_appearance(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(patch): Json<AppearancePatch>,
) -> Response {
    if let Some(rejected) = loopback_guard(&peer) {
        return rejected;
    }
    match set_appearance(&state.data_dir, &patch) {
        Ok(state_dto) => Json(state_dto).into_response(),
        Err(e) => store_error(e),
    }
}

/// `PUT /api/settings/mode` — select the active credential mode.
pub(crate) async fn put_mode(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(patch): Json<ModePatch>,
) -> Response {
    if let Some(rejected) = loopback_guard(&peer) {
        return rejected;
    }
    match set_mode(&state.data_dir, patch.mode) {
        Ok(()) => Json(serde_json::json!({ "mode": patch.mode })).into_response(),
        Err(e) => store_error(e),
    }
}

/// `PUT /api/settings/credentials/{feature}` — set a BYOK key/base_url/model.
pub(crate) async fn put_feature(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    UrlPath(feature): UrlPath<String>,
    Json(patch): Json<FeaturePatch>,
) -> Response {
    if let Some(rejected) = loopback_guard(&peer) {
        return rejected;
    }
    if !FEATURES.contains(&feature.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown_feature", "feature": feature })),
        )
            .into_response();
    }
    match set_feature(&state.data_dir, &feature, &patch) {
        Ok(status) => Json(status).into_response(),
        Err(e) => store_error(e),
    }
}

/// `POST /api/account/energy/refresh` — force a broker energy poll and return the
/// fresh snapshot (`null` in BYOK / when the poll can't run).
pub async fn post_energy_refresh(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(rejected) = loopback_guard(&peer) {
        return rejected;
    }
    let fresh = crate::foundation::broker::poll_energy_now(&state.data_dir).await;
    let energy = fresh.map(|e| energy_snapshot(e));
    Json(serde_json::json!({ "energy": energy })).into_response()
}

// --- core logic (pure, unit-tested without a live server) -------------------

/// Reject a non-loopback peer with `403`; `None` if the peer is loopback. The server
/// binds `0.0.0.0`, so this per-handler check is the only thing keeping a LAN client
/// off the credential surface.
fn loopback_guard(peer: &SocketAddr) -> Option<Response> {
    if peer.ip().is_loopback() {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "loopback_only" })),
            )
                .into_response(),
        )
    }
}

/// Map a config-store write failure to a `500` (the store is local; a failure here is
/// an engine problem, not a bad request).
fn store_error(e: anyhow::Error) -> Response {
    tracing::warn!(error = %e, "settings: config store write failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "store_write_failed" })),
    )
        .into_response()
}

fn snapshot(data_dir: &Path) -> SettingsSnapshot {
    let creds = Credentials::load(data_dir);
    SettingsSnapshot {
        appearance: appearance_state(data_dir),
        account: AccountState {
            mode: creds.mode,
            identity: identity_state(&creds),
            energy: account_energy(&creds),
            features: FEATURES
                .iter()
                .filter_map(|f| feature_status(&creds, f))
                .collect(),
        },
        about: AboutState {
            version: env!("CARGO_PKG_VERSION").to_string(),
            website: WEBSITE.to_string(),
        },
    }
}

fn appearance_state(data_dir: &Path) -> AppearanceState {
    let theme = credentials::get_setting(data_dir, KEY_THEME).unwrap_or_else(|| "system".into());
    let language =
        credentials::get_setting(data_dir, KEY_LANGUAGE).unwrap_or_else(|| "system".into());
    let gestures = config::flag_on(credentials::get_setting(data_dir, KEY_GESTURES));
    AppearanceState {
        // Theme applies live (the shell re-applies NSAppearance on change); language
        // and gestures are boot-time decisions, so they apply on restart.
        theme: ChoiceSetting { value: theme, options: choices(THEMES), applies: "live" },
        language: ChoiceSetting {
            value: language,
            options: choices(LANGUAGES),
            applies: "restart",
        },
        gestures: FlagSetting { value: gestures, applies: "restart" },
    }
}

fn choices(pairs: &[(&str, &str)]) -> Vec<Choice> {
    pairs
        .iter()
        .map(|(v, l)| Choice { value: (*v).to_string(), label: (*l).to_string() })
        .collect()
}

fn identity_state(creds: &Credentials) -> IdentityState {
    match &creds.identity {
        Some(id) => IdentityState {
            signed_in: true,
            name: non_empty(&id.name),
            email: non_empty(&id.email),
        },
        None => IdentityState { signed_in: false, name: None, email: None },
    }
}

/// Energy for the snapshot: only meaningful in xiaoyuanzhu mode (BYOK has no broker
/// balance), and only when a snapshot has been polled.
fn account_energy(creds: &Credentials) -> Option<EnergySnapshot> {
    if creds.mode != Mode::Xiaoyuanzhu {
        return None;
    }
    creds.energy.clone().map(energy_snapshot)
}

fn energy_snapshot(e: Energy) -> EnergySnapshot {
    EnergySnapshot {
        tier: e.tier,
        remaining: e.remaining,
        total: e.total,
        resets_at: e.resets_at,
        out_of_energy: energy_state::is_out(),
    }
}

/// A feature's BYOK status without the key. `None` for an unknown feature key.
fn feature_status(creds: &Credentials, feature: &str) -> Option<FeatureStatus> {
    let (base_url, api_key, model) = match feature {
        "llm" => (&creds.llm.base_url, &creds.llm.api_key, &creds.llm.model),
        "stt" => (&creds.stt.base_url, &creds.stt.api_key, &creds.stt.model),
        "tts" => (&creds.tts.base_url, &creds.tts.api_key, &creds.tts.model),
        "vision" => (&creds.vision.base_url, &creds.vision.api_key, &creds.vision.model),
        "image" => (&creds.image.base_url, &creds.image.api_key, &creds.image.model),
        "video" => (&creds.video.base_url, &creds.video.api_key, &creds.video.model),
        _ => return None,
    };
    Some(FeatureStatus {
        feature: feature.to_string(),
        configured: !api_key.trim().is_empty(),
        base_url: non_empty(base_url),
        model: model.as_deref().and_then(non_empty),
    })
}

fn set_appearance(data_dir: &Path, patch: &AppearancePatch) -> anyhow::Result<AppearanceState> {
    if let Some(theme) = &patch.theme {
        credentials::set_setting(data_dir, KEY_THEME, theme.trim())?;
    }
    if let Some(language) = &patch.language {
        credentials::set_setting(data_dir, KEY_LANGUAGE, language.trim())?;
    }
    if let Some(gestures) = patch.gestures {
        credentials::set_setting(data_dir, KEY_GESTURES, if gestures { "on" } else { "off" })?;
    }
    Ok(appearance_state(data_dir))
}

fn set_mode(data_dir: &Path, mode: Mode) -> anyhow::Result<()> {
    let mut creds = Credentials::load(data_dir);
    creds.mode = mode;
    creds.save(data_dir)
}

fn set_feature(
    data_dir: &Path,
    feature: &str,
    patch: &FeaturePatch,
) -> anyhow::Result<FeatureStatus> {
    let mut creds = Credentials::load(data_dir);
    {
        // llm and VendorKey differ in extra fields but share (base_url, api_key, model).
        let (base_url, api_key, model): (&mut String, &mut String, &mut Option<String>) =
            match feature {
                "llm" => (&mut creds.llm.base_url, &mut creds.llm.api_key, &mut creds.llm.model),
                "stt" => (&mut creds.stt.base_url, &mut creds.stt.api_key, &mut creds.stt.model),
                "tts" => (&mut creds.tts.base_url, &mut creds.tts.api_key, &mut creds.tts.model),
                "vision" => {
                    (&mut creds.vision.base_url, &mut creds.vision.api_key, &mut creds.vision.model)
                }
                "image" => {
                    (&mut creds.image.base_url, &mut creds.image.api_key, &mut creds.image.model)
                }
                "video" => {
                    (&mut creds.video.base_url, &mut creds.video.api_key, &mut creds.video.model)
                }
                _ => anyhow::bail!("unknown feature: {feature}"),
            };
        write_key_fields(base_url, api_key, model, patch);
    }
    creds.save(data_dir)?;
    feature_status(&creds, feature).ok_or_else(|| anyhow::anyhow!("unknown feature: {feature}"))
}

/// Apply a [`FeaturePatch`] in place: a non-empty `api_key` replaces (blank keeps);
/// `base_url`/`model` set when present (`Some("")` clears), keep when absent.
fn write_key_fields(
    base_url: &mut String,
    api_key: &mut String,
    model: &mut Option<String>,
    patch: &FeaturePatch,
) {
    if let Some(k) = patch.api_key.as_deref() {
        let k = k.trim();
        if !k.is_empty() {
            *api_key = k.to_string();
        }
    }
    if let Some(b) = &patch.base_url {
        *base_url = b.trim().to_string();
    }
    if let Some(m) = &patch.model {
        let m = m.trim();
        *model = if m.is_empty() { None } else { Some(m.to_string()) };
    }
}

/// A trimmed non-empty copy, else `None`.
fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_defaults_on_fresh_store() {
        let dir = tempfile::tempdir().unwrap();
        let snap = snapshot(dir.path());
        assert_eq!(snap.appearance.theme.value, "system");
        assert_eq!(snap.appearance.theme.applies, "live");
        assert_eq!(snap.appearance.language.applies, "restart");
        assert!(!snap.appearance.gestures.value);
        assert_eq!(snap.account.mode, Mode::Xiaoyuanzhu);
        assert!(!snap.account.identity.signed_in);
        // No broker snapshot yet on a fresh store.
        assert!(snap.account.energy.is_none());
        // All six features present, none configured.
        assert_eq!(snap.account.features.len(), FEATURES.len());
        assert!(snap.account.features.iter().all(|f| !f.configured));
    }

    #[test]
    fn appearance_writes_persist_and_normalize() {
        let dir = tempfile::tempdir().unwrap();
        let patch = AppearancePatch {
            theme: Some(" dark ".into()),
            gestures: Some(true),
            ..Default::default()
        };
        let state = set_appearance(dir.path(), &patch).unwrap();
        assert_eq!(state.theme.value, "dark"); // trimmed
        assert!(state.gestures.value);
        // Persisted for the next read.
        assert_eq!(snapshot(dir.path()).appearance.theme.value, "dark");
        assert!(snapshot(dir.path()).appearance.gestures.value);
    }

    #[test]
    fn mode_write_persists() {
        let dir = tempfile::tempdir().unwrap();
        set_mode(dir.path(), Mode::Byok).unwrap();
        assert_eq!(snapshot(dir.path()).account.mode, Mode::Byok);
    }

    #[test]
    fn feature_write_sets_key_but_never_leaks_it() {
        let dir = tempfile::tempdir().unwrap();
        let patch = FeaturePatch {
            api_key: Some("sk-secret".into()),
            base_url: Some(" https://example.test ".into()),
            model: Some("gpt-x".into()),
            ..Default::default()
        };
        let status = set_feature(dir.path(), "vision", &patch).unwrap();
        assert!(status.configured);
        assert_eq!(status.base_url.as_deref(), Some("https://example.test"));
        assert_eq!(status.model.as_deref(), Some("gpt-x"));
        // The projected DTO carries no key field at all — serialize and confirm.
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("sk-secret"), "status must not leak the api_key");
        assert!(!json.contains("api_key"));
    }

    #[test]
    fn blank_key_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        set_feature(dir.path(), "stt", &FeaturePatch { api_key: Some("sk-1".into()), ..Default::default() })
            .unwrap();
        // A blank key must not wipe the stored one; base_url still updates.
        let status = set_feature(
            dir.path(),
            "stt",
            &FeaturePatch { api_key: Some("   ".into()), base_url: Some("https://b".into()), ..Default::default() },
        )
        .unwrap();
        assert!(status.configured, "blank key should keep the existing key");
        assert_eq!(status.base_url.as_deref(), Some("https://b"));
    }

    #[test]
    fn unknown_feature_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(set_feature(dir.path(), "telepathy", &FeaturePatch::default()).is_err());
    }
}
