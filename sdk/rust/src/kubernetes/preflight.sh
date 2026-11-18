set -eu
command -v flock >/dev/null || { echo 'The guest runtime needs flock for safe cluster lifecycle operations' >&2; exit 1; }
for match in set limit physdev; do
    grep -qx "$match" /proc/net/ip_tables_matches || {
        echo "The guest kernel lacks Kubernetes network-policy support ($match); update the Smol runtime before starting this cluster" >&2
        exit 1
    }
done
grep -qx NFLOG /proc/net/ip_tables_targets || {
    echo 'The guest kernel lacks Kubernetes network-policy logging; update the Smol runtime' >&2
    exit 1
}
