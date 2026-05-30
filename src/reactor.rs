//! Reactor — per-peer queues + ephemeral routing sessions.
//!
//! One mpsc per peer, one task per peer; routing turns run serially against
//! an ephemeral ACP session. Cognition is delegated to that session; the
//! reactor never blocks on it.
//!
//! Interruption policy: a new POST for a peer whose routing turn is in
//! progress cancels the in-flight ACP session (`session/cancel`). The
//! per-peer task observes the cancel via the session stream ending, drains
//! anything else in the queue, and re-prompts with the merged batch.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;

use crate::acp::{AcpProcess, AcpSession, SessionOpts, SessionUpdate};
use crate::memory::{Memory, build_for_peer};
use crate::server::{AudioEvent, SurfaceEvent, ThoughtBus};
use crate::types::{Channel, JournalEntry, PeerId, Signal, SurfaceEnvelope, SurfaceMode, SurfaceOp};
use crate::voice::Tts;

const ROUTER_SYSTEM_PROMPT: &str = "You are a human-interface agent. \
A peer is talking to you over /thought. Reply naturally with text — your reply \
streams back to them and is spoken aloud, so keep it conversational. You have \
file access, code execution, and the rest of your harness's tools; use them \
freely when helpful.\n\
\n\
The peer often speaks in several short bursts with pauses between them, so by \
the time you reply you may be seeing the whole thing at once under \"New \
signals\". Respond the way a person who was listening the whole time would: \
take in everything they said and answer it as one. When it feels natural, open \
with a brief acknowledgement of what you've understood before your considered \
answer (\"got it — for the flights…\"); if you're still missing something, keep \
it short rather than guessing. Don't pad: a little to say means a short reply. \
What you say is for when they've finished a thought — never talk over them.\n\
\n\
To show rich visual content (an image, a chart, a web page, a table, a \
preview), emit a self-contained HTML block wrapped in surface markers: \
`[[surface:card]]` … `[[/surface]]` for a focused card, or `[[surface:full]]` \
… `[[/surface]]` for a full-screen view. The HTML renders in a sandboxed \
frame: inline all CSS/JS, reference no external resources, and assume a dark \
background. Everything OUTSIDE the markers is what you say aloud — keep the \
spoken part natural and let the surface carry the visuals. Use surfaces \
sparingly, only when a visual genuinely helps.";

const PEER_QUEUE_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct Reactor {
    inner: Arc<ReactorInner>,
}

struct ReactorInner {
    memory: Memory,
    acp: Arc<AcpProcess>,
    thought_bus: ThoughtBus,
    /// Speech synthesis. `None` → the agent's replies are text-only (Phase 1
    /// behavior); when set, each sentence is synthesized and broadcast on /audio.
    tts: Option<Arc<dyn Tts>>,
    /// Outbound audio broadcast that GET /audio subscribers drain.
    audio_out: broadcast::Sender<AudioEvent>,
    /// Outbound rich-content broadcast that GET /surface subscribers drain.
    surface_out: broadcast::Sender<SurfaceEvent>,
    /// Monotonic cognition-turn counter. Each routing turn claims the next id so
    /// the client can tell a fresh reply from a superseded draft (see AudioEvent).
    turn_seq: AtomicU64,
    peers: Mutex<HashMap<PeerId, PeerHandle>>,
}

struct PeerHandle {
    inbound: mpsc::Sender<Signal>,
    /// `None` when idle. Set to the in-flight session so the dispatcher can
    /// cancel it when a new signal arrives for this peer.
    in_flight: Arc<Mutex<Option<Arc<AcpSession>>>>,
}

pub fn start(
    memory: Memory,
    acp: Arc<AcpProcess>,
    mut inbound_rx: mpsc::Receiver<Signal>,
    thought_bus: ThoughtBus,
    tts: Option<Arc<dyn Tts>>,
    audio_out: broadcast::Sender<AudioEvent>,
    surface_out: broadcast::Sender<SurfaceEvent>,
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            acp,
            thought_bus,
            tts,
            audio_out,
            surface_out,
            turn_seq: AtomicU64::new(0),
            peers: Mutex::new(HashMap::new()),
        }),
    };
    let dispatch_reactor = reactor.clone();

    tokio::spawn(async move {
        while let Some(signal) = inbound_rx.recv().await {
            let peer = signal.from.clone();
            dispatch_reactor.deliver_to_peer(peer, signal).await;
        }
        tracing::warn!("reactor inbound channel closed; dispatch loop exiting");
    });

    reactor
}

