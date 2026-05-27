#!/usr/bin/env bash
# Test 5 — vCPU élastique (hotplug + scale-up sous charge)
# Usage : ./05-vcpu-elastic.sh [vmid]
# Prérequis : nœud Proxmox, VM démarrée, qm accessible, stress-ng dans la VM

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
VM_NODE="$(_vm_node_cached "$VMID")"
HIGH_THRESHOLD="${OMEGA_VCPU_TEST_HIGH_THRESHOLD:-45}"
LOW_THRESHOLD="${OMEGA_VCPU_TEST_LOW_THRESHOLD:-15}"
SCALE_INTERVAL="${OMEGA_VCPU_TEST_SCALE_INTERVAL_SECS:-5}"
STRESS_SECS="${OMEGA_VCPU_TEST_STRESS_SECS:-120}"
WATCH_SECS="${OMEGA_VCPU_TEST_WATCH_SECS:-120}"

desc_value() {
    local description="$1" key="$2"
    tr ' ' '\n' <<< "$description" | awk -F= -v k="$key" '$1 == k {print $2; exit}'
}

load_vm_profile() {
    local smp_max
    VM_CFG="$(qm config "$VMID" 2>/dev/null || true)"
    [[ -n "$VM_CFG" ]] || fail "impossible de lire la config Proxmox de la VM $VMID sur ${VM_NODE:-nœud inconnu}; vérifier SSH/qm avant de conclure que la VM est non conforme"
    VM_RAM_MIB=$(printf '%s\n' "$VM_CFG" | awk '/^memory:/{print $2}' | head -1)
    VM_BALLOON_MIB=$(printf '%s\n' "$VM_CFG" | awk '/^balloon:/{print $2}' | head -1)
    VM_CORES_RAW=$(printf '%s\n' "$VM_CFG" | awk '/^cores:/{print $2}' | head -1)
    VM_SOCKETS_RAW=$(printf '%s\n' "$VM_CFG" | awk '/^sockets:/{print $2}' | head -1)
    VM_VCPUS_RAW=$(printf '%s\n' "$VM_CFG" | awk '/^vcpus:/{print $2}' | head -1)
    VM_DESCRIPTION=$(printf '%s\n' "$VM_CFG" | awk -F': ' '$1 == "description" {print $2; exit}')
    VM_RAM_MIB="${VM_RAM_MIB:-1024}"
    VM_BALLOON_MIB="${VM_BALLOON_MIB:-512}"
    VM_VCPUS_RAW="${VM_VCPUS_RAW:-1}"
    VM_CORES=$(( ${VM_CORES_RAW:-1} * ${VM_SOCKETS_RAW:-1} ))
    VM_SMP="$(qm showcmd "$VMID" --pretty 2>/dev/null | grep -- '-smp' | head -1 || true)"
    smp_max="$(printf '%s\n' "$VM_SMP" | sed -n "s/.*maxcpus=\([0-9][0-9]*\).*/\1/p" | head -1)"
    if [[ "$smp_max" =~ ^[0-9]+$ && "$smp_max" -gt "$VM_CORES" ]]; then
        VM_CORES="$smp_max"
    fi
}

repair_vcpu_profile() {
    local desired_max desired_disk desired_gpu desc status
    desired_max="$(desc_value "${VM_DESCRIPTION:-}" omega_max_vcpus)"
    [[ "$desired_max" =~ ^[0-9]+$ && "$desired_max" -gt 1 ]] || desired_max="${OMEGA_VCPU_TEST_MAX_VCPUS:-4}"
    [[ "$desired_max" =~ ^[0-9]+$ && "$desired_max" -gt 1 ]] || desired_max=4
    desired_disk="$(desc_value "${VM_DESCRIPTION:-}" omega_disk_max_gib)"
    desired_gpu="$(desc_value "${VM_DESCRIPTION:-}" omega_gpu_vram_mib)"
    desired_disk="${desired_disk:-20}"
    desired_gpu="${desired_gpu:-0}"
    desc="omega_min_vcpus=1 omega_max_vcpus=${desired_max} omega_memory_min_mib=${VM_BALLOON_MIB} omega_memory_max_mib=${VM_RAM_MIB} omega_disk_max_gib=${desired_disk} omega_gpu_vram_mib=${desired_gpu}"

    warn "réparation automatique VM $VMID : stop, cores=${desired_max}, sockets=1, vcpus=1, hotplug=cpu,disk,network"
    status="$(qm status "$VMID" 2>/dev/null | awk '{print $2}' || true)"
    [[ "$status" == "running" ]] && stop_vm_for_reconfig "$VMID"
    qm set "$VMID" \
        --cores "$desired_max" \
        --sockets 1 \
        --vcpus 1 \
        --hotplug cpu,disk,network \
        --description "$desc" >/dev/null
    sanitize_vm_passthrough_for_node "$VMID" "$VM_NODE" || true
    start_vm_with_hostpci_repair "$VMID" >/dev/null || fail "impossible de redémarrer la VM $VMID après réparation vCPU"
    sleep 8
    unset "_VM_NODE_CACHE[$VMID]"
    VM_NODE="$(_vm_node_cached "$VMID")"
    load_vm_profile
    [[ "$VM_CORES" -gt 1 && "$VM_SMP" == *"maxcpus=${VM_CORES}"* ]] || \
        fail "réparation vCPU incomplète: cores=${VM_CORES}, smp='${VM_SMP:-absent}'"
}

