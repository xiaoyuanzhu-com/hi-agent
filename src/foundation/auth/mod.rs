//! Xiaoyuanzhu account sign-in — an optional OIDC login for the instance owner.
//!
//! ## What this is (and is NOT)
//!
//! hi-agent has **no access gate**. The agent's face (`/` and its `/api/in/*` ·
//! `/api/out/*` channels), the owner's Settings page, the inspect console — all of
//! it is served without a login. A hi-agent instance is single-tenant: it *is*
//! one person's agent, and whoever can reach the URL can use it. If you expose it
//! on a shared network and want to keep the control surface private, put it behind
//! your own reverse proxy / VPN — that's an operator concern, not something the
//! app gates.
//!
//! The only thing this module does is let the owner **sign in to their
//! xiaoyuanzhu account**, so the broker can (in future) mint a paid `sub`-tier
//! account instead of the anonymous `free` one. It's surfaced as a single opt-in
//! action in Settings; it never gates anything. When the OIDC vars are unset (the
//! default), sign-in is simply unavailable and the instance runs on the free tier.
//!
//! The login is delegated to an external OpenID Connect provider — in the intended
//! deployment, the Authentik instance at `account.xiaoyuanzhu.com`, whose login
//! screen offers "scan a WeChat QR". hi-agent is a plain OIDC *relying party*: it
//! never touches WeChat's non-standard OAuth and never mints identity. Authentik
//! owns the WeChat source and auto-creates the account on first scan; we run the
//! authorization-code dance and set an encrypted session cookie holding the
//! provider access token, which the settings handler forwards to the broker.
//!
//! Security-sensitive primitives are delegated to vetted crates: `openidconnect`
//! for discovery / code exchange / ID-token (JWT) verification, and
//! `axum-extra`'s `PrivateCookieJar` (AEAD-encrypted cookies) for the session.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use chrono::Utc;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};

/// Name of the long-lived, encrypted session cookie carrying the signed-in owner.
const SESSION_COOKIE: &str = "hi_session";
/// Name of the short-lived, encrypted cookie holding in-flight OAuth state
/// (PKCE verifier, nonce, CSRF token, post-login destination) between the
/// `/auth/login` redirect and the `/auth/callback` return.
const FLOW_COOKIE: &str = "hi_oauth";
/// How long a session stays valid before the browser is bounced through the IdP
/// again. The IdP can of course revoke sooner; this only bounds our cookie.
const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;
/// Generous bound on how long the login round-trip (user scanning a QR on their
/// phone, approving) may take before the flow cookie expires.
const FLOW_TTL_SECS: i64 = 15 * 60;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// OIDC parameters for the owner sign-in, from the environment (`.env` in dev).
/// When `enabled` is false (the default — no `HI_AGENT_OIDC_ISSUER`), sign-in is
/// unavailable and the instance runs on the anonymous free tier.
#[derive(Clone)]
pub struct AuthConfig {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
}

// Hand-written so the client secret never lands in logs (`Config` derives Debug
// and is traced at startup).
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("enabled", &self.enabled)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &redact(&self.client_secret))
            .field("redirect_url", &self.redirect_url)
            .finish()
    }
}

fn redact(s: &str) -> &'static str {
    if s.is_empty() { "<unset>" } else { "<redacted>" }
}