impl Reactor {
    async fn deliver_to_peer(&self, peer: PeerId, signal: Signal) {
        let (sender, in_flight) = self.get_or_create_peer(peer.clone()).await;

        let in_flight_session: Option<Arc<AcpSession>> = {
            let guard = in_flight.lock().await;
            guard.as_ref().cloned()
        };
        if let Some(session) = in_flight_session {
            if let Err(err) = session.cancel().await {
                tracing::warn!(peer = %peer, error = %err, "session/cancel failed during interruption");
            } else {
                tracing::debug!(peer = %peer, "interrupting in-flight routing turn");
            }
        }

        if let Err(err) = sender.send(signal).await {
            tracing::error!(peer = %peer, error = %err, "peer inbound channel closed; dropping signal");
        }
    }

    async fn get_or_create_peer(
        &self,
        peer: PeerId,
    ) -> (mpsc::Sender<Signal>, Arc<Mutex<Option<Arc<AcpSession>>>>) {
        let mut peers = self.inner.peers.lock().await;
        if let Some(handle) = peers.get(&peer) {
            return (handle.inbound.clone(), handle.in_flight.clone());
        }

        let (tx, rx) = mpsc::channel::<Signal>(PEER_QUEUE_CAPACITY);
        let in_flight: Arc<Mutex<Option<Arc<AcpSession>>>> = Arc::new(Mutex::new(None));
        peers.insert(
            peer.clone(),
            PeerHandle {
                inbound: tx.clone(),
                in_flight: in_flight.clone(),
            },
        );
        drop(peers);

        let task_reactor = self.clone();
        let task_peer = peer.clone();
        let task_in_flight = in_flight.clone();
        tokio::spawn(async move {
            per_peer_loop(task_reactor, task_peer, rx, task_in_flight).await;
        });

        (tx, in_flight)
    }
}

async fn per_peer_loop(
    reactor: Reactor,
    peer: PeerId,
    mut inbound: mpsc::Receiver<Signal>,
    in_flight: Arc<Mutex<Option<Arc<AcpSession>>>>,
) {
    loop {
        let first = match inbound.recv().await {
            Some(s) => s,
            None => {
                tracing::info!(peer = %peer, "per-peer inbound closed; exiting loop");
                return;
            }
        };
        let mut batch: Vec<Signal> = vec![first];
        while let Ok(extra) = inbound.try_recv() {
            batch.push(extra);
        }

        if let Err(err) = run_routing_turn(&reactor, &peer, &batch, &in_flight).await {
            tracing::warn!(peer = %peer, error = %err, "routing turn failed");
            let mut guard = in_flight.lock().await;
            *guard = None;
        }
    }
}

