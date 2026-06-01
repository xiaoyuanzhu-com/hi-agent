//! The transport adapter — binds the reactor's transport-free outbound
//! vocabulary to the HTTP wire.
//!
//! The reactor is the mind; it emits [`OutboundSignal`]s in human-channel terms
//! ("said this text", "this span of speech", "show this surface") and knows
//! nothing about HTTP. Everything HTTP-shaped lives on this side of the seam:
//! the utterance→response framing of /thought, the `Content-Type` and turn
//! binding of /audio, the broadcast of /surface. This binder is the one place
//! that translates between the two, so swapping HTTP for another wire touches
//! only this file — the reactor and its vocabulary are untouched.
//!
//! It runs as a single task draining the reactor's outbound channel in order,
//! which keeps each peer's signals serialized exactly as the mind produced them.

use chrono::Utc;
use tokio::sync::{broadcast, mpsc};

use crate::reactor::OutboundSignal;
use crate::server::{AudioEvent, SurfaceEvent, ThoughtBus};

/// Drain the reactor's outbound seam and bind each signal to its HTTP carrier.
/// Owns the producing halves of the wire-side broadcasts; runs until the reactor
/// drops `out_tx` (process teardown).
pub(crate) async fn bind_outbound(
    mut rx: mpsc::Receiver<OutboundSignal>,
    thought_bus: ThoughtBus,
    audio_out: broadcast::Sender<AudioEvent>,
    surface_out: broadcast::Sender<SurfaceEvent>,
) {
    while let Some(signal) = rx.recv().await {
        match signal {
            // /thought is buffered per peer (a reply produced with no reader
            // connected is retained, not dropped); end-of-utterance is what
            // closes one streaming GET /thought response.
            OutboundSignal::Text { peer, chunk } => {
                thought_bus.push_chunk(&peer, chunk).await;
            }
            OutboundSignal::TextEnd { peer } => {
                thought_bus.end_utterance(&peer).await;
            }
            // /audio: one utterance's span is one chunked response. The codec
            // becomes the response's Content-Type, set before the first byte;
            // `turn` keeps a handler's response bound to a single utterance so a
            // later span's frames never bleed into an earlier response.
            OutboundSignal::AudioBegin { peer, turn, codec } => {
                let _ = audio_out.send(AudioEvent::Start {
                    to: Some(peer),
                    turn,
                    mime: codec,
                });
            }
            OutboundSignal::AudioFrame { peer, turn, bytes } => {
                let _ = audio_out.send(AudioEvent::Frame {
                    to: Some(peer),
                    turn,
                    bytes,
                });
            }
            OutboundSignal::AudioEnd { peer, turn } => {
                let _ = audio_out.send(AudioEvent::End {
                    to: Some(peer),
                    turn,
                });
            }
            // /surface: a single envelope broadcast the long-poll handler filters
            // by peer.
            OutboundSignal::Surface { peer, envelope } => {
                let _ = surface_out.send(SurfaceEvent {
                    to: Some(peer),
                    envelope,
                    ts: Utc::now(),
                });
            }
        }
    }
    tracing::info!("outbound binder: reactor seam closed; exiting");
}
