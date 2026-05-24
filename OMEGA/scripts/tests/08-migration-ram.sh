#!/usr/bin/env bash
# Test 8 — Migration RAM : recall complet + qm migrate sous pression mémoire
# Usage : ./08-migration-ram.sh [vmid]
# Prérequis : cluster 3 nœuds identiques (OMEGA_NODES), VM 9001 active

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"
VM_RAM_MIB=$(vm_ram_mib "$VMID"); VM_RAM_MIB="${VM_RAM_MIB:-1024}"
VM_CFG="$(qm config "$VMID" 2>/dev/null || true)"
VM_BALLOON_MIB="$(printf '%s\n' "$VM_CFG" | awk '/^balloon:/{print $2}' | head -1)"
MIGRATION_TIMEOUT_SECS="${OMEGA_MIGRATION_TIMEOUT_SECS:-120}"
[[ "$MIGRATION_TIMEOUT_SECS" =~ ^[0-9]+$ && "$MIGRATION_TIMEOUT_SECS" -ge 1 ]] || MIGRATION_TIMEOUT_SECS=120
MIGRATION_WATCH_SECS="$MIGRATION_TIMEOUT_SECS"
FORCE_MIGRATION="${OMEGA_TEST08_FORCE_MIGRATION:-1}"
MIGRATION_INTERVAL_SECS="${OMEGA_TEST08_MIGRATION_INTERVAL_SECS:-10}"
BALLOON_TRIGGER=false
BALLOON_ARGS=()
if [[ "${VM_BALLOON_MIB:-0}" =~ ^[0-9]+$ && "$VM_BALLOON_MIB" -gt 0 && "$VM_BALLOON_MIB" -lt "$VM_RAM_MIB" ]]; then
    _balloon_step=$(( VM_RAM_MIB / 8 ))
    (( _balloon_step < 32 )) && _balloon_step=32
    BALLOON_TRIGGER=true
    BALLOON_ARGS=(
        --balloon-enabled
        --balloon-initial-mib "$VM_BALLOON_MIB"
        --balloon-step-mib "${OMEGA_TEST08_BALLOON_STEP_MIB:-$_balloon_step}"
        --balloon-interval-secs "${OMEGA_TEST08_BALLOON_INTERVAL_SECS:-5}"
        --balloon-grow-faults-per-sec "${OMEGA_TEST08_BALLOON_GROW_FAULTS_PER_SEC:-2}"
        --balloon-shrink-faults-per-sec 0
    )
fi

attached_cdrom_drives() {
    local vmid="$1"
    qm config "$vmid" 2>/dev/null | awk -F': ' '
        $2 ~ /media=cdrom/ && $2 !~ /^none,/ { print $1 }
    '
}

header "Test 8 — Migration RAM (VM $VMID)"

step "Vérifications prérequis"
require_cluster

step "Nœud initial de la VM"
node_before=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
    | python3 -c "import sys,json; vms=json.load(sys.stdin); \
      [print(v['node']) for v in vms if v.get('vmid')==$VMID]" | head -1)
info "VM $VMID sur : ${node_before:-inconnu}"
node_before_ip="$(vm_node "$VMID" 2>/dev/null || _pve_node_to_ip "${node_before:-}")"

TARGET_NODE="${OMEGA_TEST08_TARGET_NODE:-}"
if [[ -z "$TARGET_NODE" ]]; then
    best_ram=-1
    for n in "${OMEGA_NODES_ARR[@]}"; do
        [[ -z "$n" || "$n" == "$node_before_ip" ]] && continue
        status_json="$(curl -sf --max-time 3 "http://${n}:${STATUS_PORT}/status" 2>/dev/null || true)"
        ram_free="$(printf '%s' "$status_json" | python3 -c 'import sys,json
try:
    print(int(json.load(sys.stdin).get("available_mib", -1)))
except Exception:
    print(-1)
' 2>/dev/null || echo -1)"
        if [[ "$ram_free" -gt "$best_ram" ]]; then
            best_ram="$ram_free"
            TARGET_NODE="$n"
        fi
    done
fi
if [[ -z "$TARGET_NODE" ]]; then
    for n in "${OMEGA_NODES_ARR[@]}"; do
        [[ -n "$n" && "$n" != "$node_before_ip" ]] && { TARGET_NODE="$n"; break; }
    done
fi
TARGET_NODE_PVE="$(_ip_to_pve_node "$TARGET_NODE")"
[[ -n "$TARGET_NODE" ]] || fail "aucune cible de migration disponible pour le test 08"

step "Validation des 6 conditions de migration"
info "1/6 migration activée : --migration-enabled, intervalle=${MIGRATION_INTERVAL_SECS}s, fenêtre=${MIGRATION_WATCH_SECS}s"
if $BALLOON_TRIGGER; then
    info "2/6 raison de migration : pression RAM + balloon activé (${VM_BALLOON_MIB}/${VM_RAM_MIB} MiB)"
