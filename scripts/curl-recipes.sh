#!/usr/bin/env bash
# scripts/curl-recipes.sh — exercise every spec channel from the shell.
#
# Set BASE to the agent's HTTP origin (default: http://127.0.0.1:8080).
# Set ME to your peer id (default: alice@phone).
#
# Each recipe is a function. Run a single one as:
#     ./scripts/curl-recipes.sh recipe_name
# or source the file in an interactive shell and call the function directly.
# With no argument the script prints this help text.
#
# Recipes (in order):
#   1. open_thought             — open a /thought long-poll (blocks)
#   2. send_thought             — POST a thought to the agent
#   3. open_approval            — open a /approval long-poll (blocks; returns JSON)
#   4. approve                  — POST a decision on a pending approval
#   5. ask_for_reminder         — send a thought the router will likely turn into set_intent
#   6. stub_vision              — confirm /vision is 501 in v0
#   7. send_audio               — POST an audio file; STT transcribes and routes it
#   8. open_audio               — long-poll on /audio; saves the next TTS reply to a file
#   9. stub_touch_smell_taste   — confirm the other three are 501
#  10. interruption_demo        — two POSTs in rapid succession to the same peer
#  11. health_check             — fetch the homepage and confirm 200 + text/html
#
# Notes:
# - `curl -N` (`--no-buffer`) is required on GETs so the long-poll body
#   streams in as it arrives.
# - The /approval id comes from the JSON delivered by `open_approval`. The
#   `approve` recipe accepts the id as its first argument.

set -euo pipefail

BASE=${BASE:-http://127.0.0.1:8080}
ME=${ME:-alice@phone}

# ---------------------------------------------------------------------------
# 1. open_thought
# Long-poll on /thought, scoped to ME. Blocks until the agent emits to ME.
# Re-issue after each body close — that's the spec's end-of-utterance.
# ---------------------------------------------------------------------------
open_thought() {
  curl -N -H "X-HI-To: ${ME}" "${BASE}/thought"
}

# ---------------------------------------------------------------------------
# 2. send_thought
# POST one utterance to /thought. Body-close ends the utterance.
# Usage: send_thought "hello there"
# ---------------------------------------------------------------------------
send_thought() {
  local body="${1:-hello}"
  curl -i -X POST \
    -H "X-HI-From: ${ME}" \
    --data-binary "${body}" \
    "${BASE}/thought"
}

# ---------------------------------------------------------------------------
# 3. open_approval
# Long-poll on /approval. The agent broadcasts ApprovalEvent JSON to the
# subscriber whose X-HI-To matches the peer the requesting session is acting
# for. The response body is a one-shot JSON document — pull the `id` and
# pass it to `approve`.
# ---------------------------------------------------------------------------
open_approval() {
  curl -N -H "X-HI-To: ${ME}" "${BASE}/approval"
}

# ---------------------------------------------------------------------------
# 4. approve
# Send a decision to /approval. Usage: approve <approval-id> [true|false] [reason]
# ---------------------------------------------------------------------------
approve() {
  local id="${1:?approval id required}"
  local allow="${2:-true}"
  local reason="${3:-}"
  if [ -n "${reason}" ]; then
    local payload
    payload=$(printf '{"id":"%s","allow":%s,"reason":"%s"}' "${id}" "${allow}" "${reason}")
  else
    local payload
    payload=$(printf '{"id":"%s","allow":%s}' "${id}" "${allow}")
  fi
  curl -i -X POST \
    -H "X-HI-From: ${ME}" \
    -H 'Content-Type: application/json' \
    -d "${payload}" \
    "${BASE}/approval"
}

# ---------------------------------------------------------------------------
# 5. ask_for_reminder
# v0 does not expose a /intent channel for clients to write directly — only
# the router can create intents (via the set_intent MCP tool). The human-side
# form is therefore conversational: send a thought that the router will turn
# into a set_intent call. The heartbeat fires the intent when its time comes
# and injects a synthetic signal back through routing.
# ---------------------------------------------------------------------------
ask_for_reminder() {
  send_thought 'remind me at 21:00 to call mom'
}

# ---------------------------------------------------------------------------
# 6. stub_vision
# v0 returns 501 with a body explaining the omission.
# ---------------------------------------------------------------------------
stub_vision() {
  curl -i -X POST \
    -H "X-HI-From: ${ME}" \
    --data-binary 'a red square in the corner' \
    "${BASE}/vision"
}

# ---------------------------------------------------------------------------
# 7. send_audio
# POST an audio file to /audio. STT transcribes the bytes and the transcript
# is routed through the same per-peer queue that /thought uses.
# Usage:   send_audio path/to/clip.wav [mime]
# Default mime is audio/wav. Requires STT_PROVIDER configured server-side
# (otherwise the server returns 501).
# ---------------------------------------------------------------------------
send_audio() {
  local file="${1:?audio file required}"
  local mime="${2:-audio/wav}"
  curl -i -X POST \
    -H "X-HI-From: ${ME}" \
    -H "Content-Type: ${mime}" \
    --data-binary "@${file}" \
    "${BASE}/audio"
}

# ---------------------------------------------------------------------------
# 8. open_audio
# Long-poll on /audio. When the router decides to speak (calls
# `speak(channel="audio", ...)`), the synthesized bytes stream back here.
# Usage: open_audio [output-file]
# Default output file is reply.mp3 (Volcengine TTS default encoding).
# Requires TTS_PROVIDER configured server-side, otherwise this blocks
# indefinitely — the agent simply never has anywhere to speak from.
# ---------------------------------------------------------------------------
open_audio() {
  local out="${1:-reply.mp3}"
  curl -N -H "X-HI-To: ${ME}" -o "${out}" "${BASE}/audio"
  echo "saved to ${out}"
}

# ---------------------------------------------------------------------------
# 8. stub_touch_smell_taste
# Confirms the other three sensory channels are 501.
# ---------------------------------------------------------------------------
stub_touch_smell_taste() {
  for ch in touch smell taste; do
    echo "# POST /${ch}"
    curl -i -X POST \
      -H "X-HI-From: ${ME}" \
      --data-binary "placeholder for ${ch}" \
      "${BASE}/${ch}"
    echo
  done
}

# ---------------------------------------------------------------------------
# 9. interruption_demo
# Two POSTs to /thought in rapid succession with the same X-HI-From. The
# reactor's interruption policy (impl.md § Aliveness — Cognition contract)
# should cancel the in-flight routing session for ME and re-prompt with both
# signals merged. Inspect data/journal.jsonl + tracing logs to confirm.
# ---------------------------------------------------------------------------
interruption_demo() {
  send_thought 'first thought, please take your time replying' &
  sleep 0.2
  send_thought 'actually, ignore that — what time is it?'
  wait
}

# ---------------------------------------------------------------------------
# 10. health_check
# Confirms the homepage renders. Useful as a Docker healthcheck probe too.
# ---------------------------------------------------------------------------
health_check() {
  curl -i "${BASE}/"
}

# ---------------------------------------------------------------------------
# Dispatcher
# ---------------------------------------------------------------------------
main() {
  if [ "$#" -eq 0 ]; then
    sed -n '2,/^set -euo pipefail$/p' "$0" | sed 's/^# \{0,1\}//; s/^#$//'
    exit 0
  fi
  local recipe="$1"
  shift
  if ! declare -F "${recipe}" >/dev/null; then
    echo "unknown recipe: ${recipe}" >&2
    echo "run with no args to see the list" >&2
    exit 2
  fi
  "${recipe}" "$@"
}

# Only dispatch when executed, not when sourced.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
