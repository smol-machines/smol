#!/usr/bin/env bash
# Stage libkrun/libkrunfw into the Python package dir (python/smol/) so the wheel
# ships them next to the compiled `_native` extension. The extension loads them
# via the relocatable rpath (@loader_path / $ORIGIN) emitted by build.rs.
#
# Run BEFORE `maturin build` for a publishable wheel. For in-tree
# `maturin develop` this is optional — build.rs's absolute rpath covers dev.
#
# Source dir: $SMOLVM_LIB_DIR, else $LIBKRUN_BUNDLE, else the repo's lib/.
set -euo pipefail

PY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # sdk/python
LIB_DIR="${SMOLVM_LIB_DIR:-${LIBKRUN_BUNDLE:-$PY_ROOT/../../lib}}"
DEST="$PY_ROOT/python/smol"
mkdir -p "$DEST"

if [ ! -d "$LIB_DIR" ]; then
  echo "bundle-native: lib dir not found: $LIB_DIR" >&2
  echo "set SMOLVM_LIB_DIR or LIBKRUN_BUNDLE to the libkrun/libkrunfw directory" >&2
  exit 1
fi

copied=0
case "$(uname -s)" in
  Darwin)
    shopt -s nullglob
    for src in "$LIB_DIR"/libkrun.dylib "$LIB_DIR"/libkrunfw*.dylib; do
      [ -e "$src" ] || continue
      base="$(basename "$src")"
      cp -f "$src" "$DEST/$base"
      # Make the install-name relocatable.
      install_name_tool -id "@rpath/$base" "$DEST/$base" 2>/dev/null || true
      # libkrun depends on libkrunfw — give it an @loader_path rpath so it finds
      # the sibling libkrunfw bundled next to it, independent of build location.
      if [ "$base" = "libkrun.dylib" ]; then
        install_name_tool -add_rpath "@loader_path" "$DEST/$base" 2>/dev/null || true
      fi
      # Ad-hoc re-sign LAST (macOS dlopen SIGKILLs an unsigned/altered dylib).
      codesign --force --sign - "$DEST/$base" 2>/dev/null || true
      echo "bundled $base"
      copied=$((copied + 1))
    done
    ;;
  *)
    shopt -s nullglob
    for src in "$LIB_DIR"/libkrun.so* "$LIB_DIR"/libkrunfw.so*; do
      [ -e "$src" ] || continue
      base="$(basename "$src")"
      cp -f "$src" "$DEST/$base"
      echo "bundled $base"
      copied=$((copied + 1))
    done
    # The GPU-enabled libkrun carries a hard libvirglrenderer.so.1 NEEDED. Strip
    # it (mirrors the engine's build-dist.sh) so the lib loads on non-GPU hosts
    # via RTLD_LAZY and the wheel can be relabeled manylinux (auditwheel can't
    # vendor virgl). GPU is loaded by soname at runtime only — unused by the SDK.
    if command -v patchelf >/dev/null 2>&1; then
      for lk in "$DEST"/libkrun.so*; do
        [ -e "$lk" ] || continue
        if patchelf --print-needed "$lk" 2>/dev/null | grep -q libvirglrenderer; then
          patchelf --remove-needed libvirglrenderer.so.1 "$lk"
          echo "stripped libvirglrenderer NEEDED from $(basename "$lk")"
        fi
      done
    else
      echo "bundle-native: WARNING — patchelf not found; libkrun keeps its virgl NEEDED (non-GPU Linux load + manylinux relabel will fail)" >&2
    fi
    ;;
esac

# Stage the boot helper (smol-vmm) next to _native so the wheel ships it; the
# package's __init__ points SMOLVM_BOOT_BINARY at it. On macOS the hypervisor
# entitlement must be on this helper (the python process is unentitled), so
# (re)sign it. Without the helper, local transport on macOS fails at VM startup.
if [ -n "${SMOLVM_HELPER:-}" ] && [ -f "${SMOLVM_HELPER}" ]; then
  cp -f "$SMOLVM_HELPER" "$DEST/smol-vmm"
  chmod +x "$DEST/smol-vmm"
  if [ "$(uname -s)" = "Darwin" ]; then
    ENT="${SMOLVM_ENTITLEMENTS:-$PY_ROOT/../../smolvm.entitlements}"
    if [ -f "$ENT" ] && codesign --force --sign - --entitlements "$ENT" "$DEST/smol-vmm" 2>/dev/null; then
      echo "signed smol-vmm with hypervisor entitlement"
    else
      echo "bundle-native: WARNING — could not sign smol-vmm (entitlements: $ENT)" >&2
    fi
  fi
  echo "staged smol-vmm boot helper into $DEST"
else
  echo "bundle-native: WARNING — SMOLVM_HELPER not set/found; wheel ships no boot helper (local transport on macOS will fail)" >&2
fi

# Guest rootfs tarball — makes the wheel self-contained (the engine extracts it
# on first boot via SMOLVM_AGENT_ROOTFS_TAR, wired in __init__.py). A wheel can't
# ship a rootfs dir tree (symlinks/modes), so we ship the tarball. CI builds it
# with scripts/build-agent-rootfs.sh and points SMOLVM_ROOTFS_TAR at the tarball.
if [ -n "${SMOLVM_ROOTFS_TAR:-}" ] && [ -f "${SMOLVM_ROOTFS_TAR}" ]; then
  cp -f "$SMOLVM_ROOTFS_TAR" "$DEST/agent-rootfs.tar"
  echo "staged agent-rootfs.tar into $DEST"
else
  echo "bundle-native: WARNING — SMOLVM_ROOTFS_TAR not set/found; wheel ships no guest rootfs (boot needs one already on the host)" >&2
fi

if [ "$copied" -eq 0 ]; then
  echo "bundle-native: WARNING — no libkrun/libkrunfw libs found in $LIB_DIR" >&2
  exit 1
fi
echo "bundle-native: staged $copied lib(s) from $LIB_DIR into $DEST"
