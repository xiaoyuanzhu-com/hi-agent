//! Axum extractors for the spec's identity headers.
//!
//! - `X-HI-From` is RECOMMENDED but not required. When absent we default to
//!   the anonymous peer so per-peer routing still has something to key on.
//! - `X-HI-To` is optional, on POSTs (addresses an outbound to a peer) and on
//!   GET long-polls (subscriber identifies themselves).
//! - `Authorization: Bearer ...` is accepted but not validated in v0.

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;

use crate::types::PeerId;

const HDR_FROM: &str = "x-hi-from";
const HDR_TO: &str = "x-hi-to";
const HDR_AUTH: &str = "authorization";

const ANONYMOUS: &str = "anonymous";

/// `X-HI-From`. Defaults to `anonymous` when missing or empty.
#[derive(Debug, Clone)]
pub struct PeerHeader(pub PeerId);

impl<S> FromRequestParts<S> for PeerHeader
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(value) = parts.headers.get(HDR_FROM) else {
            return Ok(PeerHeader(PeerId(ANONYMOUS.to_string())));
        };
        let s = value
            .to_str()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid X-HI-From"))?
            .trim();
        if s.is_empty() {
            return Ok(PeerHeader(PeerId(ANONYMOUS.to_string())));
        }
        Ok(PeerHeader(PeerId(s.to_owned())))
    }
}

/// Optional `X-HI-To`.
#[derive(Debug, Clone)]
pub struct ToHeader(pub Option<PeerId>);

impl<S> FromRequestParts<S> for ToHeader
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(value) = parts.headers.get(HDR_TO) else {
            return Ok(ToHeader(None));
        };
        let s = value
            .to_str()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid X-HI-To"))?
            .trim();
        if s.is_empty() {
            Ok(ToHeader(None))
        } else {
            Ok(ToHeader(Some(PeerId(s.to_owned()))))
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