impl AuthConfig {
    /// Sign-in disabled — no OIDC configured. The default; the instance uses the
    /// free tier and Settings shows no sign-in action.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            issuer: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            redirect_url: String::new(),
        }
    }

    /// Load from the environment. Presence of `HI_AGENT_OIDC_ISSUER` opts the
    /// owner sign-in in; when set, the other three OIDC fields are required and a
    /// missing one is a hard startup error (a half-configured sign-in would only
    /// fail confusingly at click time). Absent issuer → sign-in disabled.
    pub fn from_env() -> anyhow::Result<Self> {
        let Some(issuer) = env_opt("HI_AGENT_OIDC_ISSUER") else {
            return Ok(Self::disabled());
        };
        let client_id = env_req("HI_AGENT_OIDC_CLIENT_ID")?;
        let client_secret = env_req("HI_AGENT_OIDC_CLIENT_SECRET")?;
        let redirect_url = env_req("HI_AGENT_OIDC_REDIRECT_URL")?;
        Ok(Self {
            enabled: true,
            issuer,
            client_id,
            client_secret,
            redirect_url,
        })
    }
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_req(name: &str) -> anyhow::Result<String> {
    env_opt(name).with_context(|| format!("{name} is required when HI_AGENT_OIDC_ISSUER is set"))
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

/// Shared state for the `/auth/*` handlers. Built only when sign-in is enabled
/// (see [`AuthState::from_config`]).
pub struct AuthState {
    cfg: AuthConfig,
    /// AEAD key for the encrypted session/flow cookies. Persisted under the data
    /// dir so sessions survive a restart.
    key: Key,
    /// Whether to mark cookies `Secure` (HTTPS-only). Derived from the redirect
    /// URL's scheme so a dev instance on `http://localhost` still receives them.
    secure: bool,
    /// SSRF-hardened HTTP client for OIDC discovery / token exchange (no
    /// redirects — an OIDC endpoint must never bounce us elsewhere).
    http: reqwest::Client,
}

impl AuthState {
    /// Build the auth state when sign-in is enabled, returning `Ok(None)` when
    /// disabled so the caller can leave the router untouched. Fallible: it
    /// generates or reads the cookie key file under `<data_dir>/auth/`.
    pub fn from_config(cfg: AuthConfig, data_dir: &Path) -> anyhow::Result<Option<Arc<AuthState>>> {
        if !cfg.enabled {
            return Ok(None);
        }
        let secure = cfg.redirect_url.starts_with("https://");
        let key = load_or_create_key(data_dir).context("loading auth cookie key")?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building OIDC http client")?;
        Ok(Some(Arc::new(AuthState { cfg, key, secure, http })))
    }

    /// Discover the provider's OIDC metadata (and its signing keys). Done per
    /// login/callback rather than cached so a restarted IdP or rotated keys are
    /// picked up without restarting hi-agent; logins are rare enough that the
    /// extra round-trip is immaterial.
    async fn discover(&self) -> anyhow::Result<CoreProviderMetadata> {
        let issuer = IssuerUrl::new(self.cfg.issuer.clone()).context("invalid OIDC issuer URL")?;
        CoreProviderMetadata::discover_async(issuer, &self.http)
            .await
            .context("OIDC discovery failed")
    }

    fn session_cookie(&self, session: &Session) -> anyhow::Result<Cookie<'static>> {
        let value = serde_json::to_string(session).context("serializing session")?;
        Ok(self.base_cookie(SESSION_COOKIE, value, SESSION_TTL_SECS))
    }

    fn flow_cookie(&self, flow: &Flow) -> anyhow::Result<Cookie<'static>> {
        let value = serde_json::to_string(flow).context("serializing oauth flow")?;
        Ok(self.base_cookie(FLOW_COOKIE, value, FLOW_TTL_SECS))
    }

    fn base_cookie(&self, name: &str, value: String, ttl: i64) -> Cookie<'static> {
        Cookie::build((name.to_owned(), value))
            .path("/")
            .http_only(true)
            // Lax (not Strict) so the cookie rides the top-level GET redirect
            // back from the IdP; the flow cookie must, and the session cookie
            // should so the post-login landing already sees the signed-in state.
            .same_site(SameSite::Lax)
            .secure(self.secure)
            .max_age(time::Duration::seconds(ttl))
            .build()
    }

    fn jar(&self, headers: &HeaderMap) -> PrivateCookieJar {
        PrivateCookieJar::from_headers(headers, self.key.clone())
    }

    /// The signed-in owner's provider access token from a valid session cookie, for
    /// forwarding to the broker (xiaoyuanzhu `sub` tier). `None` when there is no
    /// session, it's expired, or it carries no token (an older cookie).
    pub fn session_bearer(&self, headers: &HeaderMap) -> Option<String> {
        let s = self.session(headers)?;
        let t = s.access_token.trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    /// The signed-in owner's display label from a valid session cookie, for the
    /// Settings account card ("signed in as …"). `None` when signed out/expired.
    pub fn session_identity(&self, headers: &HeaderMap) -> Option<String> {
        self.session(headers).map(|s| s.label)
    }

    /// The decoded, still-valid session from the cookie jar, if any.
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let cookie = self.jar(headers).get(SESSION_COOKIE)?;
        let s: Session = serde_json::from_str(cookie.value()).ok()?;
        s.valid_now().then_some(s)
    }
}

/// A path-`/` removal cookie, matching how the session/flow cookies are set so
/// `PrivateCookieJar::remove` actually clears them in the browser.
fn removal(name: &str) -> Cookie<'static> {
    Cookie::build((name.to_owned(), "")).path("/").build()
}

