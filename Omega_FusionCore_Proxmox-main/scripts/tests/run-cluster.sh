#!/usr/bin/env bash
# Lance tous les tests sur cluster Proxmox (virtuel ou physique)
# Usage : ./run-cluster.sh [vmid] [--gpu] [--ceph] [--long] [--destructive] [--skip M3,M7] [--drain-node NODE]
#
# Variables d'environnement :
#   OMEGA_NODES=NODE1,NODE2,NODE3                   — tous les nœuds (N >= 2)
#   OMEGA_CONTROLLER=NODE1                          — nœud contrôleur (défaut: premier)
#   OMEGA_TEST_VMID=9001                            — VM principale pour les tests
#   OMEGA_BIN_DIR=/usr/local/bin                    — si binaires déployés
#   OMEGA_REMOTE_BIN_DIR=/usr/local/bin             — binaires sur les nœuds distants
#   OMEGA_SKIP=M3,M7                                — tests à ignorer
#   OMEGA_SCALE_VMIDS=9001,...,9500                 — VMs pour le test 31
#   OMEGA_SCALE_TARGET=500                          — nombre cible du test 31
#
# Tests 01-04 et 10 s'exécutent sur CONTROLLER_NODE (userfaultfd requis).
# Les tests cluster (05, 08, 09, Mx) s'exécutent aussi sur le nœud contrôleur
# pour disposer de qm/pvesh et des stores réseau actifs.

source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"; shift || true
DO_GPU=false; DO_CEPH=false; DO_LONG=false; DO_DESTRUCTIVE=false
DO_SCALE=false
SKIP_LIST="${OMEGA_SKIP:-}"
DRAIN_NODE=""

for arg in "$@"; do
    case $arg in
        --gpu)          DO_GPU=true ;;
        --ceph)         DO_CEPH=true ;;
        --long)         DO_LONG=true ;;
        --scale)        DO_SCALE=true ;;
        --destructive)  DO_DESTRUCTIVE=true ;;
        --skip=*)       SKIP_LIST="${arg#--skip=}" ;;
        --drain-node=*) DRAIN_NODE="${arg#--drain-node=}" ;;
    esac
done

DRAIN_NODE="${DRAIN_NODE:-$CONTROLLER_NODE}"

should_skip() { [[ ",$SKIP_LIST," == *",$1,"* ]]; }

quote_args() {
    printf ' %q' "$@"
}

