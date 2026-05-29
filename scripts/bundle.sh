#!/usr/bin/env bash
# scripts/bundle.sh — produce runtime/embed/<target>.tar.zst for one target.
#
# Usage: scripts/bundle.sh <rust-target-triple>
# Requires: node+npm (host, for `npm ci`), curl, shasum, tar, zstd, jq.
set -euo pipefail

TARGET="${1:?usage: bundle.sh <rust-target-triple>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/runtime/manifest.toml"
STAGE="$ROOT/runtime/staging/$TARGET"
OUT="$ROOT/runtime/embed/$TARGET.tar.zst"

read_key() { grep -E "^$1 *=" "$MANIFEST" | head -1 | sed 's/.*= *"\{0,1\}//; s/"\{0,1\} *$//'; }
read_target_key() {
  awk -v t="[targets.$TARGET]" -v k="$1" '
    $0==t {inblk=1; next}
    /^\[/ {inblk=0}
    inblk && $0 ~ "^"k" *=" { sub("^"k" *= *\"",""); sub("\" *$",""); print; exit }
  ' "$MANIFEST"
}

NODE_URL="$(read_target_key node_url)"
NODE_SHA="$(read_target_key node_sha256)"
[ -n "$NODE_URL" ] || { echo "no node_url for target $TARGET in manifest"; exit 1; }

rm -rf "$STAGE"; mkdir -p "$STAGE/node" "$STAGE/adapter"

# 1. Node — download, verify checksum, unpack into $STAGE/node (strip top dir).
TMP="$(mktemp -d)"; ARCHIVE="$TMP/node.archive"
curl -fsSL "$NODE_URL" -o "$ARCHIVE"
echo "$NODE_SHA  $ARCHIVE" | shasum -a 256 -c -
case "$NODE_URL" in
  *.zip) (cd "$STAGE/node" && unzip -q "$ARCHIVE" && mv */* . 2>/dev/null || true) ;;
  *)     tar -xzf "$ARCHIVE" -C "$STAGE/node" --strip-components=1 ;;
esac

# 2. Adapter + claude — npm ci against the committed lockfile.
cp "$ROOT/runtime/package.json" "$ROOT/runtime/package-lock.json" "$STAGE/adapter/"
(cd "$STAGE/adapter" && npm ci --omit=dev)

# 3. Resolve relative paths for runtime.json.
NODE_BIN_REL="node/bin/node"; [ -f "$STAGE/node/node.exe" ] && NODE_BIN_REL="node/node.exe"
ADAPTER_REL="adapter/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js"
CLAUDE_REL="$(cd "$STAGE/adapter" && node -e "process.stdout.write(require('path').relative('$STAGE', require.resolve('@anthropic-ai/claude-agent-sdk/cli.js')))" 2>/dev/null || true)"
[ -n "$CLAUDE_REL" ] || CLAUDE_REL="adapter/node_modules/@anthropic-ai/claude-agent-sdk/cli.js"

cat > "$STAGE/runtime.json" <<JSON
{ "node": "$NODE_BIN_REL", "adapter": "$ADAPTER_REL", "claude": "$CLAUDE_REL" }
JSON

# 4. Record resolved claude version back into the manifest (best effort).
CLAUDE_VER="$(cd "$STAGE/adapter" && node -e "process.stdout.write(require('@anthropic-ai/claude-agent-sdk/package.json').version)" 2>/dev/null || echo unknown)"
echo "resolved claude/agent-sdk version: $CLAUDE_VER (update manifest claude_version)"

# 5. Pack.
mkdir -p "$ROOT/runtime/embed"
tar -C "$STAGE" -cf - . | zstd -19 -o "$OUT" -f
echo "wrote $OUT"
