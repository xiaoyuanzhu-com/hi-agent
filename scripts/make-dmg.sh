#!/usr/bin/env bash
#
# Build a hermetic Hi Agent.app and wrap it in a styled drag-to-Applications .dmg.
#
# Produces an app whose managed runtime (Node + ACP adapter + claude), recognition
# models, and static ffmpeg all live under Contents/Resources — so it launches and
# runs fully offline, with nothing to install. Apple Silicon macOS only.
#
# Signing (read from the shell env, else from .env, else defaults):
#   CODESIGN_IDENTITY  Developer ID Application identity, or "-" for ad-hoc
#                      (default). Ad-hoc is fine for local testing; a distributable
#                      (notarizable) build needs a real Developer ID.
#   NOTARY_PROFILE     A `notarytool` keychain profile name. When set together with
#                      a real CODESIGN_IDENTITY, the .dmg is notarized + stapled.
#
#   These two live in .env (gitignored) on the build host so the identity name and
#   notary profile stay out of committed scripts — the certificate private key and
#   notary credentials themselves stay in the macOS keychain, never on disk. With
#   both set, `make dmg` produces a properly Apple-signed + notarized image ready
#   to hand to other users; with neither, it falls back to an ad-hoc local build.
#
#   SKIP_BUILD=1       Reuse an existing ./target/release/hi-agent (skip `make build`).
#   REUSE_RESOURCES=DIR  Copy an already-provisioned Resources tree (with
#                      runtime/.complete) instead of downloading again.
#
# Output: "target/dmg/Hi Agent.app" and target/dmg/hi-agent-<version>-macos.dmg
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# --- host guard -------------------------------------------------------------
if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "error: make dmg targets Apple Silicon macOS only (got $(uname -s)/$(uname -m))." >&2
  echo "       run it on the Mac mini." >&2
  exit 1
fi

