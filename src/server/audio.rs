//! POST /audio and GET /audio — Step 11 voice channel.
//!
//! Inbound (`POST /audio`): the body bytes are audio; we save them under
//! `data/media/audio/in/<uuid>.<ext>`, transcribe via the configured
//! [`Stt`](crate::voice::Stt), and feed the transcript into the same
//! per-peer routing path that `POST /thought` uses. The journal records a
//! `SignalIn { channel: Audio, body: <transcript>, media_path: Some(path) }`
//! so the router's snapshot can show that this signal arrived as speech
//! while the body remains text-searchable.
//!
//! Outbound (`GET /audio`): subscriber to the reactor's `audio_out` broadcast.
//! A turn's speech arrives as a `Start`/`Frame`*/`End` run; this handler blocks
//! until a `Start` for the subscriber, then streams that turn's frames as one
//! chunked HTTP response until the matching `End`. The client appends the bytes
//! to a single sink and plays — one continuous utterance per response, no
//! per-clip reassembly. After the response closes the client re-GETs for the
//! next turn (same loop shape as the other channels).
//!
//! Capability gating: missing STT → 501 on POST. Missing TTS → no audio events
//! will ever be broadcast; GET /audio blocks forever (same long-poll semantics
//! as the other channels — the request is fine, the agent simply never speaks).

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use chrono::Utc;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::memory::media::{self, Direction};
use crate::server::{AppState, AudioEvent};
use crate::server::headers::{AuthBearer, PeerHeader, ToHeader};
use crate::types::{Channel, JournalEntry, PeerId, Signal};

const DEFAULT_MIME: &str = "audio/wav";

#[derive(Debug, Serialize)]
struct PostAudioAck {
    transcript: String,
    media_path: String,
}

pub async fn post_audio(
    State(state): State<Arc<AppState>>,
    PeerHeader(from): PeerHeader,
    ToHeader(to): ToHeader,
    AuthBearer(auth): AuthBearer,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let stt = match state.stt.clone() {
        Some(stt) => stt,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                "audio capability not configured (set STT_PROVIDER)\n",
            )
                .into_response();
        }
    };

    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "audio body is empty\n").into_response();
    }

    let mime = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_MIME.to_string());
    let ext = mime_to_ext(&mime);

    tracing::info!(
        from = %from,
        to = ?to,
        auth = ?auth,
        mime = %mime,
        bytes = body.len(),
        "POST /audio"
    );

    // 1. Persist the raw bytes so we can replay/audit and so the journal has
    //    a stable reference. We do this before STT so a transcription failure
    //    still leaves the audio on disk.
    let media_path = match media::store_audio(&state.data_dir, Direction::In, ext, &body).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(error = %err, "failed to persist incoming audio");
            return (StatusCode::INTERNAL_SERVER_ERROR, "audio store failed\n").into_response();
        }
    };

    // 2. Transcribe. Errors surface as 502 — the upstream provider failed.
    let transcript = match stt.transcribe(body, &mime).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, media_path = %media_path, "STT transcribe failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("transcription failed: {err}\n"),
            )
                .into_response();
        }
    };

    // 3. Journal + dispatch — identical to thought.rs from this point, except
    //    the channel is Audio and we carry the media_path.
    let ts = Utc::now();
    let signal = Signal {
        channel: Channel::Audio,
        from: from.clone(),
        to: to.clone(),
        body: transcript.clone(),
        ts,
    };
    crate::channel_log::inbound(Channel::Audio, &from, &transcript);
    let entry = JournalEntry::SignalIn {
        ts,
        channel: Channel::Audio,
        from: from.clone(),
        body: transcript.clone(),
        media_path: Some(media_path.clone()),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
        return (StatusCode::SERVICE_UNAVAILABLE, "inbound channel closed\n").into_response();
    }

    let ack = PostAudioAck { transcript, media_path };
    (StatusCode::ACCEPTED, axum::Json(ack)).into_response()
}

/// Whether an event routed to `target` should reach this `subscriber`.
fn routed(target: &Option<PeerId>, subscriber: &Option<PeerId>) -> bool {
    match (target, subscriber) {
        (None, _) => true,
        (Some(t), Some(s)) => t == s,
        (Some(_), None) => true,
    }
}

pub async fn get_audio(
    State(state): State<Arc<AppState>>,
    ToHeader(subscriber): ToHeader,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.audio_out.subscribe();

    tracing::info!(subscriber = ?subscriber, auth = ?auth, "GET /audio long-poll opened");

    // Block until a turn for this subscriber starts. `Start` carries the mime,
    // which must be set before any body byte; Frame/End seen before a Start
    // (we subscribed mid-turn) are skipped — the client re-polls and catches
    // the next turn cleanly.
    let (turn, mime) = loop {
        match rx.recv().await {
            Ok(event) => {
                if !routed(event.to(), &subscriber) {
                    continue;
                }
                if let AudioEvent::Start { turn, mime, .. } = event {
                    break (turn, mime);
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "audio subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    };

    // Stream this turn's frames as a chunked body until its `End`. Frames from
    // any other turn or peer are filtered out, so a response stays bound to the
    // single turn it opened on.
    let stream = futures::stream::unfold(
        (rx, subscriber, turn, false),
        |(mut rx, subscriber, turn, done)| async move {
            if done {
                return None;
            }
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if !routed(event.to(), &subscriber) || event.turn() != turn {
                            continue;
                        }
                        match event {
                            AudioEvent::Frame { bytes, .. } => {
                                return Some((
                                    Ok::<Bytes, std::convert::Infallible>(bytes),
                                    (rx, subscriber, turn, false),
                                ));
                            }
                            AudioEvent::End { .. } => return None,
                            AudioEvent::Start { .. } => continue,
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "audio subscriber lagged mid-turn");
                        continue;
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );

    let mut response = Body::from_stream(stream).into_response();
    if let Ok(val) = HeaderValue::from_str(&mime) {
        response.headers_mut().insert(CONTENT_TYPE, val);
    }
    response
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" => "flac",
        "audio/aac" | "audio/x-aac" => "aac",
        "audio/m4a" | "audio/x-m4a" | "audio/mp4" => "m4a",
        _ => "bin",
    }
}
