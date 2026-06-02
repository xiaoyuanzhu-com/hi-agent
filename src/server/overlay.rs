//! POST /api/overlay and GET /api/overlay — the non-voice overlay channel.
//!
//! The overlay is a continuous *output* channel that any local party can write,
//! not just the reactor. Its whole reason to exist is to carry worker-driven
//! visual data — face rects at ~15 fps, a live chart, anything a delegated
//! working session computes — to the UI without routing it through the single
//! reactor voice.
//!
//! That separation is structural, not advisory. `post_overlay` broadcasts
//! straight onto `overlay_out`; it never touches the reactor's `OutboundSignal`
//! binder and never enters a cognition turn. So a worker writing 15 frames a
//! second cannot contend with — or fragment — the one serialized voice that
//! `/api/thought`, `/api/audio`, and `/api/surface` carry. Speech stays the
//! reactor's; continuous data rides here.
//!
//! Inbound (`POST /api/overlay`): raw body bytes → one `OverlayEvent`, scene
//! from `X-HI-Scene`. Returns 202. The payload is opaque (a JSON line by
//! convention, e.g. `{"rects":[...]}`); the host does not parse it.
//!
//! Outbound (`GET /api/overlay`): unlike surface's one-shot long-poll, an
//! overlay is animated, so this is a *continuous* chunked NDJSON stream — it
//! stays open and emits one `payload + "\n"` per matching event. The browser
//! reads it line by line and repaints. Scene-filtered; a mismatched scene
//! receives nothing.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;

use crate::server::headers::{AuthBearer, RequiredScene, SceneHeader};
use crate::server::{AppState, OverlayEvent};
use crate::types::Scene;

const NDJSON_MIME: &str = "application/x-ndjson";

pub async fn post_overlay(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "overlay body is empty\n").into_response();
    }

    tracing::debug!(scene = %scene, bytes = body.len(), "POST /api/overlay");

    // Broadcast directly — outside the reactor turn loop (see module docs). A
    // send error just means no subscriber is connected, which is fine.
    let _ = state.overlay_out.send(OverlayEvent {
        scene: Some(scene),
        payload: body,
        ts: Utc::now(),
    });

    StatusCode::ACCEPTED.into_response()
}

/// Whether an event routed to `target` should reach this `scene` subscriber.
/// `None` is a broadcast to every scene.
fn routed(target: &Option<Scene>, scene: &Scene) -> bool {
    match target {
        None => true,
        Some(t) => t == scene,
    }
}

/// `GET /api/overlay` — continuous NDJSON stream of overlay frames for a scene.
///
/// Stays open and emits one JSON line per matching event. Mirrors the chunked
/// `Body::from_stream` shape of [`crate::server::audio::get_audio`], minus the
/// per-turn binding — an overlay stream is not turn-scoped.
pub async fn get_overlay(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let rx = state.overlay_out.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /overlay stream opened");

    let stream = futures::stream::unfold((rx, scene), |(mut rx, scene)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !routed(&event.scene, &scene) {
                        continue;
                    }
                    // One NDJSON record: the opaque payload then a newline.
                    let mut line = Vec::with_capacity(event.payload.len() + 1);
                    line.extend_from_slice(&event.payload);
                    line.push(b'\n');
                    return Some((
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(line)),
                        (rx, scene),
                    ));
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "overlay subscriber lagged");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });

    let mut response = Body::from_stream(stream).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(NDJSON_MIME));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Scene;
    use tokio::sync::broadcast;

    fn scene(s: &str) -> Scene {
        Scene(s.to_string())
    }

    // The scene filter is the load-bearing routing decision for both the GET
    // handlers; assert deliver-on-match, drop-on-mismatch, and broadcast-on-None.
    #[test]
    fn routed_matches_same_scene_and_broadcast_only() {
        let s = scene("alice@phone");
        assert!(routed(&Some(scene("alice@phone")), &s), "same scene delivers");
        assert!(!routed(&Some(scene("bob@tv")), &s), "other scene is dropped");
        assert!(routed(&None, &s), "None broadcasts to every scene");
    }

    // A payload posted to one scene round-trips intact to a subscriber of that
    // scene, and is invisible to a subscriber of a different scene.
    #[tokio::test]
    async fn payload_round_trips_to_matching_scene_only() {
        let (tx, _) = broadcast::channel::<OverlayEvent>(16);
        let mut alice = tx.subscribe();
        let mut bob = tx.subscribe();

        let payload = Bytes::from_static(br#"{"rects":[{"x":10,"y":10,"w":50,"h":50}]}"#);
        tx.send(OverlayEvent {
            scene: Some(scene("alice@phone")),
            payload: payload.clone(),
            ts: Utc::now(),
        })
        .unwrap();

        let got = alice.recv().await.unwrap();
        assert!(routed(&got.scene, &scene("alice@phone")));
        assert_eq!(got.payload, payload, "payload round-trips byte-for-byte");

        let got_bob = bob.recv().await.unwrap();
        assert!(
            !routed(&got_bob.scene, &scene("bob@tv")),
            "bob's scene must not match alice's frame"
        );
    }
}
