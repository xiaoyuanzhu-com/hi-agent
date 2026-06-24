//! Authentication front — gate the browser-facing web app behind an OIDC login.
//!
//! ## What this gives you
//!
//! When `HI_AGENT_AUTH=on`, every browser-facing route (the SPA and its human
//! API channels) requires a logged-in session. The login itself is delegated to
//! an external OpenID Connect provider — in the intended deployment, the
//! Authentik instance at `xiaoyuanzhu.com`, whose login screen offers "scan a
//! WeChat QR". hi-agent is a plain OIDC *relying party*: it never touches
//! WeChat's non-standard OAuth and never mints identity. Authentik owns the
//! WeChat source and auto-creates the account on first scan; we just run the
//! authorization-code dance and set an encrypted session cookie.
//!
//! ## Who is allowed in (owner-gate)
//!
//! hi-agent is single-tenant: one data dir is one agent with one shared
//! memory/soul. So a successful OIDC login is necessary but not sufficient — the
//! identity must also be an *owner*:
//! - `HI_AGENT_OWNERS` set → the identity (sub / preferred_username / email)
//!   must be in that allowlist.
//! - unset → **trust on first use**: the first identity to log in is recorded in
//!   `<data_dir>/auth/owner.json` and is the only one accepted thereafter.
//!
//! ## What is NOT gated
//!
//! Machine/server-to-server callers don't carry browser cookies, so the gate
//! leaves their paths public: the MCP tool endpoint (`/mcp`), the phone-upload
//! carrier (`/api/handoff`, `/up/{token}`, `/api/up/{token}`, `/api/qr`, which
//! are already token-scoped), the unimplemented sense stubs, and `/healthz`. A
//! shared bearer (`HI_AGENT_SERVICE_TOKEN`) additionally bypasses the gate on
//! *any* route, so Feishu and the curl journey-tests keep working.
//!
//! Security-sensitive primitives are delegated to vetted crates: `openidconnect`
//! for discovery / code exchange / ID-token (JWT) verification, and
//! `axum-extra`'s `PrivateCookieJar` (AEAD-encrypted cookies) for the session.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use chrono::Utc;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};

/// Name of the long-lived, encrypted session cookie carrying the logged-in owner.
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

/// Auth parameters, all from the environment (`.env` in dev). When `enabled` is
/// false (the default), the rest is empty and the server runs wide-open exactly
/// as it did before auth existed — so existing dev/test instances are
/// unaffected until an operator opts in with `HI_AGENT_AUTH=on`.
#[derive(Clone)]
pub struct AuthConfig {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    /// Allowed identities (matched against sub / preferred_username / email).
    /// Empty ⇒ trust-on-first-use.
    pub owners: Vec<String>,
    /// Shared bearer that bypasses the browser login on any route (machine
    /// callers, journey-tests). `None` ⇒ no bypass.
    pub service_token: Option<String>,
}

// Hand-written so the client secret and service token never land in logs
// (`Config` derives Debug and is traced at startup).
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("enabled", &self.enabled)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &redact(&self.client_secret))
            .field("redirect_url", &self.redirect_url)
            .field("owners", &self.owners)
            .field("service_token", &self.service_token.as_deref().map(redact))
            .finish()
    }
}

fn redact(s: &str) -> &'static str {
    if s.is_empty() { "<unset>" } else { "<redacted>" }
}

