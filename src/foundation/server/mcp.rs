//! HTTP glue for the MCP tool endpoint.
//!
//! Binds the MCP "Streamable HTTP" transport to the reactor's tool carrier
//! ([`crate::foundation::mcp`]). A POST carries one JSON-RPC message; we route by the
//! `X-HI-Scene`/`X-HI-Role`/`X-HI-Worker-Id` headers a session's MCP attach sets
//! (see `agent::AgentLayer::session`). A request gets a single `application/json`
//! response; a notification gets `202`. We push no server-initiated messages, so
//! the optional GET SSE stream is declined with `405`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;

use crate::foundation::config::{HEADER_ROLE, HEADER_SCENE, HEADER_WORKER_ID};
use crate::foundation::mcp::{self, McpReply};
use crate::foundation::server::AppState;
use crate::types::Scene;

/// One MCP message over POST. Parses the JSON-RPC body, resolves the routing
/// identity from headers, and returns either a JSON-RPC response or an empty 202.
pub async fn post_mcp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let scene = header(HEADER_SCENE).filter(|s| !s.is_empty()).map(Scene);
    let role = header(HEADER_ROLE);
    let worker_id = header(HEADER_WORKER_ID).and_then(|v| v.parse::<u64>().ok());

    let msg: Value = match serde_json::from_slice(body.as_ref()) {
        Ok(v) => v,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON-RPC body: {err}"))
                .into_response();
        }
    };

    match mcp::handle(&state.tool_registry, &state.data_dir, &state.video_in_partial, scene, role.as_deref(), worker_id, &msg).await {
        McpReply::Json(value) => Json(value).into_response(),
        McpReply::Accepted => StatusCode::ACCEPTED.into_response(),
    }
}

/// The optional server→client SSE stream — declined; we never push to the agent.
pub async fn get_mcp() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}
