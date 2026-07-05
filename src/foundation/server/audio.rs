//! The audio channel: inbound speech and outbound voice.
//!
//! "Audio is audio." The audio *input* channel carries audio bytes — observable
//! and playable the same way vision frames are. The transcript the agent reasons
//! over is a *derived* representation, so STT output is dispatched onto the **text**
//! channel (exactly like `POST /api/in/text`); the agent consumes text, while
//! `GET /api/in/audio` lets any client hear the raw audio.
//!
//! Inbound clip (`POST /api/in/audio`): the body bytes are audio; we save them
//! as a co-located `audio-<id>.<ext>` blob beside the scene's day-log, publish
//! them on the inbound-audio broadcast (so `GET /api/in/audio` can play the
//! clip), transcribe via the configured STT capability
//! ([`crate::body::capabilities::stt`]), and feed the transcript into the same
//! per-scene path that `POST /api/in/text` uses. The journal records a
//! `SignalIn { channel: Text, body: <transcript>, media: Some(..) }` — the
//! agent reads text, while the media reference (sharing the blob's id) links
//! back to the audio this transcript was derived from.
//!
//! Inbound stream (`WS /api/in/audio/stream`): the client streams raw 16 kHz mono
//! 16-bit PCM as binary frames for the whole time the mic is open; the upstream
//! STT does the endpointing. There is no client-side VAD and nothing is sent back
//! on the socket — it is upload-only. Each frame is republished on the
//! inbound-audio broadcast (so `GET /api/in/audio` plays the live mic), and each
//! finalized sentence is dispatched as a text `SignalIn`. The agent sees no live
//! partials — a sentence reaches it once, settled — but rolling partials *are*
//! echoed to scene observers (`GET /api/in/text`, `final:false`): they're the
//! barge-in trigger, letting a client duck its playback the instant speech is
//! recognized.
//!
//! Observe (`GET /api/in/audio`): the live audio bytes for the scene, one source
//! (mic stream or posted clip) per chunked response — the inbound mirror of
//! `GET /api/out/audio`. The `Start` event's mime tells the client how to decode
//! (`audio/pcm;rate=16000;channels=1` for the mic, the clip's own type for a POST).
//!
//! Outbound (`GET /api/out/audio`): subscriber to the reactor's `audio_out`
//! broadcast. A turn's speech arrives as a `Start`/`Frame`*/`End` run; this
//! handler blocks until a `Start` for the subscriber, then streams that turn's
//! frames as one chunked HTTP response until the matching `End`. The client
//! appends the bytes to a single sink and plays — one continuous utterance per
//! response, no per-clip reassembly. After the response closes the client re-GETs
//! for the next turn (same loop shape as the other channels).
//!
//! Capability gating: missing STT → 501 on POST/stream. Missing TTS → no audio
//! events are ever broadcast; GET /api/out/audio blocks forever (same long-poll
//! semantics as the other channels — the request is fine, the agent just never
//! speaks).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::body::capabilities::stt::{self, Transcript};
use crate::body::capabilities::voiceprint;
use crate::mind::memory::layout::MediaSlot;
use crate::mind::memory::media;
use crate::mind::memory::people_vectors::{self, Modality};
use crate::foundation::pcm;
use crate::foundation::server::headers::{AuthBearer, RequiredScene, SceneHeader, StreamHeader};
use crate::foundation::server::{AppState, AudioEvent, AudioInEvent};
use crate::foundation::segment::{Segmenter, Speech};
use crate::types::{Channel, JournalEntry, Media, Origin, Scene, Signal};
use uuid::Uuid;

const DEFAULT_MIME: &str = "audio/wav";

/// Cosine floor for naming a recognized voice in the evidence note; below it the
/// voice reads "unfamiliar". Mirrors the vision channel's `RECOGNISE_MIN` — soft
/// evidence the agent weighs, not a verdict.
const VOICE_RECOGNISE_MIN: f32 = 0.4;

/// Sample rate of the live mic, and samples per millisecond (16 kHz mono). Used to
/// map a diarized utterance's `[start_ms, end_ms]` onto the timeline buffer.
const SAMPLES_PER_MS: u64 = 16;

/// Minimum sliced span to voiceprint: ~1 s. Shorter turns ("嗯", "对") embed
/// poorly and pull clusters toward a noisy centroid, so they're skipped.
const VP_MIN_SPAN_SAMPLES: u64 = SAMPLES_PER_MS * 1_000;

/// How much audio the timeline retains *before* the last consumed utterance end —
/// slack so a span whose diarized final lands slightly after its audio can still be
/// sliced. ~2 s.
const VP_PRUNE_MARGIN_SAMPLES: u64 = SAMPLES_PER_MS * 2_000;

