#!/usr/bin/env bash
# Cut a smol CLI + SDK release from version-aligned origin/main, tag vVERSION,
# and push — which triggers the
# "Release smol" (CLI dist) and "SDK Release" (npm/PyPI) workflows.
#
# HARD PRECONDITION (enforced): the smolvm engine release vVERSION must
# already exist with its platform assets. smol releases in version lockstep —
# the build checks out the engine at the same tag and downloads its runtime
# tarballs, so tagging smol before the engine has published fails every
# platform job with "release not found".
#
# Use --prepare to update the current worktree for a release PR, without publishing.
# Usage: ./scripts/cut-release.sh 1.7.0 [--prepare]
set -euo pipefail

VERSION="${1:?usage: cut-release.sh X.Y.Z}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: '$VERSION' is not X.Y.Z" >&2; exit 1; }
MODE="${2:-}"
[[ $# -le 2 && ( -z "$MODE" || "$MODE" == --prepare ) ]] || { echo "usage: cut-release.sh X.Y.Z [--prepare]" >&2; exit 1; }

bump_versions() {
  for f in Cargo.toml sdk/python/Cargo.toml sdk/python/pyproject.toml sdk/node/Cargo.toml sdk/rust/Cargo.toml crates/smol-cloud/Cargo.toml; do
    perl -i -pe "s/^version = \"[^\"]+\"/version = \"$VERSION\"/" "$f"
  done
  perl -i -pe "s/^(smol-cloud = .*version = )\"[^\"]+\"/\$1\"$VERSION\"/" sdk/rust/Cargo.toml
  perl -i -pe "s/^  \"version\": \"[^\"]+\"/  \"version\": \"$VERSION\"/" sdk/node/package.json
  perl -i -pe "s/__version__ = \"[^\"]+\"/__version__ = \"$VERSION\"/" sdk/python/python/smol/__init__.py
  perl -0777 -i -pe "s/(\"name\": \"smolmachines\",\n\\s*\"version\": )\"[^\"]+\"/\$1\"$VERSION\"/g" sdk/node/package-lock.json
  for f in Cargo.lock sdk/node/Cargo.lock sdk/python/Cargo.lock sdk/rust/Cargo.lock; do
    perl -0777 -i -pe "s/(name = \"(?:smol-cli|smol-cloud|smol-node|smol-py|smolmachines)\"\nversion = )\"[^\"]+\"/\$1\"$VERSION\"/g" "$f"
  done
  bash scripts/check-versions.sh
}

if [ "$MODE" = --prepare ]; then
  cd "$(git rev-parse --show-toplevel)"
  bump_versions
  echo ">>> Prepared $VERSION in this worktree; no commit, tag, or publication made."
  exit 0
fi

ENGINE_REPO="${ENGINE_REPO:-smol-machines/smolvm}"

# ── engine release must exist first ─────────────────────────────────────────
ASSETS="$(gh release view "v$VERSION" --repo "$ENGINE_REPO" --json assets --jq '.assets | length' 2>/dev/null || echo 0)"
if [ "${ASSETS:-0}" -lt 5 ]; then
  echo "error: engine $ENGINE_REPO v$VERSION is not published (found $ASSETS assets, need >=5)." >&2
  echo "       cut the engine first:  (in smolvm)  ./scripts/cut-release.sh $VERSION" >&2
  exit 1
fi
echo ">>> engine v$VERSION present ($ASSETS assets)"

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

git fetch origin main --tags
if git rev-parse "v$VERSION" >/dev/null 2>&1; then
  echo "error: tag v$VERSION already exists" >&2; exit 1
fi

BRANCH="release-v$VERSION"
WT="$(mktemp -d)/smol-rel-$VERSION"
git worktree add -b "$BRANCH" "$WT" origin/main
trap 'git worktree remove "$WT" --force 2>/dev/null || true' EXIT
cd "$WT"

MAIN_VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f 2)"
if [ "$MAIN_VERSION" != "$VERSION" ]; then
  echo "error: main is at $MAIN_VERSION; run --prepare and merge the $VERSION bump first." >&2
  exit 1
fi
bump_versions

git add Cargo.toml Cargo.lock sdk/ crates/
if ! git diff --cached --quiet; then
  git commit -m "Bump smol CLI and SDKs to $VERSION"
fi
git tag -a "v$VERSION" -m "smol v$VERSION"
git push -u origin "$BRANCH"
git push origin "v$VERSION"

echo ">>> v$VERSION tagged and pushed. Watch the CLI + SDK release workflows:"
echo "    gh run list --repo smol-machines/smol --limit 4"
