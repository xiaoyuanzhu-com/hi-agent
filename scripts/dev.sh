#!/usr/bin/env bash
#
# Run the two dev servers together and tear the WHOLE tree down on Ctrl-C.
#
#   - Rust backend on :12358 via `cargo watch -x 'run -- --port 12358'`
#   - Vite dev server on :12359 (src/appearance/web)
#
# Why this isn't just `trap 'kill 0'`: `cargo watch` runs the backend
# (`cargo run` -> `hi-agent` -> `node` ACP adapter -> `claude`, plus esbuild /
# ffmpeg) in its OWN process group so it can restart that subtree on file
# changes. A `kill 0` only signals the recipe shell's group, so it never reaches
# those — they're left to cargo watch's own signal forwarding, which races on
# exit and "sometimes" orphans the backend. Worse, hi-agent drains in-flight
# HTTP for up to 10s on SIGTERM (the browser holds SSE + long-poll open), so the
# backend + its node/claude children keep :12358 bound long after the prompt
# returns.
#
# Instead we snapshot the full descendant tree of each server *by PID* (so a
# parent exiting and its children reparenting to pid 1 can't hide survivors),
# SIGTERM it, give well-behaved processes a brief grace to exit, then SIGKILL
# whatever is still up. Only ever touches the trees we started — safe to run
# alongside another hi-agent instance on the same box.
set -u

cd "$(dirname "$0")/.."

# Roots of the two dev servers, filled in as they're launched.
pids=""

# All PIDs in the process tree rooted at $1, deepest first. Walks parent->child
# links (`pgrep -P`), never matching on command text, so it can't catch an
# unrelated process or self-match.
collect() {
  local child
  for child in $(pgrep -P "$1" 2>/dev/null); do
    collect "$child"
  done
  printf '%s ' "$1"
}

cleanup() {
  trap - INT TERM EXIT   # disarm so this runs once

  local tree="" root
  for root in $pids; do
    tree="$tree $(collect "$root")"
  done
  # Nothing left to do if every server already exited.
  [ -n "${tree// /}" ] || exit 0

  kill -TERM $tree 2>/dev/null

  # Let clean exits happen, then force-kill stragglers (e.g. hi-agent mid-drain)
  # by their captured PIDs — robust even after cargo watch has exited and the
  # backend reparented away from it.
  local i alive
  for i in 1 2 3 4 5 6; do
    sleep 0.5
    alive=""
    for p in $tree; do
      kill -0 "$p" 2>/dev/null && alive="$alive $p"
    done
    [ -n "$alive" ] || break
  done
  [ -n "${alive:-}" ] && kill -KILL $alive 2>/dev/null

  exit 0
}
trap cleanup INT TERM EXIT

# --- macOS: make the native face window's camera/mic actually work in dev ---------
#
# On macOS the backend's default shape is the menu-bar app, whose face window is a
# WKWebView. For `getUserMedia` (camera/mic) to work there, macOS's privacy system
# (TCC) needs BOTH of these to be true — and a bare `cargo run` binary satisfies
# NEITHER, which is why the window's camera/mic silently hangs with no prompt:
#
#   1. A bundle IDENTITY. TCC only prompts for a client that is a real .app: an
#      Info.plist carrying NSCamera/NSMicrophoneUsageDescription plus a code
#      signature. A loose Mach-O has no Info.plist, so TCC can't even present the
#      prompt. → We wrap the binary in a minimal HiAgent.app (scripts/Info.plist)
#      and codesign it on every rebuild.
#
#   2. Being its own RESPONSIBLE PROCESS. TCC attributes a request to the launching
#      "responsible process". A process started from a shell inherits the TERMINAL
#      as its responsible process — so the prompt would be attributed to this
#      terminal (which has no usage strings) and the request hangs forever. A
#      LaunchServices launch (double-clicking a .app) is its own responsible
#      process; to get the same effect while still running under cargo-watch (and
#      keeping the shell's PATH so node/claude resolve), the binary re-execs itself
#      disclaiming responsibility when it sees HI_AGENT_DISCLAIM=1, set below.
#      See reexec_disclaiming_responsibility() in src/main.rs.
#
# So each rebuild: build → drop the fresh binary into the bundle → codesign → run
# the bundled exe with HI_AGENT_DISCLAIM=1. The signature uses a STABLE self-signed
# identity (ensure-dev-signing-id.sh) rather than ad-hoc, because an ad-hoc
# signature's code requirement is keyed to the binary's cdhash — which changes every
# rebuild — so TCC would re-prompt on every save; a stable cert keeps one code
# requirement, so you approve once and it sticks.
#
# Set HI_AGENT_DEV_NO_BUNDLE=1 to opt out and run the binary bare (no camera/mic in
# the native window). Non-macOS is unchanged — it runs `cargo run` directly.
if [ "$(uname -s)" = "Darwin" ] && [ -z "${HI_AGENT_DEV_NO_BUNDLE:-}" ]; then
  APP="target/dev-app/HiAgent.app"
  EXE="$APP/Contents/MacOS/hi-agent"
  mkdir -p "$APP/Contents/MacOS"
  cp scripts/Info.plist "$APP/Contents/Info.plist"
  SIGN_ID="$(./scripts/ensure-dev-signing-id.sh)"
  if [ "$SIGN_ID" = "-" ]; then
    echo ">> native dev window: signing bundle ad-hoc — camera/mic will re-prompt each rebuild"
  else
    echo ">> native dev window: signing with stable dev identity — camera/mic enabled (approve once)"
  fi
  # Per-rebuild chain (see the requirements above): clonefile the fresh binary into
  # the bundle (instant on APFS, plain-cp fallback), re-sign, then run it with
  # HI_AGENT_DISCLAIM=1 (requirement 2). Each `&&` short-circuits, so a compile error
  # just skips the run, exactly as `cargo run` would.
  RUN_CMD="cargo build \
    && { cp -c target/debug/hi-agent '$EXE' 2>/dev/null || cp target/debug/hi-agent '$EXE'; } \
    && codesign --force -s '$SIGN_ID' '$APP' \
    && HI_AGENT_DISCLAIM=1 exec '$EXE' --port 12358"
  cargo watch -w src -w build.rs -w Cargo.toml -w Cargo.lock \
    -i 'src/appearance/web/**' -s "$RUN_CMD" &
else
  cargo watch -w src -w build.rs -w Cargo.toml -w Cargo.lock \
    -i 'src/appearance/web/**' -x 'run -- --port 12358' &
fi
pids="$pids $!"

( cd src/appearance/web && exec npm run dev ) &
pids="$pids $!"

# Keep the binary's embedded web fresh in dev. The menu-bar popover's WKWebView loads
# the binary's own port (:12358), which serves `dist/` from disk — NOT the Vite dev
# server (:12359 is HTTPS with a self-signed cert the WKWebView won't trust). So rebuild
# `dist/` on web changes; the debug binary reads it per request, so the popover shows the
# latest on reopen. The browser still gets HMR from the :12359 dev server.
( cd src/appearance/web && exec npm run build -- --watch ) &
pids="$pids $!"

wait
