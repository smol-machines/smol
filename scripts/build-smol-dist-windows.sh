#!/usr/bin/env bash
# build-smol-dist-windows.sh — assemble a self-contained `smol` CLI for Windows.
#
# CROSS build: runs on Linux/macOS, cross-compiles smol.exe with the mingw-w64
# toolchain, and reuses the runtime assets (krun.dll + libkrunfw.dll +
# agent-rootfs.tar.gz + pre-formatted ext4 templates) straight from the matching
# smolvm Windows release zip — the same "reuse the engine release" contract as
# build-smol-dist.sh, so the host CLI and guest agent share one wire protocol.
#
# Windows resolves krun.dll (+ its libkrunfw.dll dependency) and the disk
# templates from smol.exe's OWN directory, and extracts agent-rootfs.tar.gz on
# first run — so there is NO wrapper script here (unlike the Unix bundle);
# everything sits beside smol.exe and the folder is relocatable.
#
# Output: dist/smol-<version>-windows-x86_64.zip (+ .sha256)
#
# Env (all optional):
#   SMOLVM_ENGINE_VERSION  engine release tag for the runtime assets (default v<Cargo version>)
#   ENGINE_REPO            repo holding that release (default smol-machines/smolvm)
#   ENGINE_ZIP             path to an already-downloaded smolvm-<ver>-windows-x86_64.zip;
#                          when set, skip `gh release download`
#   OUT_DIR                zip destination (default <smol>/dist)
#   GH_TOKEN               token for `gh release download`
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SMOL_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PLATFORM="windows-x86_64"
TARGET="x86_64-pc-windows-gnu"

VERSION="$(grep -m1 -E '^version[[:space:]]*=' "$SMOL_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "error: could not parse version from Cargo.toml" >&2; exit 1; }
ENGINE_VERSION="${SMOLVM_ENGINE_VERSION:-v$VERSION}"
ENGINE_REPO="${ENGINE_REPO:-smol-machines/smolvm}"
OUT_DIR="${OUT_DIR:-$SMOL_ROOT/dist}"
DIST_NAME="smol-${VERSION}-${PLATFORM}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo ">> building $DIST_NAME (engine Windows assets from $ENGINE_REPO@$ENGINE_VERSION)"

# ── 1. obtain the engine Windows zip (krun.dll + libkrunfw.dll + rootfs + templates)
ASSET="smolvm-${ENGINE_VERSION#v}-${PLATFORM}.zip"
if [ -n "${ENGINE_ZIP:-}" ]; then
  SRC="$ENGINE_ZIP"
else
  echo ">> downloading $ASSET"
  gh release download "$ENGINE_VERSION" --repo "$ENGINE_REPO" --pattern "$ASSET" --dir "$WORK" --clobber
  SRC="$WORK/$ASSET"
fi
[ -f "$SRC" ] || { echo "error: engine Windows zip not found: $SRC" >&2; exit 1; }
unzip -q "$SRC" -d "$WORK/engine"
ENGINE_EXTRACT="$WORK/engine/smolvm-${ENGINE_VERSION#v}-${PLATFORM}"
for f in krun.dll libkrunfw.dll agent-rootfs.tar.gz; do
  [ -f "$ENGINE_EXTRACT/$f" ] || { echo "error: engine zip missing $f" >&2; exit 1; }
done

# ── 2. cross-compile smol.exe (no link-time libkrun — Windows dlopens krun.dll)
LINKER="${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-x86_64-w64-mingw32-gcc}"
command -v "$LINKER" >/dev/null 2>&1 || {
  echo "error: mingw linker $LINKER not found (apt: gcc-mingw-w64-x86-64; brew: mingw-w64)" >&2; exit 1; }
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$LINKER"
rustup target list --installed 2>/dev/null | grep -q "$TARGET" || rustup target add "$TARGET"
echo ">> cross-compiling smol.exe ($TARGET)"
( cd "$SMOL_ROOT" && cargo build --release --bin smol --target "$TARGET" )
EXE="$SMOL_ROOT/target/$TARGET/release/smol.exe"
[ -f "$EXE" ] || { echo "error: cross build did not produce $EXE" >&2; exit 1; }

# ── 3. assemble the dist — everything beside smol.exe, no wrapper on Windows
DIST="$WORK/$DIST_NAME"
mkdir -p "$DIST"
cp "$EXE" "$DIST/smol.exe"
cp "$ENGINE_EXTRACT/krun.dll" "$DIST/krun.dll"
cp "$ENGINE_EXTRACT/libkrunfw.dll" "$DIST/libkrunfw.dll"
cp "$ENGINE_EXTRACT/agent-rootfs.tar.gz" "$DIST/agent-rootfs.tar.gz"
for t in storage-template.ext4 overlay-template.ext4; do
  [ -f "$ENGINE_EXTRACT/$t" ] && cp "$ENGINE_EXTRACT/$t" "$DIST/"
done

cat > "$DIST/README.txt" <<EOF
smol ${VERSION} — self-contained CLI for Smol Machines (Windows x86_64)

This bundle is fully self-contained: it ships its own libkrun runtime and guest
agent (smolvm ${ENGINE_VERSION}). No separate smolvm install is required.

REQUIREMENTS
  Windows 10/11 x86_64 with the Windows Hypervisor Platform (WHP) feature enabled:
    dism /online /enable-feature /featurename:HypervisorPlatform /all   (then reboot)
  or Settings > Optional features > More Windows features > Windows Hypervisor Platform.

INSTALL
  Unzip anywhere and keep all files together — krun.dll, libkrunfw.dll, the ext4
  disk templates, and agent-rootfs.tar.gz must stay beside smol.exe. Optionally add
  the folder to PATH. Or install with PowerShell:
    irm https://raw.githubusercontent.com/smol-machines/smol/${ENGINE_VERSION}/scripts/install.ps1 | iex

USAGE
  smol.exe run -I alpine --net -- echo "Hello from Windows"
  smol.exe --help

NOT YET SUPPORTED ON WINDOWS: GPU acceleration; machine fork / snapshot.
Networking is TSI-only (outbound TCP/UDP + inbound -p; no virtio-net).
EOF

# ── 4. checksums + zip (Windows ships a .zip, not a tarball)
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
( cd "$DIST" && sha smol.exe krun.dll libkrunfw.dll agent-rootfs.tar.gz > checksums.txt )
mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR/${DIST_NAME}.zip"
( cd "$WORK" && zip -qr "$OUT_DIR/${DIST_NAME}.zip" "$DIST_NAME" )
( cd "$OUT_DIR" && sha "${DIST_NAME}.zip" > "${DIST_NAME}.zip.sha256" )
echo ">> done: $OUT_DIR/${DIST_NAME}.zip ($(du -h "$OUT_DIR/${DIST_NAME}.zip" | cut -f1))"
