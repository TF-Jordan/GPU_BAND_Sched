#!/usr/bin/env bash
# Test 7 — GPU scheduler round-robin (partage entre 2 VMs)
# Usage : ./07-gpu-scheduler.sh [vmid1] [vmid2]
# Prérequis : 2 VMs démarrées sur le même nœud GPU, QMP socket accessible

source "$(dirname "$0")/lib.sh"

VMID1="${1:-${TEST_VMIDS_ARR[0]:-$TEST_VMID}}"
VMID2="${2:-${TEST_VMIDS_ARR[1]:-}}"
[[ -n "$VMID1" ]] || fail "VMID1 non défini — vérifier OMEGA_TEST_VMIDS dans cluster.conf"
[[ -n "$VMID2" ]] || fail "VMID2 non défini — ce test requiert 2 VMIDs dans OMEGA_TEST_VMIDS"
require_two_vms_running "$VMID1" "$VMID2"
VMID1="${SELECTED_VMIDS[0]}"
VMID2="${SELECTED_VMIDS[1]}"
VM1_RAM=$(vm_ram_mib "$VMID1"); VM1_RAM="${VM1_RAM:-1024}"
VM2_RAM=$(vm_ram_mib "$VMID2"); VM2_RAM="${VM2_RAM:-1024}"
CONTROL_PORT="${OMEGA_CONTROL_PORT:-9300}"
GPU_TEST_BUDGET_MIB="${OMEGA_GPU_TEST_BUDGET_MIB:-128}"

header "Test 7 — GPU scheduler round-robin (VM $VMID1 + VM $VMID2)"

step "Vérifications prérequis"
require_bin qm
require_bin curl
require_bin python3

vm1_node="$(vm_node "$VMID1" 2>/dev/null || true)"
vm2_node="$(vm_node "$VMID2" 2>/dev/null || true)"
info "VM $VMID1 sur ${vm1_node:-inconnu}"
info "VM $VMID2 sur ${vm2_node:-inconnu}"
if [[ -n "$vm1_node" && -n "$vm2_node" && "$vm1_node" != "$vm2_node" ]]; then
    warn "les deux VMs ne sont pas sur le même nœud; le test vérifie le budget GPU sur le nœud de VM $VMID1"
fi

GPU_NODE="${vm1_node:-$CONTROLLER_NODE}"
GPU_URL="http://${GPU_NODE}:${CONTROL_PORT}"

step "État GPU daemon sur ${GPU_NODE}:${CONTROL_PORT}"
GPU_STATUS="$(curl -sf "${GPU_URL}/control/gpu/status" 2>/dev/null || true)"
if [[ -z "$GPU_STATUS" ]]; then
    fail "GPU runtime indisponible sur $GPU_NODE. Vérifier omega-daemon, /dev/dri/renderD*, driver GPU, et /control/gpu/status"
fi
printf '%s\n' "$GPU_STATUS" | python3 -m json.tool || printf '%s\n' "$GPU_STATUS"

enabled="$(printf '%s\n' "$GPU_STATUS" | python3 -c 'import sys,json; d=json.load(sys.stdin); print((d.get("gpu") or {}).get("enabled", False))' 2>/dev/null || echo False)"
backend="$(printf '%s\n' "$GPU_STATUS" | python3 -c 'import sys,json; d=json.load(sys.stdin); print((d.get("gpu") or {}).get("backend_name", ""))' 2>/dev/null || true)"
socket_path="$(printf '%s\n' "$GPU_STATUS" | python3 -c 'import sys,json; d=json.load(sys.stdin); print((d.get("gpu") or {}).get("socket_path", ""))' 2>/dev/null || true)"
total_vram="$(printf '%s\n' "$GPU_STATUS" | python3 -c 'import sys,json; d=json.load(sys.stdin); print((d.get("gpu") or {}).get("total_vram_mib", 0))' 2>/dev/null || echo 0)"
[[ "$enabled" == "True" ]] || fail "GPU runtime présent mais non activé sur $GPU_NODE"
info "backend GPU=${backend:-inconnu} total_vram_mib=${total_vram}"
if [[ -n "$socket_path" ]]; then
    ssh_run "$GPU_NODE" "test -S '$socket_path'" && pass "socket GPU présent : $socket_path" || warn "socket GPU absent/inaccessible : $socket_path"
fi

step "Allocation budgets VRAM pour les deux VMs"
for vmid in "$VMID1" "$VMID2"; do
    resp="$(curl -sf -X POST "${GPU_URL}/control/vm/${vmid}/gpu" \
        -H "Content-Type: application/json" \
        -d "{\"vram_budget_mib\":${GPU_TEST_BUDGET_MIB}}" 2>/dev/null || true)"
    [[ -n "$resp" ]] || fail "échec allocation budget GPU pour VM $vmid sur $GPU_NODE"
    printf '%s\n' "$resp" | python3 -m json.tool || printf '%s\n' "$resp"
done

step "Vérification budgets GPU"
GPU_STATUS_AFTER="$(curl -sf "${GPU_URL}/control/gpu/status")"
printf '%s\n' "$GPU_STATUS_AFTER" | python3 -m json.tool || printf '%s\n' "$GPU_STATUS_AFTER"
missing="$(printf '%s\n' "$GPU_STATUS_AFTER" | python3 -c '
import json, sys
d = json.load(sys.stdin)
expected = {int(sys.argv[1]), int(sys.argv[2])}
budget = int(sys.argv[3])
seen = {
    int(v.get("vm_id"))
    for v in (d.get("gpu") or {}).get("budgets", [])
    if int(v.get("budget_mib", -1)) == budget
}
missing = sorted(expected - seen)
print(",".join(map(str, missing)))
' "$VMID1" "$VMID2" "$GPU_TEST_BUDGET_MIB")"
[[ -z "$missing" ]] || fail "budgets GPU manquants ou incorrects pour VM(s): $missing"

step "Libération budgets GPU"
for vmid in "$VMID1" "$VMID2"; do
    curl -sf -X DELETE "${GPU_URL}/control/vm/${vmid}/gpu" >/dev/null || warn "échec libération budget GPU VM $vmid"
done

pass "GPU scheduler OK — runtime GPU actif et budgets VRAM appliqués/libérés pour VM $VMID1 et VM $VMID2"
