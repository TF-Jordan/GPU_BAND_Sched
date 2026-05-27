#!/usr/bin/env bash
# Test 35 — Fallback GPU réseau: une VM peut consommer CUDA via proxy sans
# passthrough ni migration obligatoire.

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
GPU_NODES="${OMEGA_GPU_NODES:-${OMEGA_GPU_PRIMARY_NODE:-$CONTROLLER_NODE}}"
GPU_PRIMARY="${OMEGA_GPU_PRIMARY_NODE:-${GPU_NODES%%,*}}"
PROXY_URL="${OMEGA_GPU_PROXY_URL:-http://${GPU_PRIMARY}:9400}"
PROXY_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
if [[ -z "$PROXY_TOKEN" && -n "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}" && -r "${OMEGA_GPU_PROXY_API_TOKEN_FILE}" ]]; then
    PROXY_TOKEN="$(tr -d ' \n\r\t' < "${OMEGA_GPU_PROXY_API_TOKEN_FILE}")"
fi
BUDGET_MIB="${OMEGA_GPU_FALLBACK_BUDGET_MIB:-128}"
JOB_VRAM_MIB="${OMEGA_GPU_FALLBACK_JOB_VRAM_MIB:-64}"

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
    cur = cur[part]
print(cur)
' "$1"
}

header "Test 35 — Fallback GPU réseau"

step "Préparation VM et proxy"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
source_node="$(vm_node "$VMID")"
info "VM $VMID sur $source_node"
info "proxy=$PROXY_URL primary=$GPU_PRIMARY token=$([[ -n "$PROXY_TOKEN" ]] && echo configuré || echo absent)"
curl_gpu -fsS "$PROXY_URL/health" >/dev/null || fail "proxy GPU inaccessible: $PROXY_URL"
STATUS_JSON="$(curl_gpu -fsS "$PROXY_URL/gpu/status")"
printf '%s' "$STATUS_JSON" | python3 -m json.tool
total_vram="$(printf '%s' "$STATUS_JSON" | json_path total_vram_mib)"
[[ "$total_vram" =~ ^[0-9]+$ && "$total_vram" -gt 0 ]] || fail "proxy sans VRAM CUDA détectée"

step "Budget logique et job CUDA distant"
curl_gpu -fsS -X POST "$PROXY_URL/v1/vm/$VMID/budget" \
    -H "Content-Type: application/json" \
    -d "{\"vram_budget_mib\":$BUDGET_MIB}" >/dev/null
JOB_JSON="$(curl_gpu -fsS -X POST "$PROXY_URL/v1/jobs" \
    -H "Content-Type: application/json" \
    -d "{\"vm_id\":$VMID,\"kind\":\"matrix_multiply\",\"vram_mib\":$JOB_VRAM_MIB,\"payload\":{\"n\":128,\"seed\":$VMID,\"require_cuda\":true}}")"
JOB_ID="$(printf '%s' "$JOB_JSON" | json_path job_id)"
info "job CUDA fallback=$JOB_ID"

deadline=$((SECONDS + ${OMEGA_GPU_FALLBACK_TIMEOUT_SECS:-180}))
while [[ "$SECONDS" -lt "$deadline" ]]; do
    RESULT_JSON="$(curl_gpu -fsS "$PROXY_URL/v1/jobs/$JOB_ID")"
    STATE="$(printf '%s' "$RESULT_JSON" | json_path state)"
    case "$STATE" in
        succeeded) break ;;
        failed|cancelled)
            printf '%s' "$RESULT_JSON" | python3 -m json.tool
            fail "job GPU fallback terminé en état $STATE"
            ;;
    esac
    sleep 1
done
[[ "${STATE:-}" == "succeeded" ]] || fail "timeout job GPU fallback"

printf '%s' "$RESULT_JSON" | python3 -m json.tool
backend="$(printf '%s' "$RESULT_JSON" | json_path result.output.backend 2>/dev/null || true)"
device="$(printf '%s' "$RESULT_JSON" | json_path result.output.device 2>/dev/null || true)"
gpu_name="$(printf '%s' "$RESULT_JSON" | json_path result.output.gpu_name 2>/dev/null || true)"
[[ "$backend" == "torch" && "$device" == "cuda" ]] || fail "fallback GPU non CUDA: backend=$backend device=$device"
pass "fallback GPU réseau OK — backend=torch device=cuda gpu=${gpu_name:-inconnu}"
