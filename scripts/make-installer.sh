#!/usr/bin/env bash
#
# Build the Windows installer: a per-user NSIS Setup.exe wrapping the
# cross-compiled hi-agent.exe. The Windows analog of scripts/make-dmg.sh.
#
# This is the "it runs" tier: the installer carries only the binary (+ icon).
# On first launch hi-agent.exe auto-provisions its managed runtime (Node +
# claude + esbuild + ffmpeg + recognition models) into the OS cache. A fully
# hermetic, offline-from-first-launch installer (the .dmg's bundling parity) is
# a later increment that needs a real Windows host to stage + verify.
#
# Runs on any host that can both cross-compile the exe (`make exe` — needs the
# cargo-xwin toolchain, set up on the Mac mini) and run `makensis`. No Windows
# box required.
#
#   SKIP_BUILD=1   reuse an existing target/<win>/release/hi-agent.exe
#
# Output: target/installer/hi-agent-<version>-x64-setup.exe
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# --- prerequisites ----------------------------------------------------------
if ! command -v makensis >/dev/null 2>&1; then
  echo "error: makensis (NSIS) not found on PATH." >&2
  echo "       macOS: brew install makensis   |   Debian/Ubuntu: apt-get install nsis" >&2
  exit 1
fi

WIN_TARGET="x86_64-pc-windows-msvc"
WIN_EXE="$ROOT/target/$WIN_TARGET/release/hi-agent.exe"
ICON="$ROOT/scripts/HiAgent.ico"
[ -f "$ICON" ] || { echo "error: $ICON not found" >&2; exit 1; }

VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"
VERSION4="${VERSION}.0"   # NSIS VIProductVersion wants a four-part numeric

OUT="$ROOT/target/installer"
SETUP="$OUT/hi-agent-$VERSION-x64-setup.exe"
mkdir -p "$OUT"

# --- 1. cross-compile the Windows binary (+ embedded SPA) -------------------
if [ "${SKIP_BUILD:-}" != "1" ]; then
  echo ">> cross-compiling hi-agent.exe (make exe)…"
  make exe
fi
[ -f "$WIN_EXE" ] || { echo "error: $WIN_EXE not found; run without SKIP_BUILD" >&2; exit 1; }

# --- 2. compile the installer ----------------------------------------------
echo ">> building installer with makensis…"
# makensis on macOS aborts (std::bad_alloc) under a C/POSIX locale — the case
# in a bare SSH/CI shell. Force a UTF-8 locale just for the build so it works
# regardless of the caller's environment.
export LC_ALL="en_US.UTF-8"
makensis -V2 \
  "-DVERSION=$VERSION" \
  "-DVERSION4=$VERSION4" \
  "-DSRCEXE=$WIN_EXE" \
  "-DICON=$ICON" \
  "-DOUTFILE=$SETUP" \
  "$ROOT/scripts/hi-agent.nsi"

echo ""
echo "done:"
echo "  installer: $SETUP"
du -h "$SETUP" 2>/dev/null || ls -l "$SETUP"
