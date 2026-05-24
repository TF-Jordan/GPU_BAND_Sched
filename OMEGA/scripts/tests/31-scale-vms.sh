#!/usr/bin/env bash
# Test 31 — Scalabilite VMs physiques.
# Valide un grand nombre de VMs deja creees, puis les demarre par paliers/lots
# et surveille Proxmox + Omega pendant une fenetre de soak.
#
# Usage:
#   ./31-scale-vms.sh [vmids_csv] [target_count] [batch_size] [soak_secs]
#
# Exemples:
#   ./31-scale-vms.sh 9001,9002,9003 3 2 300
#   OMEGA_SCALE_VMIDS="$(seq -s, 9001 9500)" ./31-scale-vms.sh "" 500 20 3600
#   OMEGA_SCALE_STEPS=50,100,150,200 ./31-scale-vms.sh "" 200 25 600
#
# Nettoyage:
#   OMEGA_SCALE_STOP_AFTER=1      arrete les VMs selectionnees a la fin
#   OMEGA_SCALE_DESTROY_AFTER=1   detruit les VMs selectionnees a la fin
#   OMEGA_SCALE_CLEANUP_SCOPE=all nettoie toutes les VMs candidates, pas seulement celles demarrees

set -euo pipefail
source "$(dirname "$0")/lib.sh"

VMIDS_CSV="${1:-${OMEGA_SCALE_VMIDS:-${OMEGA_TEST_VMIDS:-$TEST_VMID}}}"
TARGET_COUNT="${2:-${OMEGA_SCALE_TARGET:-500}}"
BATCH_SIZE="${3:-${OMEGA_SCALE_BATCH_SIZE:-20}}"
SOAK_SECS="${4:-${OMEGA_SCALE_SOAK_SECS:-${OMEGA_SCALE_TIMEOUT_SECS:-1800}}}"
POLL_SECS="${OMEGA_SCALE_POLL_SECS:-30}"
ALLOW_START="${OMEGA_SCALE_START:-1}"
STOP_AFTER="${OMEGA_SCALE_STOP_AFTER:-0}"
DESTROY_AFTER="${OMEGA_SCALE_DESTROY_AFTER:-0}"
CLEANUP_SCOPE="${OMEGA_SCALE_CLEANUP_SCOPE:-started}"
REQUIRE_TARGET="${OMEGA_SCALE_REQUIRE_TARGET:-1}"
STEPS_CSV="${OMEGA_SCALE_STEPS:-}"

header "Test 31 — Scalabilite cluster (${TARGET_COUNT} VMs cible)"

require_cluster
require_bin python3

[[ "$TARGET_COUNT" =~ ^[0-9]+$ && "$TARGET_COUNT" -gt 0 ]] || fail "TARGET_COUNT invalide: $TARGET_COUNT"
[[ "$BATCH_SIZE" =~ ^[0-9]+$ && "$BATCH_SIZE" -gt 0 ]] || fail "BATCH_SIZE invalide: $BATCH_SIZE"
[[ "$SOAK_SECS" =~ ^[0-9]+$ && "$SOAK_SECS" -ge 0 ]] || fail "SOAK_SECS invalide: $SOAK_SECS"
[[ "$DESTROY_AFTER" != "1" || "$STOP_AFTER" != "1" ]] || warn "DESTROY_AFTER=1 rend STOP_AFTER redondant"
[[ "$CLEANUP_SCOPE" == "started" || "$CLEANUP_SCOPE" == "all" ]] || fail "OMEGA_SCALE_CLEANUP_SCOPE invalide: $CLEANUP_SCOPE"

if [[ -z "$VMIDS_CSV" ]]; then
    fail "aucune liste VMID fournie; utiliser OMEGA_SCALE_VMIDS ou OMEGA_TEST_VMIDS"
fi

IFS=',' read -ra REQUESTED_VMIDS <<< "$VMIDS_CSV"
declare -a selected_vmids=()
declare -a started_vmids=()
declare -A _STARTED_SEEN=()

cleanup_scale_vms() {
    local rc=$?
    local cleanup_vmids=()
    local vmid

    if [[ "$DESTROY_AFTER" != "1" && "$STOP_AFTER" != "1" ]]; then
        exit "$rc"
    fi

    if [[ "$CLEANUP_SCOPE" == "all" ]]; then
        cleanup_vmids=("${selected_vmids[@]}")
    else
        cleanup_vmids=("${started_vmids[@]}")
    fi

    [[ "${#cleanup_vmids[@]}" -gt 0 ]] || exit "$rc"

    echo ""
    step "Nettoyage VMs scale (scope=${CLEANUP_SCOPE}, destroy=${DESTROY_AFTER}, stop=${STOP_AFTER})"
    for vmid in "${cleanup_vmids[@]}"; do
        [[ -n "$vmid" ]] || continue
        if [[ "$DESTROY_AFTER" == "1" ]]; then
            qm stop "$vmid" >/dev/null 2>&1 || true
            qm destroy "$vmid" --purge >/dev/null 2>&1 || warn "VM $vmid: destruction impossible"
        elif [[ "$STOP_AFTER" == "1" ]]; then
            qm stop "$vmid" >/dev/null 2>&1 || warn "VM $vmid: arret impossible"
        fi
    done
    exit "$rc"
}
trap cleanup_scale_vms EXIT INT TERM

