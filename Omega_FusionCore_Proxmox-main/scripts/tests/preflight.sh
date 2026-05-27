#!/usr/bin/env bash
# Checklist pré-vol — à exécuter avant tout test sur cluster réel
# Usage : ./preflight.sh [--cluster] [--gpu] [--ceph]
# Sans options : vérifie les prérequis locaux uniquement

source "$(dirname "$0")/lib.sh"

DO_CLUSTER=false; DO_GPU=false; DO_CEPH=false
for arg in "$@"; do
    case $arg in
        --cluster) DO_CLUSTER=true ;;
        --gpu)     DO_GPU=true ;;
        --ceph)    DO_CEPH=true ;;
    esac
done

header "Preflight — omega-remote-paging"

# ── 1. Binaires ───────────────────────────────────────────────────────────────
step "Binaires compilés"
require_omega_bins
pass "node-a-agent : $AGENT_BIN"
pass "node-bc-store : $STORE_BIN"

# ── 2. userfaultfd — vérifié sur chaque nœud du cluster ──────────────────────
step "userfaultfd"
for n in "${OMEGA_NODES_ARR[@]}"; do
    uffd=$(ssh_run "$n" \
        "sysctl -n vm.unprivileged_userfaultfd 2>/dev/null || echo 0" 2>/dev/null || echo "ERR")
    if [[ "$uffd" == "1" ]]; then
        pass "$n : vm.unprivileged_userfaultfd=1"
    elif [[ "$uffd" == "ERR" ]]; then
        warn "$n : SSH inaccessible — vérification ignorée"
    else
        fail "$n : vm.unprivileged_userfaultfd=$uffd — lancer: sysctl -w vm.unprivileged_userfaultfd=1"
    fi
done

# ── 3. Dépendances locales ────────────────────────────────────────────────────
step "Outils locaux"
for bin in curl nc python3 jq; do
    if command -v "$bin" &>/dev/null; then pass "$bin trouvé"; else warn "$bin absent"; fi
done
if command -v stress-ng &>/dev/null; then pass "stress-ng trouvé"
else warn "stress-ng absent (tests de charge impossible) — apt install stress-ng"; fi

# ── 4. Cluster Proxmox ────────────────────────────────────────────────────────
if $DO_CLUSTER; then
    step "Stores réseau"
    for n in "${OMEGA_NODES_ARR[@]}"; do
        if nc -zv "$n" "$STORE_PORT" 2>/dev/null; then pass "store $n:$STORE_PORT accessible"
        else fail "store $n:$STORE_PORT inaccessible — démarrer omega-daemon sur $n"; fi
    done

    step "HTTP status stores"
    for n in "${OMEGA_NODES_ARR[@]}"; do
        if curl -sf "http://$n:$STATUS_PORT/api/health" &>/dev/null; then
            pass "status HTTP $n:$STATUS_PORT/api/health OK"
            curl -sf "http://$n:$STATUS_PORT/api/status" | python3 -m json.tool 2>/dev/null | head -8 | sed 's/^/    /' || true
        else warn "status HTTP $n:$STATUS_PORT inaccessible (omega-daemon pas encore démarré ?)"; fi
    done

    step "Proxmox CLI (sur les nœuds)"
    for n in "${OMEGA_NODES_ARR[@]}"; do
        qm_ok=$(ssh_run "$n" "command -v qm &>/dev/null && echo yes || echo no" 2>/dev/null || echo "ERR")
        if [[ "$qm_ok" == "yes" ]]; then
            pass "$n : qm disponible"
        elif [[ "$qm_ok" == "ERR" ]]; then
            warn "$n : SSH inaccessible"
        else
            fail "$n : qm absent — Proxmox VE requis"
        fi
    done

    pvesh_node="${OMEGA_NODES_ARR[0]}"
    count=$(ssh_run "$pvesh_node" \
        "pvesh get /cluster/resources --type vm --output-format json 2>/dev/null \
        | python3 -c 'import sys,json; print(len(json.load(sys.stdin)))'" 2>/dev/null || echo "?")
    info "$count VMs dans le cluster (via $pvesh_node)"

    step "Pool vCPU"
    if [[ -f /run/omega-vcpu-pool.json ]]; then
        info "pool existant :"
        cat /run/omega-vcpu-pool.json | python3 -m json.tool 2>/dev/null | sed 's/^/    /'
    else info "/run/omega-vcpu-pool.json absent (normal si premier démarrage)"; fi
fi

# ── 5. GPU ────────────────────────────────────────────────────────────────────
if $DO_GPU; then
    step "GPU PCI (classe 0x03xx)"
    gpu_devs=$(ls /sys/bus/pci/devices/*/class 2>/dev/null \
        | xargs grep -l "^0x03" 2>/dev/null || true)
    if [[ -n "$gpu_devs" ]]; then
        for dev in $gpu_devs; do
            pci=$(basename "$(dirname "$dev")")
            info "GPU PCI : $pci"
            pass "GPU détecté : $pci"
        done
    else warn "aucun GPU PCI (classe 0x03xx) sur ce nœud — tests GPU non applicables"; fi

    step "Locks GPU existants"
    locks=$(ls /run/omega-gpu-scheduler-*.lock 2>/dev/null || echo "")
    if [[ -n "$locks" ]]; then info "locks actifs : $locks"
    else info "aucun lock GPU (normal si pas de scheduler actif)"; fi
fi

# ── 6. Ceph ───────────────────────────────────────────────────────────────────
if $DO_CEPH; then
    step "Ceph"
    if [[ -f /etc/ceph/ceph.conf ]]; then
        pass "/etc/ceph/ceph.conf présent"
        if command -v ceph &>/dev/null; then
            ceph status 2>/dev/null | head -5 | sed 's/^/    /' || warn "ceph status échoué"
        fi
        if ldconfig -p 2>/dev/null | grep -q librados; then
            pass "librados.so détecté"
        else warn "librados.so absent — apt install librados-dev pour activer le store Ceph"; fi
    else warn "/etc/ceph/ceph.conf absent — Ceph non disponible (store RAM sera utilisé)"; fi
fi

echo ""
info "Preflight terminé."
if $DO_CLUSTER; then
    echo -e "${BOLD}  → Lancer les tests cluster : ./run-cluster.sh${RESET}"
else
    echo -e "${BOLD}  → Lancer les tests locaux : ./run-local.sh${RESET}"
    echo -e "${BOLD}  → Lancer le preflight cluster : ./preflight.sh --cluster [--gpu] [--ceph]${RESET}"
fi
