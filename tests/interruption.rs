//! Interruption semantics — placeholder, requires `claude-code`.
//!
//! Per impl.md § Aliveness — Cognition contract: when a new POST arrives for
//! a scene whose queue is already running a routing turn, the reactor cancels
//! the in-flight ACP session and re-prompts with the merged batch. The
//! reactor implementation already does this (`reactor.rs` § "Interruption
//! policy") — what we lack is a hermetic way to drive it from a test.
//!
//! Driving it for real requires either:
//!
//!   (a) A live `claude-code` subprocess responsive to ACP. Tests would
//!       become integration-grade: slow, flaky, machine-dependent.
//!   (b) A mock ACP backend swapped in via a trait. That's a v1-grade
//!       refactor of `src/acp/` — too much surgery for Step 10 (docs +
//!       tests, not code).
//!
//! Step 10 picks neither. The shell-equivalent verification is below; run
//! it by hand after `cargo build --release && ./target/release/hi-agent`:
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
//! Expected: tracing logs show "session/cancel" on the first ACP session;
//! the journal shows both SignalIn entries; the prompt to the second router
//! turn contains both signals in its `recent_journal`.

#[tokio::test]
#[ignore = "requires claude-code on PATH; see file header for the shell-equivalent recipe"]
async fn new_post_aborts_in_flight_routing() {
    // Stub: see file header.
}
