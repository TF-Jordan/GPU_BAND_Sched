#!/usr/bin/env bash
# Test M4 — Stress cluster complet : RAM + CPU + stores sur tous les nœuds simultanément
# Vérifie la stabilité du système sous charge généralisée
# Usage : ./14-mixed-cluster-pressure.sh [nb_vms] [duree_secs]
# Prérequis : cluster N nœuds, N VMs démarrées (une par nœud ou plusieurs)

source "$(dirname "$0")/lib.sh"

NB_VMS="${1:-${#TEST_VMIDS_ARR[@]}}"
DUREE="${2:-120}"
VMS_TO_TEST=("${TEST_VMIDS_ARR[@]:0:$NB_VMS}")

header "Test M4 — Stress cluster complet ($NB_VMS VMs, ${DUREE}s)"
print_cluster_config

step "Prérequis"
require_cluster
STRESS_NODES=()
declare -A STRESS_MODE
for n in "${OMEGA_NODES_ARR[@]}"; do
    if _is_local_node "$n"; then
        if command -v stress-ng >/dev/null 2>&1; then
            STRESS_MODE["$n"]="stress-ng"
            STRESS_NODES+=("$n")
        elif command -v python3 >/dev/null 2>&1; then
            STRESS_MODE["$n"]="python"
            STRESS_NODES+=("$n")
        else
            warn "M4 continuera sans charge hôte sur $n; stress-ng/python3 absents"
        fi
    elif ssh_run "$n" "command -v stress-ng >/dev/null 2>&1"; then
        STRESS_MODE["$n"]="stress-ng"
        STRESS_NODES+=("$n")
    elif ssh_run "$n" "command -v python3 >/dev/null 2>&1"; then
        STRESS_MODE["$n"]="python"
        STRESS_NODES+=("$n")
    else
        warn "M4 continuera sans charge hôte sur $n; stress-ng/python3 absents"
    fi
