set -eu
umask 077
root=/workspace/.smol/kubernetes
mkdir -p "$root/data" "$root/kubelet" "$root/volumes"
exec 9>"$root/operation.lock"
flock -w 15 9
if server_running; then exit 0; fi
# Container root filesystems must be searchable by non-root pod users. Keep
# management files and the kubeconfig private, but use the normal mask
# for the server's container storage and volume creation.
(
umask 022
export K3S_PREFER_BUNDLED_BIN=true
exec nohup "$root/k3s" server \
    --data-dir="$root/data" --node-name="smol-$1" \
    --write-kubeconfig="$root/kubeconfig" --write-kubeconfig-mode=600 \
    --tls-san=127.0.0.1 --flannel-backend=host-gw \
    --kubelet-arg="root-dir=$root/kubelet" --kubelet-arg=fail-swap-on=false \
    --default-local-storage-path="$root/volumes" \
) >"$root/server.log" 2>&1 </dev/null 9>&- &
pid=$!
printf '%s %s %s\n' "$(cat /proc/sys/kernel/random/boot_id)" "$pid" "$(process_start "$pid")" >"$root/pid.new"
mv -f "$root/pid.new" "$root/pid"
