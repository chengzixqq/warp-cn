#!/usr/bin/env bash
# One-shot helper to mint a passwordless minisign keypair for the GitHub-only
# auto-update channel. Run once per fork; after that the public key lives in
# `script/warp-update.pub` (committed) and the secret key contents go into the
# `MINISIGN_SECRET_KEY` GitHub Actions secret.
#
# Why no password: the secret key never leaves GitHub Actions; gating it with
# a password would force us to also store the password as a secret, which is
# zero added security but doubles the surface that can be misconfigured.
#
# Why rsign2 (not minisign): pure Rust, installs via `cargo install`, no brew
# dependency on CI; signature format is byte-identical to minisign.

set -euo pipefail

WORKSPACE_ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PUB_PATH="$WORKSPACE_ROOT_DIR/script/warp-update.pub"
SEC_PATH="$(mktemp -d)/warp-update.key"

if ! command -v rsign >/dev/null 2>&1; then
  echo "rsign2 not installed. Run:" >&2
  echo "  cargo install rsign2" >&2
  exit 1
fi

# A real minisign public key is two lines: an "untrusted comment:" header
# plus a base64 body of >= 40 chars. The placeholder we ship has only the
# header, so we skip the comment line before checking for a key body.
if [ -s "$PUB_PATH" ] \
  && grep -v '^untrusted comment:' "$PUB_PATH" \
   | grep -Eq '^[A-Za-z0-9+/=]{40,}$'; then
  echo "Refusing to overwrite existing public key at $PUB_PATH" >&2
  echo "Delete it first if you really want to rotate keys." >&2
  exit 1
fi

# -W = no password, -f = force overwrite of dest paths
rsign generate -W -p "$PUB_PATH" -s "$SEC_PATH" -f >/dev/null

echo "============================================================"
echo "Public key written to: $PUB_PATH"
echo "  → commit this file."
echo
echo "Secret key (copy entire contents into GitHub Actions secret"
echo "  named MINISIGN_SECRET_KEY):"
echo "------------------------------------------------------------"
cat "$SEC_PATH"
echo "------------------------------------------------------------"
echo
echo "Then delete the local copy:"
echo "  rm -rf $(dirname "$SEC_PATH")"
echo "============================================================"