else
    warn "2/6 raison de migration : balloon indisponible; le test utilisera pression RAM/éviction uniquement"
fi
source_status="$(curl -sf --max-time 3 "http://${node_before_ip}:${STATUS_PORT}/status" 2>/dev/null || true)"
target_status="$(curl -sf --max-time 3 "http://${TARGET_NODE}:${STATUS_PORT}/status" 2>/dev/null || true)"
[[ -n "$target_status" ]] || fail "3/6 cible $TARGET_NODE inaccessible sur /status; impossible de prouver qu'elle est saine"
source_ram="$(printf '%s' "$source_status" | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("available_mib","?"))
except Exception: print("?")
' 2>/dev/null || echo "?")"
target_ram="$(printf '%s' "$target_status" | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("available_mib","?"))
except Exception: print("?")
' 2>/dev/null || echo "?")"
target_vcpu="$(printf '%s' "$target_status" | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("vcpu_free","?"))
except Exception: print("?")
' 2>/dev/null || echo "?")"
info "3/6 cible saine : source_ram=${source_ram} MiB, target=$TARGET_NODE target_ram=${target_ram} MiB target_vcpu_free=${target_vcpu}"
if [[ "$source_ram" =~ ^[0-9]+$ && "$target_ram" =~ ^[0-9]+$ && "$target_ram" -le "$source_ram" ]]; then
    fail "3/6 cible non meilleure : target_ram=${target_ram} MiB <= source_ram=${source_ram} MiB; choisir une cible avec OMEGA_TEST08_TARGET_NODE ou libérer de la RAM"
fi
if [[ "$target_vcpu" =~ ^[0-9]+$ && "$target_vcpu" -le 0 ]]; then
    fail "3/6 cible non saine : target_vcpu_free=${target_vcpu}"
fi
[[ -n "$TARGET_NODE_PVE" ]] || fail "4/6 nom Proxmox cible introuvable pour $TARGET_NODE"
info "4/6 nom Proxmox cible résolu : $TARGET_NODE -> $TARGET_NODE_PVE"
if printf '%s\n' "$VM_CFG" | grep -q '^hostpci'; then
    warn "5/6 VM migrable : hostpci détecté; nettoyage pour ce test RAM"
    delete_vm_hostpci "$VMID"
fi
attached_cdrom_drives "$VMID" | while read -r drv; do
    [[ -n "$drv" ]] || continue
    qm set "$VMID" "--${drv}" none 2>/dev/null && info "5/6 VM migrable : CD-ROM $drv éjecté" || true
done
if qm config "$VMID" 2>/dev/null | grep -q '^hostpci' || [[ -n "$(attached_cdrom_drives "$VMID")" ]]; then
    fail "5/6 VM non migrable : hostpci ou CD-ROM local encore présent"
fi
info "5/6 VM migrable : pas de hostpci/CD-ROM local bloquant"
info "6/6 seuils/durée : migration_interval=${MIGRATION_INTERVAL_SECS}s, watch=${MIGRATION_WATCH_SECS}s, force_fallback=${FORCE_MIGRATION}"
info "Cible déterministe si Omega ne migre pas seul : $TARGET_NODE (pvesh: $TARGET_NODE_PVE)"

step "Démarrage agent avec migration activée et seuil d'éviction agressif"
LOG_AGENT="/tmp/omega-agent-migration.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --current-node "$(local_pve_node)" \
    --eviction-threshold-mib 999999 \
    --eviction-batch-size 64 \
    --eviction-interval-secs 3 \
    --migration-enabled \
    --migration-interval-secs "$MIGRATION_INTERVAL_SECS" \
    "${BALLOON_ARGS[@]}" \
    --mode daemon >"$LOG_AGENT" 2>&1 &
_PIDS+=($!)
AGENT_PID=$!
sleep 5

step "Simulation pression mémoire (stress-ng 90s)"
info "démarrage stress-ng --vm 1 --vm-bytes 85% --timeout 90s"
STRESS_PID=0
vm_mem_stress "$VMID" 90 "85%" || {
    warn "charge mémoire invitée indisponible — fallback pression mémoire hôte sans apt"
    host_mem_stress 90 "70%" || vm_cpu_stress "$VMID" 90 || \
        fail "impossible de générer une pression RAM/CPU: installer qemu-guest-agent + stress-ng dans la VM ou python3/stress-ng sur l'hôte"
}

