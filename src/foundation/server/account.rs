//! `/api/account/*` — the device account's energy status and a signed-in upgrade
//! link. Small, public (no gate; see [`super::build`]) endpoints the out-of-energy
//! hint calls: one to know whether to show (and the reset time), one to open the
//! account page already signed in as this account.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};

use crate::foundation::broker;
use crate::foundation::credentials::{get_setting, set_setting, Credentials, KEY_SERVER_PORT};
use crate::foundation::server::AppState;

/// `app_settings` key for the one-shot CSRF nonce guarding the account-link
/// callback. Minted at `/account/link/start`, checked (and cleared) at the callback.
const KEY_LINK_NONCE: &str = "link_nonce";

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

/// Query for the account-link callback: the device-ticket the site hands back plus
/// the CSRF nonce that round-tripped through the browser.
#[derive(serde::Deserialize, Default)]
pub struct LinkCallbackQuery {
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    state: String,
}

/// `GET /account/link/start` — begin linking this device to a web account. Mints a
/// one-shot CSRF nonce and 302s the browser to the site's account page, passing a
/// loopback callback + the nonce; the site (once signed in) mints a device-ticket
/// and redirects it back to the callback below. The native Settings "Sign in" button
/// opens this so the nonce/secret never lives in the native layer.
pub async fn get_link_start(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let nonce = uuid::Uuid::now_v7().to_string();
    let _ = set_setting(&state.data_dir, KEY_LINK_NONCE, &nonce);
    let port = get_setting(&state.data_dir, KEY_SERVER_PORT)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(port) = port else {
        // Port unknown (shouldn't happen once the server has started) — degrade to
        // the plain account page rather than a dead end.
        tracing::warn!("account link: server port unknown; opening the plain account page");
        return Redirect::to("https://hi.xiaoyuanzhu.com/account");
    };
    // The callback is loopback (127.0.0.1) so the browser connects over the loop and
    // the callback's peer check passes; the site validates it's a loopback target
    // before handing back a ticket. No query-delimiter chars, so it needs no encoding.
    let callback = format!("http://127.0.0.1:{port}/account/link/callback");
    let url = format!("{}/account?link_device={}&state={}", broker::public_base_url(), callback, nonce);
    Redirect::to(&url)
}

/// `GET /account/link/callback?ticket=&state=` — the site hands a device-ticket back
/// here (loopback only). Verify the nonce, redeem the ticket at the broker to adopt
/// the signed-in account, and show a small result page.
pub async fn get_link_callback(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<LinkCallbackQuery>,
) -> impl IntoResponse {
    // Loopback only: this endpoint can switch the device's account, so a LAN peer
    // must never reach it even though the server binds 0.0.0.0 for the app's web UI.
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Html(link_page("Blocked", "This link can only be opened on this computer.")),
        )
            .into_response();
    }
    // CSRF: accept only the nonce we minted at /account/link/start, and consume it
    // (one-shot) whether or not it matches.
    let expected = get_setting(&state.data_dir, KEY_LINK_NONCE).unwrap_or_default();
    let _ = set_setting(&state.data_dir, KEY_LINK_NONCE, "");
    if q.state.trim().is_empty() || q.state != expected {
        return (
            StatusCode::FORBIDDEN,
            Html(link_page("Link expired", "Please start again from Hi Agent’s Settings.")),
        )
            .into_response();
    }

    match broker::claim_device(&state.data_dir, q.ticket.trim()).await {
        Ok(broker::ClaimOutcome::Adopted { email }) => {
            let who = if email.trim().is_empty() { "your account".to_string() } else { esc(&email) };
            Html(link_page(
                "Signed in",
                &format!("Hi Agent is now signed in as {who}. You can return to the app."),
            ))
            .into_response()
        }
        Ok(broker::ClaimOutcome::Conflict { code }) => {
            let msg = match code.as_str() {
                "keep_current" => "This device is already signed in to a recoverable account, so it wasn’t switched.",
                "chooser_required" => "This device is signed in to a different secured account. Switching between two secured accounts isn’t supported yet.",
                _ => "This account couldn’t be linked.",
            };
            (StatusCode::CONFLICT, Html(link_page("Not linked", msg))).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "account link: claim failed");
            (
                StatusCode::BAD_GATEWAY,
                Html(link_page("Couldn’t link", "Something went wrong linking the app. Please try again from Settings.")),
            )
                .into_response()
        }
    }
}

/// Minimal HTML result page for the account-link callback.
fn link_page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title></head><body>\
         <div style=\"font-family:-apple-system,system-ui,sans-serif;max-width:28rem;\
         margin:20vh auto;padding:0 1.5rem;text-align:center\">\
         <h1 style=\"font-size:1.25rem;margin:0 0 .5rem\">{title}</h1>\
         <p style=\"color:#555;line-height:1.5\">{body}</p></div></body></html>"
    )
}

/// Escape the few HTML-significant chars — used for the account email echoed into
/// the result page (broker-sourced, so treated as untrusted text).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

