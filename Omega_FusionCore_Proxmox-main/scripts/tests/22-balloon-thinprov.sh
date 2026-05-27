#!/usr/bin/env bash
# Test 22 — Balloon thin-provisioning : min RAM, croissance sous charge, puis escalade migration
# Usage : ./22-balloon-thinprov.sh [vmid]
# Prérequis : cluster Proxmox, VM démarrée avec driver virtio-balloon

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"

VM_CFG="$(qm config "$VMID" 2>/dev/null || true)"
[[ -n "$VM_CFG" ]] || fail "impossible de lire la config Proxmox de la VM $VMID; le test balloon ne doit pas continuer avec des valeurs inventées"

qm_monitor_cmd() {
    local vmid="$1" cmd="$2"
    local node_ip
    node_ip="$(vm_node "$vmid" 2>/dev/null || true)"
    [[ -n "$node_ip" ]] || node_ip="$CONTROLLER_NODE"

    # qm monitor must run on the Proxmox node that currently owns the VM.
    # After a live migration, running it on the old/controller node returns no data.
    ssh_run "$node_ip" "printf '%s\nquit\n' \"$cmd\" | qm monitor \"$vmid\" 2>/dev/null || true" 2>/dev/null || true
}

balloon_actual_mib() {
    local vmid="$1" out
    out="$(qm_monitor_cmd "$vmid" "info balloon")"
    printf '%s\n' "$out" | sed -n 's/.*actual=\([0-9][0-9]*\).*/\1/p' | head -1
}

set_balloon_actual_mib() {
    local vmid="$1" mib="$2"
    qm_monitor_cmd "$vmid" "balloon $mib" >/dev/null
}

# Source de vérité Omega : memory = plafond RAM, balloon = RAM initiale/min.
_vm_ram=$(printf '%s\n' "$VM_CFG" | awk '/^memory:/{print $2}' | head -1)
_vm_balloon=$(printf '%s\n' "$VM_CFG" | awk '/^balloon:/{print $2}' | head -1)
[[ -n "$_vm_ram" ]] || fail "VM $VMID non conforme : champ memory absent"
[[ -n "$_vm_balloon" ]] || fail "VM $VMID non conforme : champ balloon absent"
[[ "$_vm_balloon" -gt 0 && "$_vm_balloon" -lt "$_vm_ram" ]] || \
    fail "VM $VMID non conforme : balloon=${_vm_balloon} doit être > 0 et < memory=${_vm_ram}"

VM_MAX_MIB="${BALLOON_MAX_MIB:-$_vm_ram}"
VM_INIT_MIB="${BALLOON_INIT_MIB:-$_vm_balloon}"
# Grandir par paliers de 1/8 de la RAM max (min 32 MiB)
_step=$(( VM_MAX_MIB / 8 )); (( _step < 32 )) && _step=32
VM_STEP_MIB="${BALLOON_STEP_MIB:-$_step}"
BALLOON_MIGRATION_TIMEOUT_SECS="${OMEGA_BALLOON_MIGRATION_TIMEOUT_SECS:-${OMEGA_MIGRATION_TIMEOUT_SECS:-180}}"
[[ "$BALLOON_MIGRATION_TIMEOUT_SECS" =~ ^[0-9]+$ && "$BALLOON_MIGRATION_TIMEOUT_SECS" -ge 30 ]] || BALLOON_MIGRATION_TIMEOUT_SECS=180
BALLOON_STRESS_SECS=$(( BALLOON_MIGRATION_TIMEOUT_SECS + 30 ))

header "Test 22 — Balloon thin-provisioning (VM $VMID)"

step "Vérifications prérequis"
require_bin qm
ensure_guest_packages "$VMID" stress-ng qemu-guest-agent || \
    warn "installation automatique stress-ng/qemu-guest-agent impossible — le test utilisera les fallbacks disponibles"

info "Profil RAM Omega détecté : balloon=${VM_INIT_MIB} MiB / memory=${VM_MAX_MIB} MiB"

step "Nœud initial de la VM"
node_before=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
    | python3 -c "import sys,json; vms=json.load(sys.stdin); \
      [print(v['node']) for v in vms if v.get('vmid')==$VMID]" | head -1)
