#!/usr/bin/env bash
# build-smol-dist.sh — assemble a self-contained `smol` release for ONE platform.
#
# Output: dist/smol-<version>-<platform>.tar.gz, laid out so the bundled
# scripts/smol-wrapper.sh finds everything relative to itself:
#
#   smol-<version>-<platform>/
#     smol                    the wrapper (sets lib path + agent-rootfs, exec smol-bin)
#     smol-bin                the CLI binary (built against the engine, libkrun-linked)
#     lib/                    libkrun/libkrunfw/... reused from the smolvm release
#     agent-rootfs/           guest rootfs reused from the SAME smolvm release
#     init.krun               Linux guest init (from the release; absent on macOS)
#     storage-template.ext4   pre-formatted disk templates (from the release)
#     overlay-template.ext4
#
# Why reuse the release assets: building `smol` against smolvm vX.Y.Z's crates AND
# bundling vX.Y.Z's agent-rootfs guarantees the host CLI and guest agent speak the
# same wire protocol — no `export_layer`-style drift. `smol` self-boots its VMM
# (`smol _boot-vm`), so no separate smolvm binary is bundled.
#
# Usage:   scripts/build-smol-dist.sh [PLATFORM]
#   PLATFORM ∈ {darwin-arm64, linux-x86_64, linux-arm64}; default = host platform.
#
# Env (all optional):
#   SMOLVM_ENGINE_VERSION   release tag for the runtime assets (default: v<Cargo version>)
#   ENGINE_REPO             repo holding that release (default: smol-machines/smolvm)
#   ENGINE_TARBALL          path to an already-downloaded smolvm-<ver>-<platform>.tar.gz;
#                           when set, skip `gh release download` (lets CI scope the
#                           private-repo token to a separate download step)
#   TARGET                  rust target triple for a cross build (default: native)
#   USE_ZIG=1               build with cargo-zigbuild (Linux glibc-floor cross-link)
#   ZIG_GLIBC               glibc floor for zig (default: 2.34)
#   OUT_DIR                 tarball destination (default: <smol>/dist)
#   GH_TOKEN                token for `gh release download` (private engine repo in CI)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SMOL_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── platform ────────────────────────────────────────────────────────────────
# All supported targets use "arm64"/"x86_64" in the asset name (darwin is arm64
# only; linux ships both). uname -m reports aarch64 on Linux arm → map to arm64.
detect_platform() {
  local os arch
  case "$(uname -s)" in
    Darwin) os=darwin ;;
    Linux)  os=linux ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) arch=arm64 ;;
    x86_64|amd64)  arch=x86_64 ;;
    *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
  esac
  echo "${os}-${arch}"
}
HOST_PLATFORM="$(detect_platform)"
PLATFORM="${1:-$HOST_PLATFORM}"
case "$PLATFORM" in
  darwin-arm64) OS=darwin ;;
  linux-x86_64|linux-arm64) OS=linux ;;
  *) echo "unsupported platform: $PLATFORM (want darwin-arm64|linux-x86_64|linux-arm64)" >&2; exit 1 ;;
esac

# Guard against the Frankenstein bundle: a native build for a non-host platform
# would pair this host's binary with another platform's lib/agent-rootfs. Cross
# builds MUST go through TARGET (+ USE_ZIG on Linux).
if [ "$PLATFORM" != "$HOST_PLATFORM" ] && [ -z "${TARGET:-}" ]; then
  echo "error: building $PLATFORM on a $HOST_PLATFORM host needs a cross build — set TARGET (and USE_ZIG=1 for Linux)." >&2
  exit 1
fi

# ── version + engine release ────────────────────────────────────────────────
VERSION="$(grep -m1 -E '^version[[:space:]]*=' "$SMOL_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "error: could not parse version from Cargo.toml" >&2; exit 1; }
ENGINE_VERSION="${SMOLVM_ENGINE_VERSION:-v$VERSION}"
ENGINE_REPO="${ENGINE_REPO:-smol-machines/smolvm}"
OUT_DIR="${OUT_DIR:-$SMOL_ROOT/dist}"
DIST_NAME="smol-${VERSION}-${PLATFORM}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo ">> building $DIST_NAME (engine assets from $ENGINE_REPO@$ENGINE_VERSION)"

# ── 1. obtain runtime assets from the smolvm release ────────────────────────
ASSET="smolvm-${ENGINE_VERSION#v}-${PLATFORM}.tar.gz"
if [ -n "${ENGINE_TARBALL:-}" ]; then
  TARBALL_SRC="$ENGINE_TARBALL"
else
  echo ">> downloading $ASSET"
  gh release download "$ENGINE_VERSION" --repo "$ENGINE_REPO" --pattern "$ASSET" --dir "$WORK" --clobber
  TARBALL_SRC="$WORK/$ASSET"
