//! Interruption semantics — placeholder, requires `claude-code`.
//!
//! Contract (see `src/reactor/mod.rs` § "Fix-forward, no reflexive cancel"):
//! a new POST arriving for a scene whose queue is already running a turn does
//! NOT cancel the in-flight ACP session. The per-scene loop is serial, so the
//! new signal simply queues and is folded into the next turn; the warm session
//! remembers what it has already heard, so a thought spread across bursts
//! reassembles across turns. The mind corrects course rather than being cut off
//! (fix-forward); the client mutes its own speaker on a hot mic, so an
//! interruption still feels instant.
//!
//! Driving this for real requires either:
//!
//!   (a) A live `claude-code` subprocess responsive to ACP. Tests would
//!       become integration-grade: slow, flaky, machine-dependent.
//!   (b) A mock ACP backend swapped in via a trait. That's a v1-grade
//!       refactor of `src/acp/` — too much surgery for a docs/tests step.
//!
//! We pick neither. The shell-equivalent verification is below; run it by hand
//! after `cargo build --release && ./target/release/hi-agent`:
//!
//! ```sh
//! # in one terminal, watch journal.jsonl
//! tail -F data/journal.jsonl
//!
//! # in another, fire two POSTs in rapid succession to the same scene
//! BASE=http://127.0.0.1:8080
//! ME=alice@phone
//! curl -X POST -H "X-HI-Scene: $ME" \
//!     --data-binary 'first thought, take your time' "$BASE/api/in/text" &
//! sleep 0.2
//! curl -X POST -H "X-HI-Scene: $ME" \
//!     --data-binary 'actually never mind, what time is it' "$BASE/api/in/text"
//! ```
//!
//! Expected: tracing logs show NO "session/cancel" for this scene; the first
//! turn runs to completion; the journal shows both SignalIn entries; and the
//! second signal is folded into a later turn (the warm session already carries
//! the first).

#[tokio::test]
#[ignore = "requires claude-code on PATH; see file header for the shell-equivalent recipe"]
async fn new_post_does_not_cancel_in_flight_turn() {
    // Stub: see file header.
}
