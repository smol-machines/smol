#!/usr/bin/env bash
#
# Regenerate the cloud client types from smolfleet's OpenAPI document.
#
# smolfleet is the source of truth: its `openapi` subcommand emits a spec
# derived from the shared `smolfleet-api` Rust types. We snapshot that spec
# into openapi/smolfleet.json and codegen generated/smolfleet.ts from it, so
# the SDK's cloud wire shapes can never silently drift from the server.
#
# Usage:
#   npm run gen:openapi
#   SMOLFLEET_BIN=/path/to/smolfleet npm run gen:openapi
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK_DIR="$(dirname "$SCRIPT_DIR")"
SPEC="$SDK_DIR/openapi/smolfleet.json"
OUT="$SDK_DIR/generated/smolfleet.ts"

# Locate (or build) the smolfleet binary.
find_bin() {
  if [[ -n "${SMOLFLEET_BIN:-}" && -x "${SMOLFLEET_BIN}" ]]; then
    echo "$SMOLFLEET_BIN"; return
  fi
  local repo="$SDK_DIR/../../../smolfleet"
  for prof in release debug; do
    if [[ -x "$repo/target/$prof/smolfleet" ]]; then
      echo "$repo/target/$prof/smolfleet"; return
    fi
  done
  if [[ -d "$repo" ]]; then
    echo "building smolfleet (debug)…" >&2
    (cd "$repo" && cargo build --bin smolfleet >&2)
    echo "$repo/target/debug/smolfleet"; return
  fi
  echo "ERROR: smolfleet binary not found. Set SMOLFLEET_BIN or place the repo at $repo" >&2
  exit 1
}

BIN="$(find_bin)"
echo "Using smolfleet: $BIN" >&2

mkdir -p "$SDK_DIR/openapi" "$SDK_DIR/generated"
"$BIN" openapi > "$SPEC"
echo "Wrote spec: $SPEC ($(wc -c < "$SPEC" | tr -d ' ') bytes)" >&2

"$SDK_DIR/node_modules/.bin/openapi-typescript" "$SPEC" -o "$OUT"
echo "Wrote types: $OUT" >&2
