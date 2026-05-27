#!/usr/bin/env bash
# Test 34 — Placement GPU global: local, migration vers GPU, ou fallback proxy.

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
GPU_NODES="${OMEGA_GPU_NODES:-${OMEGA_GPU_PRIMARY_NODE:-}}"
PROXY_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
if [[ -z "$PROXY_TOKEN" && -n "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}" && -r "${OMEGA_GPU_PROXY_API_TOKEN_FILE}" ]]; then
    PROXY_TOKEN="$(tr -d ' \n\r\t' < "${OMEGA_GPU_PROXY_API_TOKEN_FILE}")"
fi
GPU_BUDGET_MIB="${OMEGA_GPU_PLACEMENT_BUDGET_MIB:-${OMEGA_GPU_PROXY_TEST_JOB_VRAM_MIB:-64}}"
ALLOW_FALLBACK="${OMEGA_GPU_ALLOW_NETWORK_FALLBACK:-1}"
ALLOW_MIGRATION="${OMEGA_GPU_ALLOW_MIGRATION:-1}"

curl_gpu() {
    if [[ -n "$PROXY_TOKEN" ]]; then
        curl -H "Authorization: Bearer ${PROXY_TOKEN}" "$@"
    else
        curl "$@"
    fi
}

json_path() {
    python3 -c '
import json, sys
data = json.load(sys.stdin)
cur = data
for part in sys.argv[1].split("."):
    if part == "":
        continue
    cur = cur[part]
print(cur)
' "$1"
}

discover_gpu_nodes() {
    if [[ -n "$GPU_NODES" ]]; then
        tr ',' ' ' <<<"$GPU_NODES"
        return
    fi
    local n total
    for n in "${OMEGA_NODES_ARR[@]}"; do
        total="$(curl_gpu -fsS "http://${n}:9400/gpu/status" 2>/dev/null | json_path total_vram_mib 2>/dev/null || echo 0)"
        if [[ "$total" =~ ^[0-9]+$ && "$total" -gt 0 ]]; then
            printf '%s\n' "$n"
        fi
    done
}

gpu_node_score() {
    local node="$1" status_json control_json total free mem vcpu disk
    status_json="$(curl_gpu -fsS "http://${node}:9400/gpu/status" 2>/dev/null || true)"
    [[ -n "$status_json" ]] || return 1
    total="$(printf '%s' "$status_json" | json_path total_vram_mib 2>/dev/null || echo 0)"
    free="$(printf '%s' "$status_json" | json_path free_vram_mib 2>/dev/null || echo 0)"
    [[ "$total" =~ ^[0-9]+$ && "$total" -gt 0 ]] || return 1
    [[ "$free" =~ ^[0-9]+$ && "$free" -ge "$GPU_BUDGET_MIB" ]] || return 1
    control_json="$(curl -fsS "http://${node}:9200/control/status" 2>/dev/null || true)"
    mem="$(printf '%s' "$control_json" | json_path node.mem_available_kb 2>/dev/null || echo 0)"
    vcpu="$(printf '%s' "$control_json" | json_path node.vcpu_free 2>/dev/null || echo 0)"
    disk="$(printf '%s' "$control_json" | json_path node.disk_pressure_pct 2>/dev/null || echo 100)"
    python3 - "$free" "$total" "$mem" "$vcpu" "$disk" <<'PY'
import sys
free, total, mem, vcpu, disk = map(float, sys.argv[1:])
gpu = free / total if total else 0.0
mem_ratio = min(mem / (64 * 1024 * 1024), 1.0)
vcpu_ratio = min(vcpu / 64.0, 1.0)
disk_ratio = max(0.0, 1.0 - disk / 100.0)
print(gpu * 0.45 + mem_ratio * 0.25 + vcpu_ratio * 0.15 + disk_ratio * 0.15)
PY
}