remote_env() {
    printf '%q=%q ' OMEGA_NODES "$OMEGA_NODES"
    printf '%q=%q ' OMEGA_CONTROLLER "${OMEGA_CONTROLLER:-$CONTROLLER_NODE}"
    printf '%q=%q ' OMEGA_TEST_VMID "$VMID"
    printf '%q=%q ' OMEGA_TEST_VMIDS "${OMEGA_TEST_VMIDS:-$VMID}"
    printf '%q=%q ' OMEGA_BIN_DIR "$REMOTE_BINS_DIR"
    printf '%q=%q ' OMEGA_REMOTE_BIN_DIR "$REMOTE_BINS_DIR"
    printf '%q=%q ' OMEGA_STORE_PORT "$STORE_PORT"
    printf '%q=%q ' OMEGA_STATUS_PORT "$STATUS_PORT"
    printf '%q=%q ' OMEGA_METRICS_PORT "$METRICS_PORT"
    printf '%q=%q ' OMEGA_GPU_PRIMARY_NODE "${OMEGA_GPU_PRIMARY_NODE:-}"
    printf '%q=%q ' OMEGA_GPU_PROXY_URL "${OMEGA_GPU_PROXY_URL:-}"
    printf '%q=%q ' OMEGA_GPU_PROXY_LISTEN "${OMEGA_GPU_PROXY_LISTEN:-0.0.0.0:9400}"
    printf '%q=%q ' OMEGA_GPU_PROXY_BACKEND_COMMAND "${OMEGA_GPU_PROXY_BACKEND_COMMAND:-${REMOTE_BINS_DIR}/omega-gpu-worker-app.py}"
    printf '%q=%q ' OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS "${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS:-900}"
    printf '%q=%q ' OMEGA_GPU_PROXY_TOTAL_VRAM_MIB "${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}"
    printf '%q=%q ' OMEGA_GPU_PROXY_MAX_CONCURRENT "${OMEGA_GPU_PROXY_MAX_CONCURRENT:-1}"
    printf '%q=%q ' OMEGA_GPU_PROXY_API_TOKEN_FILE "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-/etc/omega/gpu-proxy.token}"
    printf '%q=%q ' OMEGA_GPU_PROXY_API_TOKEN "${OMEGA_GPU_PROXY_API_TOKEN:-}"
    printf '%q=%q ' OMEGA_GPU_MIGRATE_TO_GPU_NODE "${OMEGA_GPU_MIGRATE_TO_GPU_NODE:-1}"
    printf '%q=%q ' OMEGA_GPU_FALLBACK_NETWORK "${OMEGA_GPU_FALLBACK_NETWORK:-1}"
    printf '%q=%q ' OMEGA_GPU_PROXY_TEST_VM_COUNT "${OMEGA_GPU_PROXY_TEST_VM_COUNT:-3}"
    printf '%q=%q ' DEPLOY_USER "$DEPLOY_USER"
    printf '%q=%q ' VMID "$VMID"
    printf '%q=%q ' CLUSTER_MODE "1"
    $DO_DESTRUCTIVE && printf '%q=%q ' OMEGA_DESTRUCTIVE "1"
    $DO_LONG && printf '%q=%q ' OMEGA_SOAK_SECS "${OMEGA_SOAK_SECS:-1800}"
    $DO_SCALE && printf '%q=%q ' OMEGA_SCALE_ENABLED "1"
    printf '%q=%q ' OMEGA_SCALE_VMIDS "${OMEGA_SCALE_VMIDS:-${OMEGA_TEST_VMIDS:-$VMID}}"
    printf '%q=%q ' OMEGA_SCALE_TARGET "${OMEGA_SCALE_TARGET:-500}"
    printf '%q=%q ' OMEGA_SCALE_BATCH_SIZE "${OMEGA_SCALE_BATCH_SIZE:-20}"
    printf '%q=%q ' OMEGA_SCALE_SOAK_SECS "${OMEGA_SCALE_SOAK_SECS:-1800}"
}

rsync_ssh_cmd() {
    printf 'ssh'
    for opt in "${SSH_OPTS[@]}"; do
        printf ' %q' "$opt"
    done
}

TESTS_DIR="$(dirname "$0")"
PASS=0; FAIL=0; SKIP=0
RESULTS=()

# ── Synchronisation scripts + binaires vers le nœud contrôleur ────────────────
REMOTE_TESTS_DIR="/tmp/omega-tests"
REMOTE_BINS_DIR="/tmp/omega-tests-bins"

sync_to_controller() {
    info "Synchronisation vers ${CONTROLLER_NODE}..."
    rsync -aq -e "$(rsync_ssh_cmd)" --delete "${TESTS_DIR}/" "${DEPLOY_USER}@${CONTROLLER_NODE}:${REMOTE_TESTS_DIR}/"
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${CONTROLLER_NODE}" "mkdir -p ${REMOTE_BINS_DIR}"
    for bin in node-a-agent node-bc-store omega-gpu-proxy; do
        local local_bin="${REPO_ROOT}/target/release/${bin}"
        if [[ -x "$local_bin" ]]; then
            rsync -aq -e "$(rsync_ssh_cmd)" "$local_bin" "${DEPLOY_USER}@${CONTROLLER_NODE}:${REMOTE_BINS_DIR}/${bin}"
        fi
    done
    for worker in omega-gpu-worker-cpu.py omega-gpu-worker-app.py; do
        local worker_path="${REPO_ROOT}/scripts/${worker}"
        if [[ -f "$worker_path" ]]; then
            rsync -aq -e "$(rsync_ssh_cmd)" "$worker_path" "${DEPLOY_USER}@${CONTROLLER_NODE}:${REMOTE_BINS_DIR}/${worker}"
            ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${CONTROLLER_NODE}" "chmod +x ${REMOTE_BINS_DIR}/${worker}" || true
        fi
    done
    pass "scripts + binaires synchronisés sur ${CONTROLLER_NODE}"
}

