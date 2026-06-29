#!/usr/bin/env bash
# Assemble the bundled native assets for the `smol` package on the current
# platform: the signed `_boot-vm` boot helper + libkrun/libkrunfw, so the SDK
# needs no system install.
#
# Run from the package dir: `npm run bundle:assets`
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$PKG_DIR/../../.." && pwd)"   # smol/sdk/node -> repo root

# libkrun/libkrunfw source dir: explicit env (CI points this at the engine
# checkout, e.g. __engine__/lib) wins; else the repo's own lib/. Mirrors the
# Python bundle script + the build.rs link-dir resolution so CI and local agree.
LIB_DIR="${SMOLVM_LIB_DIR:-${LIBKRUN_BUNDLE:-$REPO_ROOT/lib}}"

OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS-$ARCH" in
  Darwin-arm64) PLAT="darwin-arm64" ;;
  Darwin-x86_64) PLAT="darwin-x64" ;;
  Linux-x86_64) PLAT="linux-x64" ;;
  Linux-aarch64) PLAT="linux-arm64" ;;
  *) echo "unsupported platform: $OS-$ARCH" >&2; exit 1 ;;
esac

DEST="$PKG_DIR/native/$PLAT"
rm -rf "$DEST"; mkdir -p "$DEST"

# --- boot helper (handles `_boot-vm`, embeds + extracts the agent rootfs) ---
# Prefer a release smolvm; fall back to other built binaries.
HELPER=""
for cand in "${SMOLVM_HELPER:-}" "$REPO_ROOT/target/release/smolvm" "$REPO_ROOT/smol/target/release/smol" "$REPO_ROOT/target/debug/smolvm" "$REPO_ROOT/smol/target/debug/smol"; do
  [ -n "$cand" ] || continue
  if [ -f "$cand" ]; then HELPER="$cand"; break; fi
done
[ -n "$HELPER" ] || { echo "no boot helper binary found (build smolvm/smol first)" >&2; exit 1; }
echo "helper: $HELPER"
cp "$HELPER" "$DEST/smol-vmm"

# Guest rootfs tarball — makes the package self-contained (the engine extracts it
# on first boot via SMOLVM_AGENT_ROOTFS_TAR, wired in assets.ts). CI builds it
# with scripts/build-agent-rootfs.sh and points SMOLVM_ROOTFS_TAR at the tarball.
if [ -n "${SMOLVM_ROOTFS_TAR:-}" ] && [ -f "${SMOLVM_ROOTFS_TAR}" ]; then
  cp "$SMOLVM_ROOTFS_TAR" "$DEST/agent-rootfs.tar"
  echo "bundled agent-rootfs.tar ($(du -h "$DEST/agent-rootfs.tar" | cut -f1))"
else
  echo "bundle-native: WARNING — SMOLVM_ROOTFS_TAR not set/found; package ships no guest rootfs (boot needs one already on the host)" >&2
fi

if [ "$OS" = "Darwin" ]; then
  # Copy the libs VERBATIM — they already resolve each other at runtime
  # (libkrun dlopens libkrunfw by name from the lib dir). Do NOT run
  # install_name_tool: it needlessly invalidates the working signatures.
  cp -p "$LIB_DIR/libkrun.dylib" "$DEST/"
  cp -p "$LIB_DIR/libkrunfw.5.dylib" "$DEST/"
  # Versioned soname symlinks — REQUIRED. libkrun is resolved as `libkrun.1.dylib`;
  # without it the VM exits 0 instantly ("boot subprocess exited during startup").
  ( cd "$DEST" \
      && ln -sf libkrunfw.5.dylib libkrunfw.dylib \
      && ln -sf libkrun.dylib libkrun.1.dylib )

  # Only the helper needs (re)signing — with the hypervisor entitlement, so the
  # user's `node` needs none. The copied dylibs keep their original signatures.
  codesign --force --sign - --entitlements "${SMOLVM_ENTITLEMENTS:-$REPO_ROOT/smolvm.entitlements}" "$DEST/smol-vmm"
  # VERIFY the entitlement actually stuck — without it, krun_start_enter fails
  # with -22 and the VM never boots. (This silently regressed once; assert it.)
  if ! codesign -d --entitlements - "$DEST/smol-vmm" 2>&1 | grep -q com.apple.security.hypervisor; then
    echo "ERROR: com.apple.security.hypervisor not applied to smol-vmm" >&2
    exit 1
  fi
  echo "smol-vmm: hypervisor entitlement verified"
else
  cp -p "$LIB_DIR"/libkrun*.so* "$DEST/" 2>/dev/null || true
  cp -p "$LIB_DIR"/libkrunfw*.so* "$DEST/" 2>/dev/null || true
  # Strip the hard libvirglrenderer.so.1 NEEDED from the GPU-enabled libkrun
  # (mirrors the engine's build-dist.sh) so it loads on non-GPU Linux hosts via
  # RTLD_LAZY. GPU is loaded by soname at runtime only — unused by the SDK.
  if command -v patchelf >/dev/null 2>&1; then
    for lk in "$DEST"/libkrun.so*; do
      [ -e "$lk" ] || continue
      if patchelf --print-needed "$lk" 2>/dev/null | grep -q libvirglrenderer; then
        patchelf --remove-needed libvirglrenderer.so.1 "$lk"
        echo "stripped libvirglrenderer NEEDED from $(basename "$lk")"
      fi
    done
  else
    echo "bundle-native: WARNING — patchelf not found; libkrun keeps its virgl NEEDED (non-GPU Linux load will fail)" >&2
  fi
fi

echo "bundled -> $DEST"
ls -la "$DEST"