/// Hard backstop on retained timeline samples (~60 s). The diarized second pass
/// lags the live stream by seconds; this is generous enough to cover that lag yet
/// bounds growth if the pass stalls. A span whose audio predates this window is
/// skipped (we never voiceprint the wrong audio) rather than mis-attributed.
const VP_RETAIN_CEILING_SAMPLES: u64 = SAMPLES_PER_MS * 60_000;

/// The live mic as an absolute-clock PCM buffer. `base` is the absolute sample
/// index of `buf[0]` (everything before it has been pruned); `consumed_end` is the
/// absolute index one past the last utterance span already voiceprinted. A diarized
/// utterance's `[start_ms, end_ms]` becomes absolute samples `start_ms*16 ..
/// end_ms*16`, sliced out of `buf` via `base`. This lets each speaker's *own* audio
/// be embedded even though the two-pass diarized finals lag the audio that spans
/// several speakers — the bug the old "take everything since the last final" had.
#[derive(Default)]
struct VpTimeline {
    buf: Vec<i16>,
    base: u64,
    consumed_end: u64,
}

impl VpTimeline {
    /// Absolute index one past the last buffered sample (== total samples ever
    /// pushed, since `base` accounts for everything dropped).
    fn end(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// Append freshly-decoded samples. Caller passes the count actually decoded
    /// (`pcm::le_i16` drops a trailing odd byte, so this must not assume bytes/2).
    fn push(&mut self, samples: &[i16]) {
        self.buf.extend_from_slice(samples);
    }

    /// Copy the samples covering absolute `[start, end)` — that speaker's own audio.
    /// `None` when `start` predates `base` (the audio was already pruned; we refuse
    /// to substitute other audio, which would re-introduce contamination) or when
    /// nothing remains after clamping `end` to what's buffered.
    fn slice(&self, start: u64, end: u64) -> Option<Vec<i16>> {
        if start < self.base {
            return None;
        }
        let lo = (start - self.base) as usize;
        let hi = (end.min(self.end()) - self.base) as usize;
        if hi <= lo || lo >= self.buf.len() {
            return None;
        }
        Some(self.buf[lo..hi.min(self.buf.len())].to_vec())
    }

    /// Drop audio no longer needed: keep a `margin` of samples before `consumed_end`
    /// and never retain more than `ceiling` trailing samples. When the unconsumed
    /// backlog itself exceeds `ceiling` (the second pass stalled), the ceiling wins
    /// and the stale audio is dropped — its later span then fails `slice` and is
    /// skipped rather than mis-sliced.
    fn prune(&mut self, margin: u64, ceiling: u64) {
        let keep_from = self
            .consumed_end
            .saturating_sub(margin)
            .max(self.end().saturating_sub(ceiling));
        if keep_from > self.base {
            let drop = ((keep_from - self.base) as usize).min(self.buf.len());
            self.buf.drain(..drop);
            self.base += drop as u64;
        }
    }
}

/// Recognize the speaker of a single-voice clip and render one compact evidence
/// note to append to the transcript, e.g. ` ⟨voice: 老王 ~0.82⟩`. The audio twin
/// of the vision channel's `face_note`. Returns `None` when voiceprint is
/// unconfigured, the clip can't be decoded/embedded, or the clip is diarized into
/// multiple speakers (a single blended embedding would be misleading — the
/// labeled transcript already attributes the turns). Best-effort: the signal
/// stands regardless.
async fn voice_note(bytes: &Bytes, mime: &str, transcript: &str, data_dir: &std::path::Path) -> Option<String> {
    if !voiceprint::available() {
        return None;
    }
    // A diarized, multi-speaker clip ("说话人0：…") is not one voice; skip rather
    // than embed a blend of several speakers into one misleading sample.
    if transcript.starts_with("说话人") {
        return None;
    }
    let samples = pcm::to_i16_16k_mono(bytes, mime).ok().filter(|s| !s.is_empty())?;
    let embedding = match voiceprint::embed(samples).await {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(error = %err, "voiceprint embed failed");
            return None;
        }
    };
    let top = people_vectors::nearest(data_dir, Modality::Voice, &embedding, 1)
        .await
        .unwrap_or_default()
        .into_iter()
        .next();
    let who = match top {
        Some(c) if c.similarity >= VOICE_RECOGNISE_MIN => format!("{} ~{:.2}", c.subject, c.similarity),
        _ => "unfamiliar".to_string(),
    };
    Some(format!(" ⟨voice: {who}⟩"))
}

/// Format of the live mic stream: raw 16 kHz mono signed 16-bit little-endian PCM.
/// Carried on the inbound-audio `Start` so a listener knows how to decode it.
const PCM_MIME: &str = "audio/pcm;rate=16000;channels=1";

