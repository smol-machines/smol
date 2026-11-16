set -eu
umask 077
root=/workspace/.smol/kubernetes
staged=$1
expected=$2
trap 'rm -f "$staged"' EXIT
exec 9>"$root/operation.lock"
flock -w 15 9
actual=$(sha256sum "$staged")
test "${actual%% *}" = "$expected" || { echo 'Uploaded Kubernetes artifact failed checksum verification' >&2; exit 1; }
if server_running; then
    current=$(sha256sum "$root/k3s")
    test "${current%% *}" = "$expected" || { echo 'Stop the cluster before changing its Kubernetes binary' >&2; exit 1; }
    exit 0
fi
chmod 700 "$staged"
mv -f "$staged" "$root/k3s"
