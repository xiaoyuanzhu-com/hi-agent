//! `/api/account/*` — the device account's energy status and a signed-in upgrade
//! link. Small, public (no gate; see [`super::build`]) endpoints the out-of-energy
//! hint calls: one to know whether to show (and the reset time), one to open the
//! account page already signed in as this account.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::foundation::credentials::Credentials;
use crate::foundation::server::AppState;

/// `GET /api/account/energy` — whether the account is currently out of energy, plus
/// the reset time, for the out-of-energy hint. `out_of_energy` is the live vendor
/// flag ([`crate::foundation::energy_state`]) — raised the instant a 402 flips us,
/// dropped the instant the balance refills — so the web app can poll it to show/hide
/// the hint. `resets_in` is the humanized wait ("约 42 分钟后") from the cached
/// balance the broker keeps fresh.
pub async fn get_energy(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let energy = Credentials::load(&state.data_dir).energy.unwrap_or_default();
    axum::Json(serde_json::json!({
        "out_of_energy": crate::foundation::energy_state::is_out(),
        "tier": energy.tier,
        "resets_at": energy.resets_at,
        "resets_in": crate::body::reactor::humanize_until_reset(&energy.resets_at),
    }))
}

/// `GET /api/account/subscribe` — mint a one-time web-handoff ticket and return the
/// browser URL that lands the user on the **account** page **already signed in as this
/// device account** (the same handoff the tray's Subscribe uses). The hint's 升级
/// button opens the returned `url` in a new tab. On failure it degrades to the plain
/// (not-signed-in) account page so the button is never a dead end.
pub async fn get_subscribe(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match crate::foundation::broker::subscribe_url(&state.data_dir, Some("/account")).await {
        Ok(url) => (StatusCode::OK, axum::Json(serde_json::json!({ "url": url, "signed_in": true }))),
        Err(e) => {
            tracing::warn!(error = %e, "GET /api/account/subscribe: could not mint a signed-in link");
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "url": "https://hi.xiaoyuanzhu.com/account",
                    "signed_in": false,
                })),
            )
        }
    }
}
