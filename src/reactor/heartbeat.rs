//! Heartbeat hot-swap — bound the persistent reactor session's growth without
//! the conversation ever seeing a cold restart.
//!
//! A persistent session is a warm, continuous mind, but it also grows without
//! bound: every turn appends to its context. Left alone it eventually rots
//! (early context crowded out) or overflows the model's window. The heartbeat
//! keeps it bounded *invisibly*: once a session has accumulated enough, the
//! next gap between turns is used to (1) ask the live session for a compact
//! self-briefing, (2) open a replacement seeded with that briefing plus the
//! recent journal tail, and (3) hand it back so the loop swaps it in. The
//! conversation experiences continuity, never a cold restart; the journal stays
//! the durable backstop if a swap fails.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::acp::{AcpSession, SessionOpts};
use crate::agent::SessionRole;
use crate::capabilities::{face, voiceprint};
use crate::memory::journal::after_cursor;
use crate::pcm;
use crate::memory::{Snapshot, build_for_scene, episodes, facets, layout, people_vectors, refresh_hot};
use crate::observatory::EventKind;
use crate::types::{Channel, JournalEntry, Scene};
use crate::vendors::ffmpeg_frame;

use super::Reactor;

/// Default soft ceiling on a session's accumulated prompt+reply characters
/// before the heartbeat swaps it. A coarse proxy for context pressure — we
/// don't see the model's token count, and an over-estimate just swaps a little
/// early, which is harmless (the replacement is seeded). Kept well below a
/// typical model window so the briefing-plus-tail seed always fits with room to
/// grow. Overridable via `HI_AGENT_COMPACT` — see [`swap_after_chars`].
pub(crate) const DEFAULT_SWAP_AFTER_CHARS: usize = 48_000;

/// Resolve the hot-swap ceiling: `HI_AGENT_COMPACT` if it parses to a positive
/// integer, else [`DEFAULT_SWAP_AFTER_CHARS`]. Read fresh so the observatory
/// denominator and a budget opened mid-run agree on the same value.
pub(crate) fn swap_after_chars() -> usize {
    std::env::var(crate::config::ENV_COMPACT)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SWAP_AFTER_CHARS)
}

/// Tracks how much the live session has accumulated since it was opened, so the
/// per-scene loop can decide when to hot-swap. Cheap; lives in that loop.
pub(super) struct ContextBudget {
    chars: usize,
    /// Ceiling this budget swaps at — captured from [`swap_after_chars`] when
    /// the budget is opened so it stays stable across the session's turns.
    limit: usize,
}

impl ContextBudget {
    pub(super) fn new() -> Self {
        Self {
            chars: 0,
            limit: swap_after_chars(),
        }
    }

    /// Fold one completed turn's prompt and reply sizes into the running total.
    pub(super) fn record_turn(&mut self, prompt_chars: usize, reply_chars: usize) {
        self.chars = self
            .chars
            .saturating_add(prompt_chars)
            .saturating_add(reply_chars);
    }

    pub(super) fn should_swap(&self) -> bool {
        self.chars >= self.limit
    }

    /// Current accumulated chars, mirrored into the observatory for display.
    pub(super) fn chars(&self) -> usize {
        self.chars
    }

    /// Reset after a swap (or after the session is discarded on error).
    pub(super) fn reset(&mut self) {
        self.chars = 0;
    }
}

/// Instruction the live session answers to brief its successor. Framed as an
/// internal request so the model produces a dense briefing, not a spoken reply.
const SUMMARIZE_PROMPT: &str = "[internal request — this is not from the person you're talking \
with, and you are not speaking to anyone; produce no spoken reply] Write a compact briefing of our \
conversation so far for your future self: who you're talking with, what they are working on, \
decisions and facts established, anything still open or promised, and where the current \
thread stands. Be terse and information-dense — this seeds a fresh session that must \
continue the conversation seamlessly. Output only the briefing.";

