//! Raw ACP wire tap — a business-logic-agnostic mirror of the JSON-RPC frames
//! flowing between hi-agent and every session's ACP subprocess.
//!
//! The [`Observatory`](crate::foundation::observatory::Observatory) renders the *reactor's*
//! view of a session (turns, context budget, hot-swaps, alarms). This is the
//! opposite: the rawest possible window, knowing nothing about the reactor. It
//! taps the one place every frame transits — the `with_debug` hook on the ACP
//! connection (see [`crate::foundation::acp::process`]) — and records each line verbatim,
//! tagged with a per-connection id (one subprocess hosts one session, so this
//! groups a session's frames together), the scene, the direction, and whatever
//! `sessionId`/`method`/`id` can be parsed out of the JSON.
//!
//! It mirrors the observatory's ring+broadcast shape so the SSE handler reads it
//! the same way (replay-then-live), but its [`record`](AcpTap::record) is
//! **synchronous** — the `with_debug` hook is a plain `Fn`, so the tap cannot
//! await. A `std::sync::Mutex` guards a short, IO-free critical section.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// How many recent frames the in-memory ring retains for SSE replay-on-connect.
/// Larger than the observatory's event ring: frames are higher-frequency (every
/// chunk of a streamed reply is a notification) and this is a debug surface.
const RING_CAP: usize = 4000;
/// Broadcast backlog; a subscriber that lags past this misses frames (surfaced as
/// a gap, never blocks the producer).
const BROADCAST_CAP: usize = 1024;

/// Direction of a raw frame on the wire, from hi-agent's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Sent to the subprocess (requests we issue, responses to its requests).
    Send,
    /// Received from the subprocess (its responses, its notifications/requests).
    Recv,
    /// A line the subprocess wrote to stderr.
    Stderr,
}

/// One raw JSON-RPC line, verbatim, plus the little we parse out for grouping.
#[derive(Debug, Clone, Serialize)]
pub struct RawFrame {
    /// Monotonic, gap-free sequence number assigned at record time.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    /// Which subprocess/connection emitted this. One subprocess hosts exactly one
    /// session, so the inspector groups a session's frames by this — including the
    /// `initialize`/`session/new` frames that precede (and so carry no) `sessionId`.
    pub conn: u64,
    pub scene: String,
    pub dir: Dir,
    /// `sessionId` parsed from `params`/`result`, when present. The `initialize`
    /// handshake and the `session/new` request carry `None` (the id doesn't exist
    /// yet); they still group with the session via `conn`.
    pub session_id: Option<String>,
    /// The JSON-RPC `method`, for requests and notifications.
    pub method: Option<String>,
    /// The JSON-RPC `id`, for request/response correlation (number or string).
    pub id: Option<Value>,
    /// The line exactly as it crossed the wire.
    pub raw: String,
}

/// Cloneable handle over the shared raw-frame ring + live broadcast.
#[derive(Clone)]
pub struct AcpTap {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    tx: broadcast::Sender<RawFrame>,
}

struct State {
    seq: u64,
    ring: VecDeque<RawFrame>,
}

impl AcpTap {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State { seq: 0, ring: VecDeque::new() }),
                tx,
            }),
        }
    }

    /// Record one raw line. Synchronous and non-blocking: assigns a seq, pushes
    /// to the bounded ring, and broadcasts. Safe to call from the `with_debug`
    /// hook (no await, no IO under the lock). A poisoned lock is ignored — the
    /// tap is a convenience, never load-bearing. `conn` identifies the emitting
    /// subprocess so the inspector can group one session's frames together.
    pub fn record(&self, conn: u64, scene: &str, dir: Dir, line: &str) {
        let (session_id, method, id) = parse_meta(line);
        let mut state = match self.inner.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.seq += 1;
        let frame = RawFrame {
            seq: state.seq,
            ts: Utc::now(),
            conn,
            scene: scene.to_string(),
            dir,
            session_id,
            method,
            id,
            raw: line.to_string(),
        };
        state.ring.push_back(frame.clone());
        while state.ring.len() > RING_CAP {
            state.ring.pop_front();
        }
        drop(state);
        // No subscribers is fine.
        let _ = self.inner.tx.send(frame);
    }

    /// Snapshot the ring and subscribe to the live feed atomically, mirroring
    /// [`Observatory::subscribe`](crate::foundation::observatory::Observatory::subscribe):
    /// the replay `Vec` is every frame so far, the receiver yields frames
    /// recorded after this call, with no overlap (both happen under the lock).
    pub fn subscribe(&self) -> (Vec<RawFrame>, broadcast::Receiver<RawFrame>) {
        let state = match self.inner.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let rx = self.inner.tx.subscribe();
        let replay = state.ring.iter().cloned().collect();
        (replay, rx)
    }
}

impl Default for AcpTap {
    fn default() -> Self {
        Self::new()
    }
}

/// Pull the grouping metadata out of a raw JSON-RPC line. Best-effort: a line
/// that doesn't parse as JSON (rare; stderr spew) yields all-`None`, which the
/// inspector still shows verbatim.
fn parse_meta(line: &str) -> (Option<String>, Option<String>, Option<Value>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return (None, None, None);
    };
    // ACP rides JSON-RPC, so the session id lives under `params` (requests and
    // notifications) or `result` (the `session/new` response), camelCased.
    let session_id = v
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .or_else(|| v.get("result").and_then(|r| r.get("sessionId")))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    let method = v.get("method").and_then(|m| m.as_str()).map(str::to_string);
    let id = v.get("id").cloned();
    (session_id, method, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_id_from_params() {
        let line = r#"{"jsonrpc":"2.0","method":"session/prompt","id":3,"params":{"sessionId":"sess-abc","prompt":[]}}"#;
        let (sid, method, id) = parse_meta(line);
        assert_eq!(sid.as_deref(), Some("sess-abc"));
        assert_eq!(method.as_deref(), Some("session/prompt"));
        assert_eq!(id, Some(serde_json::json!(3)));
    }

    #[test]
    fn parses_session_id_from_new_session_result() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"sess-xyz"}}"#;
        let (sid, method, _) = parse_meta(line);
        assert_eq!(sid.as_deref(), Some("sess-xyz"));
        assert_eq!(method, None, "responses carry no method");
    }

    #[test]
    fn non_json_line_is_all_none_but_still_recordable() {
        let (sid, method, id) = parse_meta("Unexpected case: whatever");
        assert!(sid.is_none() && method.is_none() && id.is_none());
    }

    #[tokio::test]
    async fn subscribe_replays_then_streams_live() {
        let tap = AcpTap::new();
        tap.record(0, "alice@phone", Dir::Send, r#"{"method":"initialize","id":0}"#);
        let (replay, mut rx) = tap.subscribe();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, 1);
        assert_eq!(replay[0].conn, 0);
        assert_eq!(replay[0].dir, Dir::Send);

        tap.record(0, "alice@phone", Dir::Recv, r#"{"id":0,"result":{}}"#);
        let live = rx.recv().await.unwrap();
        assert_eq!(live.seq, 2, "live frame follows replay with no gap or dup");
        assert_eq!(live.dir, Dir::Recv);
    }
}
