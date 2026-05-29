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

use chrono::Utc;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::acp::{AcpProcess, AcpSession, SessionOpts, SessionUpdate};
use crate::memory::{Memory, build_for_peer};
use crate::server::{AudioEvent, ThoughtBus};
use crate::types::{Channel, JournalEntry, PeerId, Signal};
use crate::voice::Tts;

const ROUTER_SYSTEM_PROMPT: &str = "You are a human-interface agent. \
A peer is talking to you over /thought. Reply naturally with text — \
your reply streams back to them and closes when you stop talking. \
You have file access, code execution, and the rest of your harness's \
tools; use them freely when helpful.";

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
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            acp,
            thought_bus,
            tts,
            audio_out,
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
        let (synth_tx, synth_handle) = match tts {
            Some(tts) => {
                let (tx, rx) = mpsc::channel::<String>(64);
                let handle =
                    tokio::spawn(synth_loop(tts, reactor.inner.audio_out.clone(), peer.clone(), rx));
                (Some(tx), Some(handle))
            }
            None => (None, None),
        };

        loop {
            match run.next_update().await {
                Some(SessionUpdate::Text(text)) => {
                    if let Some(tx) = &synth_tx {
                        for sentence in splitter.push(&text) {
                            let _ = tx.send(sentence).await;
                        }
                    }
                    emit_thought_chunk(reactor, peer, text).await;
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

        // Flush the trailing partial sentence to TTS.
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
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(sentence) = rx.recv().await {
        match tts.synthesize(&sentence).await {
            Ok(blob) => {
                let event = AudioEvent {
                    to: Some(peer.clone()),
                    mime: blob.mime,
                    bytes: blob.bytes,
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
