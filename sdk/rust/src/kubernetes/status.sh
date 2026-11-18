set -eu
root=/workspace/.smol/kubernetes
if ! test -s "$root/pid"; then echo starting; exit 0; fi
if ! server_running; then
    echo failed
    exit 0
fi
if grep -Eq 'Skipping network policy|Aborting sync|Failed to list conntrack' "$root/server.log"; then
    echo failed
    exit 0
fi
k() { "$root/k3s" kubectl --kubeconfig "$root/kubeconfig" --request-timeout=3s "$@"; }
if ! grep -q 'Starting network policy controller version' "$root/server.log"; then echo starting; exit 0; fi
if ! k get --raw=/readyz >/dev/null 2>&1; then echo starting; exit 0; fi
deployments=$(k -n kube-system get deployment coredns local-path-provisioner traefik \
    -o 'jsonpath={range .items[*]}{.metadata.name}={.status.availableReplicas}{"\n"}{end}' 2>/dev/null) || { echo starting; exit 0; }
for name in coredns local-path-provisioner traefik; do
    printf '%s\n' "$deployments" | grep -Eq "^$name=[1-9][0-9]*$" || { echo starting; exit 0; }
done
echo ready
