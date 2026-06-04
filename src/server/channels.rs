//! `GET /api/scenes/{scene}/channels` — one scene's channels, observed live.
//!
//! The inspect console's channel inspector wants a single window onto everything flowing
//! through a scene's senses and expressions. Each channel already fans out on
//! its own broadcast in [`AppState`](crate::server::AppState); this handler
//! subscribes to all of them, keeps only what belongs to the path scene (plus
//! un-targeted broadcasts, which reach every scene), and merges them into one
//! Server-Sent Events stream of uniform [`ChannelSignal`] frames.
//!
//! It is *presence*, not history: the underlying broadcasts are lossy with no
//! replay, so an observer sees activity from the moment it connects — matching
//! the live semantics of `/api/in/<channel>` and the outbound channels. Byte
//! payloads (audio frames, vision frames) are summarized to metadata, never
//! streamed raw — this is an inspector, not a media pipe.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Utc};
use futures::stream::Stream;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::server::{AppState, AudioEvent, AudioInEvent};
use crate::types::{Channel, Scene};

/// One unit of channel activity, uniform across every channel and direction.
/// `body` is always a short human-readable line — recognized/spoken text for the
/// text channel, a metadata summary for binary or structured channels.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelSignal {
    pub ts: DateTime<Utc>,
    pub channel: Channel,
    /// `"in"` (world→agent) or `"out"` (agent→world).
    pub direction: &'static str,
    pub body: String,
    /// `false` for a rolling partial, `true` once the utterance is settled.
    #[serde(rename = "final")]
    pub is_final: bool,
}