mark_started() {
    local vmid="$1"
    if [[ -z "${_STARTED_SEEN[$vmid]:-}" ]]; then
        started_vmids+=("$vmid")
        _STARTED_SEEN[$vmid]=1
    fi
}

step "Inventaire des VMs candidates"
existing_vmids=()
missing=0
for vmid in "${REQUESTED_VMIDS[@]}"; do
    [[ -n "$vmid" ]] || continue
    if qm config "$vmid" >/dev/null 2>&1; then
        existing_vmids+=("$vmid")
    else
        warn "VM $vmid absente du cluster"
        ((missing++)) || true
    fi
done

if [[ "${#existing_vmids[@]}" -lt "$TARGET_COUNT" ]]; then
    msg="VMs disponibles=${#existing_vmids[@]}, cible=${TARGET_COUNT}, absentes=${missing}"
    if [[ "$REQUIRE_TARGET" == "1" ]]; then
        fail "$msg — creer les VMs manquantes avant le test de scalabilite"
    fi
    warn "$msg — poursuite avec ${#existing_vmids[@]} VM(s)"
fi

selected_vmids=("${existing_vmids[@]:0:$TARGET_COUNT}")
[[ "${#selected_vmids[@]}" -gt 0 ]] || fail "aucune VM candidate utilisable"
info "VMs retenues: ${#selected_vmids[@]} / $TARGET_COUNT"

if [[ -n "$STEPS_CSV" ]]; then
    IFS=',' read -ra SCALE_STEPS <<< "$STEPS_CSV"
else
    SCALE_STEPS=()
    step_size=50
    while [[ "$step_size" -lt "$TARGET_COUNT" ]]; do
        SCALE_STEPS+=("$step_size")
        step_size=$((step_size + 50))
    done
    SCALE_STEPS+=("$TARGET_COUNT")
fi

valid_steps=()
for s in "${SCALE_STEPS[@]}"; do
    [[ "$s" =~ ^[0-9]+$ && "$s" -gt 0 ]] || fail "palier invalide: $s"
    [[ "$s" -le "$TARGET_COUNT" ]] || fail "palier $s > target $TARGET_COUNT"
    if [[ "${#valid_steps[@]}" -eq 0 || "$s" -gt "${valid_steps[-1]}" ]]; then
        valid_steps+=("$s")
    else
        fail "les paliers doivent etre strictement croissants: ${SCALE_STEPS[*]}"
    fi
done
SCALE_STEPS=("${valid_steps[@]}")
info "Paliers scale: ${SCALE_STEPS[*]} (batch=${BATCH_SIZE}, soak=${SOAK_SECS}s par palier)"

step "Conformite statique rapide"
bad_conformity=0
for vmid in "${selected_vmids[@]}"; do
    cfg="$(qm config "$vmid" 2>/dev/null || true)"
    if [[ "$cfg" != *$'\nhotplug:'* || "$cfg" != *"hotplug:"*"cpu"* ]]; then
        warn "VM $vmid: hotplug CPU absent"
        ((bad_conformity++)) || true
    fi
    if ! awk -F': ' '$1=="vcpus" && $2>=1 {ok=1} END{exit ok?0:1}' <<< "$cfg"; then
        warn "VM $vmid: champ vcpus absent ou invalide"
        ((bad_conformity++)) || true
    fi
    if ! qm showcmd "$vmid" --pretty 2>/dev/null | grep -q "maxcpus="; then
        warn "VM $vmid: -smp sans maxcpus"
        ((bad_conformity++)) || true
    fi
done
[[ "$bad_conformity" -eq 0 ]] || fail "$bad_conformity anomalie(s) de conformite; lancer d'abord le test 30"
pass "Conformite rapide OK"

start_vm_if_needed() {
    local vmid="$1"
    local status
    status="$(_cluster_vm_status "$vmid")"
    if [[ "$status" == "running" ]]; then
        return 0
    fi
    [[ "$ALLOW_START" == "1" ]] || return 1
    _try_start_vm "$vmid"
}

count_running_selected() {
    local running=0 vmid status
    for vmid in "${selected_vmids[@]}"; do
        status="$(_cluster_vm_status "$vmid")"
        [[ "$status" == "running" ]] && ((running++)) || true
    done
    echo "$running"
}

failures=0
max_controller_latency_ms=0

count_running_limit() {
    local limit="$1"
    local running=0 idx vmid status
    for ((idx=0; idx<limit; idx++)); do
        vmid="${selected_vmids[$idx]}"
        status="$(_cluster_vm_status "$vmid")"
        [[ "$status" == "running" ]] && ((running++)) || true
    done
    echo "$running"
}

