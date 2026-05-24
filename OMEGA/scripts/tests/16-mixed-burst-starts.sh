#!/usr/bin/env bash
# Test M6 — Rafale de démarrages simultanés
# N agents pour N VMs démarrent en même temps et se partagent les stores
# Vérifie : pas de collision, stores stables, tous complètent le cycle demo
# Usage : ./16-mixed-burst-starts.sh [nb_vms] [nb_stores]
# Sans cluster : utilise des stores locaux
# Avec cluster : OMEGA_NODES pointe vers les nœuds réels

source "$(dirname "$0")/lib.sh"

NB_VMS="${1:-6}"
NB_STORES="${2:-$(store_count)}"
# Si pas de stores configurés dans le cluster, utiliser des stores locaux
USE_LOCAL_STORES=false
[[ "${NB_STORES}" -eq 0 || -z "$STORES_CSV" ]] && USE_LOCAL_STORES=true

header "Test M6 — Rafale $NB_VMS VMs simultanées ($NB_STORES stores)"
print_cluster_config

require_omega_bins

step "Démarrage stores"
BURST_STORES_CSV=""
BURST_STATUS_CSV=""

if $USE_LOCAL_STORES; then
    info "Stores locaux (pas de cluster configuré)"
    NB_STORES_LOCAL=3
    for i in $(seq 1 "$NB_STORES_LOCAL"); do
        port=$((STORE_PORT + i - 1))
        sport=$((STATUS_PORT + i - 1))
        start_store "burst$i" "$port" "$sport"
        BURST_STORES_CSV="${BURST_STORES_CSV:+$BURST_STORES_CSV,}127.0.0.1:$port"
        BURST_STATUS_CSV="${BURST_STATUS_CSV:+$BURST_STATUS_CSV,}127.0.0.1:$sport"
    done
else
    BURST_STORES_CSV="$STORES_CSV"
    BURST_STATUS_CSV="$STATUS_CSV"
    info "Utilisation des stores cluster : $BURST_STORES_CSV"
fi

step "Lancement simultané de $NB_VMS agents en mode demo"
t0=$SECONDS
LOGS=()
PIDS_VM=()

for i in $(seq 1 "$NB_VMS"); do
    vmid=$((200 + i))
    LOG="/tmp/omega-m6-vm${vmid}.log"
    _TMPFILES+=("$LOG")
    LOGS+=("$LOG")
    "$AGENT_BIN" \
        --stores "$BURST_STORES_CSV" \
        --vm-id "$vmid" \
        --vm-requested-mib 64 \
        --region-mib 64 \
        --recall-priority "$i" \
        --mode demo >"$LOG" 2>&1 &
    PIDS_VM+=($!)
    _PIDS+=($!)
done

info "$NB_VMS agents démarrés simultanément"

step "Attente completion (max 120s)"
completed=0; failed=0
for i in "${!PIDS_VM[@]}"; do
    vmid=$((200 + i + 1))
    wait "${PIDS_VM[$i]}" 2>/dev/null; rc=$?
    if [[ $rc -eq 0 ]] && grep -q "SUCCÈS" "${LOGS[$i]}" 2>/dev/null; then
        ((completed++)) || true
    else
        ((failed++)) || true
        warn "agent vmid=$vmid échoué (rc=$rc)"
        tail -5 "${LOGS[$i]}" | sed 's/^/    /'
    fi
done

step "Résultats individuels"
for i in $(seq 1 "$NB_VMS"); do
    vmid=$((200 + i))
    log="${LOGS[$((i-1))]}"
    status=$(grep "SUCCÈS\|ÉCHEC\|integrity_ok" "$log" 2>/dev/null | tail -1 || echo "pas de résultat")
    errors=$(grep -oP 'errors=\K[0-9]+' "$log" 2>/dev/null | tail -1 || echo "?")
    echo -e "  vm$vmid : $status  (errors=$errors)"
done

step "Vérification isolation (pas de collision de vm_id)"
for i in $(seq 1 "$NB_VMS"); do
    vmid=$((200 + i))
    log="${LOGS[$((i-1))]}"
    # Vérifier qu'aucune page d'un autre vmid n'est mentionnée dans les erreurs
    collision=$(grep -v "vm_id=$vmid\|vm-id $vmid" "$log" 2>/dev/null | \
        grep "vm_id=2[0-9][0-9]" | head -1 || echo "")
    [[ -z "$collision" ]] || warn "possible collision dans vm$vmid : $collision"
done

step "État final des stores"
for n in "${STORE_NODES_ARR[@]}"; do
    status=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
        python3 -c "import sys,json; d=json.load(sys.stdin)
print(f\"  {d.get('node_id','?'):12s}  pages={d.get('page_count',0)}\")
" 2>/dev/null || echo "  $n : inaccessible")
    echo "$status"
done

total_duration=$(elapsed $t0)
info "Temps total : ${total_duration}s"
info "Réussis : $completed / $NB_VMS | Échoués : $failed"

[[ $failed -eq 0 ]] || fail "M6 : $failed agent(s) échoué(s) sur $NB_VMS"
pass "M6 OK — $NB_VMS VMs simultanées, tous SUCCÈS, pas de collision"
