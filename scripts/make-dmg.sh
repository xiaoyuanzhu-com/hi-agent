#!/usr/bin/env bash
#
# Build a hermetic HiAgent.app and wrap it in a .dmg.
#
# Produces an app whose managed runtime (Node + ACP adapter + claude), recognition
# models, and static ffmpeg all live under Contents/Resources — so it launches and
# runs fully offline, with nothing to install. Apple Silicon macOS only.
#
# Signing:
#   CODESIGN_IDENTITY  Developer ID Application identity, or "-" for ad-hoc
#                      (default). Ad-hoc is fine for local testing; a distributable
#                      (notarizable) build needs a real Developer ID.
#   NOTARY_PROFILE     A `notarytool` keychain profile name. When set together with
#                      a real CODESIGN_IDENTITY, the .dmg is notarized + stapled.
#   SKIP_BUILD=1       Reuse an existing ./target/release/hi-agent (skip `make build`).
#   REUSE_RESOURCES=DIR  Copy an already-provisioned Resources tree (with
#                      runtime/.complete) instead of downloading again.
#
# Output: target/dmg/HiAgent.app and target/dmg/hi-agent-<version>-arm64.dmg
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# --- host guard -------------------------------------------------------------
if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "error: make dmg targets Apple Silicon macOS only (got $(uname -s)/$(uname -m))." >&2
  echo "       run it on the Mac mini." >&2
  exit 1
fi

IDENTITY="${CODESIGN_IDENTITY:--}"          # default ad-hoc
ENT="$ROOT/scripts/hi-agent.entitlements"
VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"

OUT="$ROOT/target/dmg"
APP="$OUT/HiAgent.app"
RES="$APP/Contents/Resources"
MACOS="$APP/Contents/MacOS"
DMG="$OUT/hi-agent-$VERSION-arm64.dmg"

# --- 1. build the binary + embedded SPA ------------------------------------
if [ "${SKIP_BUILD:-}" != "1" ]; then
  echo ">> building release binary…"
  make build
fi
BIN="$ROOT/target/release/hi-agent"
[ -x "$BIN" ] || { echo "error: $BIN not found; run without SKIP_BUILD" >&2; exit 1; }

# --- 2. assemble the .app skeleton -----------------------------------------
echo ">> assembling $APP …"
rm -rf "$APP"
mkdir -p "$MACOS" "$RES"
cp "$BIN" "$MACOS/hi-agent"

# Static bundle metadata. The version it carries is committed and bumped via
# `make bump-version` — packaging does nothing version-related.
cp "$ROOT/scripts/Info.plist" "$APP/Contents/Info.plist"

# --- 3. provision the bundled dependencies ---------------------------------
# Run the just-built binary to download + lay out runtime/models/ffmpeg into
# Resources. Forces the managed install even though this host has system tools.
# Set REUSE_RESOURCES=/path/to/staged/Resources to copy a previously provisioned
# tree instead of downloading again (handy for retries / offline packaging).
if [ -n "${REUSE_RESOURCES:-}" ]; then
  echo ">> reusing provisioned Resources from $REUSE_RESOURCES …"
  [ -f "$REUSE_RESOURCES/runtime/.complete" ] || { echo "error: $REUSE_RESOURCES is not a complete bundle (no runtime/.complete)" >&2; exit 1; }
  rm -rf "$RES"
  cp -R "$REUSE_RESOURCES" "$RES"
else
  echo ">> provisioning runtime + models + ffmpeg into Resources (large download)…"
  "$BIN" --provision-into "$RES"
fi

# App icon: drop in the .icns generated from the hi logo. Placed after
# provisioning so the REUSE_RESOURCES path (which replaces Resources) can't
# clobber it, and before codesign so it gets sealed into the bundle.
cp "$ROOT/scripts/HiAgent.icns" "$RES/AppIcon.icns"

# --- 4. codesign, inside-out -----------------------------------------------
sign_one() {
  if [ "$IDENTITY" = "-" ]; then
    codesign --force -s - "$1"
  else
    codesign --force --options runtime --timestamp \
      --entitlements "$ENT" -s "$IDENTITY" "$1"
  fi
}

echo ">> signing nested Mach-O binaries (identity: $IDENTITY)…"
# Every Mach-O under the bundle (node, claude, ffmpeg, esbuild, *.node addons),
# excluding the main executable — the bundle sign below seals that one. -type f
# skips symlinks (npm's .bin/*), whose real targets are signed directly.
while IFS= read -r -d '' f; do
  [ "$f" = "$MACOS/hi-agent" ] && continue
  if file -b "$f" | grep -q '^Mach-O'; then
    sign_one "$f"
  fi
done < <(find "$APP" -type f -print0)

echo ">> signing the app bundle…"
sign_one "$APP"
codesign --verify --deep --strict --verbose=2 "$APP" || {
  echo "warn: codesign verification reported issues (expected for ad-hoc)." >&2
}

# --- 5. notarize (real identity only) --------------------------------------
notarize=false
if [ "$IDENTITY" != "-" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
  notarize=true
fi

# --- 6. build the .dmg ------------------------------------------------------
echo ">> building $DMG …"
DMGROOT="$OUT/dmgroot"
rm -rf "$DMGROOT" "$DMG"
mkdir -p "$DMGROOT"
cp -R "$APP" "$DMGROOT/"
ln -s /Applications "$DMGROOT/Applications"
hdiutil create -volname "Hi Agent" -srcfolder "$DMGROOT" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$DMGROOT"

if $notarize; then
  echo ">> notarizing $DMG (profile: $NOTARY_PROFILE)…"
  xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
  xcrun stapler staple "$APP"
else
  echo ">> skipping notarization (set CODESIGN_IDENTITY + NOTARY_PROFILE to enable)."
fi

echo ""
echo "done:"
echo "  app: $APP"
echo "  dmg: $DMG"
du -sh "$APP" "$DMG" 2>/dev/null || true
