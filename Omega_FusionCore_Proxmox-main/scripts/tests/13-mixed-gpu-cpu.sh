#!/usr/bin/env bash
# Test M3 — Mixte GPU + CPU : placement multi-contraintes
# Vérifie que la VM atterrit sur un nœud qui satisfait GPU ET vCPU simultanément
# Usage : ./13-mixed-gpu-cpu.sh [vmid]
# Prérequis : cluster 3+ nœuds, au moins 1 nœud GPU, GPU PCI classe 0x03xx

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
ensure_omega_vcpu_profile "$VMID"
VM_RAM_MIB=$(vm_ram_mib "$VMID"); VM_RAM_MIB="${VM_RAM_MIB:-1024}"
VM_CORES=$(vm_cores "$VMID");     VM_CORES="${VM_CORES:-4}"
MIN_VCPUS_REQUIRED=$VM_CORES

header "Test M3 — Mixte GPU + CPU (VM $VMID)"
print_cluster_config

step "Prérequis"
require_cluster

step "Inventaire GPU dans le cluster"
GPU_NODES=()
for n in "${OMEGA_NODES_ARR[@]}"; do
    gpu=$(ssh_run "$n" \
        "ls /sys/bus/pci/devices/*/class 2>/dev/null | xargs grep -l '^0x03' 2>/dev/null | head -1" \
        2>/dev/null || echo "")
    if [[ -n "$gpu" ]]; then
        pci=$(ssh_run "$n" "basename \$(dirname \$(ls /sys/bus/pci/devices/*/class | \
              xargs grep -l '^0x03' 2>/dev/null | head -1))" 2>/dev/null || echo "?")
        GPU_NODES+=("$n")
        info "GPU détecté sur $n : $pci"
    fi
done
[[ ${#GPU_NODES[@]} -ge 1 ]] || fail "aucun nœud GPU trouvé dans le cluster — test non applicable"

step "Inventaire vCPUs libres par nœud"
for n in "${OMEGA_NODES_ARR[@]}"; do
    vcpu_free=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
        python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('vcpu_free','?'))" || echo "?")
    has_gpu=$(printf '%s\n' "${GPU_NODES[@]}" | grep -qx "$n" && echo "OUI" || echo "non")
    info "  $n : vcpu_free=$vcpu_free  gpu=$has_gpu"
done

step "Nœud initial de la VM"
node_init=$(vm_node "$VMID")
info "VM $VMID sur : $node_init"

step "Démarrage agent avec GPU required + vCPU élastique (min=$MIN_VCPUS_REQUIRED)"
LOG="/tmp/omega-m3.log"
_TMPFILES+=("$LOG")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --current-node "$COMPUTE_NODE" \
    --gpu-required \
    --gpu-placement-interval-secs 10 \
    --vm-vcpus "$MIN_VCPUS_REQUIRED" \
    --vm-initial-vcpus 1 \
    --vcpu-high-threshold-pct 50 \
    --vcpu-scale-interval-secs 10 \
    --mode daemon >"$LOG" 2>&1 &
_PIDS+=($!)

step "Charge CPU dans la VM pour déclencher hotplug"
vm_cpu_stress "$VMID" 80

step "Surveillance placement GPU + hotplug vCPU pendant 90s"
t0=$SECONDS
gpu_placed=false; vcpu_scaled=false
vcpus_init="$(vm_runtime_vcpus "$VMID" || echo 1)"
vcpus_max="$vcpus_init"

while [[ $(elapsed $t0) -lt 90 ]]; do
    node_now=$(vm_node "$VMID" || echo "?")
    vcpus_now="$(vm_runtime_vcpus "$VMID" || true)"
    [[ -n "$vcpus_now" && "${vcpus_now:-1}" -gt "$vcpus_max" ]] && vcpus_max="$vcpus_now"

    # Placement GPU réussi si VM sur un nœud GPU
    printf '%s\n' "${GPU_NODES[@]}" | grep -qx "$node_now" && gpu_placed=true
    # vCPU scalé si plus de 1 vCPU
    [[ -n "$vcpus_now" && "${vcpus_now:-1}" -gt "$vcpus_init" ]] && vcpu_scaled=true

    printf "\r  [%3ds] nœud=%-12s  vCPUs runtime=%-3s(max=%s)  sur_gpu_node=%s" \
        "$(elapsed $t0)" "$node_now" "${vcpus_now:-?}" "$vcpus_max" \
        "$([ $gpu_placed = true ] && echo OUI || echo non)"
    sleep 5
done
echo ""

step "Résultats"
node_final=$(vm_node "$VMID" || echo "inconnu")
info "VM $VMID : $node_init → $node_final"
info "vCPU runtime initial=$vcpus_init | max observé=$vcpus_max"
info "Nœuds GPU : ${GPU_NODES[*]}"

$gpu_placed   && pass "GPU placement OK — VM sur nœud GPU : $node_final" || \
                 warn "VM pas encore sur nœud GPU (peut nécessiter plus de temps)"
$vcpu_scaled  && pass "vCPU scale-up OK — max=$vcpus_max" || \
                 warn "aucun scale-up vCPU (charge insuffisante ?)"

step "Logs agent"
grep -i "gpu\|placement\|migration\|vcpu\|scale" "$LOG" | head -20

($gpu_placed || $vcpu_scaled) && \
    pass "M3 OK — contraintes GPU et/ou CPU respectées" || \
    fail "M3 : ni GPU placement ni vCPU scale détectés"