/// One routing turn: build snapshot, open ephemeral ACP session, stream text
/// updates to `/thought`, broadcast `EndOfUtterance` when the turn ends.
async fn run_routing_turn(
    reactor: &Reactor,
    peer: &PeerId,
    batch: &[Signal],
    in_flight: &Arc<Mutex<Option<Arc<AcpSession>>>>,
) -> anyhow::Result<()> {
    let turn_id = reactor.inner.turn_seq.fetch_add(1, Ordering::Relaxed);
    let snap = build_for_peer(&reactor.inner.memory, peer).await?;
    let prompt_text = format!(
        "{}\n\n## New signals\n{}",
        snap.render_for_prompt(),
        render_batch(batch),
    );

    let session = Arc::new(
        reactor
            .inner
            .acp
            .new_session(SessionOpts {
                system_prompt: Some(ROUTER_SYSTEM_PROMPT.to_string()),
                cwd: None,
            })
            .await?,
    );

    {
        let mut guard = in_flight.lock().await;
        *guard = Some(session.clone());
    }

    let tts = reactor.inner.tts.clone();

    let outcome: anyhow::Result<()> = async {
        let mut run = session.prompt(prompt_text).await?;

        // Per-sentence TTS: a background task fed by an mpsc queue synthesizes
        // completed sentences in order and broadcasts them on /audio, so text
        // streaming is never blocked on synthesis. The queue/task exist only
        // when TTS is configured.
        let mut splitter = SentenceSplitter::new();
        let mut extractor = SurfaceExtractor::new();
        // Accumulate the spoken text so the whole reply is logged once at end of
        // turn on the `channel` stream, rather than per-chunk (which is noisy).
        let mut full_reply = String::new();
        let (synth_tx, synth_handle) = match tts {
            Some(tts) => {
                let (tx, rx) = mpsc::channel::<String>(64);
                let handle = tokio::spawn(synth_loop(
                    tts,
                    reactor.inner.audio_out.clone(),
                    peer.clone(),
                    turn_id,
                    rx,
                ));
                (Some(tx), Some(handle))
            }
            None => (None, None),
        };

        loop {
            match run.next_update().await {
                Some(SessionUpdate::Text(text)) => {
                    // Split rich-content surface blocks out of the spoken text.
                    let (clean, surfaces) = extractor.push(&text);
                    for envelope in surfaces {
                        emit_surface(reactor, peer, envelope);
                    }
                    if !clean.is_empty() {
                        full_reply.push_str(&clean);
                        if let Some(tx) = &synth_tx {
                            for sentence in splitter.push(&clean) {
                                let _ = tx.send(sentence).await;
                            }
                        }
                        emit_thought_chunk(reactor, peer, clean).await;
                    }
                }
                Some(SessionUpdate::Thought(_)) => {
                    // Internal reasoning; do not surface.
                }
                Some(SessionUpdate::ToolCall(stub)) => {
                    tracing::debug!(peer = %peer, variant = stub.raw_variant, "tool call");
                }
                Some(SessionUpdate::Other(name)) => {
                    tracing::trace!(peer = %peer, variant = %name, "ignored ACP update");
                }
                None => break,
            }
        }

        // Drain any text the surface extractor was still holding, then flush
        // the trailing partial sentence to TTS.
        let clean_tail = extractor.flush();
        if !clean_tail.is_empty() {
            full_reply.push_str(&clean_tail);
            if let Some(tx) = &synth_tx {
                for sentence in splitter.push(&clean_tail) {
                    let _ = tx.send(sentence).await;
                }
            }
            emit_thought_chunk(reactor, peer, clean_tail).await;
        }
        if !full_reply.trim().is_empty() {
            crate::channel_log::outbound(Channel::Thought, peer, full_reply.trim());
        }
        if let Some(tx) = &synth_tx {
            if let Some(tail) = splitter.flush() {
                let _ = tx.send(tail).await;
            }
        }

        let mut cancelled = false;
        match run.wait().await {
            Ok(result) => {
                tracing::debug!(peer = %peer, stop = ?result.stop_reason, "routing turn finished");
            }
            Err(err) => {
                cancelled = true;
                tracing::debug!(peer = %peer, error = %err, "routing run ended with error (likely cancel)");
            }
        }

        // Closing the queue lets the synth task drain queued sentences; on a
        // cancel (barge-in) abort it so stale audio isn't spoken over the user.
        drop(synth_tx);
        if let Some(handle) = synth_handle {
            if cancelled {
                handle.abort();
            }
        }
        Ok(())
    }
    .await;

    // End of utterance — closes the GET /thought response that's been
    // streaming this turn's chunks.
    emit_end_of_utterance(reactor, peer).await;

    {
        let mut guard = in_flight.lock().await;
        *guard = None;
    }
    drop(session);

    outcome
}

async fn emit_thought_chunk(reactor: &Reactor, peer: &PeerId, text: String) {
    let ts = Utc::now();
    let entry = JournalEntry::SignalOut {
        ts,
        channel: Channel::Thought,
        to: peer.clone(),
        body: text.clone(),
        media_path: None,
    };
    if let Err(err) = reactor.inner.memory.journal.append(entry).await {
        tracing::error!(peer = %peer, error = %err, "journal append failed for outbound thought");
    }
    reactor.inner.thought_bus.push_chunk(peer, text).await;
}

async fn emit_end_of_utterance(reactor: &Reactor, peer: &PeerId) {
    reactor.inner.thought_bus.end_utterance(peer).await;
}

fn render_batch(batch: &[Signal]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for sig in batch {
        let ts = sig.ts.format("%H:%M:%S");
        let _ = writeln!(
            s,
            "[{}] {} on /{}: \"{}\"",
            ts, sig.from, sig.channel, sig.body
        );
    }
    s
}