#[derive(Debug, Serialize)]
struct PostAudioAck {
    transcript: String,
    media_path: String,
}

pub async fn post_audio(
    State(state): State<Arc<AppState>>,
    SceneHeader(scene): SceneHeader,
    StreamHeader(stream): StreamHeader,
    AuthBearer(auth): AuthBearer,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !stt::available() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "audio capability not configured (set an STT key in Settings)\n",
        )
            .into_response();
    }

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
        scene = %scene,
        auth = ?auth,
        mime = %mime,
        bytes = body.len(),
        "POST /audio"
    );

    // The signal's id (uuidv7) also names its co-located blob; `ts` places both
    // in the same day-folder. Generate them before storing so the two agree.
    let ts = Utc::now();
    let id = Uuid::now_v7().to_string();

    // 1. Persist the raw bytes so we can replay/audit and so the log has a
    //    stable reference. We do this before STT so a transcription failure
    //    still leaves the audio on disk.
    let media_path = match media::store_blob(&state.data_dir, &scene, Channel::Audio, ts, MediaSlot::InputOneOff, ext, &body).await {
        Ok(f) => f,
        Err(err) => {
            tracing::error!(error = %err, "failed to persist incoming audio");
            return (StatusCode::INTERNAL_SERVER_ERROR, "audio store failed\n").into_response();
        }
    };

    // 2. Publish the clip on the inbound-audio channel as one source, so any
    //    `GET /api/in/audio` listener can play it. Bytes are refcounted, so with
    //    no listener this is a cheap drop.
    let turn = state.audio_in_turn.fetch_add(1, Ordering::Relaxed);
    let _ = state.audio_in.send(AudioInEvent::Start {
        scene: Some(scene.clone()),
        turn,
        mime: mime.clone(),
    });
    let _ = state.audio_in.send(AudioInEvent::Frame {
        scene: Some(scene.clone()),
        turn,
        bytes: body.clone(),
    });
    let _ = state.audio_in.send(AudioInEvent::End { scene: Some(scene.clone()), turn });

    // Keep the raw bytes for voiceprint before STT consumes them below.
    let vp_bytes = body.clone();

    // 3. Transcribe. Errors surface as 502 — the upstream provider failed.
    let transcript = match stt::transcribe(body, &mime).await {
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

    // Empty transcript = the clip held no recognizable speech. The upstream
    // cannot distinguish silence from un-transcribable sound, so we don't try:
    // there's nothing to journal or dispatch. Return a benign ack (the raw
    // audio is already persisted for audit) — the SPA reads the empty
    // transcript and drops back to idle rather than treating it as a failure.
    if transcript.trim().is_empty() {
        tracing::info!(scene = %scene, media_path = %media_path, "audio clip held no speech");
        let ack = PostAudioAck { transcript: String::new(), media_path };
        return (StatusCode::ACCEPTED, axum::Json(ack)).into_response();
    }

    // 4. The transcript is text: dispatch it onto the text channel exactly like a
    //    typed line. The agent reads text; the audio stays on the audio channel.
    //    The clip's (ts, id, media) ride along so the journal entry links back to
    //    the stored blob by the shared id.
    let media = Media {
        file: media_path.clone(),
        mime: mime.clone(),
        duration_ms: None,
        width: None,
        height: None,
    };
    // Fold a voiceprint recognition note into the agent-facing transcript (who is
    // speaking), the way the vision path folds in recognized faces. The ack keeps
    // the raw transcript so the SPA caption isn't cluttered with the evidence tag.
    let mut delivered = transcript.clone();
    if let Some(note) = voice_note(&vp_bytes, &mime, &transcript, &state.data_dir).await {
        delivered.push_str(&note);
    }
    if !deliver_transcript(&state, &scene, stream, &delivered, Some((ts, id, media))).await {
        return (StatusCode::SERVICE_UNAVAILABLE, "inbound channel closed\n").into_response();
    }

    let ack = PostAudioAck { transcript, media_path };
    (StatusCode::ACCEPTED, axum::Json(ack)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// The streaming scene. Browsers can't set `X-HI-Scene` on a WebSocket
    /// handshake, so the scene rides in the query string instead.
    scene: Option<String>,
    /// The named stream within the scene, same role as `X-HI-Stream` on the POST
    /// path; absent/empty → the default stream. Rides the query string for the
    /// same handshake reason as `scene`.
    stream: Option<String>,
}

/// `GET /api/in/audio/stream` — continuous inbound speech over a WebSocket.
///
/// Upload-only: the client streams raw 16 kHz mono 16-bit PCM as binary frames
/// for the whole time the mic is open; the upstream STT does the endpointing.
/// There is no client-side VAD and nothing is sent back on the socket. Each frame
/// is republished on the inbound-audio broadcast so `GET /api/in/audio` plays the
/// live mic; each finalized sentence is dispatched on the text channel (the path
/// `POST /api/in/audio` uses for its transcript).
pub async fn get_audio_stream(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StreamParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !stt::available() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "audio capability not configured (set an STT key in Settings)\n",
        )
            .into_response();
    }
    let scene = Scene(
        params
            .scene
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "anonymous".to_string()),
    );
    let stream = params.stream.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    tracing::info!(scene = %scene, stream = ?stream, "WS /api/in/audio/stream opened");
    ws.on_upgrade(move |socket| stream_audio_in(state, scene, stream, socket))
}