info "VM $VMID sur : ${node_before:-inconnu}"

step "RAM actuelle de la VM"
ram_current="$(balloon_actual_mib "$VMID")"
info "RAM totale VM : ${VM_MAX_MIB} MiB"
info "RAM visible QEMU (balloon actual) actuelle : ${ram_current:-<non défini>} MiB"
[[ -n "$ram_current" ]] || fail "impossible de lire 'info balloon' via qm monitor; vérifier virtio-balloon/QMP"

step "Réinitialisation balloon à la valeur initiale"
set_balloon_actual_mib "$VMID" "$VM_INIT_MIB"
sleep 2
ram_after_init="$(balloon_actual_mib "$VMID")"
info "RAM visible QEMU après reset : ${ram_after_init:-?} MiB (attendu : $VM_INIT_MIB)"
[[ -n "$ram_after_init" ]] || fail "impossible de relire le balloon après reset"
if [[ "$ram_after_init" -gt "$(( VM_INIT_MIB + VM_STEP_MIB ))" ]]; then
    warn "RAM visible QEMU encore élevée après reset ($ram_after_init MiB); le guest peut refuser de rendre la mémoire immédiatement"
fi

step "Démarrage agent avec BalloonManager + migration activés"
LOG_AGENT="/tmp/omega-agent-balloon.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "$STORES_CSV" \
    --status-addrs "$STATUS_CSV" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_MAX_MIB" \
    --region-mib "$VM_MAX_MIB" \
    --current-node "$(local_pve_node)" \
    --eviction-threshold-mib 999999 \
    --eviction-batch-size 64 \
    --eviction-interval-secs 3 \
    --migration-enabled \
    --migration-interval-secs 10 \
    --compaction-enabled \
    --balloon-enabled \
    --balloon-initial-mib "$VM_INIT_MIB" \
    --balloon-step-mib "$VM_STEP_MIB" \
    --balloon-interval-secs 5 \
    --balloon-grow-faults-per-sec 2 \
    --balloon-shrink-faults-per-sec 0 \
    --mode daemon >"$LOG_AGENT" 2>&1 &
_PIDS+=($!)
AGENT_PID=$!
sleep 3

step "Vérification balloon = initial après démarrage de l'agent"
ram_agent_start="$(balloon_actual_mib "$VMID")"
info "RAM visible QEMU au démarrage agent : ${ram_agent_start:-?} MiB (attendu : $VM_INIT_MIB)"
[[ "${ram_agent_start:-0}" -le "$(( VM_INIT_MIB + VM_STEP_MIB ))" ]] || \
    warn "RAM visible QEMU plus grande que prévu ($ram_agent_start > $(( VM_INIT_MIB + VM_STEP_MIB )))"

step "Simulation charge mémoire dans la VM pour déclencher croissance puis migration (${BALLOON_STRESS_SECS}s)"
vm_mem_stress "$VMID" "$BALLOON_STRESS_SECS" "85%" || vm_cpu_stress "$VMID" "$BALLOON_STRESS_SECS"
STRESS_PID=${_PIDS[-1]:-0}
info "stress-ng démarré dans la VM $VMID"

step "Surveillance croissance balloon + migration pendant ${BALLOON_MIGRATION_TIMEOUT_SECS}s"
t0=$SECONDS
ram_max="${ram_agent_start:-$VM_INIT_MIB}"
grew=false
migration_triggered=false
migration_search_seen=false
while [[ $(elapsed $t0) -lt "$BALLOON_MIGRATION_TIMEOUT_SECS" ]]; do
    ram_now="$(balloon_actual_mib "$VMID")"
    ram_now="${ram_now:-$VM_INIT_MIB}"
    if [[ "${ram_now:-0}" -gt "${ram_max:-0}" ]]; then
        ram_max="$ram_now"
        grew=true
        info "RAM visible QEMU grandie → $ram_max MiB à $(elapsed $t0)s"
    fi
    if grep -qi "recherche migration\|spawning démon migration\|nœud candidat retenu\|lancement qm migrate\|migration réussie\|migration déclenchée" "$LOG_AGENT" 2>/dev/null; then
        migration_search_seen=true
    fi
    if grep -qi "lancement qm migrate\|migration réussie\|migration déclenchée" "$LOG_AGENT" 2>/dev/null; then
        migration_triggered=true
    fi
    printf "\r  [%3ds] visible=%-6s MiB  max=%-6s MiB  grandi=%s  migration=%s" \
        "$(elapsed $t0)" "${ram_now:-?}" "$ram_max" \
        "$([ $grew = true ] && echo OUI || echo non)" \
        "$([ $migration_triggered = true ] && echo OUI || { $migration_search_seen && echo recherche || echo non; })"
    sleep 5
