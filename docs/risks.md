# Risks

Open and known-unverified items for the hi-agent v0 implementation. This is
the Step 0 spike output, deferred until the user batch-builds and runs.

The implementation is complete; the risks below are about things the code
*assumes* but no one has measured yet, plus the v0 acceptance checklist for
first build.

## Concurrent ACP sessions

**Status:** unverified.

The architecture assumes one `claude-code` subprocess hosts N concurrent ACP
sessions: one ephemeral router per active peer and one long-lived session per
worker, all multiplexed over the same stdio pair. The
`agent-client-protocol` 0.12 crate documents this support and `src/acp/`
opens sessions independently, but `claude-code`'s behavior under sustained
concurrent prompts is not observed.

**Verify with:**
```
cargo run --example acp_spike
```
The example opens 3 sessions in parallel, prompts each, and waits. If the
wall-clock latency is roughly max-of-three (not sum-of-three), concurrency
works. If it's sum-of-three, claude-code is serializing — see the fallback.

**Fallback if no concurrency:** wrap routing-session creation in a
`tokio::sync::Semaphore` with permits = 1 (or small N). Workers can keep
their own permit pool. The reactor's dispatch model already serializes one
turn per peer; adding a global cap is a localized change in `reactor.rs`
(wrap the `AcpSession::new_prompt(...)` call in `acquire().await`).

## MCP attachment per ephemeral session

**Status:** unverified.

Each routing session needs the toolbelt available. The hub launches a tiny
shim subprocess (`hi-agent mcp-shim`) that bridges stdio↔Unix socket, and
attaches it through the ACP `mcp_servers` capability. Per-session attach
cost is unknown; impl.md flags this as something to measure in the same
spike.

**Verify with:** same `acp_spike` example — extend it to attach the hub's
shim to each session and confirm `tools/list` returns the seven router
tools within a reasonable budget (target: < 500 ms per attach).

**Fallback:** investigate session-template / shared-MCP-server reuse if
`claude-code` exposes one. Otherwise pre-spawn shims in a small pool keyed
by peer.

## Journal-as-context coherence

**Status:** designed correct; not load-tested.

Routers depend on memory snapshots being faithful. The reactor writes to
`journal.jsonl` **before** spawning the routing session — `server::thought::post_thought`
journals the inbound, then sends to the reactor's mpsc, then the per-peer
task builds the snapshot. The snapshot read sees the just-written entry by
construction.

**Verify by:** sending a burst of POSTs to `/thought` for the same peer
under load, then inspecting `journal.jsonl` and the prompts emitted to the
ACP session (trace logs show snapshot contents). Each subsequent prompt's
`recent_journal` must contain every earlier signal in this batch.

## Interruption semantics on ACP

**Status:** implemented; manual verification only.

Per impl.md § Aliveness — Cognition contract, a new POST arriving for a
peer while their queue is running a routing turn must cancel the in-flight
router and re-prompt with both signals merged. The reactor's `dispatch_signal`
checks the peer's `in_flight` slot, calls `session.cancel()`, and the
per-peer task observes the cancel via `SessionRun::next_update` returning a
`Cancelled` variant, then re-prompts with the merged batch.

**Verify by:** issuing two POSTs in rapid succession with
`X-HI-From: alice@phone` and observing the second prompt's snapshot
includes both signals. The shell recipe is in `scripts/curl-recipes.sh`
under "interruption demo".

**Tests:** `tests/interruption.rs` is `#[ignore]`-d — exercising it would
require a mock ACP backend, which is a v1-grade refactor not undertaken
here.

## Single-binary deployment with claude-code dependency

**Status:** documented; sibling-container layout illustrative.

The Rust binary is self-contained. At runtime it needs `claude-code` on
`PATH`, or `CLAUDE_CODE_BIN` pointing at a launcher. In Docker, the
`docker-compose.yml` in the repo shows a two-container layout with hi-agent
and claude-code connected over a shared Unix socket volume, with the
`CLAUDE_CODE_BIN=socat` trick to redirect ACP stdio to the socket.

**Unverified:** the exact `claude-code` container image and entrypoint
command. The compose file uses placeholder values and is commented as
such. Pin the image and command after first end-to-end Docker test.

## Web embedding rebuild

**Status:** mechanism in place; verify on first round-trip.

`build.rs` declares `cargo:rerun-if-changed=src/appearance/web/dist`, so
modifying the SPA, running `pnpm build`, then `cargo build` should re-embed
the new `dist/` into the binary.

**Verify by:** running `cargo build --release`, noting the embedded asset
hash (or just open `GET /` and view-source), then editing
`src/appearance/web/src/App.tsx`, re-running `pnpm build`, re-running
`cargo build --release`, and confirming the served HTML changed without
needing `cargo clean`.

## v0 acceptance checklist

When the user runs first-build, walk this list. If anything fails, the
issue is implementation-side, not docs-side.

- [ ] `cargo check` passes
- [ ] `cargo build --release` produces `./target/release/hi-agent`
- [ ] `cargo test` passes (the `#[ignore]`-d tests stay ignored)
- [ ] `cargo run --example acp_spike` succeeds with parallel timing roughly
      equal to single-session timing (concurrency works)
- [ ] `cd src/appearance/web && pnpm install && pnpm build` succeeds
- [ ] `./target/release/hi-agent` starts; `curl http://127.0.0.1:8080/`
      returns the embedded SPA HTML (200, `text/html`)
- [ ] `curl -X POST -H 'X-HI-From: alice@phone' --data-binary 'hi'
      http://127.0.0.1:8080/thought` returns 202 and writes a `SignalIn`
      line to `data/journal.jsonl`
- [ ] A `GET /thought` long-poll opened beforehand receives the router's
      reply on the same peer
- [ ] `set_intent` (via a router decision, e.g. "remind me in 2 minutes")
      followed by waiting two minutes fires the intent — observable as an
      `IntentFired` line in `journal.jsonl` and an outbound signal on the
      long-poll
- [ ] `POST /vision` returns 501 with a body explaining "not implemented in v0"
- [ ] `POST /thought` without `X-HI-From` returns 400

## Items not in this register

Items deferred per impl.md § Scope (no risk to flag, they were never in
v0): cron / relative intents, forgetting curve / journal compaction, multi-peer
shared workspaces, authorization validation, handle discovery, federation,
end-to-end encryption beyond TLS, OS sleep/wake bridge for battery devices.
