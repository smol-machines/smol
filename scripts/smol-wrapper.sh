#!/bin/bash
# smol - the unified Smol Machines CLI (local microVMs + cloud)
#
# This wrapper makes the distribution relocatable: the `smol` binary embeds the
# smolvm engine, which discovers libkrun/libkrunfw and the agent rootfs relative
# to its own install directory. We point the dynamic loader at the bundled `lib/`
# and the engine at the bundled `agent-rootfs/`, both shipped next to this script,
# so a self-contained `smol` needs no separately-installed smolvm.

set -e

# Resolve symlinks to get the actual script location (so a ~/.local/bin/smol
# symlink still finds the bundle it points into).
resolve_symlink() {
    local target="$1"
    while [[ -L "$target" ]]; do
        local link_dir
        link_dir="$(cd "$(dirname "$target")" && pwd)"
        target="$(readlink "$target")"
        # Handle relative symlinks
        if [[ "$target" != /* ]]; then
            target="$link_dir/$target"
        fi
    done
    echo "$target"
}

# The directory where the real script (and the bundle) lives.
SCRIPT_PATH="$(resolve_symlink "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd)"

# The binary and bundled runtime live in the same directory.
SMOL_BIN="$SCRIPT_DIR/smol-bin"
SMOL_LIB="$SCRIPT_DIR/lib"
SMOL_BUNDLED_ROOTFS="$SCRIPT_DIR/agent-rootfs"

# Hand the engine the bundled rootfs (it also probes <exe>/agent-rootfs, but an
# explicit env keeps it working when invoked through an unusual symlink layout).
if [[ -d "$SMOL_BUNDLED_ROOTFS" ]]; then
    export SMOLVM_AGENT_ROOTFS="${SMOLVM_AGENT_ROOTFS:-$SMOL_BUNDLED_ROOTFS}"
fi

# Linux/KVM needs the guest init (init.krun) injected into the rootfs; the engine
# resolves it from the data dir, not this bundle. macOS/HVF doesn't need it (the
# rootfs's /sbin/init is the guest init), so the darwin bundle ships none. Stage
# the bundled init.krun into the data dir once so a bare-extracted bundle works
# without a separate install step. Best-effort; never block smol on it.
if [[ "$(uname -s)" == "Linux" && -f "$SCRIPT_DIR/init.krun" ]]; then
    SMOL_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/smolvm"
    # Refresh when absent OR when the staged copy differs from this bundle's — a
    # stale init.krun from an older smol/engine version mismatches the guest ABI
    # and SIGKILLs the VM at boot, so re-stage on every version change.
    if ! cmp -s "$SCRIPT_DIR/init.krun" "$SMOL_DATA_DIR/init.krun" 2>/dev/null; then
        mkdir -p "$SMOL_DATA_DIR" \
            && cp "$SCRIPT_DIR/init.krun" "$SMOL_DATA_DIR/init.krun" \
            && chmod +x "$SMOL_DATA_DIR/init.krun" || true
    fi
fi

if [[ ! -x "$SMOL_BIN" ]]; then
    echo "Error: smol binary not found at $SMOL_BIN" >&2
    echo "Make sure you extracted the full distribution." >&2
    exit 1
fi

# Verify the bundled libkrun/libkrunfw are actually present — not just that a
# lib/ directory exists. A partial or interrupted extraction can leave an empty
# lib/, in which case the engine's own discovery fails deep in the boot path with
# the cryptic "libkrun/libkrunfw not found"; catch it here with a clear message.
if [[ "$(uname -s)" == "Darwin" ]]; then
    _krun="$SMOL_LIB/libkrun.dylib"; _krunfw="$SMOL_LIB/libkrunfw.dylib"
else
    _krun="$SMOL_LIB/libkrun.so"; _krunfw="$SMOL_LIB/libkrunfw.so"
fi
if [[ ! -e "$_krun" || ! -e "$_krunfw" ]]; then
    echo "Error: bundled libkrun/libkrunfw missing under $SMOL_LIB" >&2
    echo "The install is incomplete — re-run the installer to fetch a full bundle." >&2
    exit 1
fi

# Hand the engine the bundled lib dir authoritatively via SMOLVM_LIB_DIR (its
# first and highest-priority lookup), exactly like the language SDKs do. This is
# more robust than relying solely on the engine re-deriving its own location from
# the executable path, which can miss under unusual symlink/re-exec layouts and
# is the failure mode this wrapper existed to prevent. Also point the dynamic
# loader at the same dir for the initial dlopen.
export SMOLVM_LIB_DIR="${SMOLVM_LIB_DIR:-$SMOL_LIB}"
if [[ "$(uname -s)" == "Darwin" ]]; then
    export DYLD_LIBRARY_PATH="$SMOL_LIB${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
else
    export LD_LIBRARY_PATH="$SMOL_LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
exec "$SMOL_BIN" "$@"
