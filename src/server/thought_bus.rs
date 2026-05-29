//! Per-peer outbound buffer for `/thought`, replacing a lossy broadcast.
//!
//! POST `/thought` is fire-and-forget (`202`); the agent's reply streams back
//! out on GET `/thought`. The previous design broadcast each chunk over a
//! `tokio::broadcast`, which only delivers to receivers that already exist at
//! `send()` time. So a reply produced before the first GET — or in the
//! reconnect gap between two utterances — was dropped on the floor. The field
//! symptom was "send hi, nothing responds": the reply was produced and
//! journalled, but the client's GET re-subscribed milliseconds too late.
//!
//! This bus buffers utterances per peer. A GET that opens late still receives
//! the pending utterance; one that opens mid-utterance streams it from its
//! first chunk. Delivery is FIFO and an utterance is removed once a reader has
//! drained it to completion, so the next GET picks up the next utterance.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Bytes;
use futures::stream::{Stream, unfold};
use tokio::sync::{Mutex, Notify};

use crate::types::PeerId;

/// Cap on buffered utterances per peer. Bounds growth when a peer produces
/// output that no client ever connects to consume; the oldest utterances are
/// evicted first. Per-peer routing turns are serial, so reaching this many
/// undelivered utterances means nobody has polled in a long while.
const MAX_BUFFERED_PER_PEER: usize = 32;

/// Outbound `/thought` buffer, keyed by recipient peer. Cloneable handle over
/// shared state.
#[derive(Clone, Default)]
pub struct ThoughtBus {
    inner: Arc<Mutex<HashMap<PeerId, PeerOut>>>,
}

struct PeerOut {
    queue: VecDeque<Utterance>,
    /// Pulsed whenever `queue` changes (new chunk, new utterance, completion)
    /// so a parked reader re-checks.
    notify: Arc<Notify>,
    /// Monotonic utterance id, unique within this peer.
    next_id: u64,
}

impl PeerOut {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            notify: Arc::new(Notify::new()),
            next_id: 0,
        }
    }
}

struct Utterance {
    id: u64,
    chunks: Vec<String>,
    complete: bool,
}

impl ThoughtBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk of agent text destined for `peer`. Starts a new utterance
    /// when the previous one has completed (or none exists). Empty chunks are
    /// dropped so they neither open an utterance nor emit empty body frames.
    pub async fn push_chunk(&self, peer: &PeerId, text: String) {
        if text.is_empty() {
            return;
        }
        let mut map = self.inner.lock().await;
        let entry = map.entry(peer.clone()).or_insert_with(PeerOut::new);

        let need_new = match entry.queue.back() {
            Some(u) => u.complete,
            None => true,
        };
        if need_new {
            while entry.queue.len() >= MAX_BUFFERED_PER_PEER {
                entry.queue.pop_front();
            }
            let id = entry.next_id;
            entry.next_id += 1;
            entry.queue.push_back(Utterance {
                id,
                chunks: Vec::new(),
                complete: false,
            });
        }
        if let Some(u) = entry.queue.back_mut() {
            u.chunks.push(text);
        }
        entry.notify.notify_waiters();
    }

    /// Mark the peer's open utterance complete. A reader streaming it will close
    /// its HTTP body once it has drained the buffered chunks — the spec's
    /// body-close = end-of-utterance contract.
    pub async fn end_utterance(&self, peer: &PeerId) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(peer) {
            if let Some(u) = entry.queue.back_mut() {
                u.complete = true;
            }
            entry.notify.notify_waiters();
        }
    }

    /// A stream yielding the bytes of exactly one utterance for `subscriber`,
    /// closing when that utterance ends. Binds to the oldest buffered utterance
    /// (waiting for one to appear if the queue is empty), streams its chunks as
    /// they arrive, and removes it once fully drained.
    pub fn subscribe(
        &self,
        subscriber: PeerId,
    ) -> impl Stream<Item = Result<Bytes, Infallible>> + use<> {
        struct Reader {
            inner: Arc<Mutex<HashMap<PeerId, PeerOut>>>,
            peer: PeerId,
            bound: Option<u64>,
            cursor: usize,
        }

        let state = Reader {
            inner: self.inner.clone(),
            peer: subscriber,
            bound: None,
            cursor: 0,
        };

        unfold(state, |mut s| async move {
            // Hold the lock via a local Arc so we can freely mutate `s`'s other
            // fields while the guard is alive.
            let inner = s.inner.clone();
            loop {
                let mut map = inner.lock().await;
                let entry = map.entry(s.peer.clone()).or_insert_with(PeerOut::new);

                if s.bound.is_none()
                    && let Some(front) = entry.queue.front()
                {
                    s.bound = Some(front.id);
                    s.cursor = 0;
                }

                if let Some(id) = s.bound {
                    match entry.queue.iter().find(|u| u.id == id) {
                        Some(u) if s.cursor < u.chunks.len() => {
                            let chunk = u.chunks[s.cursor].clone();
                            s.cursor += 1;
                            return Some((Ok(Bytes::from(chunk)), s));
                        }
                        Some(u) if u.complete => {
                            // Drained and done: drop it so the next GET picks up
                            // the following utterance, and close the body.
                            entry.queue.retain(|x| x.id != id);
                            return None;
                        }
                        Some(_) => {}        // open, awaiting more chunks
                        None => return None, // removed out from under us
                    }
                }

                // Nothing to yield yet. Enroll on the notify *while still
                // holding the lock* so a `notify_waiters()` between here and the
                // await cannot be lost, then release the lock and park.
                let notify = entry.notify.clone();
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                drop(map);
                notified.await;
            }
        })
    }
}