/// Background task: synthesize each queued sentence and broadcast it on /audio.
/// One queue, sequential await → audio plays in order. Send errors are ignored
/// (no subscriber connected is fine); synth failures are logged, not fatal.
async fn synth_loop(
    tts: Arc<dyn Tts>,
    audio_out: broadcast::Sender<AudioEvent>,
    peer: PeerId,
    turn: u64,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(sentence) = rx.recv().await {
        match tts.synthesize(&sentence).await {
            Ok(blob) => {
                tracing::info!(
                    target: "channel",
                    dir = "out",
                    channel = "audio",
                    peer = %peer,
                    turn = turn,
                    bytes = blob.bytes.len(),
                    text = %sentence,
                    "channel out (tts)",
                );
                let event = AudioEvent {
                    to: Some(peer.clone()),
                    mime: blob.mime,
                    bytes: blob.bytes,
                    turn,
                    ts: Utc::now(),
                };
                let _ = audio_out.send(event);
            }
            Err(err) => {
                tracing::warn!(peer = %peer, error = %err, "TTS synthesize failed");
            }
        }
    }
}

/// Minimal incremental sentence splitter for per-sentence TTS, mirroring the
/// frontend `sentences.ts`: CJK terminators (。！？) split immediately; Latin
/// terminators (.!?…) split only when followed by whitespace, so decimals and
/// abbreviations aren't broken. The trailing partial waits for `flush()`.
struct SentenceSplitter {
    buf: String,
}

