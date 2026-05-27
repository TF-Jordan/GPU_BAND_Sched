#!/usr/bin/env bash
# Test 27 — GPU réel : render node, daemon GPU, passthrough/VM si disponible.
# Usage : ./27-gpu-real-render.sh [vmid]

set -euo pipefail
source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"

header "Test 27 — GPU réel / rendu minimal (VM $VMID)"

GPU_NODES_CFG="${OMEGA_GPU_NODES:-${OMEGA_GPU_PRIMARY_NODE:-}}"
GPU_PRIMARY="${OMEGA_GPU_PRIMARY_NODE:-${GPU_NODES_CFG%%,*}}"
PROXY_URL="${OMEGA_GPU_PROXY_URL:-}"
PROXY_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
REQUIRE_CUDA="${OMEGA_GPU_PROXY_REQUIRE_CUDA:-1}"
[[ -z "$PROXY_TOKEN" && -n "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}" && -r "${OMEGA_GPU_PROXY_API_TOKEN_FILE}" ]] && \
    PROXY_TOKEN="$(tr -d ' \n\r\t' < "${OMEGA_GPU_PROXY_API_TOKEN_FILE}")"

json_path() {
    python3 -c '
import json, sys
data = json.load(sys.stdin)
cur = data
for part in sys.argv[1].split("."):
    if not part:
        continue
    cur = cur[part]
print(cur)
' "$1"
}

curl_gpu_proxy() {
    if [[ -n "$PROXY_TOKEN" ]]; then
        curl -H "Authorization: Bearer ${PROXY_TOKEN}" "$@"
    else
        curl "$@"
    fi
}

guest_exec() {
    local cmd="$1"
    local started pid status
    started=$(qm guest exec "$VMID" -- bash -lc "$cmd" 2>/dev/null || true)
    pid=$(printf '%s' "$started" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("pid",""))' 2>/dev/null || true)
    [[ -n "$pid" ]] || return 1
    for _ in $(seq 1 30); do
        status=$(qm guest exec-status "$VMID" "$pid" 2>/dev/null || true)
        if printf '%s' "$status" | python3 -c 'import sys,json; sys.exit(0 if json.load(sys.stdin).get("exited") else 1)' 2>/dev/null; then
            printf '%s' "$status" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("out-data","") + d.get("err-data",""), end="")' 2>/dev/null
            return 0
        fi
        sleep 1
    done
    return 1
}

gpu_nodes=()
render_nodes_found=false
for node in "${OMEGA_NODES_ARR[@]}"; do
    step "Nœud $node — inventaire GPU"
    pci=$(ssh_run "$node" "ls /sys/bus/pci/devices/*/class 2>/dev/null | xargs grep -l '^0x03' 2>/dev/null | head -5" || true)
    renders=$(ssh_run "$node" "ls /dev/dri/renderD* 2>/dev/null || true")
    nvidia=$(ssh_run "$node" "command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L 2>/dev/null || true" || true)
    if [[ -n "$pci$renders" ]]; then
        gpu_nodes+=("$node")
        info "PCI GPU:"
        printf '%s\n' "$pci" | sed 's/^/    /'
        info "Render nodes:"
        printf '%s\n' "${renders:-<aucun>}" | sed 's/^/    /'
        if [[ -n "$nvidia" ]]; then
            info "NVIDIA:"
            printf '%s\n' "$nvidia" | sed 's/^/    /'
        fi
        [[ -n "$renders" ]] && render_nodes_found=true
    else
        warn "$node : aucun GPU/render node détecté"
    fi
done

