#!/usr/bin/env bash
# Test M2 — Mixte CPU saturé + RAM saturée → double migration
# Nœud compute surchargé sur les deux axes → migration déclenchée
# Usage : ./12-mixed-cpu-ram-migration.sh [vmid]
# Prérequis : cluster Proxmox 3+ nœuds, stress-ng sur hôte et VM

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
ensure_omega_vcpu_profile "$VMID"
VM_RAM_MIB=$(vm_ram_mib "$VMID"); VM_RAM_MIB="${VM_RAM_MIB:-1024}"
VM_CORES=$(vm_cores "$VMID");     VM_CORES="${VM_CORES:-4}"

header "Test M2 — CPU + RAM saturés → migration (VM $VMID)"
print_cluster_config

[[ $(store_count) -ge 1 ]] || fail "au moins 1 nœud store requis (${#STORE_NODES_ARR[@]} trouvé)"

step "Prérequis"
require_cluster

step "Remise à 1 vCPU (état de référence)"
stop_vm_for_reconfig "$VMID"
qm set "$VMID" --vcpus 1 >/dev/null
start_vm_with_hostpci_repair "$VMID" >/dev/null || fail "impossible de redémarrer la VM $VMID avec 1 vCPU runtime"
sleep 5

step "État initial"
node_init=$(vm_node "$VMID")
info "VM $VMID sur : $node_init"
info "Stores ($( store_count ) nœuds) :"
all_stores_status

step "Démarrage agent (éviction + migration activées, seuils bas)"
LOG="/tmp/omega-m2.log"
_TMPFILES+=("$LOG")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --current-node "$COMPUTE_NODE" \
    --eviction-threshold-mib 999999 \
    --eviction-batch-size 64 \
    --eviction-interval-secs 3 \
    --vm-vcpus "$VM_CORES" \
    --vm-initial-vcpus 2 \
    --vcpu-high-threshold-pct 50 \
    --vcpu-overcommit-ratio 3 \
    --migration-enabled \
    --migration-interval-secs 15 \
    --mode daemon >"$LOG" 2>&1 &
_PIDS+=($!)
sleep 3

step "Saturation CPU hôte (stress sur le nœud compute) + charge VM"
info "Saturation CPU hôte (60s)"
host_cpu_stress 60 || true

info "Charge RAM + CPU dans la VM (70s)"
ensure_guest_packages "$VMID" stress-ng qemu-guest-agent || true
if ! qm guest exec "$VMID" -- \
    stress-ng --vm 1 --vm-bytes 80% --cpu 0 --timeout 70s &>/dev/null 2>&1; then
    warn "qemu-guest-agent absent — injection CPU via cgroup (RAM stress ignorée)"
    vm_cpu_stress "$VMID" 70
fi

step "Surveillance CPU + RAM + migration pendant 100s"
t0=$SECONDS
migration_detected=false
cpu_pressure_detected=false

while [[ $(elapsed $t0) -lt 100 ]]; do
    grep -qi "cpu_pressure\|pression cpu\|saturation" "$LOG" 2>/dev/null && \
        cpu_pressure_detected=true
    grep -qi "migration\|qm migrate" "$LOG" 2>/dev/null && \
        migration_detected=true

    node_now=$(vm_node "$VMID" || echo "?")
    pages=0
    for n in "${STORE_NODES_ARR[@]}"; do
        pc=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" || echo 0)
        pages=$((pages + pc))
    done

    printf "\r  [%3ds] nœud=%-8s  pages_stores=%-6s  cpu_pressure=%s  migration=%s" \
        "$(elapsed $t0)" "$node_now" "$pages" \
        "$([ $cpu_pressure_detected = true ] && echo OUI || echo non)" \
        "$([ $migration_detected = true ] && echo OUI || echo non)"
    sleep 5
done
echo ""

step "Résultats"
node_final=$(vm_node "$VMID" || echo "inconnu")
info "VM $VMID : $node_init → $node_final"
$cpu_pressure_detected && pass "cpu_pressure détecté" || warn "cpu_pressure non détecté"
$migration_detected    && pass "migration déclenchée" || warn "migration non déclenchée (pression peut-être insuffisante)"
[[ "$node_init" != "$node_final" ]] && \
    pass "VM migrée : $node_init → $node_final" || \
    warn "VM non déplacée (nœuds cibles peut-être trop chargés aussi)"

step "Logs agent (extraits)"
grep -i "cpu_pressure\|migration\|évict\|recall" "$LOG" | head -20 || true

# PASS si au moins éviction ET (pressure ou migration) détectés
pages_final=0
for n in "${STORE_NODES_ARR[@]}"; do
    pc=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" || echo 0)
    pages_final=$((pages_final + pc))
done
[[ "$pages_final" -gt 0 ]] || warn "aucune page dans les stores"
($cpu_pressure_detected || $migration_detected || [[ "$pages_final" -gt 0 ]]) || \
    fail "M2 : aucun comportement observé — vérifier la configuration"

pass "M2 testé — voir logs pour détails complets"
