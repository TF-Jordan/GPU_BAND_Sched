#!/usr/bin/env bash
# Test 28 — Partition réseau contrôlée entre contrôleur et un nœud.
# DANGEREUX : modifie temporairement iptables sur le contrôleur.
# Usage : OMEGA_DESTRUCTIVE=1 ./28-network-partition.sh [node]

set -euo pipefail
source "$(dirname "$0")/lib.sh"

TARGET_NODE="${1:-${OMEGA_PARTITION_NODE:-${OMEGA_NODES_ARR[1]:-}}}"

header "Test 28 — Partition réseau contrôlée"

require_cluster
[[ -n "$TARGET_NODE" ]] || fail "aucun nœud cible — passer un node ou OMEGA_PARTITION_NODE"
[[ "${OMEGA_DESTRUCTIVE:-0}" == "1" ]] || fail "test destructif bloqué. Relancer avec OMEGA_DESTRUCTIVE=1"
[[ "$TARGET_NODE" != "$CONTROLLER_NODE" ]] || fail "ne pas partitionner le contrôleur contre lui-même"

require_bin iptables

cleanup_partition() {
    for port in "$STORE_PORT" "$STATUS_PORT" 9300; do
        iptables -D OUTPUT -p tcp -d "$TARGET_NODE" --dport "$port" -j REJECT 2>/dev/null || true
        iptables -D INPUT  -p tcp -s "$TARGET_NODE" --sport "$port" -j REJECT 2>/dev/null || true
    done
}
trap cleanup_partition EXIT INT TERM

step "Baseline avant partition"
curl -sf "http://${TARGET_NODE}:${STATUS_PORT}/status" >/dev/null \
    || curl -sf "http://${TARGET_NODE}:${STATUS_PORT}/api/status" >/dev/null \
    || fail "$TARGET_NODE : status inaccessible avant partition"
pass "$TARGET_NODE : accessible avant partition"

step "Application partition TCP Omega vers $TARGET_NODE"
for port in "$STORE_PORT" "$STATUS_PORT" 9300; do
    iptables -A OUTPUT -p tcp -d "$TARGET_NODE" --dport "$port" -j REJECT
    iptables -A INPUT  -p tcp -s "$TARGET_NODE" --sport "$port" -j REJECT
done
sleep 5

if curl -sf --max-time 3 "http://${TARGET_NODE}:${STATUS_PORT}/status" >/dev/null 2>&1; then
    fail "partition inefficace : $TARGET_NODE répond encore sur $STATUS_PORT"
else
    pass "partition active : $TARGET_NODE isolé côté contrôleur"
fi

step "Observation des autres nœuds"
reachable=0
for node in "${OMEGA_NODES_ARR[@]}"; do
    [[ "$node" == "$TARGET_NODE" ]] && continue
    if curl -sf --max-time 3 "http://${node}:${STATUS_PORT}/status" >/dev/null 2>&1 \
       || curl -sf --max-time 3 "http://${node}:${STATUS_PORT}/api/status" >/dev/null 2>&1; then
        info "$node : toujours accessible"
        reachable=$((reachable + 1))
    else
        warn "$node : status inaccessible"
    fi
done
[[ "$reachable" -gt 0 ]] || fail "plus aucun nœud accessible pendant partition"

step "Suppression partition"
cleanup_partition
sleep 5

curl -sf "http://${TARGET_NODE}:${STATUS_PORT}/status" >/dev/null \
    || curl -sf "http://${TARGET_NODE}:${STATUS_PORT}/api/status" >/dev/null \
    || fail "$TARGET_NODE : ne revient pas après suppression partition"
pass "partition supprimée et nœud récupéré"