/// The encrypted session payload. `exp` is a unix timestamp; expiry is re-checked
/// whenever the session is read, so a stale cookie can't outlive its window even
/// if the browser keeps sending it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    sub: String,
    label: String,
    exp: i64,
    /// The provider's access token, kept so the agent can forward it to the broker
    /// (xiaoyuanzhu `sub` tier) on the owner's behalf. `default` so older cookies
    /// still load (they just carry no token → no `sub` upgrade until next sign-in).
    #[serde(default)]
    access_token: String,
}

impl Session {
    fn valid_now(&self) -> bool {
        self.exp > Utc::now().timestamp()
    }
}

/// In-flight OAuth state, parked in the flow cookie between login and callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Flow {
    pkce_verifier: String,
    nonce: String,
    csrf: String,
    next: String,
}

/// Generate (or read back) the 64-byte AEAD key for cookie encryption, persisted
/// at `<data_dir>/auth/cookie.key` with owner-only permissions. A stable key
/// means sessions survive restarts.
fn load_or_create_key(data_dir: &Path) -> anyhow::Result<Key> {
    let dir = data_dir.join("auth");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("cookie.key");
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() >= 64 => Ok(Key::from(&bytes)),
        Ok(_) => {
            // Too short to be a valid key (truncated write?) — regenerate.
            write_new_key(&path)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_new_key(&path),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_new_key(path: &Path) -> anyhow::Result<Key> {
    let key = Key::generate();
    std::fs::write(path, key.master()).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(key)
}

// ---------------------------------------------------------------------------
// Router wiring
// ---------------------------------------------------------------------------

/// Merge the `/auth/*` sign-in routes onto `router`. No gate/middleware layer —
/// these routes are the sign-in flow itself; everything on the server is public.
pub fn mount(router: Router, state: Arc<AuthState>) -> Router {
    router.merge(routes(state))
}

fn routes(state: Arc<AuthState>) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", get(logout))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

/// `GET /auth/login` — start the authorization-code flow: stash PKCE/nonce/CSRF
/// in the flow cookie and redirect to the IdP's authorization endpoint (which,
/// at Authentik, renders the WeChat QR).
async fn login(
    State(st): State<Arc<AuthState>>,
    Query(q): Query<LoginQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let meta = st.discover().await?;
    let redirect =
        RedirectUrl::new(st.cfg.redirect_url.clone()).context("invalid OIDC redirect URL")?;
    let client = CoreClient::from_provider_metadata(
        meta,
        ClientId::new(st.cfg.client_id.clone()),
        Some(ClientSecret::new(st.cfg.client_secret.clone())),
    )
    .set_redirect_uri(redirect);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .add_scope(Scope::new("email".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    let flow = Flow {
        pkce_verifier: pkce_verifier.secret().clone(),
        nonce: nonce.secret().clone(),
        csrf: csrf.secret().clone(),
        next: safe_next(q.next.as_deref()),
    };
    let jar = st.jar(&headers).add(st.flow_cookie(&flow)?);
    Ok((jar, Redirect::to(auth_url.as_str())).into_response())
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /auth/callback` — the IdP redirect target: verify CSRF, exchange the
/// code, verify the ID token, and on success set the session cookie (carrying the
/// provider access token for the broker) and land the owner back where they came
/// from. There is no owner allowlist — whoever runs the instance signs in as
/// themselves; the session only marks "this owner linked their xiaoyuanzhu account".
async fn callback(
    State(st): State<Arc<AuthState>>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(err) = q.error {
        let desc = q.error_description.unwrap_or_default();
        return Ok(message_page(
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            &format!("The identity provider returned an error: {err}. {desc}"),
        ));
    }

    let jar = st.jar(&headers);
    let flow: Flow = jar
        .get(FLOW_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok())
        .ok_or_else(|| AppError::user("Your sign-in session expired. Please try again."))?;

    let code = q.code.ok_or_else(|| AppError::user("Missing authorization code."))?;
    let state = q.state.ok_or_else(|| AppError::user("Missing state parameter."))?;
    if state != flow.csrf {
        return Err(AppError::user("State mismatch — possible CSRF; sign-in aborted."));
    }

    let meta = st.discover().await?;
    let redirect =
        RedirectUrl::new(st.cfg.redirect_url.clone()).context("invalid OIDC redirect URL")?;
    let client = CoreClient::from_provider_metadata(
        meta,
        ClientId::new(st.cfg.client_id.clone()),
        Some(ClientSecret::new(st.cfg.client_secret.clone())),
    )
    .set_redirect_uri(redirect);
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .context("exchange_code not configured")?
        .set_pkce_verifier(PkceCodeVerifier::new(flow.pkce_verifier))
        .request_async(&st.http)
        .await
        .context("OIDC token exchange failed")?;

    let id_token = token
        .id_token()
        .ok_or_else(|| AppError::internal("provider returned no ID token"))?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(flow.nonce))
        .context("ID token verification failed")?;

    let sub = claims.subject().as_str().to_string();
    let label = claims
        .preferred_username()
        .map(|u| u.as_str().to_string())
        .or_else(|| claims.email().map(|e| e.as_str().to_string()))
        .unwrap_or_else(|| sub.clone());

    let session = Session {
        sub,
        label,
        exp: Utc::now().timestamp() + SESSION_TTL_SECS,
        // Kept for the broker (xiaoyuanzhu `sub` tier). Note it expires far sooner
        // than the session — fine for the right-after-login fetch; a refresh-token
        // flow to keep it fresh long-term is future work.
        access_token: token.access_token().secret().clone(),
    };
    let jar = jar.remove(removal(FLOW_COOKIE)).add(st.session_cookie(&session)?);
    Ok((jar, Redirect::to(&safe_next(Some(&flow.next)))).into_response())
}

/// `GET /auth/logout` — drop the local session and return to the app. (Does not
/// trigger IdP-side single-logout in v1.)
async fn logout(State(st): State<Arc<AuthState>>, headers: HeaderMap) -> Response {
    let jar = st.jar(&headers).remove(removal(SESSION_COOKIE));
    (jar, Redirect::to("/settings")).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sanitize a post-login redirect target to a same-origin absolute path,
/// defending against open-redirect (`//evil.com`, `https://evil.com`). Anything
/// that isn't a clean `/path` collapses to `/settings` (where sign-in starts).
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n.to_string(),
        _ => "/settings".to_string(),
    }
}

/// Minimal HTML page for terminal sign-in outcomes (errors).
fn message_page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\"><title>{title}</title>\
         <style>body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;\
         margin:0;padding:64px 24px;display:flex;justify-content:center}}main{{max-width:480px}}\
         a{{color:inherit}}</style></head><body><main><h1>{title}</h1><p>{body}</p>\
         <p><a href=\"/settings\">Back to Settings</a></p></main></body></html>"
    );
    (status, Html(html)).into_response()
}

/// Error type for the auth handlers: anything fallible maps to a friendly HTML
/// page rather than a bare 500, since these are user-facing browser flows.
enum AppError {
    /// A user-correctable problem (expired flow, bad state) — 400 with the message.
    User(String),
    /// An internal/provider failure — 500, logged, with a generic message.
    Internal(anyhow::Error),
}

impl AppError {
    fn user(msg: &str) -> Self {
        AppError::User(msg.to_string())
    }
    fn internal(msg: &str) -> Self {
        AppError::Internal(anyhow::anyhow!(msg.to_string()))
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Internal(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::User(msg) => message_page(StatusCode::BAD_REQUEST, "Sign-in problem", &msg),
            AppError::Internal(err) => {
                tracing::error!(error = %err, "auth flow error");
                message_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Sign-in error",
                    "Something went wrong signing you in. Please try again.",
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AuthConfig {
        AuthConfig {
            enabled: true,
            issuer: "https://idp.example/".into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            redirect_url: "https://app.example/auth/callback".into(),
        }
    }

    #[test]
    fn disabled_config_builds_no_state() {
        let cfg = AuthConfig::disabled();
        assert!(!cfg.enabled);
        assert!(AuthState::from_config(cfg, Path::new("/tmp")).unwrap().is_none());
    }

    #[test]
    fn debug_redacts_client_secret() {
        let mut c = cfg();
        c.client_secret = "s3cr3t-client-value".into();
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("s3cr3t-client-value"), "client secret leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn safe_next_blocks_open_redirect() {
        assert_eq!(safe_next(Some("/settings")), "/settings");
        assert_eq!(safe_next(Some("//evil.com")), "/settings");
        assert_eq!(safe_next(Some("https://evil.com")), "/settings");
        assert_eq!(safe_next(None), "/settings");
    }

    #[test]
    fn session_expiry_check() {
        let fresh = Session { sub: "s".into(), label: "l".into(), exp: Utc::now().timestamp() + 100, access_token: String::new() };
        let stale = Session { sub: "s".into(), label: "l".into(), exp: Utc::now().timestamp() - 1, access_token: String::new() };
        assert!(fresh.valid_now());
        assert!(!stale.valid_now());
    }

    #[test]
    fn cookie_key_persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = load_or_create_key(dir.path()).unwrap();
        let k2 = load_or_create_key(dir.path()).unwrap();
        assert_eq!(k1.master(), k2.master(), "key must be stable across restarts");
    }
}