done
echo ""

log_ram_max="$(grep -oE 'new_mib=[0-9]+' "$LOG_AGENT" 2>/dev/null | cut -d= -f2 | sort -n | tail -1 || true)"
if [[ -n "${log_ram_max:-}" && "$log_ram_max" =~ ^[0-9]+$ && "$log_ram_max" -gt "${ram_max:-0}" ]]; then
    ram_max="$log_ram_max"
    grew=true
    info "RAM balloon grandie détectée dans les logs Omega → $ram_max MiB"
fi

kill "$STRESS_PID" 2>/dev/null || true

step "Logs agent (balloon + migration)"
grep -i "balloon\|grow\|shrink\|mib\|fault.rate\|migration\|qm migrate\|candidat\|recall" "$LOG_AGENT" | head -60 || \
    warn "aucun log balloon trouvé"

step "Vérification que le balloon ne dépasse pas max ($VM_MAX_MIB MiB)"
ram_final="$(balloon_actual_mib "$VMID")"
info "RAM visible QEMU finale : ${ram_final:-?} MiB"
if [[ -z "$ram_final" ]]; then
    warn "lecture finale QMP indisponible après migration; validation basée sur max_observé et logs de migration"
fi
if [[ "${ram_final:-0}" -gt "$VM_MAX_MIB" ]]; then
    fail "RAM visible QEMU a dépassé le maximum : $ram_final > $VM_MAX_MIB MiB"
fi

step "Résumé"
node_after=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
    | python3 -c "import sys,json; vms=json.load(sys.stdin); \
      [print(v['node']) for v in vms if v.get('vmid')==$VMID]" | head -1)
info "initial=$VM_INIT_MIB | max_observé=$ram_max | max_autorisé=$VM_MAX_MIB | final=${ram_final:-?}"
info "nœud initial=${node_before:-inconnu} | nœud final=${node_after:-inconnu} | migration_recherche=$($migration_search_seen && echo oui || echo non) | migration_lancée=$($migration_triggered && echo oui || echo non)"

if [[ "${ram_max:-0}" -gt "${ram_agent_start:-$VM_INIT_MIB}" ]]; then
    grew=true
fi

if $grew && { $migration_triggered || [[ -n "${node_before:-}" && -n "${node_after:-}" && "$node_before" != "$node_after" ]]; }; then
    pass "balloon + migration OK — RAM visible QEMU passée de ${ram_agent_start:-$VM_INIT_MIB} MiB à $ram_max MiB et migration déclenchée"
elif $grew && $migration_search_seen; then
    fail "balloon a grandi mais seule une recherche de migration est visible; aucune migration lancée. Vérifier cible saine, Ceph, ressources destination et logs $LOG_AGENT"
elif $grew; then
    fail "balloon a grandi mais aucune recherche/migration n'a été observée. Le test attend maintenant l'escalade balloon -> migration sous forte pression"
else
    warn "aucune croissance de RAM visible QEMU observée"
    info "vérifications :"
    info "  1. Le driver virtio-balloon est bien chargé dans le guest (lsmod | grep balloon)"
    info "  2. stress-ng a bien créé de la pression mémoire"
    info "  3. fault_rate ≥ grow_faults_per_sec ($( grep -oE 'grow.*=[0-9]+' "$LOG_AGENT" | head -1 || echo '?'))"
    fail "aucune croissance balloon observée; impossible de valider l'escalade balloon -> migration"
fi
