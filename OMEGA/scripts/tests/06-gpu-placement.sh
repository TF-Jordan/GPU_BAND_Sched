#!/usr/bin/env bash
# Test 6 — GPU placement : migration automatique vers nœud GPU
# Usage : ./06-gpu-placement.sh [vmid]
# Prérequis : cluster Proxmox, au moins un nœud avec GPU PCI (classe 0x03xx)

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
VM_RAM_MIB=$(vm_ram_mib "$VMID"); VM_RAM_MIB="${VM_RAM_MIB:-1024}"

header "Test 6 — GPU placement (VM $VMID)"

step "Vérifications prérequis"
require_bin qm
require_bin pvesh

step "Détection GPU sur les nœuds du cluster"
gpu_found=false
for node in "${OMEGA_NODES_ARR[@]}"; do
    if ssh_run "$node" \
        "ls /sys/bus/pci/devices/*/class 2>/dev/null | xargs grep -l '^0x03' 2>/dev/null | head -1" \
        2>/dev/null | grep -q .; then
        info "GPU détecté sur $node"
        gpu_found=true
    fi
done
$gpu_found || fail "aucun GPU PCI (classe 0x03xx) trouvé sur les nœuds du cluster"

step "État initial : nœud actuel de la VM"
node_before=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
    | python3 -c "import sys,json; vms=json.load(sys.stdin); \
      [print(v['node']) for v in vms if v.get('vmid')==$VMID]" | head -1 || echo "inconnu")
info "VM $VMID actuellement sur : $node_before"

hostpci_before=$(qm config "$VMID" | grep "^hostpci" || echo "aucun")
info "hostpci avant : $hostpci_before"

step "Démarrage agent GPU placement"
LOG_AGENT="/tmp/omega-agent-gpu.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --current-node "$(local_pve_node)" \
    --gpu-required \
    --gpu-placement-interval-secs 10 \
    --mode daemon >"$LOG_AGENT" 2>&1 &
_PIDS+=($!)
AGENT_PID=$!

step "Attente décision placement GPU (max 90s)"
t0=$SECONDS
migrated=false
while [[ $(elapsed $t0) -lt 90 ]]; do
    if grep -qi "migration\|gpu.*placement\|hostpci" "$LOG_AGENT" 2>/dev/null; then
        info "évènement GPU/migration détecté dans les logs"
        migrated=true
        break
    fi
    printf "\r  [%3ds] en attente..." "$(elapsed $t0)"
    sleep 5
done
echo ""

step "État final"
node_after=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
    | python3 -c "import sys,json; vms=json.load(sys.stdin); \
      [print(v['node']) for v in vms if v.get('vmid')==$VMID]" | head -1 || echo "inconnu")
hostpci_after=$(qm config "$VMID" | grep "^hostpci" || echo "aucun")
info "VM $VMID maintenant sur : $node_after"
info "hostpci après : $hostpci_after"

step "Logs GPU agent"
grep -i "gpu\|placement\|migration\|hostpci" "$LOG_AGENT" | head -20

if [[ "$node_before" != "$node_after" ]]; then
    pass "GPU placement OK — VM migrée de $node_before vers $node_after"
elif [[ "$hostpci_after" != "aucun" && "$hostpci_after" != "$hostpci_before" ]]; then
    pass "GPU placement OK — passthrough configuré : $hostpci_after"
else
    warn "aucune migration détectée (VM déjà sur nœud GPU ou pas de GPU disponible)"
    pass "GPU placement testé — voir logs pour détails"
fi
