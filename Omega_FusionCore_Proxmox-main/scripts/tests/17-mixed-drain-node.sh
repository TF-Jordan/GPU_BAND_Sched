#!/usr/bin/env bash
# Test M7 — Drain d'un nœud : vider toutes ses VMs sans downtime
# Les VMs sont migrées live vers les autres nœuds du cluster
# Usage : ./17-mixed-drain-node.sh [node_a_drainer] [vmid1] [vmid2] ...
# Prérequis : cluster 3+ nœuds, VMs démarrées sur le nœud à drainer

source "$(dirname "$0")/lib.sh"

DRAIN_NODE="${1:-$COMPUTE_NODE}"; shift || true
VMS_TO_DRAIN=("${@}")

# pvesh retourne le nom d'hôte Proxmox (ex: "pve"), pas l'IP.
# On résout DRAIN_NODE (IP ou nom) en nom d'hôte Proxmox via ssh.
DRAIN_NODE_PVE=$(ssh_run "$DRAIN_NODE" "hostname" 2>/dev/null || echo "$DRAIN_NODE")

# Si pas de VMs passées en argument, détecter automatiquement les VMs du nœud
if [[ ${#VMS_TO_DRAIN[@]} -eq 0 ]]; then
    info "Détection automatique des VMs sur $DRAIN_NODE (hostname pvesh: $DRAIN_NODE_PVE)..."
    mapfile -t VMS_TO_DRAIN < <(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys, json
vms = json.load(sys.stdin)
drain = '$DRAIN_NODE_PVE'
for v in vms:
    if v.get('node') == drain and v.get('status') == 'running':
        print(v['vmid'])
" 2>/dev/null || echo "")
fi

if [[ ${#VMS_TO_DRAIN[@]} -eq 0 && -n "${TEST_VMIDS_ARR[*]:-}" ]]; then
    info "aucune VM trouvée sur $DRAIN_NODE_PVE — recherche d'un nœud portant des VMs de test"
    vmids_csv="$(IFS=,; echo "${TEST_VMIDS_ARR[*]}")"
    result=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys, json
wanted = {int(x) for x in '$vmids_csv'.split(',') if x.strip().isdigit()}
by_node = {}
for v in json.load(sys.stdin):
    vmid = v.get('vmid')
    if vmid in wanted and v.get('status') == 'running':
        by_node.setdefault(v.get('node',''), []).append(str(vmid))
if by_node:
    node, vmids = max(by_node.items(), key=lambda item: len(item[1]))
    print(node + ' ' + ','.join(vmids))
" 2>/dev/null || true)
    if [[ -n "$result" ]]; then
        DRAIN_NODE_PVE="${result%% *}"
        DRAIN_NODE="$(_pve_node_to_ip "$DRAIN_NODE_PVE")"
        IFS=',' read -ra VMS_TO_DRAIN <<< "${result#* }"
        info "drain adapté au nœud $DRAIN_NODE_PVE ($DRAIN_NODE) avec VMs: ${VMS_TO_DRAIN[*]}"
    fi
fi

# Si toujours aucune VM (ex: déjà migrée par M5), trouver la VM sur n'importe quel nœud
if [[ ${#VMS_TO_DRAIN[@]} -eq 0 ]]; then
    info "$DRAIN_NODE vide — recherche de VMs running sur le cluster..."
    result=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys, json
vms = [v for v in json.load(sys.stdin) if v.get('status') == 'running']
if vms:
    v = vms[0]
    print(v.get('node',''), v['vmid'])
" 2>/dev/null || echo "")
    if [[ -n "$result" ]]; then
        new_pve="${result%% *}"; new_vmid="${result##* }"
        DRAIN_NODE=$(_pve_node_to_ip "$new_pve")
        DRAIN_NODE_PVE="$new_pve"
        VMS_TO_DRAIN=("$new_vmid")
        info "VM $new_vmid trouvée sur $DRAIN_NODE_PVE — drain adapté"
    fi
fi

header "Test M7 — Drain nœud $DRAIN_NODE (${#VMS_TO_DRAIN[@]} VMs)"
print_cluster_config

step "Prérequis"
require_cluster
[[ $(node_count) -ge 2 ]] || fail "au moins 2 nœuds requis pour drainer"
[[ ${#VMS_TO_DRAIN[@]} -ge 1 ]] || fail "aucune VM à drainer sur le cluster"
info "VMs à drainer : ${VMS_TO_DRAIN[*]}"

step "État initial"
echo ""
for vmid in "${VMS_TO_DRAIN[@]}"; do
    node=$(vm_node "$vmid")
    vcpus=$(qm config "$vmid" 2>/dev/null | grep "^vcpus:" | awk '{print $2}' || echo "?")
    mem=$(qm config "$vmid" 2>/dev/null | grep "^memory:" | awk '{print $2}' || echo "?")
    echo -e "  VM $vmid : nœud=$node  vCPUs=$vcpus  RAM=${mem}Mo"
done

step "Pages actuellement sur les stores"
all_stores_status

step "Démarrage agents pour toutes les VMs (éviction + recall avant migration)"
LOGS=()
AGENT_PIDS=()
for vmid in "${VMS_TO_DRAIN[@]}"; do
    LOG="/tmp/omega-m7-vm${vmid}.log"
    _TMPFILES+=("$LOG")
    LOGS+=("$LOG")
    _ram=$(vm_ram_mib "$vmid"); _ram="${_ram:-1024}"
    "$AGENT_BIN" \
        --stores "$STORES_CSV" \
        --status-addrs "$STATUS_CSV" \
        --vm-id "$vmid" \
        --vm-requested-mib "$_ram" \
        --region-mib "$_ram" \
        --current-node "$DRAIN_NODE" \
        --eviction-threshold-mib 999999 \
        --eviction-batch-size 64 \
        --eviction-interval-secs 3 \
        --mode daemon >"$LOG" 2>&1 &
    AGENT_PIDS+=($!)
    _PIDS+=($!)
    info "agent vmid=$vmid démarré"
done
sleep 5

step "Éviction préventive (30s avant drain)"
info "Attente éviction pour diminuer l'empreinte locale..."
t0=$SECONDS
while [[ $(elapsed $t0) -lt 30 ]]; do
    total_pages=0
    for n in "${STORE_NODES_ARR[@]}"; do
        pc=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" || echo 0)
        total_pages=$((total_pages + pc))
    done
    printf "\r  [%3ds] pages_évincées vers stores : %s" "$(elapsed $t0)" "$total_pages"
    sleep 5
done
echo ""

step "Plan de drain : sélection des nœuds cibles"
# Nœuds disponibles = tous sauf DRAIN_NODE
TARGET_NODES=()
for n in "${OMEGA_NODES_ARR[@]}"; do
    [[ "$n" != "$DRAIN_NODE" ]] && TARGET_NODES+=("$n")
done
info "Nœuds cibles disponibles : ${TARGET_NODES[*]}"

step "Migrations live des VMs"
t_drain=$SECONDS
migrated=(); failed_migration=()
target_idx=0

for vmid in "${VMS_TO_DRAIN[@]}"; do
    source_node=$(vm_node "$vmid")
    source_pve=$(_ip_to_pve_node "$source_node")
    if [[ -z "$source_node" ]]; then
        failed_migration+=("$vmid")
        warn "source introuvable pour VM $vmid — migration ignorée"
        continue
    fi

    target="${TARGET_NODES[$((target_idx % ${#TARGET_NODES[@]}))]}"
    if [[ "$target" == "$source_node" ]]; then
        for candidate in "${OMEGA_NODES_ARR[@]}"; do
            if [[ "$candidate" != "$source_node" ]]; then
                target="$candidate"
                break
            fi
        done
    fi
    target_pve=$(_ip_to_pve_node "$target")
    ((target_idx++)) || true
    # Éjecter les CD-ROMs — qm doit être lancé sur le nœud source réel de la VM.
    ssh_run "$source_node" \
        "qm config $vmid 2>/dev/null | grep 'media=cdrom' | cut -d: -f1 | while read drv; do
             qm set $vmid \"--\${drv}\" none 2>/dev/null || true
         done" 2>/dev/null || true
    info "Migration VM $vmid : $source_node (pvesh: $source_pve) → $target (pvesh: $target_pve, live)..."
    t_vm=$SECONDS
    if ssh_run "$source_node" \
        "qm migrate $vmid $target_pve --online" 2>&1 | tee "/tmp/omega-m7-migrate-${vmid}.log"; then
        migrated+=("$vmid")
        info "VM $vmid migrée vers $target en $(elapsed $t_vm)s"
    else
        warn "Migration VM $vmid échouée — tentative offline..."
        if ssh_run "$source_node" \
            "qm migrate $vmid $target_pve" 2>&1; then
            migrated+=("$vmid")
            info "VM $vmid migrée offline vers $target en $(elapsed $t_vm)s"
        else
            failed_migration+=("$vmid")
            warn "Migration offline VM $vmid aussi échouée"
        fi
    fi
done

drain_duration=$(elapsed $t_drain)

step "Vérification état final"
echo ""
all_ok=true
for vmid in "${VMS_TO_DRAIN[@]}"; do
    node_final=$(vm_node "$vmid")
    vm_status=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "import sys,json; vms=[v for v in json.load(sys.stdin) if v['vmid']==$vmid]; print(vms[0]['status'] if vms else 'unknown')" 2>/dev/null || echo "unknown")
    if [[ "$node_final" != "$DRAIN_NODE" ]] && [[ "$vm_status" == "running" ]]; then
        echo -e "  ${GREEN}✓${RESET} VM $vmid : sur $node_final (running)"
    elif [[ "$node_final" == "$DRAIN_NODE" ]]; then
        echo -e "  ${RED}✗${RESET} VM $vmid : toujours sur $DRAIN_NODE"
        all_ok=false
    else
        echo -e "  ${YELLOW}?${RESET} VM $vmid : nœud=$node_final status=$vm_status"
    fi
done

step "État des stores après drain"
all_stores_status

step "Vérification que $DRAIN_NODE est vide"
vms_remaining=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
    python3 -c "
import sys,json
vms = json.load(sys.stdin)
count = sum(1 for v in vms if v.get('node')=='$DRAIN_NODE_PVE' and v.get('status')=='running')
print(count)
" 2>/dev/null || echo "?")
info "VMs encore sur $DRAIN_NODE : $vms_remaining"

info "Drain terminé en ${drain_duration}s"
info "Migrées : ${#migrated[@]}/${#VMS_TO_DRAIN[@]} | Échouées : ${#failed_migration[@]}"

[[ ${#failed_migration[@]} -eq 0 ]] || fail "M7 : ${#failed_migration[@]} migration(s) échouée(s) : ${failed_migration[*]}"
$all_ok || fail "M7 : certaines VMs sont toujours sur $DRAIN_NODE"
pass "M7 OK — nœud $DRAIN_NODE vidé en ${drain_duration}s, ${#migrated[@]} VMs migrées sans downtime"
