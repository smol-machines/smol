set -eu
root=/workspace/.smol/kubernetes
# K3s re-execs its extracted binary while retaining its PID. Track the boot
# and process start time instead of a path that legitimately changes at boot.
process_start() {
    record=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    fields=${record##*) }
    case "$fields" in Z\ *) return 1;; esac
    printf '%s\n' "$fields" | awk '{print $20}'
}
server_running() {
    test -s "$root/pid" || return 1
    read -r boot pid started < "$root/pid" || return 1
    case "$pid" in ''|0|*[!0-9]*) return 1;; esac
    case "$started" in ''|*[!0-9]*) return 1;; esac
    test "$boot" = "$(cat /proc/sys/kernel/random/boot_id)" || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    test "$started" = "$(process_start "$pid")"
}