[[ ${#gpu_nodes[@]} -gt 0 ]] || fail "aucun nœud GPU/render node dans OMEGA_NODES"

step "API daemon GPU"
daemon_gpu_ok=false
for node in "${gpu_nodes[@]}"; do
    status_json="$(curl -fsS "http://${node}:9300/control/gpu/status" 2>/dev/null || true)"
    if [[ -n "$status_json" ]] && printf '%s' "$status_json" | python3 -m json.tool >/tmp/omega-daemon-gpu-status.json 2>/dev/null; then
        sed 's/^/    /' /tmp/omega-daemon-gpu-status.json
        pass "$node : /control/gpu/status OK"
        daemon_gpu_ok=true
    else
        if $render_nodes_found; then
            warn "$node : endpoint GPU daemon indisponible alors qu'un render node existe — vérifier OMEGA_GPU_RENDER_NODE et omega-daemon"
        else
            warn "$node : endpoint GPU daemon non actif — normal pour NVIDIA/CUDA sans /dev/dri/renderD*; utiliser le proxy applicatif :9400"
        fi
    fi
done

step "Proxy GPU applicatif CUDA"
proxy_gpu_ok=false
if [[ -z "$GPU_PRIMARY" && "${#gpu_nodes[@]}" -gt 0 ]]; then
    GPU_PRIMARY="${gpu_nodes[0]}"
fi
[[ -n "$PROXY_URL" ]] || PROXY_URL="http://${GPU_PRIMARY}:9400"
info "proxy=${PROXY_URL:-inconnu} primary=${GPU_PRIMARY:-inconnu} token=$([[ -n "$PROXY_TOKEN" ]] && echo configuré || echo absent)"
if [[ -n "${PROXY_URL:-}" ]] && curl_gpu_proxy -fsS "$PROXY_URL/health" >/dev/null 2>&1; then
    proxy_status="$(curl_gpu_proxy -fsS "$PROXY_URL/gpu/status" 2>/dev/null || true)"
    if [[ -n "$proxy_status" ]] && printf '%s' "$proxy_status" | python3 -m json.tool >/tmp/omega-proxy-gpu-status.json 2>/dev/null; then
        sed 's/^/    /' /tmp/omega-proxy-gpu-status.json
        pass "proxy GPU applicatif joignable"
        proxy_gpu_ok=true
    else
        warn "proxy GPU joignable mais /gpu/status ne retourne pas du JSON valide"
    fi
else
    warn "proxy GPU applicatif indisponible sur ${PROXY_URL:-<non configuré>}"
fi

if $proxy_gpu_ok && [[ "$REQUIRE_CUDA" == "1" ]]; then
    step "Smoke CUDA via proxy applicatif"
    require_cuda_json=false
    [[ "$REQUIRE_CUDA" == "1" ]] && require_cuda_json=true
    curl_gpu_proxy -fsS -X POST "$PROXY_URL/v1/vm/$VMID/budget" \
        -H "Content-Type: application/json" \
        -d '{"vram_budget_mib":128}' >/dev/null
    job_json="$(curl_gpu_proxy -fsS -X POST "$PROXY_URL/v1/jobs" \
        -H "Content-Type: application/json" \
        -d "{\"vm_id\":$VMID,\"kind\":\"matrix_multiply\",\"vram_mib\":64,\"payload\":{\"n\":32,\"seed\":$VMID,\"require_cuda\":$require_cuda_json}}" 2>/dev/null || true)"
    job_id="$(printf '%s' "$job_json" | json_path job_id 2>/dev/null || true)"
    [[ -n "$job_id" ]] || fail "proxy GPU disponible mais impossible de soumettre un job CUDA de smoke-test"
    deadline=$((SECONDS + 60))
    while [[ "$SECONDS" -lt "$deadline" ]]; do
        result_json="$(curl_gpu_proxy -fsS "$PROXY_URL/v1/jobs/$job_id" 2>/dev/null || true)"
        state="$(printf '%s' "$result_json" | json_path state 2>/dev/null || true)"
        [[ "$state" == "succeeded" || "$state" == "failed" || "$state" == "cancelled" ]] && break
        sleep 1
    done
    printf '%s' "$result_json" | python3 -m json.tool | sed 's/^/    /'
    [[ "$state" == "succeeded" ]] || fail "job GPU proxy terminé en état ${state:-inconnu}"
    backend="$(printf '%s' "$result_json" | json_path result.output.backend 2>/dev/null || true)"
    device="$(printf '%s' "$result_json" | json_path result.output.device 2>/dev/null || true)"
    [[ "$backend" == "torch" && "$device" == "cuda" ]] || \
        fail "proxy GPU actif mais CUDA non validé: backend=${backend:-?} device=${device:-?}"
    pass "CUDA réel validé via proxy applicatif"
fi

step "VM — hostpci/render visible"
hostpci=$(qm config "$VMID" | grep '^hostpci' || true)
if [[ -n "$hostpci" ]]; then
    info "hostpci VM:"
    printf '%s\n' "$hostpci" | sed 's/^/    /'
else
    warn "VM $VMID sans hostpci configuré — placement GPU peut encore migrer/configurer plus tard"
fi

if qm guest cmd "$VMID" ping &>/dev/null; then
    guest_gpu=$(guest_exec "ls /dev/dri 2>/dev/null; command -v glxinfo >/dev/null && timeout 10 glxinfo -B 2>/dev/null | head -20 || true" || true)
    if [[ -n "$guest_gpu" ]]; then
        info "GPU visible dans l'invité:"
        printf '%s\n' "$guest_gpu" | sed 's/^/    /'
        pass "inspection GPU invité OK"
    else
        warn "aucun GPU visible dans l'invité via qemu-guest-agent"
    fi
else
    warn "qemu-guest-agent indisponible — test GPU invité ignoré"
fi

if $daemon_gpu_ok; then
    pass "GPU réel validé via daemon DRM"
elif $proxy_gpu_ok; then
    pass "GPU réel validé via proxy applicatif CUDA"
else
    fail "aucun chemin GPU Omega exploitable: daemon :9300 indisponible et proxy :9400 indisponible"
fi
