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

# macOS mini-delocate: a GPU-enabled libkrun.dylib hard-links Homebrew
# libepoxy/virglrenderer by ABSOLUTE path (e.g.
# /opt/homebrew/opt/virglrenderer/lib/libvirglrenderer.1.dylib). GPU is unused by
# the SDK, but those load commands must still resolve or `dlopen` fails on any Mac
# without those Homebrew formulae (the "Library not loaded" install error). Vendor
# each absolute dep next to the bundled lib and repoint it to @loader_path,
# recursively (virglrenderer itself pulls in libepoxy + libMoltenVK).
_vendor_macho_deps() {
  local target="$1" dep dbase src cand pfx
  # Read via process substitution (NOT a pipe) so this runs in the current shell:
  # a pipeline would put the loop in a subshell where a fatal `exit 1` — a wheel
  # that isn't self-contained must never publish — would be swallowed.
  while read -r dep; do
    dbase="$(basename "$dep")"
    case "$dep" in
      # Absolute Homebrew/MacPorts path: repoint to @loader_path AND vendor.
      /opt/homebrew/*|/usr/local/*|/opt/local/*) ;;
      # Already relative to its loader (e.g. virglrenderer -> @loader_path/
      # libMoltenVK): no repoint, but the sibling must still be vendored.
      @loader_path/*|@rpath/*)
        [ "$dbase" = "$(basename "$target")" ] && continue  # self-id, skip
        ;;
      *) continue ;;  # system lib / framework — leave it
    esac
    # Locate the actual dylib to vendor BEFORE repointing. The load command may
    # name an absolute Homebrew path that doesn't exist verbatim on this builder
    # (a different Homebrew prefix, or `brew install`ed after libkrun was built),
    # so fall back to searching every known Homebrew prefix.
    if [ ! -e "$DEST/$dbase" ]; then
      src=""
      if [ -e "$dep" ]; then src="$dep"
      elif [ -e "$LIB_DIR/$dbase" ]; then src="$LIB_DIR/$dbase"
      else
        for pfx in "${HOMEBREW_PREFIX:-}" /opt/homebrew /usr/local /opt/local; do
          [ -n "$pfx" ] && [ -d "$pfx" ] || continue
          cand="$(find "$pfx/opt" "$pfx/lib" -name "$dbase" 2>/dev/null | head -1)"
          [ -n "$cand" ] && { src="$cand"; break; }
        done
      fi
      if [ -z "$src" ]; then
        # FATAL, not a warning: a repoint-without-vendor ships a wheel whose
        # libkrun dlopens @loader_path/<missing> and hard-fails at VM boot on any
        # Mac lacking the Homebrew formula. Fail the build so it can't slip out.
        echo "bundle-native: FATAL — GPU dep '$dbase' (referenced by $(basename "$target")) not found on this builder; the wheel would not be self-contained and would fail to boot. Install it first: brew install libepoxy virglrenderer" >&2
        exit 1
      fi
      cp -f "$src" "$DEST/$dbase"
      chmod u+w "$DEST/$dbase"
      install_name_tool -id "@rpath/$dbase" "$DEST/$dbase" 2>/dev/null || true
      _vendor_macho_deps "$DEST/$dbase"
      codesign --force --sign - "$DEST/$dbase" 2>/dev/null || true
      echo "vendored dep $dbase"
    fi
    # Repoint the parent to the now-guaranteed-present sibling (absolute paths only).
    case "$dep" in
      /opt/homebrew/*|/usr/local/*|/opt/local/*)
        install_name_tool -change "$dep" "@loader_path/$dbase" "$target" 2>/dev/null || true
        ;;
    esac
  done < <(otool -L "$target" 2>/dev/null | awk 'NR>1{print $1}')
}

case "$(uname -s)" in
  Darwin)
    shopt -s nullglob
    for src in "$LIB_DIR"/libkrun.dylib "$LIB_DIR"/libkrunfw*.dylib; do
      [ -e "$src" ] || continue
      base="$(basename "$src")"
      cp -f "$src" "$DEST/$base"
      chmod u+w "$DEST/$base"
      # Make the install-name relocatable.
      install_name_tool -id "@rpath/$base" "$DEST/$base" 2>/dev/null || true
      # libkrun depends on libkrunfw — give it an @loader_path rpath so it finds
      # the sibling libkrunfw bundled next to it, independent of build location.
      if [ "$base" = "libkrun.dylib" ]; then
        install_name_tool -add_rpath "@loader_path" "$DEST/$base" 2>/dev/null || true
        # Vendor + repoint libkrun's absolute Homebrew GPU deps so the wheel is
        # self-contained on a Mac without those formulae.
        _vendor_macho_deps "$DEST/$base"
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
    # The GPU-enabled libkrun references libvirglrenderer by a hard NEEDED AND by
    # direct symbols. Simply removing the NEEDED (as the engine's build-dist.sh
    # does) leaves those symbols undefined, so libkrun only loads under RTLD_LAZY
    # and FAILS under RTLD_NOW / LD_BIND_NOW. Instead: drop the real NEEDED and
    # satisfy the symbols with a tiny bundled stub of WEAK no-ops. Weak matters:
    # a real system libvirglrenderer (strong symbols) OVERRIDES the stub, so GPU
    # still works on hosts that have virglrenderer installed — the stub is only a
    # fallback that lets libkrun load when virgl is absent. No graphics stack is
    # pulled in and auditwheel sees a local dep, so the wheel still relabels
    # manylinux, and libkrun loads under any binding mode.
    if command -v patchelf >/dev/null 2>&1; then
      stubbed=""
      for lk in "$DEST"/libkrun.so*; do
        [ -e "$lk" ] || continue
        if patchelf --print-needed "$lk" 2>/dev/null | grep -q libvirglrenderer; then
          patchelf --remove-needed libvirglrenderer.so.1 "$lk"
          stubbed=1
        fi
      done
      if [ -n "$stubbed" ]; then
        syms="$(nm -D --undefined-only "$DEST"/libkrun.so* 2>/dev/null \
                | awk '{print $NF}' | grep -iE 'virgl' | sort -u)"
        if [ -n "$syms" ] && command -v cc >/dev/null 2>&1; then
          stub_c="$(mktemp)"
          # WEAK so a real (strong) system virglrenderer overrides them at runtime.
          for s in $syms; do echo "__attribute__((weak)) void $s(void){}"; done > "$stub_c"
          cc -x c -shared -fPIC -Wl,-soname,smol_virgl_stub.so \
             -o "$DEST/smol_virgl_stub.so" "$stub_c"
          rm -f "$stub_c"
          for lk in "$DEST"/libkrun.so*; do
            [ -e "$lk" ] || continue
            patchelf --add-needed smol_virgl_stub.so "$lk"
            cur="$(patchelf --print-rpath "$lk" 2>/dev/null || true)"
            case ":$cur:" in
              *':$ORIGIN:'*) ;;
              *) patchelf --set-rpath "\$ORIGIN${cur:+:$cur}" "$lk" ;;
            esac
          done
          echo "bundled smol_virgl_stub.so ($(printf '%s\n' "$syms" | wc -l | tr -d ' ') weak symbols); libkrun loads under RTLD_NOW"
        else
          echo "bundle-native: WARNING — no cc/nm virgl symbols; libkrun stripped but needs RTLD_LAZY" >&2
        fi
      fi
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