fi
[ -f "$TARBALL_SRC" ] || { echo "error: engine asset not found: $TARBALL_SRC" >&2; exit 1; }
tar -xzf "$TARBALL_SRC" -C "$WORK"
ENGINE_EXTRACT="$WORK/smolvm-${ENGINE_VERSION#v}-${PLATFORM}"
LIB_DIR="$ENGINE_EXTRACT/lib"
[ -d "$LIB_DIR" ] || { echo "error: no lib/ in $ASSET" >&2; exit 1; }
[ -d "$ENGINE_EXTRACT/agent-rootfs" ] || { echo "error: no agent-rootfs/ in $ASSET" >&2; exit 1; }

# ── 2. build smol-bin against the engine, linking the release libkrun ───────
# The smol crate's path dep `smolvm = { path = ".." }` must resolve to the engine
# at this same version. Locally `..` is the monorepo (verified protocol-identical);
# CI repoints it to a v-tag checkout before invoking this script.
echo ">> building smol-bin"
export LIBKRUN_BUNDLE="$LIB_DIR"
export SMOLVM_LIB_DIR="$LIB_DIR"
if [ "${USE_ZIG:-0}" = "1" ]; then
  : "${TARGET:?USE_ZIG=1 requires TARGET}"
  # cargo-zigbuild strips the glibc suffix for the output dir (target/<triple>/release),
  # matching the engine's SDK release workflow.
  ( cd "$SMOL_ROOT" && cargo zigbuild --release --bin smol --target "${TARGET}.${ZIG_GLIBC:-2.34}" )
  BIN="$SMOL_ROOT/target/${TARGET}/release/smol"
elif [ -n "${TARGET:-}" ]; then
  ( cd "$SMOL_ROOT" && cargo build --release --bin smol --target "$TARGET" )
  BIN="$SMOL_ROOT/target/${TARGET}/release/smol"
else
  ( cd "$SMOL_ROOT" && cargo build --release --bin smol )
  BIN="$SMOL_ROOT/target/release/smol"
fi
[ -f "$BIN" ] || { echo "error: smol binary not found at $BIN" >&2; exit 1; }

# ── 3. assemble the dist directory ──────────────────────────────────────────
DIST="$WORK/$DIST_NAME"
mkdir -p "$DIST"
cp "$SCRIPT_DIR/smol-wrapper.sh" "$DIST/smol"; chmod +x "$DIST/smol"
cp "$BIN" "$DIST/smol-bin"; chmod +x "$DIST/smol-bin"
cp -a "$LIB_DIR" "$DIST/lib"
cp -a "$ENGINE_EXTRACT/agent-rootfs" "$DIST/agent-rootfs"   # preserves busybox symlinks + /sbin/init
if [ -f "$ENGINE_EXTRACT/init.krun" ]; then
  cp "$ENGINE_EXTRACT/init.krun" "$DIST/"; chmod +x "$DIST/init.krun"
fi
for t in storage-template.ext4 overlay-template.ext4; do
  if [ -f "$ENGINE_EXTRACT/$t" ]; then cp "$ENGINE_EXTRACT/$t" "$DIST/"; fi
done

cat > "$DIST/README.txt" <<EOF
smol ${VERSION} — self-contained CLI for Smol Machines (${PLATFORM})

This bundle is fully self-contained: it ships its own libkrun runtime and guest
agent (smolvm ${ENGINE_VERSION}). No separate smolvm install is required.

Run ./smol from this directory, or symlink it onto your PATH:
  ./smol --help
  ln -sf "\$PWD/smol" ~/.local/bin/smol

The ./smol wrapper points the dynamic loader at ./lib and the engine at
./agent-rootfs, so the directory is relocatable — keep its files together.
EOF

# ── 4. codesign (macOS — HVF/hypervisor access + load the bundled dylibs) ───
if [ "$OS" = darwin ]; then
  echo ">> codesigning smol-bin (hypervisor entitlement)"
  codesign --force --sign - --entitlements "$SMOL_ROOT/smolvm.entitlements" "$DIST/smol-bin"
fi

# ── 5. tarball + checksum ───────────────────────────────────────────────────
mkdir -p "$OUT_DIR"
TARBALL="$OUT_DIR/${DIST_NAME}.tar.gz"
# -S/--sparse: the storage/overlay .ext4 templates are 20 GB + 10 GB SPARSE files
# (mostly holes). macOS bsdtar detects sparseness by default, but GNU tar on the
# Linux runner does NOT — without -S it streams 30 GB of literal zeros into the
# archive, which gzip compresses to ~30 MB of pure padding (doubling the Linux
# bundle vs macOS). bsdtar also accepts -S, so this stays cross-platform.
tar -czSf "$TARBALL" -C "$WORK" "$DIST_NAME"
if command -v shasum >/dev/null 2>&1; then
  ( cd "$OUT_DIR" && shasum -a 256 "${DIST_NAME}.tar.gz" > "${DIST_NAME}.tar.gz.sha256" )
elif command -v sha256sum >/dev/null 2>&1; then
  ( cd "$OUT_DIR" && sha256sum "${DIST_NAME}.tar.gz" > "${DIST_NAME}.tar.gz.sha256" )
else
  echo "error: neither shasum nor sha256sum found" >&2; exit 1
fi
echo ">> done: $TARBALL ($(du -h "$TARBALL" | cut -f1))"