impl AuthConfig {
    /// Auth turned off — the historical wide-open behavior. Used by tests and as
    /// the default when `HI_AGENT_AUTH` is not truthy.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            issuer: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            redirect_url: String::new(),
            owners: Vec::new(),
            service_token: None,
        }
    }

    /// Load from the environment. `HI_AGENT_AUTH` must be truthy
    /// (`on`/`1`/`true`/`yes`) to enable; when enabled, the four OIDC fields are
    /// required and a missing one is a hard startup error (failing closed beats
    /// silently serving unprotected).
    pub fn from_env() -> anyhow::Result<Self> {
        if !env_flag("HI_AGENT_AUTH") {
            return Ok(Self::disabled());
        }
        let issuer = env_req("HI_AGENT_OIDC_ISSUER")?;
        let client_id = env_req("HI_AGENT_OIDC_CLIENT_ID")?;
        let client_secret = env_req("HI_AGENT_OIDC_CLIENT_SECRET")?;
        let redirect_url = env_req("HI_AGENT_OIDC_REDIRECT_URL")?;
        let owners = env_opt("HI_AGENT_OWNERS")
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|x| !x.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let service_token = env_opt("HI_AGENT_SERVICE_TOKEN");
        Ok(Self {
            enabled: true,
            issuer,
            client_id,
            client_secret,
            redirect_url,
            owners,
            service_token,
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
    env_opt(name).with_context(|| format!("{name} is required when HI_AGENT_AUTH=on"))
}

fn env_flag(name: &str) -> bool {
    matches!(
        env_opt(name).as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("on" | "1" | "true" | "yes")
    )
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

/// Shared auth state for the `/auth/*` handlers and the gate middleware. Built
/// only when auth is enabled (see [`AuthState::from_config`]).
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
    /// Where `auth/owner.json` lives (trust-on-first-use record).
    data_dir: PathBuf,
}

impl AuthState {
    /// Build the auth state when enabled, returning `Ok(None)` when disabled so
    /// the caller can leave the router untouched. Fallible: it generates or reads
    /// the cookie key file under `<data_dir>/auth/`.
    pub fn from_config(
        cfg: AuthConfig,
        data_dir: &Path,
    ) -> anyhow::Result<Option<Arc<AuthState>>> {
        if !cfg.enabled {
            return Ok(None);
        }
        let secure = cfg.redirect_url.starts_with("https://");
        let key = load_or_create_key(data_dir).context("loading auth cookie key")?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building OIDC http client")?;
        Ok(Some(Arc::new(AuthState {
            cfg,
            key,
            secure,
            http,
            data_dir: data_dir.to_path_buf(),
        })))
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
            // should so the post-login landing is already authenticated.
            .same_site(SameSite::Lax)
            .secure(self.secure)
            .max_age(time::Duration::seconds(ttl))
            .build()
    }

    fn jar(&self, headers: &HeaderMap) -> PrivateCookieJar {
        PrivateCookieJar::from_headers(headers, self.key.clone())
    }
}

/// A path-`/` removal cookie, matching how the session/flow cookies are set so
/// `PrivateCookieJar::remove` actually clears them in the browser.
fn removal(name: &str) -> Cookie<'static> {
    Cookie::build((name.to_owned(), "")).path("/").build()
}

/// The encrypted session payload. `exp` is a unix timestamp; expiry is checked
/// on every gated request so a stale cookie can't outlive its window even if the
/// browser keeps sending it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    sub: String,
    label: String,
    exp: i64,
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

/// Merge the `/auth/*` routes onto `router` and wrap everything in the gate
/// middleware. The middleware lets public paths and valid service tokens / valid
/// sessions through, and redirects/401s everyone else.
pub fn apply(router: Router, state: Arc<AuthState>) -> Router {
    router.merge(routes(state.clone())).layer(
        axum::middleware::from_fn_with_state(state, require_auth),
    )
}

fn routes(state: Arc<AuthState>) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", get(logout))
        .with_state(state)
}

/// Paths that bypass the login gate entirely: the auth endpoints themselves, the
/// machine/token-scoped carriers, the unimplemented sense stubs, and health.
fn is_public(path: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "/auth/",
        "/mcp",
        "/api/handoff",
        "/up/",
        "/api/up/",
        "/api/qr",
        "/api/in/touch",
        "/api/in/smell",
        "/api/in/taste",
        "/healthz",
    ];
    PREFIXES.iter().any(|p| path == p.trim_end_matches('/') || path.starts_with(p))
}