/// `GET /api/scenes/{scene}/channels` — merged live presence across every
/// channel of one scene. No replay; keep-alive holds the connection open.
pub async fn get_scene_channels(
    State(state): State<Arc<AppState>>,
    Path(scene): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let scene = Scene(scene);
    let stream = merge_channels(state, scene);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Does an `Option<Scene>` routing target reach `want`? A `None` target is an
/// un-addressed broadcast that reaches every scene, so it always matches.
fn targets(target: &Option<Scene>, want: &Scene) -> bool {
    match target {
        Some(s) => s == want,
        None => true,
    }
}

/// Subscribe to all per-channel broadcasts and merge the ones belonging to
/// `scene` into a single `ChannelSignal` stream. Uses `tokio::select!` so a
/// channel with no traffic never blocks the others; lagged receivers resume from
/// the live edge (dropped frames are presence we don't replay).
fn merge_channels(
    state: Arc<AppState>,
    scene: Scene,
) -> impl Stream<Item = Result<Event, Infallible>> {
    struct Subs {
        input: tokio::sync::broadcast::Receiver<crate::server::InputEcho>,
        output: tokio::sync::broadcast::Receiver<crate::server::OutputEcho>,
        audio: tokio::sync::broadcast::Receiver<crate::server::AudioEvent>,
        audio_in: tokio::sync::broadcast::Receiver<crate::server::AudioInEvent>,
        view: tokio::sync::broadcast::Receiver<crate::server::ViewEvent>,
        vision: tokio::sync::broadcast::Receiver<crate::server::VisionFrameEvent>,
    }

    let subs = Subs {
        input: state.input_echo.subscribe(),
        output: state.output_echo.subscribe(),
        audio: state.audio_out.subscribe(),
        audio_in: state.audio_in.subscribe(),
        view: state.view_out.subscribe(),
        vision: state.vision_out.subscribe(),
    };

    futures::stream::unfold((subs, scene), |(mut s, scene)| async move {
        loop {
            // Each branch resolves to `Option<ChannelSignal>`: `None` means the
            // frame was filtered out (other scene, audio frame body, lag) and we
            // re-loop; `Some` is forwarded. A `Closed` receiver ends the stream.
            let sig: Option<ChannelSignal> = tokio::select! {
                r = s.input.recv() => match r {
                    Ok(e) if e.scene == scene => Some(ChannelSignal {
                        ts: e.ts, channel: e.channel, direction: "in", body: e.text, is_final: e.is_final,
                    }),
                    Ok(_) => None,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => return None,
                },
                r = s.output.recv() => match r {
                    Ok(e) if e.scene == scene => Some(ChannelSignal {
                        ts: e.ts, channel: e.channel, direction: "out", body: e.text, is_final: e.is_final,
                    }),
                    Ok(_) => None,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => return None,
                },
                r = s.audio.recv() => match r {
                    Ok(e) if targets(e.scene(), &scene) => audio_summary(&e),
                    Ok(_) => None,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => return None,
                },
                r = s.audio_in.recv() => match r {
                    Ok(e) if targets(e.scene(), &scene) => audio_in_summary(&e),
                    Ok(_) => None,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => return None,
                },
                r = s.view.recv() => match r {
                    Ok(e) if targets(&e.scene, &scene) => Some(ChannelSignal {
                        ts: e.ts, channel: Channel::Vision, direction: "out",
                        body: view_summary(&e.envelope), is_final: true,
                    }),
                    Ok(_) => None,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => return None,
                },
                r = s.vision.recv() => match r {
                    Ok(e) if targets(&e.scene, &scene) => Some(ChannelSignal {
                        ts: e.ts, channel: Channel::Vision, direction: "in",
                        body: vision_summary(&e), is_final: true,
                    }),
                    Ok(_) => None,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => return None,
                },
            };
            if let Some(sig) = sig {
                return Some((Ok(to_sse_event(&sig)), (s, scene)));
            }
        }
    })
}

/// Summarize an outbound audio span to metadata — frames carry raw bytes we
/// never forward, so only the span's begin/end are surfaced.
fn audio_summary(e: &AudioEvent) -> Option<ChannelSignal> {
    match e {
        AudioEvent::Start { mime, .. } => Some(ChannelSignal {
            ts: Utc::now(),
            channel: Channel::Audio,
            direction: "out",
            body: format!("▶ speaking · {mime}"),
            is_final: false,
        }),
        AudioEvent::End { .. } => Some(ChannelSignal {
            ts: Utc::now(),
            channel: Channel::Audio,
            direction: "out",
            body: "■ end".to_owned(),
            is_final: true,
        }),
        // Frames are raw codec bytes — metadata only, so they're dropped here.
        AudioEvent::Frame { .. } => None,
    }
}

/// Summarize an inbound audio source to metadata — the raw bytes stream on
/// `GET /api/in/audio` for playback, so the inspector only surfaces the
/// begin/end of each source (the recognized text rides the *text* channel).
fn audio_in_summary(e: &AudioInEvent) -> Option<ChannelSignal> {
    match e {
        AudioInEvent::Start { mime, .. } => Some(ChannelSignal {
            ts: Utc::now(),
            channel: Channel::Audio,
            direction: "in",
            body: format!("▶ listening · {mime}"),
            is_final: false,
        }),
        AudioInEvent::End { .. } => Some(ChannelSignal {
            ts: Utc::now(),
            channel: Channel::Audio,
            direction: "in",
            body: "■ end".to_owned(),
            is_final: true,
        }),
        // Frames are raw bytes — metadata only, so they're dropped here.
        AudioInEvent::Frame { .. } => None,
    }
}

/// Summarize an inbound vision frame to metadata. Includes the `#stream` label
/// when the frame came from a named stream, so the inspector shows concurrent
/// feeds in one scene as distinct rows; the default stream renders bare.
fn vision_summary(e: &crate::server::VisionFrameEvent) -> String {
    match &e.stream {
        Some(label) => format!("frame · #{label} · {} · {} bytes", e.mime, e.bytes.len()),
        None => format!("frame · {} · {} bytes", e.mime, e.bytes.len()),
    }
}

/// A compact one-line summary of a view envelope for the inspector.
fn view_summary(env: &crate::types::ViewEnvelope) -> String {
    use crate::types::ViewOp;
    let url = env.module_url.as_deref().unwrap_or("");
    match env.op {
        ViewOp::Show => format!("view show · {} · {url}", env.id),
        ViewOp::Replace => format!("view replace · {} · {url}", env.id),
        ViewOp::Dismiss => format!("view dismiss · {}", env.id),
    }
}

fn to_sse_event(sig: &ChannelSignal) -> Event {
    Event::default()
        .event("channel")
        .json_data(sig)
        .unwrap_or_else(|_| Event::default().comment("serialize error"))
}
