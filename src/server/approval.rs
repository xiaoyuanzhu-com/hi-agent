//! POST /approval and GET /approval — Step 7 bridge to ACP `session/request_permission`.
//!
//! Round-trip per impl.md § "Approval":
//! 1. Any ACP session calls `session/request_permission`.
//! 2. The reactor's handler builds an `ApprovalEvent`, journals it, parks a
//!    oneshot, and broadcasts the event on `approval_out`.
//! 3. A `GET /approval` long-poll subscriber whose `X-HI-To` matches the
//!    event's peer receives it and renders.
//! 4. The deciding peer's client `POST /approval` with `{id, allow, reason}`.
//! 5. The POST handler sends an `ApprovalDecision` through the
//!    `approval_decisions` mpsc seam; the reactor consumes from it, looks the
//!    id up in `pending_approvals`, journals the decision, and resolves the
//!    parked oneshot — which unblocks the ACP handler, which returns the
//!    `RequestPermissionResponse`.
//!
//! Approval is global, not channel-scoped (impl.md): any peer with an open
//! `/approval` long-poll may be the decider, addressed by `X-HI-To`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::oneshot;

use crate::server::AppState;
use crate::server::headers::{AuthBearer, PeerHeader, ToHeader};
use crate::types::{ApprovalId, PeerId};

/// What we broadcast to GET /approval subscribers. Carries enough structure
/// for the deciding peer's client to render a meaningful prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalEvent {
    pub id: ApprovalId,
    pub peer: PeerId,
    pub action: String,
    pub summary: String,
    pub details: serde_json::Value,
    pub requested: chrono::DateTime<chrono::Utc>,
}

/// Decision payload flowing from POST /approval into the reactor.
///
/// Carries a oneshot the reactor uses to ack: `true` if the id matched a
/// pending approval, `false` otherwise. The POST handler awaits this so it
/// can return 202 or 404 per impl.md § Step 7.
#[derive(Debug)]
pub struct ApprovalDecision {
    pub id: ApprovalId,
    pub allow: bool,
    pub reason: Option<String>,
    pub decided_by: PeerId,
    pub ack: oneshot::Sender<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ApprovalDecisionBody {
    pub id: ApprovalId,
    pub allow: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn post_approval(
    State(state): State<Arc<AppState>>,
    PeerHeader(from): PeerHeader,
    AuthBearer(auth): AuthBearer,
    Json(body): Json<ApprovalDecisionBody>,
) -> impl IntoResponse {
    tracing::info!(
        from = %from,
        auth = ?auth,
        id = %body.id,
        allow = body.allow,
        reason = ?body.reason,
        "POST /approval"
    );

    let (ack_tx, ack_rx) = oneshot::channel::<bool>();
    let approval_id = body.id;
    let decision = ApprovalDecision {
        id: body.id,
        allow: body.allow,
        reason: body.reason,
        decided_by: from,
        ack: ack_tx,
    };

    if let Err(err) = state.approval_decisions.send(decision).await {
        tracing::warn!(error = %err, "approval decisions channel closed");
        return (StatusCode::SERVICE_UNAVAILABLE, "reactor unavailable").into_response();
    }

    match ack_rx.await {
        Ok(true) => StatusCode::ACCEPTED.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no pending approval with that id").into_response(),
        Err(_) => {
            tracing::warn!(id = %approval_id, "approval ack oneshot dropped");
            (StatusCode::INTERNAL_SERVER_ERROR, "ack lost").into_response()
        }
    }
}

pub async fn get_approval(
    State(state): State<Arc<AppState>>,
    ToHeader(subscriber): ToHeader,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.approval_out.subscribe();
    tracing::info!(subscriber = ?subscriber, auth = ?auth, "GET /approval long-poll opened");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let deliver = match &subscriber {
                    Some(sub) => event.peer == *sub,
                    None => true,
                };
                if !deliver {
                    continue;
                }
                return (StatusCode::OK, Json(event)).into_response();
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "approval subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed").into_response();
            }
        }
    }
}
