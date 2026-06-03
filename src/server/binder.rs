//! The transport adapter — binds the reactor's transport-free outbound
//! vocabulary to the HTTP wire.
//!
//! The reactor is the mind; it emits [`OutboundSignal`]s in human-channel terms
//! ("said this text", "this span of speech", "show this surface") and knows
//! nothing about HTTP. Everything HTTP-shaped lives on this side of the seam:
//! the utterance→response framing of /out/text, the `Content-Type` and turn
//! binding of /out/audio, the broadcast of /out/surface. This binder is the one place
//! that translates between the two, so swapping HTTP for another wire touches
//! only this file — the reactor and its vocabulary are untouched.
//!
//! It runs as a single task draining the reactor's outbound channel in order,
//! which keeps each scene's signals serialized exactly as the mind produced them.

use chrono::Utc;
use tokio::sync::{broadcast, mpsc};

use crate::reactor::OutboundSignal;
use crate::server::{AudioEvent, OutputEcho, SurfaceEvent, TextBus};
use crate::types::Channel;

/// Drain the reactor's outbound seam and bind each signal to its HTTP carrier.
/// Owns the producing halves of the wire-side broadcasts; runs until the reactor
/// drops `out_tx` (process teardown).
pub(crate) async fn bind_outbound(
    mut rx: mpsc::Receiver<OutboundSignal>,
    text_bus: TextBus,
    audio_out: broadcast::Sender<AudioEvent>,
    surface_out: broadcast::Sender<SurfaceEvent>,
    output_echo: broadcast::Sender<OutputEcho>,
) {
    while let Some(signal) = rx.recv().await {
        match signal {
            // /out/text is buffered per scene (a reply produced with no reader
            // connected is retained, not dropped); end-of-utterance is what
            // closes one streaming GET /out/text response.
            OutboundSignal::Text { scene, chunk } => {
                // Echo a non-draining copy for observers before the bus consumes it.
                let _ = output_echo.send(OutputEcho {
                    scene: scene.clone(),
                    channel: Channel::Text,
                    text: chunk.clone(),
                    is_final: false,
                    ts: Utc::now(),
                });
                text_bus.push_chunk(&scene, chunk).await;
            }
            OutboundSignal::TextEnd { scene } => {
                let _ = output_echo.send(OutputEcho {
                    scene: scene.clone(),
                    channel: Channel::Text,
                    text: String::new(),
                    is_final: true,
                    ts: Utc::now(),
                });
                text_bus.end_utterance(&scene).await;
            }
            // /audio: one utterance's span is one chunked response. The codec
            // becomes the response's Content-Type, set before the first byte;
            // `turn` keeps a handler's response bound to a single utterance so a
            // later span's frames never bleed into an earlier response.
            OutboundSignal::AudioBegin { scene, turn, codec } => {
                let _ = audio_out.send(AudioEvent::Start {
                    scene: Some(scene),
                    turn,
                    mime: codec,
                });
            }
            OutboundSignal::AudioFrame { scene, turn, bytes } => {
                let _ = audio_out.send(AudioEvent::Frame {
                    scene: Some(scene),
                    turn,
                    bytes,
                });
            }
            OutboundSignal::AudioEnd { scene, turn } => {
                let _ = audio_out.send(AudioEvent::End {
                    scene: Some(scene),
                    turn,
                });
            }
            // /surface: a single envelope broadcast the long-poll handler filters
            // by scene.
            OutboundSignal::Surface { scene, envelope } => {
                let _ = surface_out.send(SurfaceEvent {
                    scene: Some(scene),
                    envelope,
                    ts: Utc::now(),
                });
            }
        }
    }
    tracing::info!("outbound binder: reactor seam closed; exiting");
}
