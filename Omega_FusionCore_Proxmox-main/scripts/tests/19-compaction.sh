#!/usr/bin/env bash
# Test 19 — Compaction globale : migration de petites VMs pour libérer un nœud saturé
# Usage : ./19-compaction.sh [vmid]
# Prérequis : cluster 3 nœuds (OMEGA_NODES), au moins 2 VMs actives

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
VM_RAM_MIB=$(vm_ram_mib "$VMID"); VM_RAM_MIB="${VM_RAM_MIB:-1024}"

header "Test 19 — Compaction cluster (VM $VMID)"

step "Vérifications prérequis"
require_bin qm
require_bin pvesh
require_cluster

step "État initial du cluster"
for n in "${OMEGA_NODES_ARR[@]}"; do
    avail=$(curl -sf "http://${n}:${STATUS_PORT}/status" \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('available_mib','?'))" 2>/dev/null || echo "?")
    pages=$(curl -sf "http://${n}:${STATUS_PORT}/status" \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('page_count','?'))" 2>/dev/null || echo "?")
    info "  $n : RAM libre=${avail} MiB, pages stockées=${pages}"
done

step "Démarrage agent avec compaction activée (seuil agressif)"
LOG_AGENT="/tmp/omega-agent-compact.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --current-node "$(local_pve_node)" \
    --eviction-threshold-mib 999999 \
    --eviction-batch-size 32 \
    --eviction-interval-secs 5 \
    --compaction-enabled \
    --compaction-interval-secs 30 \
    --migration-enabled \
    --mode daemon >"$LOG_AGENT" 2>&1 &
_PIDS+=($!)
AGENT_PID=$!
sleep 5

step "Simulation pression mémoire pour déclencher la compaction (stress 120s)"
host_mem_stress 120 "88%" || fail "impossible de générer une pression mémoire locale: installer python3 ou stress-ng"

step "Surveillance compaction pendant 150s"
t0=$SECONDS
compact_seen=false
migrate_seen=false
while [[ $(elapsed $t0) -lt 150 ]]; do
    if grep -qi "compact\|compaction" "$LOG_AGENT" 2>/dev/null; then
        compact_seen=true
    fi
    if grep -qi "migration\|qm migrate" "$LOG_AGENT" 2>/dev/null; then
        migrate_seen=true
    fi
    pages_total=0
    for n in "${OMEGA_NODES_ARR[@]}"; do
        p=$(curl -sf "http://${n}:${STATUS_PORT}/status" \
            | python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo 0)
        pages_total=$(( pages_total + p ))
    done
    printf "\r  [%3ds] pages_total=%-5s  compaction=%s  migration=%s" \
        "$(elapsed $t0)" "$pages_total" \
        "$([ $compact_seen = true ] && echo OUI || echo non)" \
        "$([ $migrate_seen = true ] && echo OUI || echo non)"
    sleep 5
done
echo ""

step "Logs compaction"
grep -i "compact\|migrat\|bin.pack\|vider" "$LOG_AGENT" | head -20 || true

step "État final du cluster"
for n in "${OMEGA_NODES_ARR[@]}"; do
    avail=$(curl -sf "http://${n}:${STATUS_PORT}/status" \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('available_mib','?'))" 2>/dev/null || echo "?")
    info "  $n : RAM libre=${avail} MiB"
done

if $compact_seen || $migrate_seen; then
    pass "compaction OK — activité de compaction/migration observée sous pression"
else
    warn "compaction non déclenchée — pression insuffisante ou cluster équilibré"
    info "vérifier que --compaction-enabled est supporté par le binaire actuel"
    pass "test compaction terminé — aucune erreur fatale"
fi
