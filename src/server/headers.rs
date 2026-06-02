//! Axum extractors for the spec's identity headers.
//!
//! - `X-HI-Scene` names the situation a signal belongs to (the context-isolation
//!   key). It rides both directions: inbound it says "this signal belongs to
//!   scene S", on a GET long-poll it says "stream me scene S's output". On
//!   inbound POSTs it is RECOMMENDED but not required — when absent we default
//!   to the anonymous scene so routing still has something to key on
//!   ([`SceneHeader`]). On GET subscriptions it MUST be present, since defaulting
//!   would silently drain the wrong mailbox ([`RequiredScene`]). (It replaces the
//!   former directional `X-HI-From` / `X-HI-To` pair; a scene is a context, not
//!   an address, so the same token serves both ways.)
//! - `Authorization: Bearer ...` is accepted but not validated in v0.

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;

use crate::types::Scene;

const HDR_SCENE: &str = "x-hi-scene";
const HDR_AUTH: &str = "authorization";

const ANONYMOUS: &str = "anonymous";

/// `X-HI-Scene`. Defaults to `anonymous` when missing or empty.
#[derive(Debug, Clone)]
pub struct SceneHeader(pub Scene);

impl<S> FromRequestParts<S> for SceneHeader
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(value) = parts.headers.get(HDR_SCENE) else {
            return Ok(SceneHeader(Scene(ANONYMOUS.to_string())));
        };
        let s = value
            .to_str()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid X-HI-Scene"))?
            .trim();
        if s.is_empty() {
            return Ok(SceneHeader(Scene(ANONYMOUS.to_string())));
        }
        Ok(SceneHeader(Scene(s.to_owned())))
    }
}

/// `X-HI-Scene` that MUST be present and non-empty. Used by subscribers (the
/// GET long-poll), where silently defaulting to the anonymous scene would drain
/// the wrong mailbox and look like a hang rather than a misconfiguration.
#[derive(Debug, Clone)]
pub struct RequiredScene(pub Scene);

impl<S> FromRequestParts<S> for RequiredScene
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let s = parts
            .headers
            .get(HDR_SCENE)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match s {
            Some(s) => Ok(RequiredScene(Scene(s.to_owned()))),
            None => Err((
                StatusCode::BAD_REQUEST,
                "X-HI-Scene required to name the subscribing scene\n",
            )),
        }
    }
}

/// Optional `Authorization: Bearer ...`. Logged, not validated.
#[derive(Debug, Clone)]
pub struct AuthBearer(pub Option<String>);

impl<S> FromRequestParts<S> for AuthBearer
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(value) = parts.headers.get(HDR_AUTH) else {
            return Ok(AuthBearer(None));
        };
        let s = value
            .to_str()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid Authorization"))?
            .trim();
        let token = s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer "));
        match token {
            Some(t) if !t.is_empty() => {
                tracing::debug!(token = %t, "authorization bearer token (not validated)");
                Ok(AuthBearer(Some(t.to_owned())))
            }
            _ => Ok(AuthBearer(None)),
        }
    }
}