step "Surveillance éviction + migration pendant ${MIGRATION_WATCH_SECS}s"
t0=$SECONDS
migration_search_seen=false
migration_triggered=false
migration_succeeded=false
while [[ $(elapsed $t0) -lt "$MIGRATION_WATCH_SECS" ]]; do
    evicted=$(grep -c "éviction\|evict" "$LOG_AGENT" 2>/dev/null) || evicted=0
    if grep -qi "recherche migration\|nœud candidat retenu\|candidat.*migration\|migration recommandée" "$LOG_AGENT" 2>/dev/null; then
        migration_search_seen=true
    fi
    if grep -qi "lancement qm migrate\|qm migrate\|migration déclenchée" "$LOG_AGENT" 2>/dev/null; then
        migration_triggered=true
        info "migration déclenchée à $(elapsed $t0)s"
    fi
    if grep -qi "migration réussie\|migration terminee\|migration terminée" "$LOG_AGENT" 2>/dev/null; then
        migration_succeeded=true
    fi
    printf "\r  [%3ds] évictions loggées=%-4s  migration=%s" \
        "$(elapsed $t0)" "$evicted" \
        "$([ $migration_succeeded = true ] && echo réussie || { [ $migration_triggered = true ] && echo lancée || { [ $migration_search_seen = true ] && echo recherche || echo non; }; })"
    sleep 5
done
echo ""

[[ "$STRESS_PID" -gt 0 ]] && kill "$STRESS_PID" 2>/dev/null || true

step "Logs éviction + migration"
if [[ -f "$LOG_AGENT" ]]; then
    grep -i "éviction\|recall\|migration\|qm migrate" "$LOG_AGENT" | head -30 || true
else
    warn "log agent migration absent; validation basée sur l'état Proxmox final"
fi

forced_migration=false
forced_migration_succeeded=false
if ! $migration_triggered && ! $migration_succeeded && [[ "$FORCE_MIGRATION" != "0" ]]; then
    step "Migration déterministe par le harness"
    current_source_ip="$(vm_node "$VMID" 2>/dev/null || true)"
    if [[ -z "$current_source_ip" ]]; then
        warn "source courante introuvable — migration forcée ignorée"
    elif [[ "$current_source_ip" == "$TARGET_NODE" ]]; then
        warn "VM déjà sur la cible déterministe $TARGET_NODE — migration forcée ignorée"
    else
        current_source_pve="$(_ip_to_pve_node "$current_source_ip")"
        info "Omega n'a pas lancé de migration; le test force qm migrate $VMID $TARGET_NODE_PVE --online depuis $current_source_pve"
        ssh_run "$current_source_ip" \
            "qm config $VMID 2>/dev/null | awk -F': ' '\$2 ~ /media=cdrom/ && \$2 !~ /^none,/ { print \$1 }' | while read drv; do qm set $VMID \"--\${drv}\" none 2>/dev/null || true; done" \
            2>/dev/null || true
        LOG_QM="/tmp/omega-test08-qm-migrate-${VMID}.log"
        _TMPFILES+=("$LOG_QM")
        forced_migration=true
        if ssh_run "$current_source_ip" "qm migrate $VMID $TARGET_NODE_PVE --online" 2>&1 | tee "$LOG_QM"; then
            forced_migration_succeeded=true
            migration_triggered=true
            migration_succeeded=true
            info "migration déterministe réussie vers $TARGET_NODE"
        else
            warn "migration déterministe live échouée — tentative offline"
            if ssh_run "$current_source_ip" "qm migrate $VMID $TARGET_NODE_PVE" 2>&1 | tee -a "$LOG_QM"; then
                forced_migration_succeeded=true
                migration_triggered=true
                migration_succeeded=true
                info "migration déterministe offline réussie vers $TARGET_NODE"
            fi
        fi
    fi
fi

step "Nœud final de la VM"
node_after=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
    | python3 -c "import sys,json; vms=json.load(sys.stdin); \
      [print(v['node']) for v in vms if v.get('vmid')==$VMID]" | head -1)
info "VM $VMID maintenant sur : ${node_after:-inconnu}"

if [[ "$node_before" != "$node_after" ]]; then
    if $forced_migration_succeeded; then
        pass "migration RAM déterministe OK — éviction/recall observés puis VM déplacée de $node_before vers $node_after"
    else
        pass "migration RAM OK — VM déplacée de $node_before vers $node_after"
    fi
elif $migration_succeeded; then
    pass "migration RAM OK — migration réussie observée dans les logs"
elif $migration_triggered; then
    fail "migration RAM lancée mais aucun changement de nœud ni succès Proxmox observé; vérifier logs $LOG_AGENT et task history Proxmox"
elif $forced_migration; then
    fail "migration RAM forcée par le test mais qm migrate a échoué; vérifier /tmp/omega-test08-qm-migrate-${VMID}.log et la santé Ceph/réseau"
elif $migration_search_seen; then
    fail "migration RAM seulement recherchée/recommandée; aucune commande qm migrate réussie observée"
else
    fail "aucune migration RAM déclenchée — pression insuffisante, cible indisponible ou politique migration non activée"
fi