# ── Exécution locale (tests ne nécessitant pas de cluster ni userfaultfd) ──────
run_test() {
    local num="$1" name="$2" script="$3"; shift 3
    if should_skip "$num"; then
        warn "Test $num ($name) — ignoré"
        RESULTS+=("SKIP  $num $name")
        ((SKIP++)) || true
        return
    fi
    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}  Test $num — $name${RESET}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    t0=$SECONDS
    if bash "$script" "$@"; then
        RESULTS+=("${GREEN}PASS${RESET}  $num $name ($(( SECONDS - t0 ))s)")
        ((PASS++)) || true
    else
        RESULTS+=("${RED}FAIL${RESET}  $num $name ($(( SECONDS - t0 ))s)")
        ((FAIL++)) || true
    fi
    sleep 2
}

# ── Exécution distante — daemon omega reste actif ─────────────────────────────
# Pour tests cluster : qm/pvesh disponibles, stores réseau actifs
run_test_on_node() {
    local num="$1" name="$2" script="$3"; shift 3
    if should_skip "$num"; then
        warn "Test $num ($name) — ignoré"
        RESULTS+=("SKIP  $num $name")
        ((SKIP++)) || true
        return
    fi
    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}  Test $num — $name  [${CONTROLLER_NODE}]${RESET}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    t0=$SECONDS
    local exit_code=0
    local remote_script="${REMOTE_TESTS_DIR}/$(basename "$script")"
    # Passer les arguments en chaîne (pas de tableaux via SSH)
    local args_str
    args_str="$(quote_args "$@")"
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=10 -o BatchMode=yes "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "$(remote_env) bash $(printf '%q' "$remote_script")${args_str}" \
        || exit_code=$?
    if [[ $exit_code -eq 0 ]]; then
        RESULTS+=("${GREEN}PASS${RESET}  $num $name [remote] ($(( SECONDS - t0 ))s)")
        ((PASS++)) || true
    else
        RESULTS+=("${RED}FAIL${RESET}  $num $name [remote] ($(( SECONDS - t0 ))s)")
        ((FAIL++)) || true
    fi
    sleep 2
}

# ── Exécution distante isolée — arrête omega-daemon le temps du test ──────────
# Pour tests qui démarrent leurs propres stores sur les ports 9100-9202.
# Après le test (succès ou échec), omega-daemon est redémarré.
run_test_on_node_isolated() {
    local num="$1" name="$2" script="$3"; shift 3
    if should_skip "$num"; then
        warn "Test $num ($name) — ignoré"
        RESULTS+=("SKIP  $num $name")
        ((SKIP++)) || true
        return
    fi
    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}  Test $num — $name  [${CONTROLLER_NODE}, isolé]${RESET}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    t0=$SECONDS

    # Stopper omega-daemon pour libérer 9100-9202
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "systemctl stop omega-daemon 2>/dev/null || true" || true

    local exit_code=0
    local remote_script="${REMOTE_TESTS_DIR}/$(basename "$script")"
    local args_str
    args_str="$(quote_args "$@")"
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=10 -o BatchMode=yes "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "$(remote_env) bash $(printf '%q' "$remote_script")${args_str}" \
        || exit_code=$?

    # Redémarrer omega-daemon dans tous les cas
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "systemctl start omega-daemon 2>/dev/null || true; sleep 2" || true

    if [[ $exit_code -eq 0 ]]; then
        RESULTS+=("${GREEN}PASS${RESET}  $num $name [remote/isolé] ($(( SECONDS - t0 ))s)")
        ((PASS++)) || true
    else
        RESULTS+=("${RED}FAIL${RESET}  $num $name [remote/isolé] ($(( SECONDS - t0 ))s)")
        ((FAIL++)) || true
    fi
    sleep 2
}

# ── En-tête ────────────────────────────────────────────────────────────────────
header "Tests cluster — omega-remote-paging"
print_cluster_config
info "VM test      : $VMID"
info "GPU          : $DO_GPU"
info "Ceph         : $DO_CEPH"
info "Long         : $DO_LONG"
info "Scale        : $DO_SCALE"
info "Destructif   : $DO_DESTRUCTIVE"
info "Skip         : ${SKIP_LIST:-aucun}"
info "Nœuds        : $(node_count) ($(store_count) store(s))"

# ── Preflight ─────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}  Preflight${RESET}"
flags="--cluster"
$DO_GPU  && flags="$flags --gpu"
$DO_CEPH && flags="$flags --ceph"
bash "$TESTS_DIR/preflight.sh" $flags || fail "preflight échoué — corriger les prérequis"

