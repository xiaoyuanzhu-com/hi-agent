//! GET /stt/stream — live streaming speech-to-text over WebSocket.
//!
//! The SPA streams the mic in real time here instead of POSTing one finished
//! WAV: it opens this socket on speech onset, sends raw 16 kHz mono 16-bit LE
//! PCM as binary frames, then a `"end"` text frame (or closes) when VAD detects
//! the end of the utterance. We proxy the audio to the configured streaming
//! [`Stt`](crate::voice::Stt) and push each transcript update back as a JSON
//! text frame `{ "text": "...", "final": <bool> }` — the fast rolling
//! preliminary first, the polished final last.
//!
//! On the final result we replicate `POST /audio`'s journaling: persist the
//! accumulated PCM as a WAV, append a `SignalIn { channel: Audio }`, and
//! dispatch the signal so the reactor produces the agent's reply. Browsers
//! can't set custom headers on a WebSocket, so the peer identity arrives as a
//! `?peer=<id>` query param rather than the `X-HI-From` header.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::memory::media::{self, Direction};
use crate::server::AppState;
use crate::types::{Channel, JournalEntry, PeerId, Signal};
use crate::voice::stt::Transcript;

const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Peer identity (sent as the `from` on the resulting signal). Optional so a
    /// bare probe still upgrades; defaults to "anon".
    #[serde(default)]
    peer: Option<String>,
}

pub async fn get_stt_stream(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StreamQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let Some(stt) = state.stt.clone() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "audio capability not configured (set STT_PROVIDER)\n",
        )
            .into_response();
    };
    let peer = PeerId(q.peer.unwrap_or_else(|| "anon".to_string()));
    ws.on_upgrade(move |socket| drive(socket, state, stt, peer))
}

async fn drive(
    socket: WebSocket,
    state: Arc<AppState>,
    stt: Arc<dyn crate::voice::Stt>,
    peer: PeerId,
) {
    let (audio_tx, audio_rx) = mpsc::channel::<Bytes>(64);
    let (tr_tx, mut tr_rx) = mpsc::channel::<Transcript>(64);
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Recognition runs as its own task so reading audio and emitting transcripts
    // proceed concurrently.
    let stt_task = tokio::spawn(async move { stt.transcribe_streaming(audio_rx, tr_tx).await });

    // Forward each transcript update to the browser as JSON. Ends when the STT
    // task drops `tr_tx` (i.e. recognition finished).
    let log_peer = peer.clone();
    let send_task = tokio::spawn(async move {
        while let Some(t) = tr_rx.recv().await {
            // Rolling preliminaries are noisy → debug; the polished final is the
            // turn's input and is logged on the `channel` stream below.
            if t.is_final {
                tracing::debug!(target: "channel", peer = %log_peer, text = %t.text, "stt final (pre-journal)");
            } else {
                tracing::debug!(target: "channel", peer = %log_peer, text = %t.text, "stt partial");
            }
            let frame = serde_json::json!({ "text": t.text, "final": t.is_final }).to_string();
            if ws_tx.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    // Pump inbound PCM to the recognizer while keeping a copy for journaling.
    let mut pcm = Vec::new();
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(b) => {
                pcm.extend_from_slice(&b);
                if audio_tx.send(b).await.is_err() {
                    break; // recognizer gone
                }
            }
            Message::Text(t) if t.as_str().trim() == "end" => break,
            Message::Close(_) => break,
            _ => {}
        }
    }
    drop(audio_tx); // closes the recognizer's input → upstream finalizes

    let final_text = match stt_task.await {
        Ok(Ok(text)) => text,
        Ok(Err(err)) => {
            tracing::warn!(peer = %peer, error = %err, "streaming STT failed");
            String::new()
        }
        Err(err) => {
            tracing::warn!(peer = %peer, error = %err, "streaming STT task panicked");
            String::new()
        }
    };
    let _ = send_task.await;

    if final_text.trim().is_empty() {
        return; // nothing recognized (silence / dropped) — no signal to dispatch
    }

    journal_and_dispatch(&state, &peer, final_text, &pcm).await;
}

/// Persist the utterance audio and feed the transcript into the same per-peer
/// routing path `POST /audio` uses, so the agent replies exactly as before.
async fn journal_and_dispatch(state: &Arc<AppState>, peer: &PeerId, transcript: String, pcm: &[u8]) {
    let media_path = match media::store_audio(&state.data_dir, Direction::In, "wav", &pcm_to_wav(pcm)).await {
        Ok(p) => Some(p),
        Err(err) => {
            tracing::error!(error = %err, "failed to persist streamed audio");
            None
        }
    };

    crate::channel_log::inbound(Channel::Audio, peer, &transcript);
    let ts = Utc::now();
    let entry = JournalEntry::SignalIn {
        ts,
        channel: Channel::Audio,
        from: peer.clone(),
        body: transcript.clone(),
        media_path: media_path.clone(),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; dispatching signal anyway");
    }
    let signal = Signal {
        channel: Channel::Audio,
        from: peer.clone(),
        to: None,
        body: transcript,
        ts,
    };
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed; dropping streamed signal");
    }
}

/// Wrap raw 16 kHz mono 16-bit LE PCM in a 44-byte RIFF/WAVE header.
fn pcm_to_wav(pcm: &[u8]) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = SAMPLE_RATE * 2; // mono, 16-bit
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}