done
[[ ${#VMS_TO_TEST[@]} -ge 1 ]] || fail "aucune VM configurée — vérifier OMEGA_TEST_VMIDS dans cluster.conf"
[[ ${#STRESS_NODES[@]} -ge 1 ]] || warn "aucun noeud hôte avec générateur de charge; M4 validera surtout les agents/stores"
SELECTED_STRESS_VMS=()
for _vmid in "${VMS_TO_TEST[@]}"; do
    if require_vm_running "$_vmid"; then
        SELECTED_STRESS_VMS+=("$SELECTED_VMID")
    fi
done
mapfile -t VMS_TO_TEST < <(printf '%s\n' "${SELECTED_STRESS_VMS[@]}" | awk 'NF && !seen[$0]++')
[[ ${#VMS_TO_TEST[@]} -ge 1 ]] || fail "aucune VM utilisable pour le stress cluster"
NB_VMS="${#VMS_TO_TEST[@]}"

step "Normalisation profil Omega des VMs"
NORMALIZED_VMS=()
for vmid in "${VMS_TO_TEST[@]}"; do
    if ensure_omega_vcpu_profile_safe "$vmid"; then
        _refresh_vm_node_cache "$vmid" >/dev/null
        _cores=$(vm_cores "$vmid")
        info "VM $vmid conforme : max_vcpus=${_cores:-?}"
        NORMALIZED_VMS+=("$vmid")
    else
        warn "VM $vmid ignorée pour M4 : config distante illisible ou réparation impossible"
        unset "_VM_NODE_CACHE[$vmid]"
    fi
done
VMS_TO_TEST=("${NORMALIZED_VMS[@]}")
[[ ${#VMS_TO_TEST[@]} -ge 1 ]] || fail "aucune VM normalisée utilisable pour M4"
NB_VMS="${#VMS_TO_TEST[@]}"

step "Inventaire cluster avant stress"
info "$(node_count) nœuds, $(store_count) stores"
echo ""
for n in "${OMEGA_NODES_ARR[@]}"; do
    status=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
        python3 -c "
import sys,json; d=json.load(sys.stdin)
print(f\"  {d.get('node_id','?'):12s}  ram={d.get('available_mib','?')} Mio  vcpu_free={d.get('vcpu_free','?')}  pages={d.get('page_count',0)}\")
" 2>/dev/null || echo "  $n : inaccessible")
    echo "$status"
done

step "Démarrage ${#VMS_TO_TEST[@]} agents"
LOGS=()
for vmid in "${VMS_TO_TEST[@]}"; do
    _ram=$(vm_ram_mib "$vmid"); _ram="${_ram:-1024}"
    LOG="/tmp/omega-m4-vm${vmid}.log"
    _TMPFILES+=("$LOG")
    LOGS+=("$LOG")
    "$AGENT_BIN" \
        --stores "$STORES_CSV" \
        --status-addrs "$STATUS_CSV" \
        --vm-id "$vmid" \
        --vm-requested-mib "$_ram" \
        --region-mib "$_ram" \
        --eviction-threshold-mib 999999 \
        --eviction-batch-size 32 \
        --eviction-interval-secs 3 \
        --mode daemon >"$LOG" 2>&1 &
    _PIDS+=($!)
    info "agent vmid=$vmid RAM=${_ram} MiB démarré (log: $LOG)"
done
sleep 3

step "Saturation RAM sur les nœuds avec générateur disponible"
for n in "${STRESS_NODES[@]}"; do
    if [[ "${STRESS_MODE[$n]}" == "stress-ng" ]]; then
        info "stress-ng --vm 1 --vm-bytes 75% sur $n"
        ssh_run_bg "$n" "stress-ng --vm 1 --vm-bytes 75% --timeout ${DUREE}s &>/dev/null"
    else
        info "fallback Python mémoire sur $n"
        ssh_run_bg "$n" "python3 -c \"import time; chunks=[]; deadline=time.time()+${DUREE}; chunk=16*1024*1024
while time.time()<deadline:
    try:
        b=bytearray(chunk)
        b[0]=1
        chunks.append(b)
    except MemoryError:
        time.sleep(1)
    time.sleep(0.05)\" &>/dev/null"
    fi
done

step "Saturation CPU sur les nœuds avec générateur disponible"
for n in "${STRESS_NODES[@]}"; do
    if [[ "${STRESS_MODE[$n]}" == "stress-ng" ]]; then
        info "stress-ng --cpu 0 sur $n"
        ssh_run_bg "$n" "stress-ng --cpu 0 --timeout ${DUREE}s &>/dev/null"
    else
        info "fallback shell CPU sur $n"
        ssh_run_bg "$n" "timeout ${DUREE}s bash -lc 'for i in \$(seq 1 \$(nproc 2>/dev/null || echo 1)); do while :; do :; done & done; wait' &>/dev/null"
    fi
done

step "Observation pendant ${DUREE}s"
t0=$SECONDS
declare -A pages_evicted_per_node
for n in "${STORE_NODES_ARR[@]}"; do pages_evicted_per_node["$n"]=0; done

while [[ $(elapsed $t0) -lt $DUREE ]]; do
    total_pages=0
    status_line=""
    for n in "${STORE_NODES_ARR[@]}"; do
        pc=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" || echo 0)
        total_pages=$((total_pages + pc))
        status_line+="${n##*.}:${pc}  "
    done
    printf "\r  [%3ds/%ds] pages_stores=%s  (%s)" \
        "$(elapsed $t0)" "$DUREE" "$total_pages" "$status_line"
    sleep 10
done
echo ""

step "État cluster après stress"
for n in "${OMEGA_NODES_ARR[@]}"; do
    status=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
        python3 -c "
import sys,json; d=json.load(sys.stdin)
print(f\"  {d.get('node_id','?'):12s}  ram={d.get('available_mib','?')} Mio  pages={d.get('page_count',0)}\")
" 2>/dev/null || echo "  $n : inaccessible")
    echo "$status"
done

step "Vérification stabilité"
errors_total=0
for log in "${LOGS[@]}"; do
    panics=$(grep -c "panic\|FATAL\|thread.*main.*panicked" "$log" 2>/dev/null) || panics=0
    [[ "$panics" -gt 0 ]] && { warn "panic dans $log"; ((errors_total++)) || true; }
done

total_pages_final=0
for n in "${STORE_NODES_ARR[@]}"; do
    pc=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" || echo 0)
    total_pages_final=$((total_pages_final + pc))
done
info "Pages totales dans les stores : $total_pages_final"

step "Vérification que les stores sont toujours joignables"
stores_ok=0
for n in "${STORE_NODES_ARR[@]}"; do
    curl -sf "http://${n}:${STATUS_PORT}/status" &>/dev/null && \
        { pass "store $n toujours joignable"; ((stores_ok++)) || true; } || \
        warn "store $n inaccessible après le stress"
done

[[ $errors_total -eq 0 ]] || fail "M4 : $errors_total panic(s) détecté(s) dans les logs"
[[ $stores_ok -eq $(store_count) ]] || warn "$(( $(store_count) - stores_ok )) store(s) inaccessible(s)"

pass "M4 OK — cluster stable sous charge (${#VMS_TO_TEST[@]} VMs, $(node_count) nœuds, ${DUREE}s)"