# Pull the signing selectors from .env when not already set in the shell env, so
# `make dmg` signs + notarizes by default on a host whose .env carries them. An
# explicit env var still wins. We read only these two keys (not `source .env`) so
# unrelated secrets in .env aren't pulled in or shell-evaluated.
ENV_FILE="$ROOT/.env"
env_get() {  # env_get KEY -> value from .env with one layer of quotes stripped
  [ -f "$ENV_FILE" ] || return 0
  local line
  line="$(grep -E "^[[:space:]]*$1=" "$ENV_FILE" | tail -n1)" || return 0
  [ -n "$line" ] || return 0
  line="${line#*=}"
  case "$line" in
    \"*\") line="${line#\"}"; line="${line%\"}" ;;
    \'*\') line="${line#\'}"; line="${line%\'}" ;;
  esac
  printf '%s' "$line"
}

IDENTITY="${CODESIGN_IDENTITY:-$(env_get CODESIGN_IDENTITY)}"
IDENTITY="${IDENTITY:--}"                    # default ad-hoc
NOTARY_PROFILE="${NOTARY_PROFILE:-$(env_get NOTARY_PROFILE)}"
ENT="$ROOT/scripts/hi-agent.entitlements"
VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"

OUT="$ROOT/target/dmg"
APP="$OUT/Hi Agent.app"
RES="$APP/Contents/Resources"
MACOS="$APP/Contents/MacOS"
DMG="$OUT/hi-agent-$VERSION-macos.dmg"

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
APPNAME="$(basename "$APP")"          # "Hi Agent.app" — Finder shows it as "Hi Agent"
BG_DIR="$ROOT/scripts/dmg"

# Plain image: the .app + an Applications symlink, no window styling. Used
# everywhere the styled path can't run (headless/SSH) so a build never fails.
build_plain_dmg() {
  echo ">> building plain $DMG …"
  local DMGROOT="$OUT/dmgroot"
  rm -rf "$DMGROOT" "$DMG"; mkdir -p "$DMGROOT"
  cp -R "$APP" "$DMGROOT/"
  ln -s /Applications "$DMGROOT/Applications"
  hdiutil create -volname "Hi Agent" -srcfolder "$DMGROOT" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$DMGROOT"
}

# Styled image: brand background (scripts/dmg/background*.png) with the app and
# Applications icons pinned either side of a drag arrow. Driving Finder needs a
# real desktop session (window server + Automation permission), so this is
# skipped over SSH and falls back to the plain image. Window math: the art is
# 660x400 (the content area); TITLEBAR pads the outer window bounds so the
# content matches the art exactly. Icon centers are in content coordinates and
# line up with the empty slots baked into the background.
build_styled_dmg() {
  local VOL="Hi Agent" W=660 H=400 TITLEBAR=23 ICON=112
  local STAGE="$OUT/dmgroot" RW="$OUT/rw.dmg" MNT="/Volumes/$VOL"
  fail() { echo "styled: $1 failed" >&2; }   # name the step so a fallback isn't a mystery
  rm -rf "$STAGE" "$RW" "$DMG"; mkdir -p "$STAGE/.background"
  cp -R "$APP" "$STAGE/" || { fail "cp app"; return 1; }
  ln -s /Applications "$STAGE/Applications" || { fail "ln Applications"; return 1; }
  # Multi-resolution TIFF so Finder stays crisp on Retina and non-Retina.
  tiffutil -cathidpicheck "$BG_DIR/background.png" "$BG_DIR/background@2x.png" \
    -out "$STAGE/.background/background.tiff" >/dev/null 2>&1 \
    || cp "$BG_DIR/background@2x.png" "$STAGE/.background/background.tiff" \
    || { fail "background tiff"; return 1; }

  hdiutil create -volname "$VOL" -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov "$RW" >/dev/null \
    || { fail "hdiutil create"; return 1; }
  # Detach any stale mount of the same volume from a prior interrupted run,
  # else attach lands on a different mountpoint ("Hi Agent 1") and the osascript,
  # which addresses the disk by volume name, styles the wrong window.
  hdiutil detach "$MNT" -force >/dev/null 2>&1 || true
  hdiutil attach "$RW" -mountpoint "$MNT" -nobrowse -noverify -noautoopen >/dev/null \
    || { fail "hdiutil attach"; return 1; }

  osascript - "$VOL" "$APPNAME" "$W" "$H" "$TITLEBAR" "$ICON" <<'APPLESCRIPT' || { fail "osascript (Finder Automation permission?)"; hdiutil detach "$MNT" -force >/dev/null 2>&1 || true; return 1; }
on run argv
  set {volName, appName} to {item 1 of argv, item 2 of argv}
  set {w, h, tb, iconSize} to {item 3 of argv as integer, item 4 of argv as integer, item 5 of argv as integer, item 6 of argv as integer}
  tell application "Finder"
    tell disk volName
      open
      set theWindow to container window
      set current view of theWindow to icon view
      set toolbar visible of theWindow to false
      set statusbar visible of theWindow to false
      set the bounds of theWindow to {300, 140, 300 + w, 140 + h + tb}
      set opts to the icon view options of theWindow
      set arrangement of opts to not arranged
      set icon size of opts to iconSize
      set text size of opts to 12
      set background picture of opts to file ".background:background.tiff"
      set position of item appName of theWindow to {175, 205}
      set position of item "Applications" of theWindow to {485, 205}
      close
      open
      update without registering applications
      delay 1
    end tell
  end tell
end run
APPLESCRIPT

  sync
  # Detaching is what actually frees the RW image. If it stays attached, the
  # convert below hits EAGAIN forever no matter how many times we retry — so
  # escalate to a forced detach and confirm the volume is gone before moving on.
  local tries=0
  until hdiutil detach "$MNT" >/dev/null 2>&1; do
    tries=$((tries + 1))
    [ "$tries" -ge 5 ] && { hdiutil detach "$MNT" -force >/dev/null 2>&1 || true; break; }
    sleep 1
  done
  tries=0
  while [ -d "$MNT" ] && [ "$tries" -lt 5 ]; do   # still mounted → keep forcing
    hdiutil detach "$MNT" -force >/dev/null 2>&1 || true
    tries=$((tries + 1)); sleep 1
  done
  [ -d "$MNT" ] && { fail "hdiutil detach (volume still mounted)"; return 1; }

  # Even after detach returns the kernel can briefly hold the backing store, so
  # a convert issued immediately still fails with EAGAIN ("Resource temporarily
  # unavailable"). Retry with backoff; on the last attempt let the error through
  # so a genuine failure is diagnosable rather than a silent fallback.
  tries=0
  until hdiutil convert "$RW" -format UDZO -imagekey zlib-level=9 -ov -o "$DMG" >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 6 ]; then
      hdiutil convert "$RW" -format UDZO -imagekey zlib-level=9 -ov -o "$DMG" >/dev/null || true
      fail "hdiutil convert"; return 1
    fi
    sleep 2
  done
  rm -f "$RW"; rm -rf "$STAGE"
}

styled=false
if [ -n "${SSH_CONNECTION:-}" ]; then
  echo ">> styled DMG skipped (headless / SSH — no window server); building plain image."
elif [ ! -f "$BG_DIR/background.png" ] || [ ! -f "$BG_DIR/background@2x.png" ]; then
  echo ">> styled DMG skipped (missing scripts/dmg/background*.png); building plain image."
elif build_styled_dmg; then
  styled=true
  echo ">> styled DMG built (brand background + drag-to-Applications layout)."
else
  echo "warn: styled DMG build failed; falling back to plain image." >&2
fi
$styled || build_plain_dmg

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
