#!/usr/bin/env bash
#
# Ensure a STABLE self-signed code-signing identity exists, and print its SHA-1
# hash on stdout (or "-" for ad-hoc as a graceful fallback).
#
# Why a stable identity: `make dev` signs the native dev .app on every cargo-watch
# rebuild so its WKWebView can get camera/mic (TCC needs a bundle identity). An
# *ad-hoc* signature is keyed to the binary's cdhash, which changes every rebuild,
# so macOS would re-prompt for camera/mic on every save. A stable cert keeps the
# same code requirement across rebuilds, so you approve once and it stays silent.
#
# Why the hash, not the name: a self-signed cert is untrusted, so it's hidden from
# `find-identity -v` and `codesign -s <name>` is ambiguous if one ever gets
# duplicated. The 40-char SHA-1 is unique and always resolvable.
#
# Everything here is best-effort: any failure prints "-" (ad-hoc), which still
# works — it just re-prompts — so it can never break `make dev`.
set -uo pipefail

NAME="Hi Agent Dev Signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
# Use the system LibreSSL explicitly: its default PKCS#12 output (3DES) is what
# `security import` accepts. A Homebrew OpenSSL 3 on PATH would emit AES-256 p12s
# that older `security` can't read.
OPENSSL=/usr/bin/openssl

# The SHA-1 of the first code-signing identity named $NAME, if any. Plain
# `find-identity` (no -v) lists untrusted self-signed certs too.
existing_hash() {
  security find-identity 2>/dev/null | grep -F "$NAME" | grep -oE '[0-9A-F]{40}' | head -1
}

hash="$(existing_hash)"
if [ -n "$hash" ]; then echo "$hash"; exit 0; fi

tmp="$(mktemp -d)" || { echo "-"; exit 0; }
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/cfg" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

# Self-signed cert + private key, then bundle into a PKCS#12 for import.
"$OPENSSL" req -x509 -newkey rsa:2048 -sha256 -nodes -days 3650 \
  -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/cfg" >/dev/null 2>&1 \
  || { echo "-"; exit 0; }
"$OPENSSL" pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
  -out "$tmp/id.p12" -passout pass:hiagent -name "$NAME" >/dev/null 2>&1 \
  || { echo "-"; exit 0; }

# Import into the login keychain; -T grants codesign access to the private key so
# signing isn't gated by a keychain ACL prompt on every run.
security import "$tmp/id.p12" -k "$KEYCHAIN" -P hiagent -T /usr/bin/codesign \
  >/dev/null 2>&1 || { echo "-"; exit 0; }

# Widen the key's partition list so codesign can use it non-interactively. This
# needs the login-keychain password (which we don't have here), so it's expected
# to fail silently — the first codesign then shows a one-time "Always Allow"
# dialog you click once. Left in for the case where it's already unlocked/blank.
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "" \
  "$KEYCHAIN" >/dev/null 2>&1 || true

hash="$(existing_hash)"
[ -n "$hash" ] && echo "$hash" || echo "-"