/// The gate. Order: public path → service token → session cookie → reject.
async fn require_auth(State(st): State<Arc<AuthState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if is_public(path) {
        return next.run(req).await;
    }

    if let Some(expected) = &st.cfg.service_token {
        if bearer(req.headers()).as_deref() == Some(expected.as_str()) {
            return next.run(req).await;
        }
    }

    if let Some(session) = st.jar(req.headers()).get(SESSION_COOKIE) {
        if let Ok(s) = serde_json::from_str::<Session>(session.value()) {
            if s.valid_now() {
                return next.run(req).await;
            }
        }
    }

    // Unauthenticated. A top-level navigation (the browser asking for HTML) gets
    // bounced to login with a sanitized return path; an XHR/asset/API request
    // gets a plain 401 so the SPA can react without following an opaque redirect.
    if wants_html(req.headers()) {
        let next_to = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        let url = format!("/auth/login?next={}", encode_param(next_to));
        Redirect::to(&url).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "authentication required\n").into_response()
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"))
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
/// code, verify the ID token, enforce the owner-gate, and on success set the
/// session cookie and land the user back where they came from.
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

    let code = q
        .code
        .ok_or_else(|| AppError::user("Missing authorization code."))?;
    let state = q
        .state
        .ok_or_else(|| AppError::user("Missing state parameter."))?;
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
    let username = claims
        .preferred_username()
        .map(|u| u.as_str().to_string());
    let email = claims
        .email()
        .map(|e| e.as_str().to_string());
    let label = username
        .clone()
        .or_else(|| email.clone())
        .unwrap_or_else(|| sub.clone());

    let ids: Vec<String> = std::iter::once(sub.clone())
        .chain(username)
        .chain(email)
        .collect();
    if !authorize_owner(&st.cfg, &st.data_dir, &ids, &sub, &label)? {
        return Ok(message_page(
            StatusCode::FORBIDDEN,
            "Not authorized",
            "This account is not an owner of this hi-agent instance.",
        ));
    }

    let session = Session {
        sub,
        label,
        exp: Utc::now().timestamp() + SESSION_TTL_SECS,
    };
    let jar = jar
        .remove(removal(FLOW_COOKIE))
        .add(st.session_cookie(&session)?);
    Ok((jar, Redirect::to(&safe_next(Some(&flow.next)))).into_response())
}

/// `GET /auth/logout` — drop the local session and return to the app. (Does not
/// trigger IdP-side single-logout in v1.)
async fn logout(State(st): State<Arc<AuthState>>, headers: HeaderMap) -> Response {
    let jar = st.jar(&headers).remove(removal(SESSION_COOKIE));
    (jar, Redirect::to("/")).into_response()
}

// ---------------------------------------------------------------------------
// Owner-gate
// ---------------------------------------------------------------------------

/// Stored trust-on-first-use record: the single identity bound to this instance.
#[derive(Debug, Serialize, Deserialize)]
struct OwnerRecord {
    sub: String,
    label: String,
}

/// Decide whether a freshly-authenticated identity may use this instance.
/// - explicit allowlist (`owners`) → membership test over its candidate ids.
/// - empty allowlist → trust-on-first-use against `<data_dir>/auth/owner.json`.
fn authorize_owner(
    cfg: &AuthConfig,
    data_dir: &Path,
    ids: &[String],
    sub: &str,
    label: &str,
) -> anyhow::Result<bool> {
    if !cfg.owners.is_empty() {
        return Ok(ids.iter().any(|id| cfg.owners.iter().any(|o| o == id)));
    }

    let path = data_dir.join("auth").join("owner.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let rec: OwnerRecord =
                serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
            Ok(rec.sub == sub)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let rec = OwnerRecord {
                sub: sub.to_string(),
                label: label.to_string(),
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&rec)?)
                .with_context(|| format!("writing {}", path.display()))?;
            tracing::info!(owner = %label, "bound instance owner on first login (TOFU)");
            Ok(true)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sanitize a post-login redirect target to a same-origin absolute path,
/// defending against open-redirect (`//evil.com`, `https://evil.com`). Anything
/// that isn't a clean `/path` collapses to `/`.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n.to_string(),
        _ => "/".to_string(),
    }
}

