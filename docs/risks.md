# Risks

Open and known-unverified items for the hi-agent v0 implementation. This is
the Step 0 spike output, deferred until the user batch-builds and runs.

The implementation is complete; the risks below are about things the code
*assumes* but no one has measured yet, plus the v0 acceptance checklist for
first build.

## Concurrent ACP sessions

**Status:** retired by the per-session process model (2026-06-07).

The original architecture multiplexed N concurrent ACP sessions over one
`claude-code` subprocess (shared stdio), and it was unverified whether
`claude-code` ran concurrent prompts in parallel or serialized them. That
question is moot now: each session runs in **its own subprocess**
(`architecture.md` §6), so cross-session concurrency comes from the OS, not the
adapter. The per-scene reactor loop still serializes one turn per scene — that is
a reactor policy, not a subprocess limit. The cost this introduces — a subprocess
spawn per session — is tracked under "MCP attachment" below.

## MCP attachment per ephemeral session

**Status:** built (2026-06-05).

Each session needs its tool surface. Rather than the stdio shim sketched
originally, hi-agent attaches its own HTTP MCP endpoint (`/mcp`, served by the
running axum app) through the ACP `mcp_servers` capability — `McpServer::Http`
with scene/role/worker-id carried as `X-HI-Scene` / `X-HI-Role` /
`X-HI-Worker-Id` headers, so one endpoint routes every session's calls. No
subprocess, no socket. The reactor's whole expression + side-effect contract
rides these tools: `say` / `show_view` (output), `delegate` / `alarm` (reactor
side-effects), `ask` (worker).

The deployed adapter (`@agentclientprotocol/claude-agent-acp` 0.36.1) advertises
`mcpCapabilities { http: true, sse: true }` and forwards http servers with
headers to the SDK, so HTTP works; the stdio shim is the fallback only if a
future adapter drops http support.

**Still to measure (now the live cost):** with one subprocess per session, each
session pays a full process spawn + ACP `initialize` + MCP `tools/list`
round-trip — every delegated worker and every heartbeat hot-swap, not just the
reactor session. Not yet load-tested; this is the main thing to watch when many
sessions are live at once.

## Journal-as-context coherence

**Status:** designed correct; not load-tested.

Routers depend on memory snapshots being faithful. The reactor writes to
`journal.jsonl` **before** spawning the routing session — `server::thought::post_thought`
journals the inbound, then sends to the reactor's mpsc, then the per-scene
task builds the snapshot. The snapshot read sees the just-written entry by
construction.

**Verify by:** sending a burst of POSTs to `/thought` for the same scene
under load, then inspecting `journal.jsonl` and the prompts emitted to the
ACP session (trace logs show snapshot contents). Each subsequent prompt's
`recent_journal` must contain every earlier signal in this batch.

## Interruption semantics on ACP

**Status:** implemented; manual verification only.

Per impl.md § Aliveness — Cognition contract, a new POST arriving for a
scene while its queue is running a routing turn must cancel the in-flight
router and re-prompt with both signals merged. The reactor's `dispatch_signal`
checks the scene's `in_flight` slot, calls `session.cancel()`, and the
per-scene task observes the cancel via `SessionRun::next_update` returning a
`Cancelled` variant, then re-prompts with the merged batch.

**Verify by:** issuing two POSTs in rapid succession with
`X-HI-Scene: alice@phone` and observing the second prompt's snapshot
includes both signals.

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
- [ ] concurrent thoughts from three distinct scenes route with parallel
      timing roughly equal to single-session timing (concurrency works —
      see "Concurrent ACP sessions" above)
- [ ] `cd src/appearance/web && pnpm install && pnpm build` succeeds
- [ ] `./target/release/hi-agent` starts; `curl http://127.0.0.1:8080/`
      returns the embedded SPA HTML (200, `text/html`)
- [ ] `curl -X POST -H 'X-HI-Scene: alice@phone' --data-binary 'hi'
      http://127.0.0.1:8080/thought` returns 202 and writes a `SignalIn`
      line to `data/journal.jsonl`
- [ ] A `GET /thought` long-poll opened beforehand receives the router's
      reply on the same scene
- [ ] `set_intent` (via a router decision, e.g. "remind me in 2 minutes")
      followed by waiting two minutes fires the intent — observable as an
      `IntentFired` line in `journal.jsonl` and an outbound signal on the
      long-poll
- [ ] `POST /vision` returns 501 with a body explaining "not implemented in v0"
- [ ] `GET /thought` without `X-HI-Scene` returns 400

## Items not in this register

Items deferred per impl.md § Scope (no risk to flag, they were never in
v0): cron / relative intents, forgetting curve / journal compaction, multi-scene
shared workspaces, authorization validation, handle discovery, federation,
end-to-end encryption beyond TLS, OS sleep/wake bridge for battery devices.