start_until_count() {
    local target="$1"
    local idx vmid
    local failed_start=0
    local batch=()

    step "Demarrage jusqu'au palier ${target} VMs (batch=${BATCH_SIZE})"
    for ((idx=0; idx<target; idx++)); do
        vmid="${selected_vmids[$idx]}"
        [[ "$(_cluster_vm_status "$vmid")" == "running" ]] && { mark_started "$vmid"; continue; }
        batch+=("$vmid")
        if [[ "${#batch[@]}" -lt "$BATCH_SIZE" ]]; then
            continue
        fi

        for bvmid in "${batch[@]}"; do
            start_vm_if_needed "$bvmid" &
            _PIDS+=($!)
        done
        wait || true
        sleep 2

        for bvmid in "${batch[@]}"; do
            if [[ "$(_cluster_vm_status "$bvmid")" == "running" ]]; then
                mark_started "$bvmid"
            else
                warn "VM $bvmid non running apres demarrage"
                ((failed_start++)) || true
            fi
        done
        info "Progression palier ${target}: running=$(count_running_limit "$target")/${target}"
        batch=()
    done

    if [[ "${#batch[@]}" -gt 0 ]]; then
        for bvmid in "${batch[@]}"; do
            start_vm_if_needed "$bvmid" &
            _PIDS+=($!)
        done
        wait || true
        sleep 2
        for bvmid in "${batch[@]}"; do
            if [[ "$(_cluster_vm_status "$bvmid")" == "running" ]]; then
                mark_started "$bvmid"
            else
                warn "VM $bvmid non running apres demarrage"
                ((failed_start++)) || true
            fi
        done
    fi

    running_now="$(count_running_limit "$target")"
    info "VMs running palier ${target}: $running_now / $target (failed_start=$failed_start)"
    [[ "$failed_start" -eq 0 ]] || fail "$failed_start VM(s) n'ont pas demarre au palier $target"
}

monitor_step() {
    local target="$1"
    local t0=$SECONDS
    local running_now node active start_ns control_json end_ns latency_ms vcpu_json

    step "Surveillance Omega/Proxmox palier ${target} (${SOAK_SECS}s)"
    while [[ "$(elapsed "$t0")" -lt "$SOAK_SECS" ]]; do
        running_now="$(count_running_limit "$target")"
        if [[ "$running_now" -lt "$target" ]]; then
            warn "running=$running_now attendu=$target"
            ((failures++)) || true
        fi

        for node in "${OMEGA_NODES_ARR[@]}"; do
            active="$(ssh_run "$node" "systemctl is-active omega-daemon 2>/dev/null || true" || echo "ssh-error")"
            if [[ "$active" != "active" ]]; then
                warn "$node: omega-daemon status=$active"
                ((failures++)) || true
            fi

            start_ns="$(date +%s%N)"
            control_json="$(curl -sf --max-time 5 "http://${node}:${METRICS_PORT}/control/status" 2>/dev/null || true)"
            end_ns="$(date +%s%N)"
            latency_ms=$(( (end_ns - start_ns) / 1000000 ))
            [[ "$latency_ms" -gt "$max_controller_latency_ms" ]] && max_controller_latency_ms="$latency_ms"
            if [[ -z "$control_json" ]]; then
                warn "$node: /control/status indisponible"
                ((failures++)) || true
            fi

            vcpu_json="$(curl -sf --max-time 5 "http://${node}:${METRICS_PORT}/control/vcpu/status" 2>/dev/null || true)"
            if [[ -n "$vcpu_json" ]]; then
                printf '%s' "$vcpu_json" | python3 -c '
import json, sys
d=json.load(sys.stdin)
used=d.get("used_vcpu_slots",0)
total=d.get("total_vcpu_slots",0)
states=len(d.get("vm_states",[]))
print(f"  vcpu node={d.get(\"node_id\",\"?\")} states={states} used={used}/{total}")
' || true
            else
                warn "$node: /control/vcpu/status indisponible"
                ((failures++)) || true
            fi
        done

        info "scale palier=${target}: running=$running_now/$target max_api_latency_ms=$max_controller_latency_ms failures=$failures"
        sleep "$POLL_SECS"
    done
}

for step_target in "${SCALE_STEPS[@]}"; do
    start_until_count "$step_target"
    monitor_step "$step_target"
done

step "Rapport final"
all_stores_status
info "running=$(count_running_selected)/${#selected_vmids[@]}"
info "max_control_api_latency_ms=$max_controller_latency_ms"

[[ "$failures" -eq 0 ]] || fail "scalabilite terminee avec $failures anomalie(s)"
pass "Scalabilite OK: ${#selected_vmids[@]} VMs gerees; paliers=${SCALE_STEPS[*]}; soak=${SOAK_SECS}s/palier"