/// Percent-encode a query-parameter value (RFC 3986 unreserved set passes
/// through; everything else is `%`-escaped). Small and dependency-free.
fn encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Minimal HTML page for terminal sign-in outcomes (errors, not-authorized).
fn message_page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\"><title>{title}</title>\
         <style>body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;\
         margin:0;padding:64px 24px;display:flex;justify-content:center}}main{{max-width:480px}}\
         a{{color:inherit}}</style></head><body><main><h1>{title}</h1><p>{body}</p>\
         <p><a href=\"/auth/login\">Try again</a></p></main></body></html>"
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

    fn cfg_with_owners(owners: &[&str]) -> AuthConfig {
        AuthConfig {
            enabled: true,
            issuer: "https://idp.example/".into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            redirect_url: "https://app.example/auth/callback".into(),
            owners: owners.iter().map(|s| s.to_string()).collect(),
            service_token: None,
        }
    }

    #[test]
    fn disabled_config_is_wide_open() {
        let cfg = AuthConfig::disabled();
        assert!(!cfg.enabled);
        assert!(cfg.owners.is_empty());
        assert!(AuthState::from_config(cfg, Path::new("/tmp")).unwrap().is_none());
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut cfg = cfg_with_owners(&["alice"]);
        cfg.client_secret = "s3cr3t-client-value".into();
        cfg.service_token = Some("s3cr3t-service-token".into());
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("s3cr3t-client-value"), "client secret leaked: {rendered}");
        assert!(!rendered.contains("s3cr3t-service-token"), "service token leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn public_paths_classified() {
        for p in [
            "/auth/login",
            "/auth/callback",
            "/mcp",
            "/api/handoff",
            "/up/abc",
            "/api/up/abc",
            "/api/qr",
            "/api/in/touch",
            "/healthz",
        ] {
            assert!(is_public(p), "expected public: {p}");
        }
        for p in ["/", "/api/in/text", "/api/out/text", "/views/x.mjs", "/assets/a.js"] {
            assert!(!is_public(p), "expected gated: {p}");
        }
    }

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer tok123".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("tok123"));
        h.insert(header::AUTHORIZATION, "bearer tok123".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("tok123"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }

    #[test]
    fn safe_next_blocks_open_redirect() {
        assert_eq!(safe_next(Some("/inspect/sessions")), "/inspect/sessions");
        assert_eq!(safe_next(Some("//evil.com")), "/");
        assert_eq!(safe_next(Some("https://evil.com")), "/");
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn encode_param_escapes() {
        assert_eq!(encode_param("/a/b"), "%2Fa%2Fb");
        assert_eq!(encode_param("plain-._~"), "plain-._~");
    }

    #[test]
    fn session_expiry_check() {
        let fresh = Session { sub: "s".into(), label: "l".into(), exp: Utc::now().timestamp() + 100 };
        let stale = Session { sub: "s".into(), label: "l".into(), exp: Utc::now().timestamp() - 1 };
        assert!(fresh.valid_now());
        assert!(!stale.valid_now());
    }

    #[test]
    fn owner_allowlist_matches_any_id() {
        let cfg = cfg_with_owners(&["alice@example.com"]);
        let dir = tempfile::tempdir().unwrap();
        let ids = vec!["sub-123".to_string(), "alice@example.com".to_string()];
        assert!(authorize_owner(&cfg, dir.path(), &ids, "sub-123", "alice").unwrap());

        let ids_no = vec!["sub-999".to_string(), "bob@example.com".to_string()];
        assert!(!authorize_owner(&cfg, dir.path(), &ids_no, "sub-999", "bob").unwrap());
    }

    #[test]
    fn owner_tofu_binds_first_then_rejects_others() {
        let cfg = cfg_with_owners(&[]); // empty ⇒ TOFU
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("auth")).unwrap();

        // First login binds.
        assert!(authorize_owner(&cfg, dir.path(), &["sub-1".into()], "sub-1", "owner").unwrap());
        // Same identity again: accepted.
        assert!(authorize_owner(&cfg, dir.path(), &["sub-1".into()], "sub-1", "owner").unwrap());
        // A different identity: rejected.
        assert!(!authorize_owner(&cfg, dir.path(), &["sub-2".into()], "sub-2", "intruder").unwrap());
    }

    #[test]
    fn cookie_key_persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = load_or_create_key(dir.path()).unwrap();
        let k2 = load_or_create_key(dir.path()).unwrap();
        assert_eq!(k1.master(), k2.master(), "key must be stable across restarts");
    }
}