choose_gpu_node() {
    local best="" best_score="-1" n score
    while read -r n; do
        [[ -n "$n" ]] || continue
        score="$(gpu_node_score "$n" 2>/dev/null || true)"
        [[ -n "$score" ]] || continue
        if python3 - "$score" "$best_score" <<'PY'
import sys
sys.exit(0 if float(sys.argv[1]) > float(sys.argv[2]) else 1)
PY
        then
            best="$n"
            best_score="$score"
        fi
    done < <(discover_gpu_nodes)
    [[ -n "$best" ]] || return 1
    printf '%s\n' "$best"
}

submit_cuda_smoke() {
    local proxy_node="$1" proxy_url="http://${proxy_node}:9400" job_json job_id result backend device
    curl_gpu -fsS -X POST "$proxy_url/v1/vm/$VMID/budget" \
        -H "Content-Type: application/json" \
        -d "{\"vram_budget_mib\":$((GPU_BUDGET_MIB * 2))}" >/dev/null
    job_json="$(curl_gpu -fsS -X POST "$proxy_url/v1/jobs" \
        -H "Content-Type: application/json" \
        -d "{\"vm_id\":$VMID,\"kind\":\"matrix_multiply\",\"vram_mib\":$GPU_BUDGET_MIB,\"payload\":{\"n\":64,\"seed\":$VMID,\"require_cuda\":true}}")"
    job_id="$(printf '%s' "$job_json" | json_path job_id)"
    for _ in $(seq 1 120); do
        result="$(curl_gpu -fsS "$proxy_url/v1/jobs/$job_id")"
        state="$(printf '%s' "$result" | json_path state)"
        [[ "$state" == "succeeded" ]] && break
        [[ "$state" == "failed" || "$state" == "cancelled" ]] && {
            printf '%s' "$result" | python3 -m json.tool
            return 1
        }
        sleep 1
    done
    backend="$(printf '%s' "$result" | json_path result.output.backend 2>/dev/null || true)"
    device="$(printf '%s' "$result" | json_path result.output.device 2>/dev/null || true)"
    printf '%s' "$result" | python3 -m json.tool
    [[ "$backend" == "torch" && "$device" == "cuda" ]]
}

header "Test 34 — Placement GPU global"

step "Préparation VM et inventaire GPU"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
source_node="$(vm_node "$VMID")"
target_node="$(choose_gpu_node)" || fail "aucun nœud GPU avec proxy CUDA et budget ${GPU_BUDGET_MIB} MiB"
info "VM $VMID actuellement sur $source_node"
info "meilleur nœud GPU calculé : $target_node"

step "Décision de placement"
if [[ "$source_node" == "$target_node" ]]; then
    pass "VM déjà placée sur un nœud GPU sain"
    submit_cuda_smoke "$target_node" || fail "smoke CUDA proxy échoué sur $target_node"
    pass "placement GPU local + CUDA OK"
    exit 0
fi

if [[ "$ALLOW_MIGRATION" == "1" ]]; then
    source_pve="$(_ip_to_pve_node "$source_node")"
    target_pve="$(_ip_to_pve_node "$target_node")"
    info "migration demandée : VM $VMID $source_pve -> $target_pve"
    ssh_run "$source_node" "qm set $VMID --ide2 none 2>/dev/null || true"
    if ssh_run "$source_node" "qm migrate $VMID $target_pve --online" >/tmp/omega-gpu-placement-migrate.log 2>&1; then
        sleep 5
        final_node="$(vm_node "$VMID")"
        info "VM $VMID après migration : $final_node"
        [[ "$final_node" == "$target_node" ]] || fail "migration lancée mais VM sur $final_node au lieu de $target_node"
        submit_cuda_smoke "$target_node" || fail "smoke CUDA proxy échoué après migration"
        pass "placement GPU par migration + CUDA OK"
        exit 0
    fi
    warn "migration GPU échouée; log:"
    sed -n '1,80p' /tmp/omega-gpu-placement-migrate.log || true
fi

[[ "$ALLOW_FALLBACK" == "1" ]] || fail "VM hors nœud GPU et fallback réseau désactivé"
step "Fallback proxy GPU réseau"
submit_cuda_smoke "$target_node" || fail "fallback proxy CUDA échoué"
pass "fallback GPU réseau OK — VM hors nœud GPU servie par proxy CUDA"
