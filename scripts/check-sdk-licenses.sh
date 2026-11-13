#!/usr/bin/env bash
#
# Assert every published SDK directory carries the repository LICENSE.
#
# A package built from a subdirectory only ships what is in that directory
# (cmd/go alone falls back to the root LICENSE). Keep the Apache-2.0 terms
# verbatim: pkg.go.dev classifies them with github.com/google/licensecheck, and
# one stray word made it report UNKNOWN and hide the Go SDK docs through v1.33.0.
# Runs in CI on every push/PR; needs no engine or secrets.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0
for dir in sdk/go sdk/node sdk/python sdk/rust sdk/mcp; do
    if [ ! -f "$dir/LICENSE" ]; then
        echo "error: $dir/LICENSE is missing (copy the repository LICENSE)" >&2
        status=1
    elif ! cmp -s LICENSE "$dir/LICENSE"; then
        echo "error: $dir/LICENSE differs from the repository LICENSE" >&2
        status=1
    fi
done
[ "$status" -eq 0 ] && echo "SDK licenses ok"
exit "$status"
