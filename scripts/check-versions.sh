#!/usr/bin/env bash
#
# Assert every version-bearing manifest agrees on one version.
#
# The smol CLI and both SDKs share a single version (it tracks the smolvm engine
# release). A drift between the Rust Cargo.toml and the *published* manifests
# (pyproject.toml -> PyPI, package.json -> npm) silently ships a stale SDK — e.g.
# the Cargo.toml-at-1.2.0 / pyproject-at-1.1.2 / untagged state that this guard
# now prevents. Runs in CI on every push/PR; needs no engine or secrets.
set -euo pipefail
cd "$(dirname "$0")/.."

# Extract the first X.Y.Z[-pre] from a `version = "…"` / `"version": "…"` line.
extract() {
    grep -m1 -E "$2" "$1" \
        | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+[^"]*)".*/\1/'
}

declare -A versions
versions["Cargo.toml"]=$(extract "Cargo.toml" '^version[[:space:]]*=')
versions["sdk/python/pyproject.toml"]=$(extract "sdk/python/pyproject.toml" '^version[[:space:]]*=')
versions["sdk/python/Cargo.toml"]=$(extract "sdk/python/Cargo.toml" '^version[[:space:]]*=')
versions["sdk/python/python/smol/__init__.py"]=$(extract "sdk/python/python/smol/__init__.py" '^__version__[[:space:]]*=')
versions["sdk/node/package.json"]=$(extract "sdk/node/package.json" '"version"[[:space:]]*:')
versions["sdk/node/Cargo.toml"]=$(extract "sdk/node/Cargo.toml" '^version[[:space:]]*=')

ref="${versions["Cargo.toml"]}"
mismatch=0
# Sort the keys via command substitution (not `| sort`, which would subshell the
# loop and lose `mismatch`). Paths have no spaces, so word-splitting is safe.
for file in $(printf '%s\n' "${!versions[@]}" | sort); do
    printf '  %-42s %s\n' "$file" "${versions[$file]}"
    [ "${versions[$file]}" = "$ref" ] || mismatch=1
done

if [ "$mismatch" -ne 0 ]; then
    echo "::error::version drift — all manifests must match Cargo.toml ($ref). Bump them together."
    exit 1
fi

# On a tag build, the tag must match too (releases are cut as `vX.Y.Z`).
if [ "${GITHUB_REF_TYPE:-}" = "tag" ]; then
    tag="${GITHUB_REF_NAME#v}"
    if [ "$tag" != "$ref" ]; then
        echo "::error::tag ($GITHUB_REF_NAME) != manifest version ($ref)."
        exit 1
    fi
fi

echo "OK: all manifests at $ref"
