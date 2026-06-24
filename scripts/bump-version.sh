#!/usr/bin/env bash
#
# Bump the project version everywhere it is committed, in one shot.
#
# The version is a committed value — there is no build-time generation. `make dmg`
# just copies the static scripts/Info.plist, so packaging does nothing
# version-related. This script is edit-only: it does NOT commit or tag.
#
#   scripts/bump-version.sh 0.2.0
#   make bump-version VERSION=0.2.0
#
# Pure awk — no cargo/npm/plutil needed, runs on Linux or macOS.
set -euo pipefail

NEW="${1:?usage: bump-version.sh X.Y.Z}"
[[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || {
  echo "error: version must look like X.Y.Z (got '$NEW')" >&2; exit 1; }

cd "$(dirname "$0")/.."

# Rewrite $1 in place by running awk program $2 over it. awk to a temp file is
# portable (sed -i differs GNU vs BSD); cat-back preserves perms/inode.
edit() {
  local f="$1" prog="$2" tmp
  tmp="$(mktemp)"
  awk -v new="$NEW" "$prog" "$f" > "$tmp"
  cat "$tmp" > "$f"
  rm -f "$tmp"
}

# Cargo.toml — the [package] version is the first `version = "…"` line.
edit Cargo.toml '
  !done && /^version *=/ { sub(/"[^"]*"/, "\"" new "\""); done=1 }
  { print }'

# Cargo.lock — the version line inside the `name = "hi-agent"` package block.
edit Cargo.lock '
  prev ~ /^name = "hi-agent"$/ && /^version *=/ { sub(/"[^"]*"/, "\"" new "\"") }
  { prev=$0; print }'

# scripts/Info.plist — both CFBundle*Version keys carry the value on their line.
edit scripts/Info.plist '
  /<key>CFBundleShortVersionString<\/key>/ || /<key>CFBundleVersion<\/key>/ {
    sub(/<string>[^<]*<\/string>/, "<string>" new "</string>") }
  { print }'

# web/package.json — the only `"version": "…"` is the package version.
edit src/appearance/web/package.json '
  !done && /"version":/ { sub(/"version": *"[^"]*"/, "\"version\": \"" new "\""); done=1 }
  { print }'

# web/package-lock.json — the first two `"version"` fields are the lockfile’s own
# declared version and the root package version; later ones belong to deps.
edit src/appearance/web/package-lock.json '
  c<2 && /"version":/ { sub(/"version": *"[^"]*"/, "\"version\": \"" new "\""); c++ }
  { print }'

echo "bumped version to $NEW in:"
echo "  Cargo.toml, Cargo.lock, scripts/Info.plist,"
echo "  src/appearance/web/package.json, src/appearance/web/package-lock.json"
echo "review with: git diff"