# ── Tests unitaires (local — cargo test) ──────────────────────────────────────
run_test "00" "Tests unitaires" "$TESTS_DIR/00-unit-tests.sh"

# ── Synchronisation vers le nœud contrôleur ───────────────────────────────────
sync_to_controller

# ── Tests store + agent locaux (isolés — daemon arrêté temporairement) ────────
# Ces tests démarrent leurs propres stores sur 9100-9202.
run_test_on_node_isolated "01" "Smoke test"           "$TESTS_DIR/01-smoke-test.sh"
run_test_on_node_isolated "02" "Réplication 2 stores" "$TESTS_DIR/02-replication.sh"
run_test_on_node_isolated "03" "Failover store"       "$TESTS_DIR/03-failover.sh"
run_test_on_node_isolated "04" "Éviction daemon"      "$TESTS_DIR/04-eviction-daemon.sh" 20
run_test_on_node_isolated "10" "Multi-VM 3 agents"    "$TESTS_DIR/10-multi-vm.sh"
run_test_on_node_isolated "18" "Recall LIFO"          "$TESTS_DIR/18-recall-lifo.sh"
run_test_on_node_isolated "20" "Prefetch stride"      "$TESTS_DIR/20-prefetch-stride.sh"
run_test_on_node_isolated "21" "TLS TOFU"             "$TESTS_DIR/21-tls-tofu.sh"
run_test_on_node_isolated "23" "Disk I/O scheduler"   "$TESTS_DIR/23-disk-io-scheduler.sh"

# ── Normalisation des VMs de test avant les tests cluster ────────────────────
# Évite que chaque test retombe sur une VM ancienne encore lancée avec
# -smp maxcpus=1 malgré une description Omega correcte.
run_test_on_node "30A" "Normalisation VMs Omega" "$TESTS_DIR/30-vm-conformity.sh" "${OMEGA_TEST_VMIDS:-$VMID}"

# ── Tests cluster (daemon actif, qm/pvesh disponibles) ────────────────────────
run_test_on_node "05" "vCPU élastique"              "$TESTS_DIR/05-vcpu-elastic.sh"         "$VMID"
run_test_on_node "08" "Migration RAM"               "$TESTS_DIR/08-migration-ram.sh"        "$VMID"
run_test_on_node "09" "Orphan cleaner"              "$TESTS_DIR/09-orphan-cleaner.sh"
run_test_on_node "19" "Compaction cluster"          "$TESTS_DIR/19-compaction.sh"           "$VMID"
run_test_on_node "22" "Balloon thin-provisioning"   "$TESTS_DIR/22-balloon-thinprov.sh"     "$VMID"
run_test_on_node "23C" "Disk I/O scheduler cluster" "$TESTS_DIR/23-disk-io-scheduler.sh"    "$VMID"
run_test_on_node "30" "Conformité VMs Omega"        "$TESTS_DIR/30-vm-conformity.sh"        "${OMEGA_TEST_VMIDS:-$VMID}"
run_test_on_node "24" "Installation doctor"         "$TESTS_DIR/24-install-doctor.sh"
run_test_on_node "25" "Réseau VM invitée"           "$TESTS_DIR/25-vm-network.sh"            "$VMID"

# ── Tests GPU (optionnels) ─────────────────────────────────────────────────────
if $DO_GPU; then
    run_test_on_node "06" "GPU placement"           "$TESTS_DIR/06-gpu-placement.sh"        "$VMID"
    GPU_SECOND_VMID="${TEST_VMIDS_ARR[1]:-}"
    if [[ -n "$GPU_SECOND_VMID" ]]; then
        run_test_on_node "07" "GPU scheduler"       "$TESTS_DIR/07-gpu-scheduler.sh"        "$VMID" "$GPU_SECOND_VMID"
    else
        warn "Test 07 (GPU scheduler) — ignoré, OMEGA_TEST_VMIDS doit contenir au moins 2 VMs"
        RESULTS+=("SKIP  07 GPU scheduler (2e VM absente)")
        ((SKIP++)) || true
    fi
    run_test_on_node "27" "GPU réel / rendu minimal" "$TESTS_DIR/27-gpu-real-render.sh"      "$VMID"
    run_test_on_node "32" "GPU proxy applicatif"      "$TESTS_DIR/32-gpu-proxy.sh"           "$VMID"