async fn stream_audio_in(
    state: Arc<AppState>,
    scene: Scene,
    stream: Option<String>,
    mut socket: axum::extract::ws::WebSocket,
) {
    // The WS is just one source of PCM frames. Forward its binary frames into the
    // shared ingest; when the socket closes the sender drops, the ingest sees the
    // stream end, and it finalizes. A browser mic carries no source tag.
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let pump = tokio::spawn(async move {
        while let Some(msg) = socket.recv().await {
            match msg {
                Ok(WsMessage::Binary(b)) => {
                    if tx.send(b).await.is_err() {
                        break;
                    }
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    ingest_pcm_stream(state, scene.clone(), stream, None, rx).await;
    pump.abort();
    tracing::info!(scene = %scene, "WS /api/in/audio/stream closed");
}

/// Ingest a stream of raw 16 kHz mono 16-bit PCM frames as live inbound speech,
/// from any source — the browser mic over a WebSocket ([`stream_audio_in`]) or a
/// native capture (the press-hold-⌘ attention gesture). Frames flow through the
/// same path regardless: republished on the inbound-audio broadcast (so
/// `GET /api/in/audio` plays the live mic), persisted on the wall-clock-minute grid,
/// fed to streaming STT, segmented into sentences, voiceprint-tagged, and dispatched
/// on the audio channel. Returns when the `frames` sender drops (the source closed).
///
/// `source_tag`, when present, rides the **first** delivered sentence as a context
/// note (e.g. press-hold attention is headless, screen-aware) using the same `⟨…⟩`
/// convention as the voiceprint tag — so the mind knows where this speech came from
/// without any branch downstream. The browser mic passes `None`.
pub async fn ingest_pcm_stream(
    state: Arc<AppState>,
    scene: Scene,
    stream: Option<String>,
    source_tag: Option<String>,
    mut frames: mpsc::Receiver<Bytes>,
) {
    // PCM source → STT; Transcripts STT → dispatch. Bounded so a stalled upstream
    // exerts backpressure rather than buffering unboundedly.
    let (audio_tx, audio_rx) = mpsc::channel::<Bytes>(64);
    let (tr_tx, mut tr_rx) = mpsc::channel::<Transcript>(64);

    let stt_task = tokio::spawn(async move { stt::transcribe_streaming(audio_rx, tr_tx).await });

    // Live-mic voiceprint: who is speaking, from the vendor's diarized segments.
    // The PCM pump (below) appends raw samples to `timeline` (an absolute-clock
    // buffer); when a diarized utterance finalizes, the out task slices *that
    // speaker's own* audio by the utterance's `[start_ms, end_ms]`, embeds it, and
    // clusters it into the people store, caching `speaker_id → person` so each
    // delivered sentence can be tagged with the speaker. Only armed when the
    // voiceprint capability is configured.
    let vp_on = voiceprint::available();
    let timeline: Arc<Mutex<VpTimeline>> = Arc::new(Mutex::new(VpTimeline::default()));
    let speaker_names: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

    // An explicit Segmenter — not the upstream's silence flag — decides where the
    // continuous word-stream is cut into sentences for the agent. A periodic tick
    // drives the time-based cut rules when the speaker has gone quiet. Each
    // finalized sentence is delivered on the text channel; there are no partials.
    let relay_state = state.clone();
    let relay_scene = scene.clone();
    let relay_stream = stream.clone();
    let relay_pcm = timeline.clone();
    let relay_names = speaker_names.clone();
    let relay_tag = source_tag.clone();
    let out_task = tokio::spawn(async move {
        let mut seg = Segmenter::new(Speech::default(), Instant::now());
        let mut ticker = tokio::time::interval(Duration::from_millis(150));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The speaker of the latest diarized final, and the last subject we tagged
        // a sentence with — so we mark turn changes, not every line.
        let mut current_speaker: Option<String> = None;
        let mut last_tagged: Option<String> = None;
        // The source note rides the first sentence only.
        let mut source_noted = false;
        loop {
            let cuts = tokio::select! {
                msg = tr_rx.recv() => match msg {
                    Some(t) => {
                        // Echo every rolling partial to the scene's observers
                        // (`final:false`). This is the duck trigger: the client
                        // stops its own playback the moment speech is
                        // recognized, hundreds of ms before a sentence settles.
                        // The same moment is reported to the barge-in registry,
                        // whose own clock decides whether the agent's voice was
                        // probably still sounding (→ "what went unheard" note).
                        if !t.is_final && !t.text.trim().is_empty() {
                            relay_state.echo_input(&relay_scene, Channel::Text, &t.text, false);
                            relay_state.interrupts.note_speech(&relay_scene, tokio::time::Instant::now()).await;
                        }
                        // A diarized utterance just finalized. Each segment names a
                        // speaker and its `[start_ms, end_ms]`; slice that speaker's
                        // *own* audio out of the timeline (not the whole stretch
                        // since the last final, which spans several speakers because
                        // the diarized second pass lags) and resolve each off-thread.
                        if t.is_final {
                            if vp_on && !t.segments.is_empty() {
                                let sliced: Vec<(String, Vec<i16>)> = {
                                    let mut tl = relay_pcm.lock().unwrap();
                                    t.segments
                                        .iter()
                                        .filter_map(|sp| {
                                            let start = sp.start_ms.saturating_mul(SAMPLES_PER_MS);
                                            let end = sp.end_ms.saturating_mul(SAMPLES_PER_MS);
                                            if end <= tl.consumed_end {
                                                return None; // already handled (re-sent span)
                                            }
                                            let pcm = tl.slice(start, end);
                                            tl.consumed_end = tl.consumed_end.max(end);
                                            pcm.filter(|p| p.len() as u64 >= VP_MIN_SPAN_SAMPLES)
                                                .map(|p| (sp.speaker_id.clone(), p))
                                        })
                                        .collect()
                                };
                                for (spk, pcm) in sliced {
                                    resolve_speaker(&relay_state, relay_names.clone(), spk, pcm);
                                }
                            }
                            // Tag the dispatched sentence with the last finalized
                            // speaker — the turn the Segmenter is about to emit.
                            if let Some(spk) = t.speaker_id.clone() {
                                current_speaker = Some(spk);
                            }
                        }
                        seg.observe(&t.text, t.is_final, Instant::now())
                    }
                    None => break, // STT session ended
                },
                _ = ticker.tick() => seg.tick(Instant::now()),
            };
            for sentence in cuts {
                // Tag the sentence with the current speaker's identity when it's
                // known and the turn changed — soft evidence, low noise (a 1:1
                // chat shows it once; a multi-party one marks each handoff).
                let mut line = sentence;
                if let Some(spk) = &current_speaker
                    && let Some(subject) = relay_names.lock().unwrap().get(spk).cloned()
                    && last_tagged.as_deref() != Some(subject.as_str())
                {
                    line.push_str(&format!(" ⟨voice: {subject}⟩"));
                    last_tagged = Some(subject);
                }
                if let Some(tag) = &relay_tag
                    && !source_noted
                {
                    line.push_str(&format!(" ⟨{tag}⟩"));
                    source_noted = true;
                }
                deliver_transcript(&relay_state, &relay_scene, relay_stream.clone(), &line, None).await;
            }
        }
        // Flush any trailing words as a final sentence when the session ends.
        if let Some(sentence) = seg.flush() {
            let mut line = sentence;
            if let Some(tag) = &relay_tag
                && !source_noted
            {
                line.push_str(&format!(" ⟨{tag}⟩"));
            }
            deliver_transcript(&relay_state, &relay_scene, relay_stream.clone(), &line, None).await;
        }
    });

    // One source is one inbound-audio source: its frames carry a shared `turn` so a
    // `GET /api/in/audio` listener stays bound to this mic alone.
    let turn = state.audio_in_turn.fetch_add(1, Ordering::Relaxed);
    let mut started = false;

    // Persist the live mic on a wall-clock-minute grid: PCM accumulates per
    // minute and flushes to `audio/<date>/<HH>/<MM>.wav` at each rollover (and
    // at close). The bytes are the raw signal; utterance lines (journaled by
    // `deliver_transcript`) stay media-less and correlate to a minute by ts.
    let mut cap_minute: Option<String> = None;
    let mut cap_ts = Utc::now();
    let mut cap_buf: Vec<u8> = Vec::new();

    // Pump inbound PCM until the source closes or the STT session ends (a send
    // error means `audio_rx` was dropped because `transcribe_streaming` returned).
    while let Some(b) = frames.recv().await {
        // Republish the raw PCM for `GET /api/in/audio` listeners. The `Start`
        // (carrying the format) precedes the first frame.
        if !started {
            started = true;
            let _ = state.audio_in.send(AudioInEvent::Start {
                scene: Some(scene.clone()),
                turn,
                mime: PCM_MIME.to_owned(),
            });
        }
        let _ = state.audio_in.send(AudioInEvent::Frame {
            scene: Some(scene.clone()),
            turn,
            bytes: b.clone(),
        });
        // Fold the frame into the current minute's WAV buffer, flushing the
        // completed minute when the wall clock rolls over.
        let now = Utc::now();
        let minute = now.format("%Y-%m-%dT%H:%M").to_string();
        match &cap_minute {
            Some(m) if *m != minute => {
                flush_mic_minute(&state, &scene, cap_ts, &cap_buf).await;
                cap_buf.clear();
                cap_minute = Some(minute);
                cap_ts = now;
            }
            None => {
                cap_minute = Some(minute);
                cap_ts = now;
            }
            _ => {}
        }
        cap_buf.extend_from_slice(&b);
        // Feed the voiceprint timeline (same raw 16 kHz mono PCM). Push the samples
        // actually decoded (le_i16 drops a trailing odd byte, so a byte-derived
        // clock would drift), then prune audio the out task has already consumed —
        // bounded so a stalled diarized pass can't grow it.
        if vp_on {
            let samples = pcm::le_i16(&b);
            let mut tl = timeline.lock().unwrap();
            tl.push(&samples);
            tl.prune(VP_PRUNE_MARGIN_SAMPLES, VP_RETAIN_CEILING_SAMPLES);
        }
        if audio_tx.send(b).await.is_err() {
            break;
        }
    }

    // Close the inbound-audio source so listeners end their current response.
    if started {
        let _ = state.audio_in.send(AudioInEvent::End { scene: Some(scene.clone()), turn });
    }
    // Flush the final, partial minute of mic audio.
    if !cap_buf.is_empty() {
        flush_mic_minute(&state, &scene, cap_ts, &cap_buf).await;
    }

    // Closing the audio side lets the STT session flush its last utterance.
    drop(audio_tx);
    match tokio::time::timeout(Duration::from_secs(5), stt_task).await {
        Ok(Ok(Err(err))) => {
            // A 402 here means the managed account is out of energy (STT draws the
            // same budget) — raise the out-of-energy hint now, without waiting for the
            // next balance poll. No-op in BYOK / for non-402 STT failures.
            let text = err.to_string();
            if text.contains("402") {
                crate::foundation::energy_state::note_402(&state.data_dir);
            }
            tracing::warn!(scene = %scene, error = %err, "audio ingest STT ended");
        }
        Err(_) => tracing::warn!(scene = %scene, "audio ingest STT did not finalize in time"),
        _ => {}
    }
    out_task.abort();
}

/// Deliver one finalized transcript on the **audio** channel — journal it, echo
/// it to scene observers (settled), and hand it to the reactor. The transcript is
/// the signal's text surface (`body`); the modality stays `audio` so its bytes,
/// when present, land under `audio/`. The reactor reads `body` regardless. For a
/// posted clip, `clip` carries the `(ts, id, media)` of the stored audio blob so
/// the journal entry references it; the live mic passes `None` for now (its bytes
/// aren't persisted yet — Phase 2), so the journal records no `media`.
async fn deliver_transcript(
    state: &AppState,
    scene: &Scene,
    stream: Option<String>,
    text: &str,
    clip: Option<(DateTime<Utc>, String, Media)>,
) -> bool {
    let (ts, id, media) = match clip {
        Some((ts, id, media)) => (ts, id, Some(media)),
        None => (Utc::now(), Uuid::now_v7().to_string(), None),
    };
    let signal = Signal {
        channel: Channel::Audio,
        scene: scene.clone(),
        body: text.to_owned(),
        stream: stream.clone(),
        ts,
    };
    crate::foundation::channel_log::inbound(Channel::Audio, scene, text);
    let entry = JournalEntry::SignalIn {
        id,
        ts,
        channel: Channel::Audio,
        scene: scene.clone(),
        body: text.to_owned(),
        stream,
        media,
        origin: Some(Origin::Human),
    };
    if let Err(err) = state.memory.journal.append(entry).await {
        tracing::error!(error = %err, "journal append failed; accepting signal anyway");
    }
    // Echo before dispatching inward. The caption display rides the text channel
    // (a display concern), so a spoken line shows the same way a typed line does.
    state.echo_input(scene, Channel::Text, text, true);
    if let Err(err) = state.inbound.send(signal).await {
        tracing::error!(error = %err, "inbound channel closed");
        return false;
    }
    true
}

/// Resolve a diarized speaker's identity off the hot path: embed the utterance's
/// PCM into a voiceprint, cluster it into the people store
/// ([`people_vectors::assign`] — append to a near cluster, or mint a fresh id),
/// and cache `speaker_id → subject` so the stream can tag this speaker's
/// sentences. Detached and best-effort — a failure just leaves the speaker
/// untagged. Unlike clips and stills, the live mic persists no per-utterance media
/// for the reflection pass to re-derive, so the clustering must happen inline here.
fn resolve_speaker(
    state: &Arc<AppState>,
    names: Arc<Mutex<HashMap<String, String>>>,
    speaker_id: String,
    pcm: Vec<i16>,
) {
    if pcm.is_empty() {
        return;
    }
    let data_dir = state.data_dir.clone();
    // A playable WAV of this turn, built before the PCM is consumed by `embed`, so
    // the cluster keeps an audible preview of the live-mic voice (the stream stores
    // no per-utterance clip otherwise).
    let pcm_bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    let wav = pcm16_mono_16k_to_wav(&pcm_bytes);
    tokio::spawn(async move {
        let embedding = match voiceprint::embed(pcm).await {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "live voiceprint embed failed");
                return;
            }
        };
        match people_vectors::assign(&data_dir, Modality::Voice, &embedding, &wav, "wav").await {
            Ok(subject) => {
                names.lock().unwrap().insert(speaker_id, subject);
            }
            Err(err) => tracing::warn!(error = %err, "live voice assign failed"),
        }
    });
}

/// Persist one wall-clock minute of live mic PCM as a WAV under
/// `audio/<date>/<HH>/<MM>.wav`. Best-effort: a failure is logged, never fatal.
async fn flush_mic_minute(state: &AppState, scene: &Scene, ts: DateTime<Utc>, pcm: &[u8]) {
    let wav = pcm16_mono_16k_to_wav(pcm);
    if let Err(err) =
        media::store_blob(&state.data_dir, scene, Channel::Audio, ts, MediaSlot::InputStream, "wav", &wav).await
    {
        tracing::warn!(scene = %scene, error = %err, "persisting mic minute failed");
    }
}

/// Wrap raw 16 kHz mono signed-16-bit-LE PCM (the live mic format, [`PCM_MIME`])
/// in a canonical 44-byte WAV header so the minute file is independently
/// playable.
fn pcm16_mono_16k_to_wav(pcm: &[u8]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * (BITS as u32 / 8);
    let block_align = CHANNELS * (BITS / 8);
    let data_len = pcm.len() as u32;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    w.extend_from_slice(&CHANNELS.to_le_bytes());
    w.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&BITS.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// Whether an event routed to `target` should reach this `scene` subscriber.
fn routed(target: &Option<Scene>, scene: &Scene) -> bool {
    match target {
        None => true,
        Some(t) => t == scene,
    }
}

/// `GET /api/in/audio` — the live audio bytes on this scene, one source per
/// long-poll. The inbound mirror of [`get_out_audio`].
pub async fn get_in_audio(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.audio_in.subscribe();

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/in/audio long-poll opened");

    // Block until a source for this subscriber starts. `Start` carries the mime,
    // which must be set before any body byte; Frame/End seen before a Start (we
    // subscribed mid-source) are skipped — the client re-polls and catches the
    // next source cleanly.
    let (turn, mime) = loop {
        match rx.recv().await {
            Ok(event) => {
                if !routed(event.scene(), &scene) {
                    continue;
                }
                if let AudioInEvent::Start { turn, mime, .. } = event {
                    break (turn, mime);
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "inbound-audio subscriber lagged");
                continue;
            }
            Err(RecvError::Closed) => {
                return (StatusCode::SERVICE_UNAVAILABLE, "broadcast closed\n").into_response();
            }
        }
    };

    // Stream this source's frames as a chunked body until its `End`. Frames from
    // any other source or scene are filtered out, so a response stays bound to the
    // single source it opened on.
    let stream = futures::stream::unfold((rx, scene, turn), |(mut rx, scene, turn)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !routed(event.scene(), &scene) || event.turn() != turn {
                        continue;
                    }
                    match event {
                        AudioInEvent::Frame { bytes, .. } => {
                            return Some((
                                Ok::<Bytes, std::convert::Infallible>(bytes),
                                (rx, scene, turn),
                            ));
                        }
                        AudioInEvent::End { .. } => return None,
                        AudioInEvent::Start { .. } => continue,
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "inbound-audio subscriber lagged mid-source");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });

    let mut response = Body::from_stream(stream).into_response();
    if let Ok(val) = HeaderValue::from_str(&mime) {
        response.headers_mut().insert(CONTENT_TYPE, val);
    }
    response
}

/// `GET /api/out/audio` — the agent's voice, one turn per long-poll.
pub async fn get_out_audio(
    State(state): State<Arc<AppState>>,
    RequiredScene(scene): RequiredScene,
    AuthBearer(auth): AuthBearer,
) -> impl IntoResponse {
    let mut rx = state.audio_out.subscribe();
    // A held audio long-poll = their ears are on; counted while we wait for a turn.
    let _presence = state.presence.connect(&scene, crate::body::presence::OutChannel::Audio);

    tracing::info!(scene = %scene, auth = ?auth, "GET /api/out/audio long-poll opened");

    // Opening this long-poll is a scene-presence signal: warm the scene up so its
    // process + session + upstream cache are hot before the first utterance.
    state.warm_scene(&scene);

    // Block until a turn for this subscriber starts. `Start` carries the mime,
    // which must be set before any body byte; Frame/End seen before a Start
    // (we subscribed mid-turn) are skipped — the client re-polls and catches
    // the next turn cleanly.
    let (turn, mime) = loop {
        match rx.recv().await {
            Ok(event) => {
                if !routed(event.scene(), &scene) {
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
    // any other turn or scene are filtered out, so a response stays bound to the
    // single turn it opened on.
    let stream = futures::stream::unfold(
        (rx, scene, turn, false),
        |(mut rx, scene, turn, done)| async move {
            if done {
                return None;
            }
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if !routed(event.scene(), &scene) || event.turn() != turn {
                            continue;
                        }
                        match event {
                            AudioEvent::Frame { bytes, .. } => {
                                return Some((
                                    Ok::<Bytes, std::convert::Infallible>(bytes),
                                    (rx, scene, turn, false),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_canonical_16k_mono_16bit() {
        let pcm = vec![0u8; 320]; // 0.01s of silence
        let wav = pcm16_mono_16k_to_wav(&pcm);
        let u16le = |i: usize| u16::from_le_bytes([wav[i], wav[i + 1]]);
        let u32le = |i: usize| u32::from_le_bytes([wav[i], wav[i + 1], wav[i + 2], wav[i + 3]]);

        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32le(4), 36 + pcm.len() as u32); // RIFF chunk size
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32le(16), 16); // fmt chunk size
        assert_eq!(u16le(20), 1); // PCM
        assert_eq!(u16le(22), 1); // mono
        assert_eq!(u32le(24), 16_000); // sample rate
        assert_eq!(u16le(34), 16); // bits per sample
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32le(40), pcm.len() as u32); // data size
    }

    #[test]
    fn timeline_slices_a_spans_own_samples() {
        let mut tl = VpTimeline::default();
        tl.push(&(0..100).collect::<Vec<i16>>());
        // Absolute [10, 20) → exactly those samples.
        assert_eq!(tl.slice(10, 20).unwrap(), (10..20).collect::<Vec<i16>>());
        // End clamps to what's buffered.
        assert_eq!(tl.slice(95, 999).unwrap(), (95..100).collect::<Vec<i16>>());
    }

    #[test]
    fn timeline_maps_through_base_after_prune_and_refuses_dropped_audio() {
        let mut tl = VpTimeline::default();
        tl.push(&(0..100).collect::<Vec<i16>>());
        tl.consumed_end = 50;
        tl.prune(0, 1_000_000); // margin 0, huge ceiling → drop everything below consumed_end
        assert_eq!(tl.base, 50);
        // Absolute coordinates still map correctly through `base`.
        assert_eq!(tl.slice(60, 70).unwrap(), (60..70).collect::<Vec<i16>>());
        // Audio before `base` was pruned: refuse rather than substitute other audio.
        assert!(tl.slice(10, 20).is_none());
    }

    #[test]
    fn timeline_prune_caps_at_ceiling_even_when_unconsumed() {
        let mut tl = VpTimeline::default();
        tl.push(&vec![0i16; 1000]);
        // consumed_end stays 0 (second pass stalled), ceiling 400 → keep last 400.
        tl.prune(0, 400);
        assert_eq!(tl.base, 600);
        assert_eq!(tl.buf.len(), 400);
    }

    #[test]
    fn timeline_slice_is_none_on_empty_or_inverted_range() {
        let tl = VpTimeline::default();
        assert!(tl.slice(0, 10).is_none()); // nothing buffered yet
        let mut tl = VpTimeline::default();
        tl.push(&vec![1i16; 10]);
        assert!(tl.slice(5, 5).is_none()); // zero-width
    }

    #[test]
    fn one_second_gate_threshold_is_16k_samples() {
        // The out_task drops sliced spans shorter than this before voiceprinting.
        assert_eq!(VP_MIN_SPAN_SAMPLES, 16_000);
        let short: Vec<i16> = vec![0; 8_000]; // 0.5 s
        assert!((short.len() as u64) < VP_MIN_SPAN_SAMPLES);
    }
}