/// Summarize the live session and open a fresh replacement for `scene`, seeded
/// with that briefing plus the recent journal tail. Runs between turns, so the
/// session is free to take the summarize prompt. On any failure the caller
/// keeps the existing warm session — the swap is best-effort.
pub(super) async fn swap(
    reactor: &Reactor,
    scene: &Scene,
    current: &Arc<AcpSession>,
) -> anyhow::Result<Arc<AcpSession>> {
    // Ask the live session to brief its successor. The reply is captured here and
    // never emitted or spoken — it seeds the new session so the conversation
    // continues across the swap without a visible seam. (Episodes/facets are NOT
    // written here: consolidation reads the raw log, not this lossy self-summary —
    // see [`reflect`], which runs on its own periodic clock, not at this swap.)
    let briefing = {
        let run = current.prompt(SUMMARIZE_PROMPT.to_string()).await?;
        run.wait().await?.text
    };
    let briefing_chars = briefing.chars().count();

    // The verbatim recent tail — the immediate thread the briefing might compress
    // away.
    let tail = build_for_scene(&reactor.inner.memory, scene)
        .await
        .ok()
        .as_ref()
        .map(Snapshot::render_for_prompt)
        .unwrap_or_default();

    // Seed the replacement with the soul plus the briefing and recent tail, so it
    // continues without a visible seam. self.md and hot.md are referenced by the
    // soul, so the fresh session re-reads whatever the last reflection wrote.
    let seeded_system_prompt = format!(
        "{}\n\n## Briefing from your earlier conversation\n{}\n\n{}",
        reactor.inner.soul,
        briefing.trim(),
        tail.trim(),
    );

    let fresh = reactor
        .inner
        .agent
        .session(
            scene,
            crate::agent::SessionRole::Reactor,
            None,
            SessionOpts {
                system_prompt: Some(seeded_system_prompt),
                cwd: None,
            },
        )
        .await?;

    reactor
        .inner
        .observatory
        .record(
            scene,
            EventKind::HotSwap {
                old_id: current.id().0.to_string(),
                new_id: fresh.id().0.to_string(),
                briefing_chars,
            },
        )
        .await;

    Ok(Arc::new(fresh))
}

/// Below this many unconsolidated signals, a reflection round is skipped — not
/// worth a whole session (and its subprocess spawn) to file a handful of lines;
/// they wait on the frontier for the next reflection tick.
const MIN_REFLECT_SIGNALS: usize = 4;

/// Consolidate a scene's unconsolidated frontier into episodes and facets — the
/// "sleep" pass. Reads the raw log after the [`episodes::scene_cursor`], opens a
/// dedicated reflection session (its own subprocess; never the reactor's live
/// session), and drives it to completion; the session writes derived memory
/// through its tools. Best-effort and run **detached** on the scene's periodic
/// reflection timer, so it never blocks the floor — the cursor makes it idempotent
/// across runs and a crash just leaves the frontier for the next tick. A no-op when
/// too little is unconsolidated to be worth a session.
pub(super) async fn reflect(reactor: &Reactor, scene: &Scene) {
    if let Err(err) = run_reflection(reactor, scene).await {
        tracing::warn!(scene = %scene, error = %err, "reflection failed");
    }
}

async fn run_reflection(reactor: &Reactor, scene: &Scene) -> anyhow::Result<()> {
    let data_dir = reactor.inner.memory.data_dir();
    let cursor = episodes::scene_cursor(data_dir, scene).await?;
    let tail =
        after_cursor(data_dir, scene, cursor.as_deref(), episodes::REFLECTION_TAIL_LIMIT).await?;
    if tail.len() < MIN_REFLECT_SIGNALS {
        tracing::debug!(scene = %scene, n = tail.len(), "reflection skipped; frontier too small");
        return Ok(());
    }
    tracing::info!(scene = %scene, n = tail.len(), "reflection starting");

    // Prior episode gists (scene-scoped) give continue-vs-new context; the facet
    // index lets the mind reuse a subject instead of coining a near-duplicate.
    let prior = episodes::recent_gists(&reactor.inner.memory, Some(scene), 2)
        .await
        .unwrap_or_default();
    let subjects = facets::facet_subject_index(data_dir).await.unwrap_or_default();

    // Mechanically cluster any faces in the frontier's images first, so the prompt
    // can show the mind a stable id per detected face to name. No-op without the
    // face capability.
    let face_ids = cluster_faces(data_dir, scene, &tail).await;
    let voice_ids = cluster_voices(data_dir, scene, &tail).await;
    let prompt = build_reflection_prompt(&tail, &prior, &subjects, &face_ids, &voice_ids);
    let system_prompt = super::reflection_prompt(data_dir).await;

    let session = reactor
        .inner
        .agent
        .session(
            scene,
            SessionRole::Reflection,
            None,
            SessionOpts { system_prompt: Some(system_prompt), cwd: None },
        )
        .await?;
    reactor
        .inner
        .observatory
        .record(
            scene,
            EventKind::SessionOpened {
                kind: crate::observatory::SessionKind::Reflection,
                id: session.id().0.to_string(),
            },
        )
        .await;

    let run = session.prompt(prompt).await?;
    run.wait().await?;

    // hot.md now reflects the freshly written episodes.
    if let Err(err) = refresh_hot(&reactor.inner.memory).await {
        tracing::warn!(scene = %scene, error = %err, "failed to refresh hot.md after reflection");
    }
    tracing::info!(scene = %scene, "reflection finished");
    Ok(())
}

