#!/usr/bin/env bash
# Cut a smol CLI + SDK release: bump every manifest to VERSION on a fresh
# branch off origin/main, tag vVERSION, and push — which triggers the
# "Release smol" (CLI dist) and "SDK Release" (npm/PyPI) workflows.
#
# HARD PRECONDITION (enforced): the smolvm engine release vVERSION must
# already exist with its platform assets. smol releases in version lockstep —
# the build checks out the engine at the same tag and downloads its runtime
# tarballs, so tagging smol before the engine has published fails every
# platform job with "release not found".
#
# Usage: ./scripts/cut-release.sh 1.7.0
set -euo pipefail

VERSION="${1:?usage: cut-release.sh X.Y.Z}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: '$VERSION' is not X.Y.Z" >&2; exit 1; }

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

# The current baseline is whatever the CLI manifest declares on main (version
# bumps live only on release branches, so main sits at the last baseline).
BASE="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
echo ">>> bumping manifests $BASE -> $VERSION"

for f in Cargo.toml sdk/python/Cargo.toml sdk/python/pyproject.toml sdk/node/Cargo.toml; do
  perl -i -pe "s/^version = \"\Q$BASE\E\"/version = \"$VERSION\"/" "$f"
done
perl -i -pe "s/\"version\": \"\Q$BASE\E\"/\"version\": \"$VERSION\"/" sdk/node/package.json
perl -i -pe "s/__version__ = \"\Q$BASE\E\"/__version__ = \"$VERSION\"/" sdk/python/python/smol/__init__.py

# The same gate CI runs — catches any manifest this script (or a future
# layout change) missed.
bash scripts/check-versions.sh

git add Cargo.toml sdk/
git commit -m "Bump smol CLI and SDKs to $VERSION"
git tag -a "v$VERSION" -m "smol v$VERSION"
git push -u origin "$BRANCH"
git push origin "v$VERSION"

echo ">>> v$VERSION tagged and pushed. Watch the CLI + SDK release workflows:"
echo "    gh run list --repo smol-machines/smol --limit 4"
