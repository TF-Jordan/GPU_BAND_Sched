#!/usr/bin/env bash
# Test 32 — Proxy GPU applicatif Omega

source "$(dirname "$0")/lib.sh"

GPU_NODES="${OMEGA_GPU_NODES:-${OMEGA_GPU_PRIMARY_NODE:-$CONTROLLER_NODE}}"
GPU_PRIMARY="${OMEGA_GPU_PRIMARY_NODE:-${GPU_NODES%%,*}}"
PROXY_URL="${OMEGA_GPU_PROXY_URL:-http://${GPU_PRIMARY}:9400}"
PROXY_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
if [[ -z "$PROXY_TOKEN" && -n "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}" && -r "${OMEGA_GPU_PROXY_API_TOKEN_FILE}" ]]; then
    PROXY_TOKEN="$(tr -d ' \n\r\t' < "${OMEGA_GPU_PROXY_API_TOKEN_FILE}")"
fi
VMID="${1:-$TEST_VMID}"
TEST_VM_COUNT="${OMEGA_GPU_PROXY_TEST_VM_COUNT:-3}"
BUDGET_MIB="${OMEGA_GPU_PROXY_TEST_BUDGET_MIB:-128}"
JOB_VRAM_MIB="${OMEGA_GPU_PROXY_TEST_JOB_VRAM_MIB:-64}"
MATMUL_N="${OMEGA_GPU_PROXY_TEST_MATMUL_N:-512}"
REQUIRE_CUDA="${OMEGA_GPU_PROXY_REQUIRE_CUDA:-1}"
REQUIRE_PARALLEL="${OMEGA_GPU_PROXY_REQUIRE_PARALLEL:-1}"
EXPECTED_TOTAL_VRAM_MIB="${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}"
EXPECTED_MAX_CONCURRENT="${OMEGA_GPU_PROXY_MAX_CONCURRENT:-}"
REQUIRE_CUDA_JSON=false
[[ "$REQUIRE_CUDA" == "1" ]] && REQUIRE_CUDA_JSON=true