/// Assemble the reflection prompt: optional prior-episode and known-subject
/// context, then the unconsolidated frontier as a numbered, oldest-first list (the
/// mind hands back a `count` into this list, never a raw id). Image signals are
/// marked: `⟨faces: <id>…⟩` when clustering placed faces (the ids the mind can
/// name), else `⟨image⟩`. Audio clips are marked `⟨voice: <id>…⟩` when voiceprint
/// clustering placed a speaker — the same nameable handles, for voices.
fn build_reflection_prompt(
    tail: &[JournalEntry],
    prior: &[String],
    subjects: &[String],
    face_ids: &HashMap<usize, Vec<String>>,
    voice_ids: &HashMap<usize, Vec<String>>,
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if !prior.is_empty() {
        s.push_str("## Your last episodes here (for continue-vs-new judgment)\n");
        for g in prior {
            let _ = writeln!(s, "- {}", g.replace('\n', " "));
        }
        s.push('\n');
    }
    if !subjects.is_empty() {
        s.push_str("## Subjects you already model (reuse these refs)\n");
        let _ = writeln!(s, "{}", subjects.join(", "));
        s.push('\n');
    }
    s.push_str("## Unconsolidated signals (oldest first)\n");
    for (i, e) in tail.iter().enumerate() {
        let mut line = render_signal(e);
        match face_ids.get(&i).filter(|v| !v.is_empty()) {
            Some(ids) => {
                let _ = write!(line, " ⟨faces: {}⟩", ids.join(", "));
            }
            None if is_image(e) => line.push_str(" ⟨image⟩"),
            None => {}
        }
        if let Some(ids) = voice_ids.get(&i).filter(|v| !v.is_empty()) {
            let _ = write!(line, " ⟨voice: {}⟩", ids.join(", "));
        }
        let _ = writeln!(s, "[{}] {}", i + 1, line);
    }
    s.push_str("\nConsolidate these now.");
    s
}

/// One frontier signal as a transcript line, reusing the snapshot's renderer so
/// the speaker glyph and channel formatting match what the reactor sees.
fn render_signal(e: &JournalEntry) -> String {
    use crate::memory::snapshot::{Speaker, transcript_line};
    match e {
        JournalEntry::SignalIn { channel, body, stream, .. } => {
            transcript_line(Speaker::Them, &channel.with_stream(stream.as_deref()), body.as_str())
        }
        JournalEntry::SignalOut { channel, body, .. } => {
            transcript_line(Speaker::You, channel.as_str(), body.as_str())
        }
    }
}

/// Whether a frontier signal carries a still image — so the prompt can mark it
/// `⟨image⟩` even when face clustering found nothing or is unconfigured.
fn is_image(e: &JournalEntry) -> bool {
    matches!(e, JournalEntry::SignalIn { media: Some(m), .. } if m.mime.starts_with("image/"))
}

