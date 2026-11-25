#!/usr/bin/env bash
#
# Fail when any ELF file under the given paths needs a glibc newer than the
# floor. A wheel or npm package that installs on a host but carries a binary
# linked against a newer glibc loads nothing there: the boot helper dies in the
# dynamic loader with "GLIBC_x.y not found" before smolvm can say why.
#
# Usage: scripts/check-glibc-floor.sh 2.28 <file-or-dir>...
set -euo pipefail

floor="${1:?glibc floor, e.g. 2.28}"
shift
status=0
checked=0
while IFS= read -r -d '' file; do
    [ "$(head -c 4 "$file" | od -An -tx1 | tr -d ' ')" = "7f454c46" ] || continue
    checked=$((checked + 1))
    newest="$(objdump -T "$file" 2>/dev/null | grep -o 'GLIBC_2\.[0-9]*' | sort -uV | tail -1 || true)"
    [ -n "$newest" ] || continue
    if [ "$(printf '%s\n%s\n' "GLIBC_$floor" "$newest" | sort -V | tail -1)" != "GLIBC_$floor" ]; then
        echo "::error::$file needs $newest, above the $floor floor"
        status=1
    fi
done < <(find "$@" -type f -print0)

[ "$checked" -gt 0 ] || { echo "::error::no ELF files under $*"; exit 1; }
[ "$status" = 0 ] && echo "OK: $checked ELF files need glibc $floor or older"
exit "$status"
