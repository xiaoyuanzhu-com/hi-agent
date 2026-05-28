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
use crate::server::ThoughtEvent;
use crate::types::{Channel, JournalEntry, PeerId, Signal};

const ROUTER_SYSTEM_PROMPT: &str = "You are a human-interface agent. \
A peer is talking to you over /thought. Reply naturally with text — \
your reply streams back to them and closes when you stop talking. \
You have file access, code execution, and the rest of your harness's \
tools; use them freely when helpful.";

const PEER_QUEUE_CAPACITY: usize = 64;
const AGENT_SENDER: &str = "agent@self";

#[derive(Clone)]
pub struct Reactor {
    inner: Arc<ReactorInner>,
}

struct ReactorInner {
    memory: Memory,
    acp: Arc<AcpProcess>,
    thought_out: broadcast::Sender<ThoughtEvent>,
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
    thought_out: broadcast::Sender<ThoughtEvent>,
) -> Reactor {
    let reactor = Reactor {
        inner: Arc::new(ReactorInner {
            memory,
            acp,
            thought_out,
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

    let outcome: anyhow::Result<()> = async {
        let mut run = session.prompt(prompt_text).await?;
        loop {
            let update = run.next_update().await;
            match update {
                Some(SessionUpdate::Text(text)) => {
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
        match run.wait().await {
            Ok(result) => {
                tracing::debug!(peer = %peer, stop = ?result.stop_reason, "routing turn finished");
            }
            Err(err) => {
                tracing::debug!(peer = %peer, error = %err, "routing run ended with error (likely cancel)");
            }
        }
        Ok(())
    }
    .await;

    // End of utterance — closes the GET /thought response that's been
    // streaming this turn's chunks.
    emit_end_of_utterance(reactor, peer);

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
    let event = ThoughtEvent::Chunk {
        to: Some(peer.clone()),
        from: PeerId(AGENT_SENDER.to_string()),
        text,
    };
    if let Err(err) = reactor.inner.thought_out.send(event) {
        tracing::debug!(peer = %peer, error = %err, "no thought subscribers for chunk");
    }
}

fn emit_end_of_utterance(reactor: &Reactor, peer: &PeerId) {
    let event = ThoughtEvent::EndOfUtterance {
        to: Some(peer.clone()),
    };
    let _ = reactor.inner.thought_out.send(event);
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
