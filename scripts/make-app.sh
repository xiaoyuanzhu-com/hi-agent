#!/usr/bin/env bash
#
# Assemble a minimal, ad-hoc-signed "Hi Agent.app" around the dev binary — enough of
# a bundle to give macOS TCC an identity, so features that need one (the mic/camera
# permission prompts, driven by the Info.plist usage strings) actually work.
#
# Unlike `make dmg`, this does NOT provision the hermetic runtime. It deliberately
# ships no Contents/Resources, so `bundle::resources_dir()` stays None and the app
# uses the dev box's system runtime (node + claude on PATH) and the cwd-relative
# ./data dir — i.e. it behaves exactly like the bare `target/release/hi-agent`, just
# wrapped so the camera/mic prompts can fire. For a distributable build, use `make dmg`.
#
#   SKIP_BUILD=1   Reuse an existing ./target/release/hi-agent (skip `make build`).
#
# Output: "target/app/Hi Agent.app"
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: make app targets macOS only (got $(uname -s))." >&2
  exit 1
fi

BIN="$ROOT/target/release/hi-agent"
if [ "${SKIP_BUILD:-}" != "1" ]; then
  echo ">> building release binary…"
  make build
fi
[ -x "$BIN" ] || { echo "error: $BIN not found; run without SKIP_BUILD" >&2; exit 1; }

APP="$ROOT/target/app/Hi Agent.app"
echo ">> assembling $APP …"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/hi-agent"
cp "$ROOT/scripts/Info.plist" "$APP/Contents/Info.plist"

# Ad-hoc signature — gives the bundle a stable-enough code identity for TCC to
# attribute (and remember) the camera/mic grants on this machine. No entitlements:
# those matter only under the hardened runtime that `make dmg`'s real signing turns
# on; an ad-hoc, non-hardened build allows JIT (Node/V8) and dylib loads by default.
echo ">> ad-hoc signing…"
codesign --force -s - "$APP"

echo ">> done: $APP"
echo "   run it (from the repo root, so ./data + system node/claude resolve):"
echo "     \"$APP/Contents/MacOS/hi-agent\" --port 12358 > server.log 2>&1 &"
echo "   if the camera/mic prompt is misattributed to your terminal, launch via LaunchServices instead:"
echo "     open \"$APP\" --args --port 12358 --data-dir \"$ROOT/data\""
