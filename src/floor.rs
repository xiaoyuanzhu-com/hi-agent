//! Live "is the speaker holding the floor right now" signal, per peer.
//!
//! ## Why this exists (the human-interface division of labor)
//!
//! In our model the client is a *dumb face*: it streams the mic and renders
//! whatever the channels emit (audio → speakers, thought → text). It does NOT
//! decide turns — turn-taking is a property of the *mind*, which lives here in
//! the backend. The client's only output-side behavior is a reflex (mute the
//! speaker while its own mic is hot); everything about *when the agent speaks*
//! is decided server-side.
//!
//! But to choose its moment, the mind needs one thing only the client can know:
//! is the human still talking? That signal is born at the mic. We don't add a
//! bespoke control channel for it — the live STT WebSocket already *is* the
//! signal: a socket is open for `/stt/stream` exactly while one utterance is in
//! flight (the client opens it on speech onset, closes it when the utterance
//! ends). So `stt_stream` marks a peer "speaking" for the socket's lifetime,
//! and the reactor reads this to wait for a *settled silence* before it replies
//! ("commit-after-quiet"). A count, not a bool, so overlapping sockets during a
//! barge-in (new utterance opens before the previous closes) read correctly.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::types::PeerId;

/// Shared, cloneable handle to the per-peer floor signal.
#[derive(Clone, Default)]
pub struct FloorState {
    /// Per peer: how many live mic sockets are currently open. >0 ⇒ speaking.
    open: Arc<Mutex<HashMap<PeerId, u32>>>,
}

impl FloorState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A live utterance began (a `/stt/stream` socket opened) for `peer`.
    pub async fn enter_speaking(&self, peer: &PeerId) {
        let mut map = self.open.lock().await;
        *map.entry(peer.clone()).or_insert(0) += 1;
    }

    /// A live utterance ended (its socket closed) for `peer`.
    pub async fn leave_speaking(&self, peer: &PeerId) {
        let mut map = self.open.lock().await;
        if let Some(n) = map.get_mut(peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(peer);
            }
        }
    }

    /// True while any mic socket for `peer` is open — i.e. the human is speaking
    /// and still holds the floor.
    pub async fn is_speaking(&self, peer: &PeerId) -> bool {
        let map = self.open.lock().await;
        map.get(peer).is_some_and(|n| *n > 0)
    }
}