header "Test 5 — vCPU élastique (VM $VMID)"

step "Vérifications prérequis"
require_bin qm
load_vm_profile
pass "VM $VMID en cours d'exécution — RAM=${VM_RAM_MIB} MiB cores=${VM_CORES}"

if [[ "$VM_CORES" -le 1 || "$VM_SMP" != *"maxcpus=${VM_CORES}"* ]]; then
    [[ -n "$VM_SMP" ]] && warn "SMP QEMU détecté : $VM_SMP"
    repair_vcpu_profile
    pass "VM $VMID réparée — RAM=${VM_RAM_MIB} MiB cores=${VM_CORES}"
fi

step "Remise à 1 vCPU (état de référence)"
stop_vm_for_reconfig "$VMID"
qm set "$VMID" --vcpus 1 >/dev/null
start_vm_with_hostpci_repair "$VMID" >/dev/null || fail "impossible de redémarrer la VM $VMID avec 1 vCPU initial"
sleep 5

step "État initial vCPU"
vcpus_init="$(vm_runtime_vcpus "$VMID" || true)"
[[ -n "$vcpus_init" ]] || fail "impossible de lire les vCPUs runtime via QMP/qm monitor; l'UI Proxmox ne peut pas être validée"
info "vCPUs runtime actuels : $vcpus_init"
[[ "$vcpus_init" -eq 1 ]] || fail "état de référence invalide: QEMU expose $vcpus_init vCPUs au lieu de 1"

step "Démarrage agent avec vCPU initial=1, max=$VM_CORES"
LOG_AGENT="/tmp/omega-agent-vcpu.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --vm-vcpus "$VM_CORES" \
    --vm-initial-vcpus 1 \
    --vcpu-high-threshold-pct "$HIGH_THRESHOLD" \
    --vcpu-low-threshold-pct "$LOW_THRESHOLD" \
    --vcpu-scale-interval-secs "$SCALE_INTERVAL" \
    --vcpu-overcommit-ratio 3 \
    --current-node "$(local_pve_node)" \
    --mode daemon >"$LOG_AGENT" 2>&1 &
_PIDS+=($!)
AGENT_PID=$!
sleep 3

step "Vérification vCPU initial = 1"
vcpus_current="$(vm_runtime_vcpus "$VMID" || true)"
[[ -n "$vcpus_current" ]] || fail "impossible de relire les vCPUs runtime après démarrage agent"
info "vCPUs runtime après démarrage agent : $vcpus_current"
[[ "${vcpus_current:-1}" -eq 1 ]] || fail "vCPUs runtime déjà != 1 au démarrage agent ($vcpus_current)"

step "Pool vCPU partagé"
if [[ -f /run/omega-vcpu-pool.json ]]; then
    cat /run/omega-vcpu-pool.json | python3 -m json.tool 2>/dev/null || cat /run/omega-vcpu-pool.json
else
    warn "/run/omega-vcpu-pool.json absent (normal si premier démarrage)"
fi

step "Simulation charge CPU forte (stress-ng dans la VM — ${STRESS_SECS}s)"
info "seuil scale-up=${HIGH_THRESHOLD}% | seuil scale-down=${LOW_THRESHOLD}% | intervalle=${SCALE_INTERVAL}s"
vm_cpu_stress "$VMID" "$STRESS_SECS"
STRESS_PID=${_PIDS[-1]:-0}

step "Surveillance scale-up pendant ${WATCH_SECS}s"
t0=$SECONDS
vcpus_max="$vcpus_init"
while [[ $(elapsed $t0) -lt "$WATCH_SECS" ]]; do
    vcpus_now="$(vm_runtime_vcpus "$VMID" || true)"
    [[ -n "$vcpus_now" && "${vcpus_now:-1}" -gt "$vcpus_max" ]] && vcpus_max="$vcpus_now"
    printf "\r  [%3ds] vCPUs runtime : %-3s (max observé : %s)" "$(elapsed $t0)" "${vcpus_now:-?}" "$vcpus_max"
    sleep 3
done
echo ""

kill "$STRESS_PID" 2>/dev/null || true

step "Surveillance scale-down (60s sans charge)"
info "Attente scale-down..."
t0=$SECONDS
while [[ $(elapsed $t0) -lt 60 ]]; do
    vcpus_now="$(vm_runtime_vcpus "$VMID" || true)"
    printf "\r  [%3ds] vCPUs runtime : %s" "$(elapsed $t0)" "${vcpus_now:-?}"
    sleep 5
done
echo ""

vcpus_final="$(vm_runtime_vcpus "$VMID" || true)"
info "vCPUs runtime finaux : ${vcpus_final:-?}"

step "Résultats"
info "vCPU runtime initial : $vcpus_init | max observé sous charge : $vcpus_max | final : ${vcpus_final:-?}"

[[ "$vcpus_max" -gt "${vcpus_init:-1}" ]] || fail "aucun scale-up runtime observé sous charge (l'UI Proxmox restera à ${vcpus_init} CPU)"

step "Logs agent (scale events)"
grep -i "vcpu\|scale\|ajust" "$LOG_AGENT" | head -20 || warn "aucun log vCPU trouvé"

pass "vCPU élastique OK — scale-up de ${vcpus_init} à ${vcpus_max} vCPUs détecté"