/// Mechanically cluster the faces in the frontier's vision signals: for each one,
/// detect+embed and [`people_vectors::assign`] every salient face to the people
/// store (append to a near cluster, or mint a fresh id). Returns, per tail index,
/// the cluster ids the faces landed in — the stable handles the reflection prompt
/// shows so the mind can name a face, even a first-time one. Covers both posted
/// stills and camera-stream minutes (one keyframe decoded out of the video, the
/// same frame the perceive-time note used). No-op (empty) when the face capability
/// is unconfigured; a per-signal failure is logged and skipped.
async fn cluster_faces(
    data_dir: &Path,
    scene: &Scene,
    tail: &[JournalEntry],
) -> HashMap<usize, Vec<String>> {
    let mut out: HashMap<usize, Vec<String>> = HashMap::new();
    if !face::available() {
        return out;
    }
    for (i, e) in tail.iter().enumerate() {
        let JournalEntry::SignalIn { channel: Channel::Vision, media: Some(m), ts, scene: sig_scene, .. } = e
        else {
            continue;
        };
        let is_image = m.mime.starts_with("image/");
        let is_video = m.mime.starts_with("video/");
        if !is_image && !is_video {
            continue;
        }
        let path = layout::channel_day_dir(data_dir, sig_scene, Channel::Vision, *ts).join(&m.file);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(scene = %scene, error = %err, "cluster: reading vision media failed");
                continue;
            }
        };
        // A still is ready for the face pipeline; a camera minute needs one
        // keyframe decoded out first (the same path the perceive-time note takes).
        let image: bytes::Bytes = if is_video {
            match ffmpeg_frame::first_frame(bytes.into()).await {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(scene = %scene, error = %err, "cluster: keyframe extraction failed");
                    continue;
                }
            }
        } else {
            bytes.into()
        };
        // Clone for detection (it consumes the bytes); keep `image` to crop the
        // recognized faces out of for previews.
        let faces = match face::detect_and_embed(image.clone()).await {
            Ok(f) => f,
            Err(err) => {
                tracing::warn!(scene = %scene, error = %err, "cluster: face detect failed");
                continue;
            }
        };
        for f in faces.iter().filter(|f| salient(f)) {
            match people_vectors::assign(data_dir, people_vectors::Modality::Face, &f.embedding).await {
                Ok(id) => {
                    // Keep a viewable crop of this face beside the gallery.
                    if let Ok(jpg) = face::crop_to_jpeg(image.as_ref(), f.bbox, 0.3)
                        && let Err(err) = people_vectors::save_preview(
                            data_dir, &id, people_vectors::Modality::Face, &jpg, "jpg",
                        ).await
                    {
                        tracing::warn!(scene = %scene, error = %err, "cluster: face preview save failed");
                    }
                    out.entry(i).or_default().push(id);
                }
                Err(err) => tracing::warn!(scene = %scene, error = %err, "cluster: assign failed"),
            }
        }
    }
    out
}

/// Mechanically cluster the voices in the frontier's audio clips: for each clip
/// that carries persisted audio, decode it, embed a voiceprint, and
/// [`people_vectors::assign`] it to the people store (append to a near cluster, or
/// mint a fresh id). Returns, per tail index, the cluster ids — the audio twin of
/// [`cluster_faces`], so the mind can name a voice the same way it names a face.
/// No-op (empty) without the voiceprint capability. Only clips have media here;
/// live-mic utterances are media-less and are clustered inline on the stream.
async fn cluster_voices(
    data_dir: &Path,
    scene: &Scene,
    tail: &[JournalEntry],
) -> HashMap<usize, Vec<String>> {
    let mut out: HashMap<usize, Vec<String>> = HashMap::new();
    if !voiceprint::available() {
        return out;
    }
    for (i, e) in tail.iter().enumerate() {
        let JournalEntry::SignalIn { channel: Channel::Audio, media: Some(m), ts, scene: sig_scene, body, .. } = e
        else {
            continue;
        };
        // A diarized, multi-speaker clip ("说话人0：…") is not one voice; embedding
        // the blend into a single sample would contaminate a cluster. Mirror the
        // hear-time guard in `voice_note` and skip it — the labeled transcript
        // already attributes the turns.
        if body.starts_with("说话人") {
            continue;
        }
        let path = layout::channel_day_dir(data_dir, sig_scene, Channel::Audio, *ts).join(&m.file);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(scene = %scene, error = %err, "cluster: reading audio failed");
                continue;
            }
        };
        let samples = match pcm::to_i16_16k_mono(&bytes, &m.mime) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => continue,
            Err(err) => {
                tracing::warn!(scene = %scene, error = %err, "cluster: audio decode failed");
                continue;
            }
        };
        let embedding = match voiceprint::embed(samples).await {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(scene = %scene, error = %err, "cluster: voiceprint embed failed");
                continue;
            }
        };
        match people_vectors::assign(data_dir, people_vectors::Modality::Voice, &embedding).await {
            Ok(id) => {
                // Keep the clip itself beside the gallery as a playable preview.
                let ext = std::path::Path::new(&m.file).extension().and_then(|e| e.to_str()).unwrap_or("wav");
                if let Err(err) = people_vectors::save_preview(
                    data_dir, &id, people_vectors::Modality::Voice, &bytes, ext,
                ).await
                {
                    tracing::warn!(scene = %scene, error = %err, "cluster: voice preview save failed");
                }
                out.entry(i).or_default().push(id);
            }
            Err(err) => tracing::warn!(scene = %scene, error = %err, "cluster: voice assign failed"),
        }
    }
    out
}

/// Skip incidental/background faces: require a confident detection and a face big
/// enough (in original-image pixels) to embed reliably.
fn salient(f: &crate::capabilities::face::DetectedFace) -> bool {
    let w = (f.bbox[2] - f.bbox[0]).max(0.0);
    let h = (f.bbox[3] - f.bbox[1]).max(0.0);
    f.score >= 0.6 && w >= 50.0 && h >= 50.0
}

