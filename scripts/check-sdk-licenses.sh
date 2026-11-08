#!/usr/bin/env bash
#
# Assert every published SDK directory carries the repository LICENSE.
#
# A package built from a subdirectory only ships what is in that directory. The
# Go module zip for sdk/go had no LICENSE, so pkg.go.dev showed "Documentation
# not displayed due to license restrictions" for every version up to v1.32.4.
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
