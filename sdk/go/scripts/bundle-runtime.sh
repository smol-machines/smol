#!/usr/bin/env bash
# Package the Go SDK's runtime bundle for one platform: the native core
# (libsmol_go) beside the boot helper, hypervisor libraries and guest rootfs
# that the npm platform package carries (sdk/node/native/<platform>/, from
# sdk/node/scripts/bundle-native.sh). The SDK downloads this tarball from the
# smol release on first local use.
#
#   bundle-runtime.sh <native-dir> <libsmol_go> <platform> <out-dir>
#
# <platform> is darwin-arm64, linux-x64 or linux-arm64. Writes
# <out-dir>/smol-go-runtime-<version>-<platform>.tar.gz, flat.
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 <native-dir> <libsmol_go> <platform> <out-dir>" >&2; exit 2; }
NATIVE="$1"; LIB="$2"; PLAT="$3"; OUT="$4"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="$(sed -nE 's/^const Version = "([^"]+)"/\1/p' "$SCRIPT_DIR/../assets.go")"
[ -n "$VERSION" ] || { echo "no Version in assets.go" >&2; exit 1; }

case "$PLAT" in
  darwin-arm64) LIBNAME=libsmol_go.dylib ;;
  linux-x64|linux-arm64) LIBNAME=libsmol_go.so ;;
  *) echo "unsupported platform: $PLAT" >&2; exit 1 ;;
esac

for f in "$NATIVE/smol-vmm" "$NATIVE/agent-rootfs.tar" "$LIB"; do
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done

NAME="smol-go-runtime-$VERSION-$PLAT"
STAGE="$(mktemp -d)/$NAME"
mkdir -p "$STAGE" "$OUT"
# -R copies subdirectories such as licenses/; -P keeps
# libkrunfw.dylib -> libkrunfw.5.dylib a symlink.
cp -RP "$NATIVE"/* "$STAGE/"
# The npm package's addon is not part of the Go runtime.
rm -f "$STAGE"/*.node
cp "$LIB" "$STAGE/$LIBNAME"
chmod 755 "$STAGE/$LIBNAME" "$STAGE/smol-vmm"

tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" .
echo "$OUT/$NAME.tar.gz ($(du -h "$OUT/$NAME.tar.gz" | cut -f1))"
tar -tzf "$OUT/$NAME.tar.gz" | sed 's/^/  /'
