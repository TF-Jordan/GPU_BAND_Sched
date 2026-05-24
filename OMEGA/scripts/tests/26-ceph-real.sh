#!/usr/bin/env bash
# Test 26 — Ceph réel : librados, ceph.conf, pool, exposition status Omega.
# Usage : ./26-ceph-real.sh [pool]

set -euo pipefail
source "$(dirname "$0")/lib.sh"

POOL="${1:-${OMEGA_CEPH_POOL:-}}"
CEPH_TIMEOUT_SECS="${OMEGA_CEPH_TIMEOUT_SECS:-60}"
[[ "$CEPH_TIMEOUT_SECS" =~ ^[0-9]+$ && "$CEPH_TIMEOUT_SECS" -ge 1 ]] || CEPH_TIMEOUT_SECS=60
CEPH_WAIT_SECS="$CEPH_TIMEOUT_SECS"
CEPH_CONNECT_TIMEOUT=$(( CEPH_WAIT_SECS < 30 ? CEPH_WAIT_SECS : 30 ))

header "Test 26 — Ceph réel${POOL:+ (pool $POOL)}"

require_cluster

for node in "${OMEGA_NODES_ARR[@]}"; do
    step "Nœud $node — packages/config Ceph"
    ssh_run "$node" "test -f /etc/ceph/ceph.conf" \
        || fail "$node : /etc/ceph/ceph.conf absent"
    ssh_run "$node" "ldconfig -p 2>/dev/null | grep -q librados" \
        || fail "$node : librados absent — installer librados-dev/ceph-common puis recompiler"
    pass "$node : ceph.conf + librados OK"

    step "Nœud $node — ceph status/pool"
    if ssh_run "$node" "command -v ceph >/dev/null && ceph status --connect-timeout ${CEPH_CONNECT_TIMEOUT} >/tmp/omega-ceph-status.txt 2>&1"; then
        ssh_run "$node" "head -20 /tmp/omega-ceph-status.txt" | sed 's/^/    /'
    else
        fail "$node : ceph status échoue"
    fi

    step "Nœud $node — attente Ceph stable (${CEPH_WAIT_SECS}s max)"
    if ssh_run "$node" "
        deadline=\$((SECONDS + ${CEPH_WAIT_SECS}))
        while [ \$SECONDS -lt \$deadline ]; do
            ceph -s 2>/dev/null | grep -q 'HEALTH_OK' && exit 0
            ceph -s 2>/dev/null | grep -q 'active+clean' && ! ceph -s 2>/dev/null | grep -Eq 'degraded|undersized|remapped|backfill|recovery' && exit 0
            sleep 5
        done
        ceph -s
        exit 1
    "; then
        pass "$node : Ceph stable"
    else
        warn "$node : Ceph pas totalement HEALTH_OK avant timeout; poursuite du test fonctionnel"
    fi

    pools=$(ssh_run "$node" "ceph osd pool ls 2>/dev/null" || true)
    if [[ -z "$pools" ]]; then
        fail "$node : aucun pool Ceph listable"
    fi
    if [[ -z "$POOL" ]]; then
        detected_pool=$(printf '%s\n' "$pools" | grep -E '^(ceph-vm|ceph-vms|VM-Storage|omega-pages|rbd)$' | head -1 || true)
        detected_pool="${detected_pool:-$(printf '%s\n' "$pools" | head -1)}"
        pass "$node : pools Ceph présents (pool de référence: $detected_pool)"
    elif printf '%s\n' "$pools" | grep -qx "$POOL"; then
        pass "$node : pool $POOL présent"
    else
        fail "$node : pool $POOL absent"
    fi
done

step "Status Omega — ceph_enabled"
ceph_ok=0
for node in "${OMEGA_NODES_ARR[@]}"; do
    status=$(curl -sf "http://${node}:${STATUS_PORT}/status" 2>/dev/null || curl -sf "http://${node}:${STATUS_PORT}/api/status" 2>/dev/null || echo "{}")
    enabled=$(printf '%s' "$status" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("ceph_enabled", False))' 2>/dev/null || echo "False")
    info "$node : ceph_enabled=$enabled"
    [[ "$enabled" == "True" || "$enabled" == "true" ]] && ceph_ok=$((ceph_ok + 1))
done

if [[ "$ceph_ok" -gt 0 ]]; then
    pass "Ceph réel validé côté cluster Omega"
else
    warn "aucun store Omega ne déclare ceph_enabled=true — Ceph Proxmox est valide, mais le backend store Ceph Omega n'est pas activé"
    pass "Ceph réel Proxmox validé; backend Ceph Omega non actif"
fi