else
    for t in "06:GPU placement" "07:GPU scheduler" "27:GPU réel / rendu minimal" "32:GPU proxy applicatif"; do
        num="${t%:*}"; name="${t#*:}"
        RESULTS+=("SKIP  $num $name (passer --gpu)")
        ((SKIP++)) || true
    done
fi

if $DO_CEPH; then
    run_test_on_node "26" "Ceph réel"               "$TESTS_DIR/26-ceph-real.sh"
else
    RESULTS+=("SKIP  26 Ceph réel (passer --ceph)")
    ((SKIP++)) || true
fi

# ── Tests mixtes ──────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${BLUE}  ── Tests mixtes ──────────────────────────────────${RESET}"

run_test_on_node "M1" "RAM + CPU simultanés"          "$TESTS_DIR/11-mixed-ram-cpu.sh"              "$VMID"
run_test_on_node "M2" "CPU+RAM saturés → migration"   "$TESTS_DIR/12-mixed-cpu-ram-migration.sh"    "$VMID"
run_test_on_node "M4" "Stress cluster complet"        "$TESTS_DIR/14-mixed-cluster-pressure.sh"     "$(node_count)" 60
run_test_on_node "M5" "Migration live sous pression"  "$TESTS_DIR/15-mixed-live-migration-pressure.sh" "$VMID"
run_test_on_node "M6" "Rafale démarrages simultanés"  "$TESTS_DIR/16-mixed-burst-starts.sh"         6
run_test_on_node "M7" "Drain nœud complet"            "$TESTS_DIR/17-mixed-drain-node.sh"           "$DRAIN_NODE"

# ── Tests GPU mixtes (optionnels) ─────────────────────────────────────────────
if $DO_GPU; then
    run_test_on_node "M3" "GPU + CPU multi-contraintes" "$TESTS_DIR/13-mixed-gpu-cpu.sh"    "$VMID"
else
    RESULTS+=("SKIP  M3 GPU+CPU multi-contraintes (passer --gpu)")
    ((SKIP++)) || true
fi

# ── Tests destructifs / longue durée ─────────────────────────────────────────
if $DO_DESTRUCTIVE; then
    run_test_on_node "28" "Partition réseau contrôlée" "$TESTS_DIR/28-network-partition.sh" "${OMEGA_NODES_ARR[1]:-}"
else
    RESULTS+=("SKIP  28 Partition réseau contrôlée (passer --destructive)")
    ((SKIP++)) || true
fi

if $DO_LONG; then
    run_test_on_node "29" "Soak long physique" "$TESTS_DIR/29-long-run-soak.sh" "$VMID" "${OMEGA_SOAK_SECS:-1800}"
else
    RESULTS+=("SKIP  29 Soak long physique (passer --long)")
    ((SKIP++)) || true
fi

if $DO_SCALE; then
    run_test_on_node "31" "Scalabilité VMs physiques" "$TESTS_DIR/31-scale-vms.sh" \
        "${OMEGA_SCALE_VMIDS:-${OMEGA_TEST_VMIDS:-$VMID}}" \
        "${OMEGA_SCALE_TARGET:-500}" \
        "${OMEGA_SCALE_BATCH_SIZE:-20}" \
        "${OMEGA_SCALE_SOAK_SECS:-1800}"
else
    RESULTS+=("SKIP  31 Scalabilité VMs physiques (passer --scale)")
    ((SKIP++)) || true
fi

# ── Résumé ────────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
echo -e "${BOLD}  Résumé — cluster $(node_count) nœuds${RESET}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
for r in "${RESULTS[@]}"; do echo -e "  $r"; done
echo ""
echo -e "  ${GREEN}PASS${RESET}: $PASS  ${RED}FAIL${RESET}: $FAIL  ${YELLOW}SKIP${RESET}: $SKIP"

[[ $FAIL -eq 0 ]] && \
    echo -e "\n${GREEN}${BOLD}  ✓ Tous les tests cluster passent${RESET}" || \
    { echo -e "\n${RED}${BOLD}  ✗ $FAIL test(s) échoué(s)${RESET}"; exit 1; }
