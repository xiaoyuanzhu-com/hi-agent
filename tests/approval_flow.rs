//! /approval round-trip — placeholder, requires `claude-code`.
//!
//! Per impl.md § Approval: a worker (or router) calls ACP
//! `session/request_permission`; the reactor builds an `ApprovalEvent`,
//! journals it, broadcasts to GET /approval subscribers, and parks a
//! oneshot. POST /approval flows back through `approval_decisions_rx` into
//! the reactor's pending-approval map; the matching oneshot resolves, the
//! ACP handler returns, and the requesting session resumes (or aborts).
//!
//! Exercising this end-to-end needs a live ACP backend to *generate* the
//! permission request. The handlers themselves are unit-testable in
//! isolation, but the test you actually want — "the round-trip is correct"
//! — is integration-grade. The shell-equivalent is below.
//!
//! ```sh
//! # terminal A — open the approval long-poll
//! curl -N -H 'X-HI-To: alice@phone' http://127.0.0.1:8080/approval
//! # waits for the agent to broadcast an ApprovalEvent JSON
//!
//! # terminal B — drive the agent to request a permission (the worker has to
//! # call ACP session/request_permission; phrase the thought so the router
//! # spawns a worker that needs approval)
//! curl -X POST -H 'X-HI-From: alice@phone' \
//!   --data-binary 'rename ~/notes.txt — but ask me first' \
//!   http://127.0.0.1:8080/thought
//!
//! # terminal A receives JSON like {"id":"<uuid>","peer":"alice@phone", ...}
//! # use that id to POST a decision:
//! curl -X POST -H 'X-HI-From: alice@phone' \
//!   -H 'Content-Type: application/json' \
//!   -d '{"id":"<uuid>","allow":true}' \
//!   http://127.0.0.1:8080/approval
//! # responds 202; the worker then proceeds and emits the result on /thought
//! ```
//!
//! Verify the journal contains, in order: WorkerSpawn, ApprovalRequest,
//! ApprovalDecision (allow=true), then a SignalOut for the worker's reply.

#[tokio::test]
#[ignore = "requires claude-code on PATH; see file header for the shell-equivalent recipe"]
async fn approval_round_trip() {
    // Stub: see file header.
}