json_get() {
    python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
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

curl_gpu() {
    if [[ -n "$PROXY_TOKEN" ]]; then
        curl -H "Authorization: Bearer ${PROXY_TOKEN}" "$@"
    else
        curl "$@"
    fi
}

select_vmids() {
    local raw="${OMEGA_TEST_VMIDS:-$VMID}" id count=0
    declare -A seen=()
    for id in ${raw//,/ }; do
        [[ "$id" =~ ^[0-9]+$ ]] || continue
        [[ -n "${seen[$id]:-}" ]] && continue
        seen[$id]=1
        printf '%s\n' "$id"
        count=$((count + 1))
        [[ "$count" -ge "$TEST_VM_COUNT" ]] && return
    done
}

header "Test 32 — Proxy GPU applicatif"

step "Vérification proxy"
info "proxy=$PROXY_URL gpu_nodes=${GPU_NODES:-inconnu} primary=${GPU_PRIMARY:-inconnu} token=$([[ -n "$PROXY_TOKEN" ]] && echo configuré || echo absent)"
for gpu_node in ${GPU_NODES//,/ }; do
    [[ -n "$gpu_node" ]] || continue
    node_url="http://${gpu_node}:9400"
    if curl_gpu -fsS "$node_url/gpu/status" >/tmp/omega-gpu-status-"$gpu_node".json 2>/dev/null; then
        node_total="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("total_vram_mib",0))' "/tmp/omega-gpu-status-$gpu_node.json" 2>/dev/null || echo 0)"
        node_free="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("free_vram_mib",0))' "/tmp/omega-gpu-status-$gpu_node.json" 2>/dev/null || echo 0)"
        info "proxy GPU node ${gpu_node}: OK vram=${node_free}/${node_total}MiB"
    else
        fail "proxy GPU indisponible sur nœud GPU déclaré: ${node_url}"
    fi
done
if ! curl -fsS "$PROXY_URL/health" >/dev/null 2>&1; then
    if [[ -x "${OMEGA_BIN_DIR:-}/omega-gpu-proxy" ]]; then
        info "proxy GPU absent — démarrage local via ${OMEGA_BIN_DIR}/omega-gpu-proxy"
        ARGS=(
            --listen "${OMEGA_GPU_PROXY_LISTEN:-127.0.0.1:9400}"
            --node-id "$(local_pve_node 2>/dev/null || hostname -s)"
        )
        if [[ -n "${OMEGA_GPU_PROXY_BACKEND_COMMAND:-}" ]]; then
            ARGS+=(--backend-command "$OMEGA_GPU_PROXY_BACKEND_COMMAND")
        fi
        if [[ -n "${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS:-}" ]]; then
            ARGS+=(--backend-timeout-secs "$OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS")
        fi
        if [[ -n "${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-}" ]]; then
            ARGS+=(--total-vram-mib "$OMEGA_GPU_PROXY_TOTAL_VRAM_MIB")
        fi
        if [[ -n "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}" ]]; then
            ARGS+=(--api-token-file "$OMEGA_GPU_PROXY_API_TOKEN_FILE")
        elif [[ -n "$PROXY_TOKEN" ]]; then
            ARGS+=(--api-token "$PROXY_TOKEN")
        fi
        "${OMEGA_BIN_DIR}/omega-gpu-proxy" "${ARGS[@]}" >/tmp/omega-gpu-proxy-test.log 2>&1 &
        _PIDS+=($!)
        for _ in $(seq 1 20); do
            curl -fsS "$PROXY_URL/health" >/dev/null 2>&1 && break
            sleep 1
        done
    fi
fi
curl -fsS "$PROXY_URL/health" >/dev/null || fail "proxy GPU inaccessible: $PROXY_URL"
STATUS_JSON="$(curl_gpu -fsS "$PROXY_URL/gpu/status")"
printf '%s' "$STATUS_JSON" | python3 -m json.tool

status_max_concurrent="$(printf '%s' "$STATUS_JSON" | json_path max_concurrent_jobs)"
status_total_vram="$(printf '%s' "$STATUS_JSON" | json_path total_vram_mib)"
if [[ -n "$EXPECTED_MAX_CONCURRENT" && "$status_max_concurrent" != "$EXPECTED_MAX_CONCURRENT" ]]; then
    fail "max_concurrent_jobs=$status_max_concurrent mais attendu=$EXPECTED_MAX_CONCURRENT"
fi
if [[ "$EXPECTED_TOTAL_VRAM_MIB" =~ ^[0-9]+$ && "$EXPECTED_TOTAL_VRAM_MIB" -gt 0 && "$status_total_vram" != "$EXPECTED_TOTAL_VRAM_MIB" ]]; then
    fail "total_vram_mib=$status_total_vram mais attendu=$EXPECTED_TOTAL_VRAM_MIB"
fi
if [[ "$REQUIRE_CUDA" == "1" ]]; then
    info "CUDA obligatoire pour ce test (OMEGA_GPU_PROXY_REQUIRE_CUDA=1)"
fi

mapfile -t VMIDS < <(select_vmids)
[[ "${#VMIDS[@]}" -gt 0 ]] || fail "aucune VMID disponible pour le test GPU proxy"
info "VMs utilisées : ${VMIDS[*]}"

step "Budgets logiques par VM"
for id in "${VMIDS[@]}"; do
    info "budget VM ${id}: ${BUDGET_MIB} MiB"
    curl_gpu -fsS -X POST "$PROXY_URL/v1/vm/$id/budget" \
        -H "Content-Type: application/json" \
        -d "{\"vram_budget_mib\":$BUDGET_MIB}" >/tmp/omega-gpu-budget-"$id".json
done
curl_gpu -fsS "$PROXY_URL/gpu/status" | python3 -m json.tool

step "Vérification refus dépassement budget"
OVER_BUDGET=$((BUDGET_MIB + 1))
if curl_gpu -fsS -X POST "$PROXY_URL/v1/jobs" \
    -H "Content-Type: application/json" \
    -d "{\"vm_id\":${VMIDS[0]},\"kind\":\"matrix_multiply\",\"vram_mib\":$OVER_BUDGET,\"payload\":{\"n\":8,\"seed\":${VMIDS[0]},\"require_cuda\":$REQUIRE_CUDA_JSON}}" \
    >/tmp/omega-gpu-over-budget.json 2>/tmp/omega-gpu-over-budget.err
then
    cat /tmp/omega-gpu-over-budget.json
    fail "le proxy a accepté un job au-dessus du budget VM"
else
    pass "job au-dessus du budget refusé"
fi

step "Soumission multi-VM"
JOB_IDS=()
for id in "${VMIDS[@]}"; do
    JOB_JSON="$(curl_gpu -fsS -X POST "$PROXY_URL/v1/jobs" \
        -H "Content-Type: application/json" \
        -d "{\"vm_id\":$id,\"kind\":\"matrix_multiply\",\"vram_mib\":$JOB_VRAM_MIB,\"payload\":{\"n\":$MATMUL_N,\"seed\":$id,\"require_cuda\":$REQUIRE_CUDA_JSON}}")"
    JOB_ID="$(printf '%s' "$JOB_JSON" | json_get job_id)"
    JOB_IDS+=("$JOB_ID")
    info "VM ${id}: job_id=${JOB_ID}"
done

step "État file d'attente après soumission"
QUEUE_JSON="$(curl_gpu -fsS "$PROXY_URL/gpu/status")"
printf '%s' "$QUEUE_JSON" | python3 -m json.tool
running_after_submit="$(printf '%s' "$QUEUE_JSON" | json_path running_jobs)"
if [[ "$REQUIRE_PARALLEL" == "1" && "${#JOB_IDS[@]}" -gt 1 && "$status_max_concurrent" -gt 1 && "$running_after_submit" -lt 2 ]]; then
    fail "concurrence non observée: running_jobs=$running_after_submit malgré max_concurrent_jobs=$status_max_concurrent"
fi

step "Attente résultats multi-VM"
deadline=$((SECONDS + ${OMEGA_GPU_PROXY_TEST_TIMEOUT_SECS:-180}))
while [[ "$SECONDS" -lt "$deadline" ]]; do
    done_count=0
    for job_id in "${JOB_IDS[@]}"; do
        STATE_JSON="$(curl_gpu -fsS "$PROXY_URL/v1/jobs/$job_id")"
        STATE="$(printf '%s' "$STATE_JSON" | json_get state)"
        case "$STATE" in
            succeeded)
                done_count=$((done_count + 1))
                ;;
            failed|cancelled)
                printf '%s' "$STATE_JSON" | python3 -m json.tool
                fail "job GPU proxy ${job_id} terminé en état $STATE"
                ;;
        esac
    done
    printf "\r  jobs terminés: %s/%s" "$done_count" "${#JOB_IDS[@]}"
    if [[ "$done_count" -eq "${#JOB_IDS[@]}" ]]; then
        echo
        break
    fi
    sleep 1
done

[[ "${done_count:-0}" -eq "${#JOB_IDS[@]}" ]] || fail "timeout attente jobs GPU proxy"

step "Résultats"
for job_id in "${JOB_IDS[@]}"; do
    RESULT_JSON="$(curl_gpu -fsS "$PROXY_URL/v1/jobs/$job_id")"
    printf '%s' "$RESULT_JSON" | python3 -m json.tool
    if [[ "$REQUIRE_CUDA" == "1" ]]; then
        backend="$(printf '%s' "$RESULT_JSON" | json_path result.output.backend)"
        device="$(printf '%s' "$RESULT_JSON" | json_path result.output.device)"
        cuda_available="$(printf '%s' "$RESULT_JSON" | json_path result.output.cuda_available)"
        [[ "$backend" == "torch" ]] || fail "job $job_id backend=$backend, attendu=torch"
        [[ "$device" == "cuda" ]] || fail "job $job_id device=$device, attendu=cuda"
        [[ "$cuda_available" == "True" || "$cuda_available" == "true" ]] || fail "job $job_id cuda_available=$cuda_available"
    fi
done

step "Métriques finales"
curl_gpu -fsS "$PROXY_URL/metrics" || true
if [[ "$REQUIRE_CUDA" == "1" ]]; then
    pass "proxy GPU applicatif multi-VM CUDA OK (${#JOB_IDS[@]} jobs)"
else
    pass "proxy GPU applicatif multi-VM OK (${#JOB_IDS[@]} jobs)"
fi
