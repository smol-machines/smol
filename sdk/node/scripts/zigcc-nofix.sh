#!/usr/bin/env bash
# Linker wrapper for the aarch64-linux Node addon.
#
# rustc emits `-Wl,--fix-cortex-a53-843419` (a Cortex-A53 erratum workaround)
# for the aarch64-unknown-linux-gnu target. napi-rs's raw `--zig` linker driver
# (zig 0.13) does not recognise that arg and aborts with
# "unsupported linker arg: --fix-cortex-a53-843419", so the addon fails to link.
#
# This wrapper drops that one unsupported arg and links via zig against
# glibc 2.34, so the addon still loads on glibc 2.34+ (not just the 2.39 runner).
set -euo pipefail
args=()
for a in "$@"; do
  [ "$a" = "-Wl,--fix-cortex-a53-843419" ] && continue
  args+=("$a")
done
exec zig cc -target aarch64-linux-gnu.2.34 "${args[@]}"