impl SentenceSplitter {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(idx) = find_boundary(&self.buf) {
            let sentence = self.buf[..idx].trim().to_string();
            self.buf = self.buf[idx..].trim_start().to_string();
            if !sentence.is_empty() {
                out.push(sentence);
            }
        }
        out
    }

    fn flush(&mut self) -> Option<String> {
        let s = self.buf.trim().to_string();
        self.buf.clear();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// Byte index just past the first sentence terminator that qualifies as a
/// boundary, or `None` if the buffer holds no complete sentence yet.
fn find_boundary(s: &str) -> Option<usize> {
    let mut chars = s.char_indices().peekable();
    while let Some((off, c)) = chars.next() {
        let end = off + c.len_utf8();
        if matches!(c, '。' | '！' | '？') {
            return Some(end);
        }
        if matches!(c, '.' | '!' | '?' | '…') {
            if let Some(&(_, next)) = chars.peek() {
                if next.is_whitespace() {
                    return Some(end);
                }
            }
        }
    }
    None
}

/// Emit one rich-content envelope to GET /surface subscribers for this peer.
fn emit_surface(reactor: &Reactor, peer: &PeerId, envelope: SurfaceEnvelope) {
    tracing::info!(
        target: "channel",
        dir = "out",
        channel = "surface",
        peer = %peer,
        op = ?envelope.op,
        mode = ?envelope.mode,
        html_len = envelope.html.as_deref().map(str::len).unwrap_or(0),
        "channel out (surface)",
    );
    let event = SurfaceEvent {
        to: Some(peer.clone()),
        envelope,
        ts: Utc::now(),
    };
    let _ = reactor.inner.surface_out.send(event);
}

const OPEN_CARD: &str = "[[surface:card]]";
const OPEN_FULL: &str = "[[surface:full]]";
const CLOSE: &str = "[[/surface]]";

/// Streaming extractor that pulls `[[surface:…]] … [[/surface]]` HTML blocks out
/// of the agent's text. Text outside the markers passes through (spoken +
/// displayed); the inner HTML becomes a `SurfaceEnvelope`. A short tail that
/// could be a partial opener is held back so a marker split across chunks is
/// still recognized. Mirrors the convention taught in ROUTER_SYSTEM_PROMPT.
struct SurfaceExtractor {
    buf: String,
    inside: Option<SurfaceMode>,
}

impl SurfaceExtractor {
    fn new() -> Self {
        Self { buf: String::new(), inside: None }
    }

    fn push(&mut self, chunk: &str) -> (String, Vec<SurfaceEnvelope>) {
        self.buf.push_str(chunk);
        let mut text_out = String::new();
        let mut envelopes = Vec::new();

        loop {
            match self.inside {
                None => {
                    let card = self.buf.find(OPEN_CARD);
                    let full = self.buf.find(OPEN_FULL);
                    let opener = match (card, full) {
                        (Some(c), Some(f)) if c <= f => Some((c, SurfaceMode::Card, OPEN_CARD.len())),
                        (Some(_), Some(f)) => Some((f, SurfaceMode::Full, OPEN_FULL.len())),
                        (Some(c), None) => Some((c, SurfaceMode::Card, OPEN_CARD.len())),
                        (None, Some(f)) => Some((f, SurfaceMode::Full, OPEN_FULL.len())),
                        (None, None) => None,
                    };
                    if let Some((idx, mode, tok_len)) = opener {
                        text_out.push_str(&self.buf[..idx]);
                        self.buf = self.buf[idx + tok_len..].to_string();
                        self.inside = Some(mode);
                        continue;
                    }
                    // No opener: emit everything except a tail that could be the
                    // start of one continuing in the next chunk.
                    let keep = partial_open_suffix_len(&self.buf);
                    let emit_to = self.buf.len() - keep;
                    text_out.push_str(&self.buf[..emit_to]);
                    self.buf = self.buf[emit_to..].to_string();
                    break;
                }
                Some(mode) => {
                    if let Some(idx) = self.buf.find(CLOSE) {
                        let html = self.buf[..idx].trim().to_string();
                        self.buf = self.buf[idx + CLOSE.len()..].to_string();
                        self.inside = None;
                        envelopes.push(SurfaceEnvelope {
                            id: Uuid::now_v7().to_string(),
                            op: SurfaceOp::Show,
                            mode: Some(mode),
                            html: Some(html),
                            ttl_ms: None,
                        });
                        continue;
                    }
                    break; // close not present yet; keep buffering the HTML
                }
            }
        }
        (text_out, envelopes)
    }

    /// Emit any held-back text at end of turn. An unterminated block is dropped.
    fn flush(&mut self) -> String {
        let out = if self.inside.is_none() {
            std::mem::take(&mut self.buf)
        } else {
            String::new()
        };
        self.buf.clear();
        self.inside = None;
        out
    }
}

/// Length (bytes) of the longest suffix of `buf` that is a proper prefix of a
/// surface opener — i.e. a marker possibly split across chunks.
fn partial_open_suffix_len(buf: &str) -> usize {
    let max = OPEN_CARD.len().max(OPEN_FULL.len()) - 1;
    let start = buf.len().saturating_sub(max);
    for i in start..buf.len() {
        if !buf.is_char_boundary(i) {
            continue;
        }
        let suffix = &buf[i..];
        if OPEN_CARD.starts_with(suffix) || OPEN_FULL.starts_with(suffix) {
            return buf.len() - i;
        }
    }
    0
}

#[cfg(test)]
mod surface_tests {
    use super::SurfaceExtractor;
    use crate::types::SurfaceMode;

    #[test]
    fn passes_plain_text_through() {
        let mut e = SurfaceExtractor::new();
        let (t, s) = e.push("just talking, nothing to show");
        assert!(s.is_empty());
        assert_eq!(format!("{t}{}", e.flush()), "just talking, nothing to show");
    }

    #[test]
    fn extracts_a_card_across_chunks() {
        let mut e = SurfaceExtractor::new();
        let (t1, s1) = e.push("Here you go. [[surface:card]]<b>hi</b>");
        assert!(s1.is_empty());
        assert_eq!(t1, "Here you go. ");
        let (t2, s2) = e.push("[[/surface]] Done.");
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].mode, Some(SurfaceMode::Card));
        assert_eq!(s2[0].html.as_deref(), Some("<b>hi</b>"));
        assert_eq!(format!("{t2}{}", e.flush()), " Done.");
    }

    #[test]
    fn recognizes_marker_split_across_chunks() {
        let mut e = SurfaceExtractor::new();
        let (t1, s1) = e.push("look [[surf");
        assert!(s1.is_empty());
        assert_eq!(t1, "look ");
        let (t2, s2) = e.push("ace:full]]<p>x</p>[[/surface]]");
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].mode, Some(SurfaceMode::Full));
        assert_eq!(s2[0].html.as_deref(), Some("<p>x</p>"));
        assert_eq!(t2, "");
    }
}

#[cfg(test)]
mod tests {
    use super::SentenceSplitter;

    #[test]
    fn splits_latin_on_terminator_plus_space() {
        let mut s = SentenceSplitter::new();
        assert_eq!(s.push("Hello world"), Vec::<String>::new());
        assert_eq!(s.push(". How are"), vec!["Hello world.".to_string()]);
        assert_eq!(s.flush(), Some("How are".to_string()));
    }

    #[test]
    fn does_not_split_decimals() {
        let mut s = SentenceSplitter::new();
        assert_eq!(s.push("pi is 3.14 today"), Vec::<String>::new());
    }

    #[test]
    fn splits_cjk_immediately() {
        let mut s = SentenceSplitter::new();
        assert_eq!(
            s.push("你好。最近怎么样？"),
            vec!["你好。".to_string(), "最近怎么样？".to_string()]
        );
    }
}
