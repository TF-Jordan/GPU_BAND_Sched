#!/usr/bin/env bash
# omega-lab.sh — Lab interactif : configuration + installation + tests
#
# Usage : ./scripts/omega-lab.sh [--gpu] [--ceph] [--long] [--scale] [--destructive] [--provision] [--production] [--auto]
#
# Le script lit scripts/cluster.conf pour la configuration du cluster.
# Si le fichier n'existe pas ou si les nœuds ne sont pas encore définis,
# le menu propose de les saisir directement.
#
# Options :
#   --gpu    activer les tests GPU
#   --ceph   activer les tests Ceph
#   --long   active les tests longue durée
#   --scale  active le test de scalabilité VMs physiques
#   --destructive active les tests qui modifient temporairement le réseau/services
#   --provision cree les VMs physiques configurees avant les tests en mode --auto
#   --production validation complete stricte: GPU/Ceph/long/scale/destructif, aucun skip accepte
#   --auto   toutes les sections sans pause (mode CI)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESTS_DIR="${SCRIPT_DIR}/tests"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONF_FILE="${SCRIPT_DIR}/cluster.conf"

# ── Options CLI ───────────────────────────────────────────────────────────────
DO_GPU=false; DO_CEPH=false; DO_LONG=false; DO_SCALE=false; DO_DESTRUCTIVE=false; DO_PROVISION=false; DO_PRODUCTION=false; AUTO=false
for arg in "$@"; do
    case "$arg" in
        --gpu)         DO_GPU=true  ;;
        --ceph)        DO_CEPH=true ;;
        --long)        DO_LONG=true ;;
        --scale)       DO_SCALE=true ;;
        --destructive) DO_DESTRUCTIVE=true ;;
        --provision)   DO_PROVISION=true ;;
        --production)  DO_PRODUCTION=true; DO_GPU=true; DO_CEPH=true; DO_LONG=true; DO_SCALE=true; DO_DESTRUCTIVE=true ;;
        --auto)        AUTO=true    ;;
    esac
done

# ── Couleurs ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
DIM='\033[2m'; MAG='\033[0;35m'

_ok()   { echo -e "${GREEN}[OK]${RESET}  $*"; }
_err()  { echo -e "${RED}[ERR]${RESET} $*"; }
_info() { echo -e "${CYAN}[INF]${RESET} $*"; }
_warn() { echo -e "${YELLOW}[WRN]${RESET} $*"; }
_sep()  { echo -e "${DIM}────────────────────────────────────────────────────────────${RESET}"; }
_hdr()  { echo -e "\n${BOLD}${BLUE}$*${RESET}"; }
_ask()  { echo -en "${YELLOW}  → ${RESET}$* : "; }

# ── Chargement/rechargement de la configuration ───────────────────────────────
# Appelé à chaque modification de cluster.conf et au démarrage.
OMEGA_NODES=""
OMEGA_CONTROLLER=""
OMEGA_TEST_VMID="9001"
OMEGA_TEST_VMIDS=""
OMEGA_PROVISION_VMIDS=""
OMEGA_VM_STORAGE="ceph-vm"
OMEGA_VM_BRIDGE="vmbr0"
OMEGA_VM_IMAGE_REMOTE="/var/lib/vz/template/iso/debian-12-generic-amd64.qcow2"
OMEGA_VM_IMAGE_LOCAL=""
OMEGA_VM_SSHKEY_REMOTE="/root/.ssh/id_rsa.pub"
OMEGA_VM_MEMORY=2048
OMEGA_VM_BALLOON=512
OMEGA_VM_CORES=4
OMEGA_VM_VCPUS=1
OMEGA_VM_DISK_MAX_GIB=20
OMEGA_VM_GPU_VRAM_MIB=0
OMEGA_GPU_NODES=""
OMEGA_GPU_PRIMARY_NODE=""
OMEGA_GPU_PROXY_URL=""
OMEGA_GPU_PROXY_LISTEN="0.0.0.0:9400"
OMEGA_GPU_PROXY_MAX_CONCURRENT=1
OMEGA_GPU_PROXY_TOTAL_VRAM_MIB=0
OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS=900
OMEGA_GPU_MIGRATE_TO_GPU_NODE=1
OMEGA_GPU_FALLBACK_NETWORK=1
OMEGA_GPU_PROXY_API_TOKEN_FILE="/etc/omega/gpu-proxy.token"
OMEGA_GPU_PROXY_API_TOKEN=""
OMEGA_SCALE_STEPS=""
OMEGA_SCALE_STOP_AFTER=0
OMEGA_SCALE_DESTROY_AFTER=0
OMEGA_SCALE_CLEANUP_SCOPE="started"
OMEGA_TEST_TIMEOUT_MULTIPLIER=1
OMEGA_MIGRATION_TIMEOUT_SECS=120
OMEGA_CEPH_TIMEOUT_SECS=60
OMEGA_DISK_TIMEOUT_SECS=60
OMEGA_SCALE_TIMEOUT_SECS=1800
DEPLOY_USER="root"
STORE_PORT="9100"
STATUS_PORT="9200"
NODES_ARR=()
TEST_VMIDS_ARR=()
CONTROLLER_NODE=""
STORES_CSV=""
STATUS_CSV=""
CONFIGURED=false

_load_config() {
    # Priorité : variables d'environnement exportées > cluster.conf
    # printenv ne retourne que les vraies variables exportées, pas les variables shell de session
    local env_nodes
    env_nodes="$(printenv OMEGA_NODES 2>/dev/null || true)"
    [[ -f "$CONF_FILE" ]] && source "$CONF_FILE" 2>/dev/null || true
    [[ -n "$env_nodes" ]] && OMEGA_NODES="$env_nodes"

    OMEGA_NODES="${OMEGA_NODES:-}"
    OMEGA_TEST_VMID="${OMEGA_TEST_VMID:-9001}"
    OMEGA_TEST_VMIDS="${OMEGA_TEST_VMIDS:-$OMEGA_TEST_VMID}"
    OMEGA_PROVISION_VMIDS="${OMEGA_PROVISION_VMIDS:-$OMEGA_TEST_VMIDS}"
    OMEGA_VM_STORAGE="${OMEGA_VM_STORAGE:-ceph-vm}"
    OMEGA_VM_BRIDGE="${OMEGA_VM_BRIDGE:-vmbr0}"
    OMEGA_VM_IMAGE_REMOTE="${OMEGA_VM_IMAGE_REMOTE:-/var/lib/vz/template/iso/debian-12-generic-amd64.qcow2}"
    OMEGA_VM_IMAGE_LOCAL="${OMEGA_VM_IMAGE_LOCAL:-}"
    OMEGA_VM_SSHKEY_REMOTE="${OMEGA_VM_SSHKEY_REMOTE:-/root/.ssh/id_rsa.pub}"
    OMEGA_VM_MEMORY="${OMEGA_VM_MEMORY:-2048}"
    OMEGA_VM_BALLOON="${OMEGA_VM_BALLOON:-512}"
    OMEGA_VM_CORES="${OMEGA_VM_CORES:-4}"
    OMEGA_VM_SOCKETS="${OMEGA_VM_SOCKETS:-1}"
    OMEGA_VM_VCPUS="${OMEGA_VM_VCPUS:-1}"
    OMEGA_VM_DISK_MAX_GIB="${OMEGA_VM_DISK_MAX_GIB:-20}"
    OMEGA_VM_GPU_VRAM_MIB="${OMEGA_VM_GPU_VRAM_MIB:-0}"
    OMEGA_GPU_NODES="${OMEGA_GPU_NODES:-}"
    OMEGA_GPU_PRIMARY_NODE="${OMEGA_GPU_PRIMARY_NODE:-}"
    OMEGA_GPU_PROXY_URL="${OMEGA_GPU_PROXY_URL:-}"
    OMEGA_GPU_PROXY_LISTEN="${OMEGA_GPU_PROXY_LISTEN:-0.0.0.0:9400}"
    OMEGA_GPU_PROXY_MAX_CONCURRENT="${OMEGA_GPU_PROXY_MAX_CONCURRENT:-1}"
    OMEGA_GPU_PROXY_TOTAL_VRAM_MIB="${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}"
    OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS="${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS:-900}"
    OMEGA_GPU_MIGRATE_TO_GPU_NODE="${OMEGA_GPU_MIGRATE_TO_GPU_NODE:-1}"
    OMEGA_GPU_FALLBACK_NETWORK="${OMEGA_GPU_FALLBACK_NETWORK:-1}"
    OMEGA_GPU_PROXY_API_TOKEN_FILE="${OMEGA_GPU_PROXY_API_TOKEN_FILE:-/etc/omega/gpu-proxy.token}"
    OMEGA_GPU_PROXY_API_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
    OMEGA_SCALE_STEPS="${OMEGA_SCALE_STEPS:-}"
    OMEGA_SCALE_STOP_AFTER="${OMEGA_SCALE_STOP_AFTER:-0}"
    OMEGA_SCALE_DESTROY_AFTER="${OMEGA_SCALE_DESTROY_AFTER:-0}"
    OMEGA_SCALE_CLEANUP_SCOPE="${OMEGA_SCALE_CLEANUP_SCOPE:-started}"
    OMEGA_TEST_TIMEOUT_MULTIPLIER="${OMEGA_TEST_TIMEOUT_MULTIPLIER:-1}"
    OMEGA_MIGRATION_TIMEOUT_SECS="${OMEGA_MIGRATION_TIMEOUT_SECS:-120}"
    OMEGA_CEPH_TIMEOUT_SECS="${OMEGA_CEPH_TIMEOUT_SECS:-60}"
    OMEGA_DISK_TIMEOUT_SECS="${OMEGA_DISK_TIMEOUT_SECS:-60}"
    OMEGA_SCALE_TIMEOUT_SECS="${OMEGA_SCALE_TIMEOUT_SECS:-1800}"
    IFS=',' read -ra TEST_VMIDS_ARR <<< "$OMEGA_TEST_VMIDS"
    DEPLOY_USER="${DEPLOY_USER:-root}"
    STORE_PORT="${STORE_PORT:-9100}"
    STATUS_PORT="${STATUS_PORT:-9200}"
    SSH_KEY="${SSH_KEY:-${HOME}/.ssh/omega_ed25519}"

    # Options SSH/rsync communes (clé dédiée si elle existe)
    if [[ -f "$SSH_KEY" ]]; then
        SSH_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=accept-new)
        RSYNC_OPTS=(-aq -e "ssh -i ${SSH_KEY} -o StrictHostKeyChecking=accept-new")
    else
        SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
        RSYNC_OPTS=(-aq)
    fi

    if [[ -n "$OMEGA_NODES" ]]; then
        IFS=',' read -ra NODES_ARR <<< "$OMEGA_NODES"
        CONTROLLER_NODE="${OMEGA_CONTROLLER:-${NODES_ARR[0]}}"
        STORES_CSV=""; STATUS_CSV=""
        for n in "${NODES_ARR[@]}"; do
            STORES_CSV="${STORES_CSV:+$STORES_CSV,}${n}:${STORE_PORT}"
            STATUS_CSV="${STATUS_CSV:+$STATUS_CSV,}${n}:${STATUS_PORT}"
        done
        CONFIGURED=true
    else
        NODES_ARR=(); CONTROLLER_NODE=""; STORES_CSV=""; STATUS_CSV=""
        CONFIGURED=false
    fi
}
_load_config

# ── Résultats tests ───────────────────────────────────────────────────────────
declare -A RESULTS=()
declare -A DURATIONS=()
TOTAL_PASS=0; TOTAL_FAIL=0; TOTAL_SKIP=0

reset_results() {
    RESULTS=()
    DURATIONS=()
    TOTAL_PASS=0
    TOTAL_FAIL=0
    TOTAL_SKIP=0
}

# ── Enregistrement d'un résultat ──────────────────────────────────────────────
_record() {
    local num="$1" name="$2" rc="$3" elapsed="$4"
    DURATIONS["$num"]="$elapsed"
    if [[ "$rc" -eq 0 ]]; then
        RESULTS["$num"]="PASS"; ((TOTAL_PASS++)) || true
        echo -e "\n  ${GREEN}✓ PASS${RESET}  ${num} — ${name}  (${elapsed}s)"
    else
        RESULTS["$num"]="FAIL"; ((TOTAL_FAIL++)) || true
        echo -e "\n  ${RED}✗ FAIL${RESET}  ${num} — ${name}  (${elapsed}s) [code $rc]"
    fi
}

# ── Exécution des tests ───────────────────────────────────────────────────────
_need_config() {
    if ! $CONFIGURED; then
        _warn "Cluster non configuré. Utilisez [c] pour définir les nœuds."
        return 1
    fi
    return 0
}

_sync() {
    _need_config || return
    _info "Sync scripts + binaires → ${OMEGA_NODES}..."
    local node bin b
    for node in "${NODES_ARR[@]}"; do
        [[ -n "$node" ]] || continue
        rsync "${RSYNC_OPTS[@]}" --delete "${TESTS_DIR}/" "${DEPLOY_USER}@${node}:/tmp/omega-tests/"
        ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${node}" "mkdir -p /tmp/omega-tests-bins" 2>/dev/null || true
        for bin in node-a-agent node-bc-store omega-daemon omega-qemu-launcher omega-gpu-proxy; do
            b="${ROOT_DIR}/target/release/${bin}"
            [[ -x "$b" ]] && rsync "${RSYNC_OPTS[@]}" "$b" "${DEPLOY_USER}@${node}:/tmp/omega-tests-bins/${bin}" || true
        done
        for worker in omega-gpu-worker-cpu.py omega-gpu-worker-app.py; do
            b="${ROOT_DIR}/scripts/${worker}"
            if [[ -f "$b" ]]; then
                rsync "${RSYNC_OPTS[@]}" "$b" "${DEPLOY_USER}@${node}:/tmp/omega-tests-bins/${worker}" || true
                ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${node}" "chmod +x /tmp/omega-tests-bins/${worker}" 2>/dev/null || true
            fi
        done
    done
    _ok "Sync OK"
}

_remote_env() {
    echo "OMEGA_NODES='${OMEGA_NODES}' \
OMEGA_CONTROLLER='${CONTROLLER_NODE}' \
OMEGA_TEST_VMID='${OMEGA_TEST_VMID}' \
OMEGA_TEST_VMIDS='${OMEGA_TEST_VMIDS}' \
OMEGA_PROVISION_VMIDS='${OMEGA_PROVISION_VMIDS:-${OMEGA_TEST_VMIDS}}' \
OMEGA_BIN_DIR='/tmp/omega-tests-bins' \
OMEGA_REMOTE_BIN_DIR='/tmp/omega-tests-bins' \
OMEGA_STORE_PORT='${STORE_PORT}' \
OMEGA_STATUS_PORT='${STATUS_PORT}' \
DEPLOY_USER='${DEPLOY_USER}' \
VMID='${OMEGA_TEST_VMID}' \
CLUSTER_MODE='1' \
OMEGA_DESTRUCTIVE='$($DO_DESTRUCTIVE && echo 1 || echo 0)' \
OMEGA_SOAK_SECS='${OMEGA_SOAK_SECS:-1800}' \
OMEGA_SCALE_VMIDS='${OMEGA_SCALE_VMIDS:-${OMEGA_PROVISION_VMIDS:-${OMEGA_TEST_VMIDS}}}' \
OMEGA_SCALE_TARGET='${OMEGA_SCALE_TARGET:-500}' \
OMEGA_SCALE_BATCH_SIZE='${OMEGA_SCALE_BATCH_SIZE:-20}' \
OMEGA_SCALE_SOAK_SECS='${OMEGA_SCALE_SOAK_SECS:-${OMEGA_SCALE_TIMEOUT_SECS:-1800}}' \
OMEGA_SCALE_STEPS='${OMEGA_SCALE_STEPS:-}' \
OMEGA_SCALE_STOP_AFTER='${OMEGA_SCALE_STOP_AFTER:-0}' \
OMEGA_SCALE_DESTROY_AFTER='${OMEGA_SCALE_DESTROY_AFTER:-0}' \
OMEGA_SCALE_CLEANUP_SCOPE='${OMEGA_SCALE_CLEANUP_SCOPE:-started}' \
OMEGA_TEST_TIMEOUT_MULTIPLIER='${OMEGA_TEST_TIMEOUT_MULTIPLIER:-1}' \
OMEGA_MIGRATION_TIMEOUT_SECS='${OMEGA_MIGRATION_TIMEOUT_SECS:-120}' \
OMEGA_CEPH_TIMEOUT_SECS='${OMEGA_CEPH_TIMEOUT_SECS:-60}' \
OMEGA_DISK_TIMEOUT_SECS='${OMEGA_DISK_TIMEOUT_SECS:-60}' \
OMEGA_SCALE_TIMEOUT_SECS='${OMEGA_SCALE_TIMEOUT_SECS:-1800}' \
OMEGA_GPU_PRIMARY_NODE='${OMEGA_GPU_PRIMARY_NODE:-}' \
OMEGA_GPU_NODES='${OMEGA_GPU_NODES:-}' \
OMEGA_GPU_PROXY_URL='${OMEGA_GPU_PROXY_URL:-}' \
OMEGA_GPU_PROXY_LISTEN='${OMEGA_GPU_PROXY_LISTEN:-0.0.0.0:9400}' \
OMEGA_GPU_PROXY_BACKEND_COMMAND='${OMEGA_GPU_PROXY_BACKEND_COMMAND:-/tmp/omega-tests-bins/omega-gpu-worker-app.py}' \
OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS='${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS:-900}' \
OMEGA_GPU_PROXY_TOTAL_VRAM_MIB='${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}' \
OMEGA_GPU_PROXY_MAX_CONCURRENT='${OMEGA_GPU_PROXY_MAX_CONCURRENT:-1}' \
OMEGA_GPU_PROXY_API_TOKEN_FILE='${OMEGA_GPU_PROXY_API_TOKEN_FILE:-/etc/omega/gpu-proxy.token}' \
OMEGA_GPU_PROXY_API_TOKEN='${OMEGA_GPU_PROXY_API_TOKEN:-}' \
OMEGA_GPU_MIGRATE_TO_GPU_NODE='${OMEGA_GPU_MIGRATE_TO_GPU_NODE:-1}' \
OMEGA_GPU_FALLBACK_NETWORK='${OMEGA_GPU_FALLBACK_NETWORK:-1}' \
OMEGA_GPU_PROXY_REQUIRE_CUDA='${OMEGA_GPU_PROXY_REQUIRE_CUDA:-1}' \
OMEGA_GPU_PROXY_REQUIRE_PARALLEL='${OMEGA_GPU_PROXY_REQUIRE_PARALLEL:-1}' \
OMEGA_STRICT_FULL='${OMEGA_STRICT_FULL:-0}' \
OMEGA_REQUIRE_NO_SKIP='${OMEGA_REQUIRE_NO_SKIP:-0}' \
OMEGA_GPU_PROXY_TEST_VM_COUNT='${OMEGA_GPU_PROXY_TEST_VM_COUNT:-3}'"
}

_quote_args() {
    printf ' %q' "$@"
}

_node_ip_for_pve_name() {
    local pve_name="$1" node host
    [[ -n "$pve_name" ]] || return 1
    for node in "${NODES_ARR[@]}"; do
        [[ -n "$node" ]] || continue
        if [[ "$node" == "$pve_name" ]]; then
            printf '%s' "$node"
            return 0
        fi
        host=$(ssh "${SSH_OPTS[@]}" -o ConnectTimeout=3 -o BatchMode=yes \
            "${DEPLOY_USER}@${node}" "hostname -s 2>/dev/null || hostname" 2>/dev/null || true)
        if [[ "$host" == "$pve_name" ]]; then
            printf '%s' "$node"
            return 0
        fi
    done
    return 1
}

_vm_host_node() {
    local vmid="$1" pve_name node_ip
    [[ "$vmid" =~ ^[0-9]+$ ]] || return 1
    pve_name=$(ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes \
        "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "pvesh get /cluster/resources --type vm --output-format json" 2>/dev/null | \
        python3 -c "
import sys, json
vmid = int('$vmid')
try:
    for v in json.load(sys.stdin):
        if v.get('vmid') == vmid:
            print(v.get('node', ''))
            break
except Exception:
    pass
" 2>/dev/null | head -1)
    [[ -n "$pve_name" ]] || return 1
    node_ip=$(_node_ip_for_pve_name "$pve_name" || true)
    [[ -n "$node_ip" ]] || return 1
    printf '%s' "$node_ip"
}

_normalize_vmids_spec() {
    local spec="${1// /}"
    local out="" part start end v
    [[ -n "$spec" ]] || return 1
    IFS=',' read -ra _parts <<< "$spec"
    for part in "${_parts[@]}"; do
        [[ -n "$part" ]] || continue
        if [[ "$part" =~ ^([0-9]+)-([0-9]+)$ ]]; then
            start="${BASH_REMATCH[1]}"
            end="${BASH_REMATCH[2]}"
            [[ "$start" -le "$end" ]] || return 1
            for ((v=start; v<=end; v++)); do
                out="${out:+$out,}${v}"
            done
        elif [[ "$part" =~ ^[0-9]+$ ]]; then
            out="${out:+$out,}${part}"
        else
            return 1
        fi
    done
    [[ -n "$out" ]] || return 1
    printf '%s' "$out"
}

_test_explanation() {
    local id="$1"
    case "$id" in
        00) cat <<'EOF'
Valide la base logicielle du depot.
Le script lance les tests unitaires Rust et verifie que les composants principaux compilent encore.
Un echec ici indique une regression de code, une dependance manquante ou un environnement de build incomplet.
EOF
            ;;
        01) cat <<'EOF'
Valide le chemin minimal agent -> store sur la machine de test.
Le script demarre un store temporaire, lance l'agent en mode demo, force quelques pages a etre stockees, puis verifie que le scenario termine avec SUCCES.
Ce test ne valide pas Proxmox; il valide le socle store/agent, userfaultfd, les binaires et les ports locaux.
EOF
            ;;
        02) cat <<'EOF'
Valide la replication de pages entre deux stores temporaires.
Le script demarre deux stores locaux, lance l'agent avec replication active, puis verifie que le store secondaire contient aussi des pages.
Si ce test echoue, le probleme est dans la replication, les ports, les stores temporaires ou userfaultfd, pas dans la VM Proxmox.
EOF
            ;;
        03) cat <<'EOF'
Valide le failover du store de pages.
Le script demarre deux stores temporaires: fa0 sur le port 9100 et fa1 sur le port 9101. Ces numeros sont des ports TCP, pas des VMID.
Il ecrit des pages sur les deux stores, tue brutalement fa0, puis verifie que fa1 garde les pages et peut continuer a repondre.
EOF
            ;;
        04) cat <<'EOF'
Valide le moteur d'eviction memoire en mode controle.
Le script cree une pression memoire artificielle, declenche l'eviction de pages vers le store, puis verifie que des pages ont bien ete deplacees.
Un echec indique souvent userfaultfd interdit, des permissions kernel manquantes, ou une pression memoire insuffisante.
EOF
            ;;
        05) cat <<'EOF'
Valide le vCPU elastique sur une vraie VM Proxmox.
Le script verifie d'abord que la VM a ete creee pour Omega: vcpus=1 au boot, cores/sockets donnant maxcpus > 1, hotplug CPU actif et QMP accessible.
Ensuite il met la VM sous charge CPU, attend que le daemon augmente les vCPU jusqu'au maximum autorise, puis attend le retour au minimum quand la charge disparait.
Si -smp affiche maxcpus=1, le test doit echouer: la VM a ete demarree avec un profil non hotpluggable et doit etre arretee puis recreee ou corrigee.
EOF
            ;;
        06) cat <<'EOF'
Valide la detection et le placement GPU cote daemon.
Le script interroge le noeud Proxmox et les endpoints Omega pour savoir si un backend GPU est visible et si un budget GPU peut etre attribue a une VM.
Il ne garantit pas le passthrough physique NVIDIA; il valide ce que le daemon expose et peut controler.
EOF
            ;;
        07) cat <<'EOF'
Valide le scheduler GPU entre deux VMs.
Le script prend deux VMIDs, observe les attributions GPU, puis verifie que le daemon arbitre l'acces sans laisser deux VMs bloquer le meme GPU de facon incoherente.
Il peut echouer normalement si aucun GPU exploitable n'existe, si le driver est casse, ou si le lock/QMP n'est pas accessible.
EOF
            ;;
        08) cat <<'EOF'
Valide la migration RAM/VM sur le cluster physique.
Le script choisit une VM running, genere une pression RAM/CPU, observe la decision Omega, puis tente une migration Proxmox si une cible saine existe.
Ce test ne doit pas migrer a tout prix: si Ceph, le reseau ou les ressources cible ne sont pas sains, l'echec signale que le cluster refuse correctement la migration.
EOF
            ;;
        09) cat <<'EOF'
Valide le nettoyage des ressources orphelines.
Le script force/observe des etats ou une VM n'est plus locale ou plus running, puis verifie que le daemon libere les slots CPU, budgets RAM/GPU/I/O et traces associees.
Un echec indique que le daemon garde un etat obsolete ou ne lit pas correctement l'etat Proxmox.
EOF
            ;;
        10) cat <<'EOF'
Valide plusieurs agents contre les stores temporaires.
Le script lance plusieurs agents demo avec des VMID differents pour verifier que le store accepte plusieurs workloads et conserve des metriques coherentes.
Ce n'est pas un test de creation VM; c'est un test de concurrence agent/store.
EOF
            ;;
        18) cat <<'EOF'
Valide le rappel LIFO des pages.
Le script stocke des pages dans un ordre controle puis les rappelle pour verifier que la politique de rappel respecte le comportement attendu.
Un echec indique une regression dans l'ordre interne des pages ou dans le protocole agent/store.
EOF
            ;;
        19) cat <<'EOF'
Valide la compaction cluster.
Le script observe les VMs locales, identifie celles qui peuvent rendre des ressources sans passer sous leur minimum, puis verifie que le daemon applique/rapporte cette compaction.
Il peut echouer si le daemon ne voit pas la VM, si l'API de controle est indisponible, ou si aucune VM ne peut legalement donner de ressources.
EOF
            ;;
        20) cat <<'EOF'
Valide le prefetch par stride.
Le script simule des acces memoire repetitifs, puis verifie que l'agent detecte le motif et precharge les pages suivantes.
Un echec indique souvent un seuil trop strict, un backend memoire incompatible ou une regression du detecteur de pattern.
EOF
            ;;
        21) cat <<'EOF'
Valide TLS et TOFU entre composants Omega.
Le script verifie la generation/lecture des certificats, les empreintes attendues et la capacite des composants a dialoguer avec TLS.
Il peut echouer si les certificats ont ete regeneres sans mise a jour des pairs ou si les chemins TLS ne sont pas deployes.
EOF
            ;;
        22) cat <<'EOF'
Valide le balloon thin-provisioning puis l'escalade vers migration sur une vraie VM Proxmox.
Le script remet la VM a son balloon minimum, lance une forte charge memoire dans l'invite, observe la croissance QEMU du balloon, puis surveille les logs Omega et Proxmox pour verifier qu'une migration est recherchee/lancee.
Il echoue si la RAM active grandit mais qu'aucune recherche/migration n'apparait: cela signifie que la politique RAM reste locale au lieu d'escalader vers un noeud plus sain.
EOF
            ;;
        23|23C) cat <<'EOF'
Valide le scheduler I/O disque.
Le script verifie d'abord les tests unitaires du disk scheduler, puis controle sur le cluster que cgroup v2 expose io.weight/io.stat sous qemu.slice pour les VMs.
Si io.weight n'existe pas, Omega doit signaler "support indisponible" plutot qu'une panne fonctionnelle: le noyau/systemd n'a pas active le controleur I/O pour les scopes QEMU.
EOF
            ;;
        24) cat <<'EOF'
Valide l'installation Omega sur les noeuds.
Le script controle les binaires, les services systemd, les hooks Proxmox, les endpoints HTTP et l'etat du daemon.
Un echec indique un deploiement incomplet, un service arrete ou une version de binaire differente entre noeuds.
EOF
            ;;
        25) cat <<'EOF'
Valide le reseau dans la VM invitee.
Le script cherche l'IP de la VM, teste l'interface, la route, le ping, le DNS et si possible apt.
Il peut echouer si qemu-guest-agent est absent, si DHCP ne donne pas d'IP, si vmbr1/NAT est mal configure, ou si DNS/Internet sont bloques.
EOF
            ;;
        26) cat <<'EOF'
Valide Ceph reel avant les tests qui dependent du stockage partage.
Le script inspecte mon/mgr/osd, les PG, l'etat active+clean, les pools et les storages Proxmox.
Si Ceph est degrade, les tests CPU simples peuvent continuer, mais migration/disque/scale peuvent etre lents ou echouer.
EOF
            ;;
        27) cat <<'EOF'
Valide le GPU reel et le rendu minimal quand le materiel le permet.
Le script inspecte /dev/dri, les permissions, le backend GPU expose par Omega et les commandes de diagnostic disponibles.
Sur NVIDIA, un driver DKMS casse ou incompatible avec le kernel suffit a faire echouer ce test sans remettre en cause CPU/RAM/disque.
EOF
            ;;
        32) cat <<'EOF'
Valide le proxy GPU applicatif Omega.
Le script contacte omega-gpu-proxy, attribue un budget VRAM logique a plusieurs VMs, verifie qu'un job au-dessus du budget est refuse, puis soumet plusieurs jobs matrix_multiply.
Ce test prouve le chemin multi-VM -> API proxy -> budgets VRAM -> file d'attente -> worker applicatif. Il prouve CUDA uniquement si le resultat indique backend=torch device=cuda.
EOF
            ;;
        33) cat <<'EOF'
Collecte les métriques production sur tous les axes du projet.
Le script mesure les endpoints Omega par nœud, l'état Proxmox des VMs, qemu-guest-agent, Ceph, cgroup I/O, le proxy GPU et un micro-benchmark CUDA.
Il écrit un rapport JSON dans /tmp/omega-production-metrics-*.json pour comparer si le cluster est lent ou rapide après plusieurs runs.
EOF
            ;;
        34|M8) cat <<'EOF'
Valide le placement GPU global.
Le script choisit le meilleur nœud GPU selon VRAM libre, RAM, vCPU et pression disque; si la VM n'est pas déjà dessus, il tente une migration Proxmox live, puis vérifie un job CUDA via le proxy.
Si la migration est impossible mais que le fallback réseau est autorisé, il valide que la VM peut quand même consommer le GPU via omega-gpu-proxy.
EOF
            ;;
        35|M9) cat <<'EOF'
Valide le fallback GPU réseau.
Le script ne dépend pas du passthrough GPU dans la VM: il attribue un budget VRAM logique et soumet un job CUDA au proxy applicatif.
Un succès prouve le scénario VM sans GPU local -> API proxy -> worker CUDA sur nœud GPU.
EOF
            ;;
        36) cat <<'EOF'
Valide la concurrence GPU CUDA.
Le script lance plusieurs jobs matrix_multiply CUDA depuis plusieurs VMIDs et exige que le proxy respecte les budgets, refuse le hors-budget et exécute réellement sur backend=torch device=cuda.
Un échec indique souvent une mauvaise configuration OMEGA_GPU_PYTHON, un max_concurrent trop bas ou un worker qui retombe en CPU.
EOF
            ;;
        28) cat <<'EOF'
Valide la resilience a une partition reseau controlee.
Le script coupe volontairement une partie du reseau pour observer la reaction du cluster, puis restaure la connectivite.
Ce test est destructif et ne doit etre lance que si --destructive est active et que tu acceptes une perturbation temporaire.
EOF
            ;;
        29) cat <<'EOF'
Valide la stabilite longue duree.
Le script maintient une charge sur plusieurs dimensions, surveille les services, les metriques Omega et les erreurs Proxmox/Ceph.
Il sert a detecter fuites, timeouts, instabilite daemon, degradation progressive ou decisions de migration incoherentes.
EOF
            ;;
        30) cat <<'EOF'
Valide la conformite des VMs de test Omega avant les tests lourds.
Le script controle hotplug, agent, balloon, vcpus, cores/sockets, -smp maxcpus, disque virtio-scsi, bridge reseau et metadata omega_*.
Si ce test echoue, il faut corriger ou recreer les VMs avant d'interpreter les echecs des tests CPU/RAM/GPU/disque.
EOF
            ;;
        31) cat <<'EOF'
Valide la scalabilite physique du cluster.
Le script inventorie les VMs, les demarre par lots, surveille Proxmox/Omega, puis peut nettoyer les VMs creees pour le test selon la configuration.
Il peut echouer si les VMs sont absentes, non conformes, si Ceph est lent, ou si le cluster manque de CPU/RAM/stockage.
EOF
            ;;
        M1) cat <<'EOF'
Scenario mixte RAM + CPU sur VM reelle.
Le script applique simultanement une pression memoire et CPU, puis observe si Omega arbitre correctement balloon, vCPU et partage local.
Il valide l'interaction entre politiques, pas seulement un composant isole.
EOF
            ;;
        M2) cat <<'EOF'
Scenario CPU + RAM pouvant mener a migration.
Le script met une VM sous pression mixte, observe si le noeud devient defavorable, puis verifie si une migration est proposee ou executee vers une cible meilleure.
Il peut echouer si aucune cible n'ameliore reellement l'etat global du cluster.
EOF
            ;;
        M3) cat <<'EOF'
Scenario GPU + CPU.
Le script combine pression CPU et demande GPU pour verifier que le scheduler ne donne pas un budget GPU incoherent a une VM deja mal placee.
Il depend fortement du support GPU reel sur le noeud.
EOF
            ;;
        M4) cat <<'EOF'
Scenario pression cluster globale.
Le script installe/cherche stress-ng sur les noeuds, lance une pression sur plusieurs VMs/noeuds, puis observe l'etat Omega, Proxmox et Ceph.
Il valide la reaction globale du cluster, pas seulement une VM; un noeud injoignable, Ceph lent ou un outil absent fait echouer le scenario.
EOF
            ;;
        M5) cat <<'EOF'
Scenario live migration sous pression.
Le script charge une VM pendant qu'une migration live est tentee, puis verifie que la VM reste running et que les ressources sont nettoyees apres deplacement.
Il depend du stockage partage, du reseau de migration et de la capacite de la cible a accueillir la VM.
EOF
            ;;
        M6) cat <<'EOF'
Scenario rafale de demarrages VM.
Le script demarre plusieurs VMs rapidement pour observer les limites Proxmox, Ceph, hooks Omega et admission de ressources.
Il peut echouer si les VMs manquent, si Ceph est lent ou si Proxmox refuse trop de starts simultanes.
EOF
            ;;
        M7) cat <<'EOF'
Scenario drain d'un noeud.
Le script considere un noeud source comme a vider, puis verifie si les VMs peuvent etre migrees vers les autres noeuds sans casser CPU/RAM/GPU/disque.
Il echoue normalement si les cibles ne peuvent pas absorber les VMs ou si la migration est interdite par Proxmox/Ceph.
EOF
            ;;
        *) cat <<'EOF'
Execute le script de test Omega correspondant avec la configuration courante.
Le test peut echouer si ses prerequis propres ne sont pas satisfaits; lire les etapes affichees juste apres cette description pour identifier le blocage exact.
EOF
            ;;
    esac
}

_explain_test() {
    local id="$1"
    echo ""
    echo -e "${BOLD}${CYAN}Ce que ce test va verifier${RESET}"
    _test_explanation "$id" | sed 's/^/  /'
}

_run_isolated() {
    local num="$1" name="$2" script="$3"; shift 3
    _hdr "  Test ${num} — ${name}  [isolé]"
    _sep
    _explain_test "$num"
    local t0=$SECONDS rc=0
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "systemctl stop omega-daemon 2>/dev/null || true" || true
    local args_str
    args_str="$(_quote_args "$@")"
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=10 -o BatchMode=yes "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "$(_remote_env) bash '/tmp/omega-tests/$(basename "$script")'${args_str}" || rc=$?
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${CONTROLLER_NODE}" \
        "systemctl start omega-daemon 2>/dev/null || true; sleep 2" || true
    _record "$num" "$name" "$rc" "$(( SECONDS - t0 ))"
}

_run_local() {
    local num="$1" name="$2" script="$3"; shift 3
    _hdr "  Test ${num} — ${name}  [local]"
    _sep
    _explain_test "$num"
    local t0=$SECONDS rc=0
    REPO_ROOT="$ROOT_DIR" \
    OMEGA_BIN_DIR="$ROOT_DIR/target/release" \
    OMEGA_NODES="${OMEGA_NODES}" \
    bash "${TESTS_DIR}/$(basename "$script")" "$@" || rc=$?
    _record "$num" "$name" "$rc" "$(( SECONDS - t0 ))"
}

_run_cluster() {
    local num="$1" name="$2" script="$3"; shift 3
    _hdr "  Test ${num} — ${name}  [cluster]"
    _sep
    _explain_test "$num"
    local t0=$SECONDS rc=0
    local args_str target_node
    args_str="$(_quote_args "$@")"
    target_node="$CONTROLLER_NODE"
    if [[ "${1:-}" =~ ^[0-9]+$ ]]; then
        target_node="$(_vm_host_node "$1" || echo "$CONTROLLER_NODE")"
        [[ "$target_node" != "$CONTROLLER_NODE" ]] && \
            _info "VM $1 hébergée sur ${target_node}; test lancé sur ce nœud"
    fi
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=10 -o BatchMode=yes "${DEPLOY_USER}@${target_node}" \
        "$(_remote_env) bash '/tmp/omega-tests/$(basename "$script")'${args_str}" || rc=$?
    _record "$num" "$name" "$rc" "$(( SECONDS - t0 ))"
}

# ── Affichage du résumé en ligne ──────────────────────────────────────────────
_results_line() {
    local total=$(( TOTAL_PASS + TOTAL_FAIL + TOTAL_SKIP ))
    if [[ $total -eq 0 ]]; then
        echo -e "  ${DIM}aucun test exécuté${RESET}"
    else
        echo -ne "  Tests : ${GREEN}✓ $TOTAL_PASS${RESET}  ${RED}✗ $TOTAL_FAIL${RESET}  ${YELLOW}— $TOTAL_SKIP${RESET}  / $total"
        # Lister les FAIL
        local fails=""
        for id in "${!RESULTS[@]}"; do
            [[ "${RESULTS[$id]}" == "FAIL" ]] && fails="$fails ${RED}$id${RESET}"
        done
        [[ -n "$fails" ]] && echo -e "    FAIL :${fails}" || echo ""
    fi
}

show_results() {
    _hdr "══ Résumé des tests ══"
    echo ""
    local ids=()
    for id in $(echo "${!RESULTS[@]}" | tr ' ' '\n' | sort -V); do ids+=("$id"); done
    for id in "${ids[@]}"; do
        local s="${RESULTS[$id]}" d="${DURATIONS[$id]:-?}s"
        case "$s" in
            PASS) echo -e "  ${GREEN}✓${RESET} $id  ($d)" ;;
            FAIL) echo -e "  ${RED}✗${RESET} $id  ($d)" ;;
            SKIP) echo -e "  ${YELLOW}–${RESET} $id  (ignoré)" ;;
        esac
    done
    echo ""
    echo -e "  ${GREEN}PASS${RESET} $TOTAL_PASS   ${RED}FAIL${RESET} $TOTAL_FAIL   ${YELLOW}SKIP${RESET} $TOTAL_SKIP"
    echo ""
}

# ── Configuration du cluster ──────────────────────────────────────────────────
do_configure() {
    clear
    _hdr "══ Configuration du cluster ══"
    echo ""

    # Afficher la configuration actuelle
    if $CONFIGURED; then
        echo -e "  Configuration actuelle :"
        echo -e "    Nœuds       : ${CYAN}${OMEGA_NODES}${RESET}"
        echo -e "    Contrôleur  : ${CYAN}${CONTROLLER_NODE}${RESET}"
        echo -e "    VM test     : ${CYAN}${OMEGA_TEST_VMID}${RESET}"
        echo -e "    User SSH    : ${CYAN}${DEPLOY_USER}${RESET}"
        echo -e "    GPU/proxy   : nodes=${CYAN}${OMEGA_GPU_NODES:-auto}${RESET} · primary=${CYAN}${OMEGA_GPU_PRIMARY_NODE:-auto}${RESET} · ${CYAN}${OMEGA_GPU_PROXY_URL:-auto}${RESET} · vram=${CYAN}${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}${RESET}MiB"
        echo -e "    Timeouts    : ${CYAN}x${OMEGA_TEST_TIMEOUT_MULTIPLIER}, migration=${OMEGA_MIGRATION_TIMEOUT_SECS}s, ceph=${OMEGA_CEPH_TIMEOUT_SECS}s, disk=${OMEGA_DISK_TIMEOUT_SECS}s, scale=${OMEGA_SCALE_TIMEOUT_SECS}s${RESET}"
        echo ""
        read -rp "  Modifier ? [o/N] " mod
        [[ "$mod" =~ ^[oOyY]$ ]] || return
    else
        _warn "Aucune configuration trouvée dans ${CONF_FILE}"
    fi

    echo ""
    echo -e "  Entrez les IPs ou hostnames des nœuds, séparés par des virgules."
    echo -e "  ${DIM}Exemples :${RESET}"
    echo -e "  ${DIM}  2 nœuds : 192.168.1.10,192.168.1.11${RESET}"
    echo -e "  ${DIM}  3 nœuds : pve1,pve2,pve3${RESET}"
    echo -e "  ${DIM}  4 nœuds : 10.0.0.1,10.0.0.2,10.0.0.3,10.0.0.4${RESET}"
    echo ""
    while true; do
        _ask "Nœuds du cluster (séparés par virgule)"
        read -r input_nodes
        input_nodes="${input_nodes// /}"   # supprimer espaces
        [[ -n "$input_nodes" ]] && break
        _warn "La liste des nœuds ne peut pas être vide."
    done

    IFS=',' read -ra tmp_arr <<< "$input_nodes"
    local first_node="${tmp_arr[0]}"

    _ask "Nœud contrôleur [défaut : ${first_node}]"
    read -r input_ctrl
    input_ctrl="${input_ctrl:-$first_node}"

    echo ""
    echo -e "  VMIDs des VMs de test (séparés par virgule, doivent exister dans le cluster)."
    echo -e "  ${DIM}Exemple : 9001,9002,9003${RESET}"
    echo -e "  ${DIM}Tests qui ont besoin de 2 VMs (07-gpu-scheduler) utilisent les 2 premiers.${RESET}"
    echo -e "  ${DIM}Test M7 drain utilise toute la liste.${RESET}"
    echo ""
    _ask "VMIDs de test [défaut : ${OMEGA_TEST_VMIDS:-9001}]"
    read -r input_vmids
    input_vmids="${input_vmids:-${OMEGA_TEST_VMIDS:-9001}}"
    input_vmids="${input_vmids// /}"
    # Premier VMID = OMEGA_TEST_VMID (compat legacy)
    IFS=',' read -ra _tmp_vmids <<< "$input_vmids"
    input_vmid="${_tmp_vmids[0]}"

    echo ""
    echo -e "  VMIDs à créer par [p] provisioning."
    echo -e "  ${DIM}Peut être identique aux VMs de test, ou plus large pour scale.${RESET}"
    echo -e "  ${DIM}Formats acceptés : 9001,9002,9003 ou 9001-9150 ou 9001,9005-9010${RESET}"
    echo ""
    _ask "VMIDs à provisionner [défaut : ${OMEGA_PROVISION_VMIDS:-$input_vmids}]"
    read -r input_provision_vmids
    input_provision_vmids="${input_provision_vmids:-${OMEGA_PROVISION_VMIDS:-$input_vmids}}"
    if ! normalized_provision_vmids="$(_normalize_vmids_spec "$input_provision_vmids")"; then
        _warn "Format VMIDs provisioning invalide, fallback sur VMIDs de test"
        normalized_provision_vmids="$input_vmids"
    fi

    _ask "Utilisateur SSH [défaut : ${DEPLOY_USER}]"
    read -r input_user
    input_user="${input_user:-${DEPLOY_USER}}"

    echo ""
    echo -e "  ${BOLD}Timeouts des tests réels${RESET}"
    echo -e "  Ces valeurs évitent les faux échecs sur cluster lent, Ceph en recovery,"
    echo -e "  lien réseau à 100 Mbps, ou stockage chargé. Elles ne valident pas les"
    echo -e "  performances datacenter : elles donnent seulement plus de temps aux tests."
    echo -e "  ${DIM}Profil normal: multiplier=1, migration=120, ceph=60, disk=60, scale=1800.${RESET}"
    echo -e "  ${DIM}Profil lent recommandé: multiplier=3, migration=900, ceph=900, disk=600, scale=1200.${RESET}"
    echo ""

    _ask "Multiplicateur global timeouts [${OMEGA_TEST_TIMEOUT_MULTIPLIER}]"
    read -r input_timeout_multiplier
    input_timeout_multiplier="${input_timeout_multiplier:-${OMEGA_TEST_TIMEOUT_MULTIPLIER}}"

    _ask "Timeout migration secondes [${OMEGA_MIGRATION_TIMEOUT_SECS}]"
    read -r input_migration_timeout
    input_migration_timeout="${input_migration_timeout:-${OMEGA_MIGRATION_TIMEOUT_SECS}}"

    _ask "Timeout Ceph secondes [${OMEGA_CEPH_TIMEOUT_SECS}]"
    read -r input_ceph_timeout
    input_ceph_timeout="${input_ceph_timeout:-${OMEGA_CEPH_TIMEOUT_SECS}}"

    _ask "Timeout disque/I/O secondes [${OMEGA_DISK_TIMEOUT_SECS}]"
    read -r input_disk_timeout
    input_disk_timeout="${input_disk_timeout:-${OMEGA_DISK_TIMEOUT_SECS}}"

    _ask "Timeout/soak scale secondes [${OMEGA_SCALE_TIMEOUT_SECS}]"
    read -r input_scale_timeout
    input_scale_timeout="${input_scale_timeout:-${OMEGA_SCALE_TIMEOUT_SECS}}"

    for _timeout_value in "$input_timeout_multiplier" "$input_migration_timeout" "$input_ceph_timeout" "$input_disk_timeout" "$input_scale_timeout"; do
        if [[ ! "$_timeout_value" =~ ^[0-9]+$ || "$_timeout_value" -lt 1 ]]; then
            _err "Timeout invalide: $_timeout_value"
            return 1
        fi
    done

    echo ""
    echo -e "  ${BOLD}GPU/proxy applicatif Omega${RESET}"
    echo -e "  Ces valeurs restent portables: utilisez un hostname/IP de OMEGA_NODES."
    echo -e "  Nœuds GPU accepte une liste CSV. Vide = détection automatique par [d]/[I]."
    echo -e "  Nœud GPU principal reste un fallback/préférence; les proxys démarrent sur tous les nœuds GPU."
    echo ""

    _ask "Nœuds GPU CSV [${OMEGA_GPU_NODES:-auto}]"
    read -r input_gpu_nodes
    input_gpu_nodes="${input_gpu_nodes:-${OMEGA_GPU_NODES}}"

    _ask "Nœud GPU principal [${OMEGA_GPU_PRIMARY_NODE:-auto}]"
    read -r input_gpu_primary
    input_gpu_primary="${input_gpu_primary:-${OMEGA_GPU_PRIMARY_NODE}}"

    _ask "URL proxy GPU [${OMEGA_GPU_PROXY_URL:-auto}]"
    read -r input_gpu_proxy_url
    input_gpu_proxy_url="${input_gpu_proxy_url:-${OMEGA_GPU_PROXY_URL}}"

    _ask "Listen proxy GPU [${OMEGA_GPU_PROXY_LISTEN}]"
    read -r input_gpu_proxy_listen
    input_gpu_proxy_listen="${input_gpu_proxy_listen:-${OMEGA_GPU_PROXY_LISTEN}}"

    _ask "Concurrence jobs GPU [${OMEGA_GPU_PROXY_MAX_CONCURRENT}]"
    read -r input_gpu_proxy_max_concurrent
    input_gpu_proxy_max_concurrent="${input_gpu_proxy_max_concurrent:-${OMEGA_GPU_PROXY_MAX_CONCURRENT}}"

    _ask "VRAM totale proxy GPU MiB (0=auto/non forcé) [${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}]"
    read -r input_gpu_proxy_total_vram
    input_gpu_proxy_total_vram="${input_gpu_proxy_total_vram:-${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}}"

    _ask "Timeout worker GPU secondes [${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS}]"
    read -r input_gpu_proxy_backend_timeout
    input_gpu_proxy_backend_timeout="${input_gpu_proxy_backend_timeout:-${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS}}"

    _ask "Fichier token proxy GPU [${OMEGA_GPU_PROXY_API_TOKEN_FILE}]"
    read -r input_gpu_proxy_token_file
    input_gpu_proxy_token_file="${input_gpu_proxy_token_file:-${OMEGA_GPU_PROXY_API_TOKEN_FILE}}"

    _ask "Migrer vers nœud GPU si possible 1/0 [${OMEGA_GPU_MIGRATE_TO_GPU_NODE}]"
    read -r input_gpu_migrate
    input_gpu_migrate="${input_gpu_migrate:-${OMEGA_GPU_MIGRATE_TO_GPU_NODE}}"

    _ask "Fallback requêtes GPU réseau si migration impossible 1/0 [${OMEGA_GPU_FALLBACK_NETWORK}]"
    read -r input_gpu_fallback
    input_gpu_fallback="${input_gpu_fallback:-${OMEGA_GPU_FALLBACK_NETWORK}}"

    for _gpu_num in "$input_gpu_proxy_max_concurrent" "$input_gpu_proxy_total_vram" "$input_gpu_proxy_backend_timeout" "$input_gpu_migrate" "$input_gpu_fallback"; do
        if [[ ! "$_gpu_num" =~ ^[0-9]+$ ]]; then
            _err "Valeur GPU numérique invalide: $_gpu_num"
            return 1
        fi
    done

    echo ""
    echo -e "  ${BOLD}Profil de création VM pour l'option [p]${RESET}"
    echo -e "  Ces valeurs doivent produire des VMs Omega conformes: boot vCPU minimum,"
    echo -e "  max hotpluggable via cores*sockets, balloon initial et RAM max."
    echo ""

    _ask "Storage Proxmox cible [${OMEGA_VM_STORAGE}]"
    read -r input_vm_storage
    input_vm_storage="${input_vm_storage:-${OMEGA_VM_STORAGE}}"

    _ask "Bridge VM [${OMEGA_VM_BRIDGE}]"
    read -r input_vm_bridge
    input_vm_bridge="${input_vm_bridge:-${OMEGA_VM_BRIDGE}}"

    _ask "Image qcow2 distante [${OMEGA_VM_IMAGE_REMOTE}]"
    read -r input_vm_image_remote
    input_vm_image_remote="${input_vm_image_remote:-${OMEGA_VM_IMAGE_REMOTE}}"

    _ask "Image qcow2 locale à copier si absente [${OMEGA_VM_IMAGE_LOCAL:-vide}]"
    read -r input_vm_image_local
    input_vm_image_local="${input_vm_image_local:-${OMEGA_VM_IMAGE_LOCAL}}"

    _ask "Clé publique distante injectée cloud-init [${OMEGA_VM_SSHKEY_REMOTE}]"
    read -r input_vm_sshkey_remote
    input_vm_sshkey_remote="${input_vm_sshkey_remote:-${OMEGA_VM_SSHKEY_REMOTE}}"

    _ask "RAM max VM MiB [${OMEGA_VM_MEMORY}]"
    read -r input_vm_memory
    input_vm_memory="${input_vm_memory:-${OMEGA_VM_MEMORY}}"

    _ask "RAM initiale/balloon MiB [${OMEGA_VM_BALLOON}]"
    read -r input_vm_balloon
    input_vm_balloon="${input_vm_balloon:-${OMEGA_VM_BALLOON}}"

    _ask "Cores QEMU [${OMEGA_VM_CORES}]"
    read -r input_vm_cores
    input_vm_cores="${input_vm_cores:-${OMEGA_VM_CORES}}"

    _ask "Sockets QEMU [${OMEGA_VM_SOCKETS}]"
    read -r input_vm_sockets
    input_vm_sockets="${input_vm_sockets:-${OMEGA_VM_SOCKETS}}"

    _ask "vCPU au boot [${OMEGA_VM_VCPUS}]"
    read -r input_vm_vcpus
    input_vm_vcpus="${input_vm_vcpus:-${OMEGA_VM_VCPUS}}"

    _ask "Budget disque logique GiB [${OMEGA_VM_DISK_MAX_GIB}]"
    read -r input_vm_disk_max_gib
    input_vm_disk_max_gib="${input_vm_disk_max_gib:-${OMEGA_VM_DISK_MAX_GIB}}"

    _ask "Budget VRAM MiB [${OMEGA_VM_GPU_VRAM_MIB}]"
    read -r input_vm_gpu_vram_mib
    input_vm_gpu_vram_mib="${input_vm_gpu_vram_mib:-${OMEGA_VM_GPU_VRAM_MIB}}"

    for _vm_num in "$input_vm_memory" "$input_vm_balloon" "$input_vm_cores" "$input_vm_sockets" "$input_vm_vcpus" "$input_vm_disk_max_gib" "$input_vm_gpu_vram_mib"; do
        if [[ ! "$_vm_num" =~ ^[0-9]+$ ]]; then
            _err "Valeur VM numérique invalide: $_vm_num"
            return 1
        fi
    done
    local input_vm_max_vcpus=$(( input_vm_cores * input_vm_sockets ))
    if [[ "$input_vm_balloon" -le 0 || "$input_vm_balloon" -ge "$input_vm_memory" ]]; then
        _err "RAM VM invalide: balloon=${input_vm_balloon} doit être > 0 et < memory=${input_vm_memory}"
        return 1
    fi
    if [[ "$input_vm_max_vcpus" -le 1 ]]; then
        _err "CPU VM invalide: cores*sockets=${input_vm_max_vcpus}; il faut au moins 2 vCPU max"
        return 1
    fi
    if [[ "$input_vm_vcpus" -ge "$input_vm_max_vcpus" ]]; then
        _err "CPU VM invalide: vcpus=${input_vm_vcpus} >= cores*sockets=${input_vm_max_vcpus}"
        return 1
    fi

    # Sauvegarder dans cluster.conf
    cat > "$CONF_FILE" <<EOF
# Configuration du cluster omega-remote-paging.
# Généré par omega-lab.sh — $(date)

# Nœuds du cluster (IPs ou hostnames résolvables depuis tous les nœuds).
OMEGA_NODES="${input_nodes}"

# Nœud qui exécute le contrôleur (un seul au choix).
OMEGA_CONTROLLER="${input_ctrl}"

# VMIDs des VMs de test existantes dans le cluster (séparées par virgule).
# Ces VMs doivent être démarrées avant de lancer les tests cluster.
OMEGA_TEST_VMIDS="${input_vmids}"
OMEGA_TEST_VMID="${input_vmid}"

# VMIDs créés par l'action [p] provisioning.
# Peut être plus large que OMEGA_TEST_VMIDS pour les tests scale.
OMEGA_PROVISION_VMIDS="${normalized_provision_vmids}"

# Utilisateur SSH pour la connexion aux nœuds.
DEPLOY_USER="${input_user}"

# Ports (ne pas modifier sauf si conflit)
STORE_PORT=9100
STATUS_PORT=9200

# Timeouts pour tests réels.
# Augmenter ces valeurs sur cluster lent/degrade évite les faux échecs,
# mais ne rend pas les mesures de performance valides.
OMEGA_TEST_TIMEOUT_MULTIPLIER="${input_timeout_multiplier}"
OMEGA_MIGRATION_TIMEOUT_SECS="${input_migration_timeout}"
OMEGA_CEPH_TIMEOUT_SECS="${input_ceph_timeout}"
OMEGA_DISK_TIMEOUT_SECS="${input_disk_timeout}"
OMEGA_SCALE_TIMEOUT_SECS="${input_scale_timeout}"

# GPU/proxy applicatif Omega.
# OMEGA_GPU_NODES vide = detection automatique au deploiement.
# OMEGA_GPU_PRIMARY_NODE vide = premier noeud GPU detecte.
# OMEGA_GPU_PROXY_URL vide = derivee depuis OMEGA_GPU_PRIMARY_NODE.
OMEGA_GPU_NODES="${input_gpu_nodes}"
OMEGA_GPU_PRIMARY_NODE="${input_gpu_primary}"
OMEGA_GPU_PROXY_URL="${input_gpu_proxy_url}"
OMEGA_GPU_PROXY_LISTEN="${input_gpu_proxy_listen}"
OMEGA_GPU_PROXY_MAX_CONCURRENT="${input_gpu_proxy_max_concurrent}"
OMEGA_GPU_PROXY_TOTAL_VRAM_MIB="${input_gpu_proxy_total_vram}"
OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS="${input_gpu_proxy_backend_timeout}"
OMEGA_GPU_PROXY_API_TOKEN_FILE="${input_gpu_proxy_token_file}"
OMEGA_GPU_MIGRATE_TO_GPU_NODE="${input_gpu_migrate}"
OMEGA_GPU_FALLBACK_NETWORK="${input_gpu_fallback}"

# Profil de création des VMs physiques par l'action [p].
OMEGA_VM_STORAGE="${input_vm_storage}"
OMEGA_VM_BRIDGE="${input_vm_bridge}"
OMEGA_VM_IMAGE_REMOTE="${input_vm_image_remote}"
OMEGA_VM_IMAGE_LOCAL="${input_vm_image_local}"
OMEGA_VM_SSHKEY_REMOTE="${input_vm_sshkey_remote}"
OMEGA_VM_MEMORY="${input_vm_memory}"
OMEGA_VM_BALLOON="${input_vm_balloon}"
OMEGA_VM_CORES="${input_vm_cores}"
OMEGA_VM_SOCKETS="${input_vm_sockets}"
OMEGA_VM_VCPUS="${input_vm_vcpus}"
OMEGA_VM_DISK_MAX_GIB="${input_vm_disk_max_gib}"
OMEGA_VM_GPU_VRAM_MIB="${input_vm_gpu_vram_mib}"
EOF

    _ok "Configuration sauvegardée dans ${CONF_FILE}"
    echo ""

    # Recharger
    _load_config

    echo -e "  ${GREEN}✓${RESET} Nœuds      : ${CYAN}${OMEGA_NODES}${RESET}"
    echo -e "  ${GREEN}✓${RESET} Contrôleur : ${CYAN}${CONTROLLER_NODE}${RESET}"
    echo -e "  ${GREEN}✓${RESET} VMs test   : ${CYAN}${OMEGA_TEST_VMIDS}${RESET}"
    echo -e "  ${GREEN}✓${RESET} VMs create : ${CYAN}${OMEGA_PROVISION_VMIDS}${RESET}"
    echo -e "  ${GREEN}✓${RESET} Profil VM  : ${CYAN}vCPU ${OMEGA_VM_VCPUS}/$(( OMEGA_VM_CORES * OMEGA_VM_SOCKETS )), RAM ${OMEGA_VM_BALLOON}/${OMEGA_VM_MEMORY} MiB, bridge ${OMEGA_VM_BRIDGE}, storage ${OMEGA_VM_STORAGE}${RESET}"
    echo -e "  ${GREEN}✓${RESET} GPU/proxy  : nodes=${CYAN}${OMEGA_GPU_NODES:-auto}${RESET} · primary=${CYAN}${OMEGA_GPU_PRIMARY_NODE:-auto}${RESET} · ${CYAN}${OMEGA_GPU_PROXY_URL:-auto}${RESET} · listen=${CYAN}${OMEGA_GPU_PROXY_LISTEN}${RESET} · vram=${CYAN}${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}${RESET}MiB"
    echo -e "  ${GREEN}✓${RESET} User SSH   : ${CYAN}${DEPLOY_USER}${RESET}"
    echo -e "  ${GREEN}✓${RESET} Timeouts   : ${CYAN}x${OMEGA_TEST_TIMEOUT_MULTIPLIER}, migration=${OMEGA_MIGRATION_TIMEOUT_SECS}s, ceph=${OMEGA_CEPH_TIMEOUT_SECS}s, disk=${OMEGA_DISK_TIMEOUT_SECS}s, scale=${OMEGA_SCALE_TIMEOUT_SECS}s${RESET}"
    echo ""

    # ── Génération et déploiement de la clé SSH ───────────────────────────────
    local SSH_KEY="${HOME}/.ssh/omega_ed25519"
    if [[ ! -f "$SSH_KEY" ]]; then
        _info "Génération d'une clé SSH dédiée omega : ${SSH_KEY}"
        ssh-keygen -t ed25519 -C "omega-lab" -N "" -f "$SSH_KEY" -q
        _ok "Clé générée : ${SSH_KEY}.pub"
    else
        _info "Clé SSH existante : ${SSH_KEY}"
    fi

    # S'assurer que la clé est chargée dans ssh-agent si disponible
    if [[ -n "${SSH_AUTH_SOCK:-}" ]]; then
        ssh-add "$SSH_KEY" &>/dev/null || true
    fi

    # Déployer la clé sur chaque nœud (demande le mot de passe si nécessaire)
    _info "Déploiement de la clé SSH sur les nœuds (le mot de passe peut être demandé)..."
    echo ""
    local ssh_ok=true
    for n in "${NODES_ARR[@]}"; do
        # Tester d'abord si la clé est déjà en place (connexion sans mot de passe)
        if ssh -o ConnectTimeout=5 -o BatchMode=yes \
               -i "$SSH_KEY" "${input_user}@${n}" "hostname" &>/dev/null; then
            _ok "  ${input_user}@${n} — clé déjà installée"
        else
            echo -e "  ${YELLOW}→${RESET}  Copie de la clé vers ${input_user}@${n} (entrez le mot de passe) :"
            if ssh-copy-id -i "${SSH_KEY}.pub" \
                           -o ConnectTimeout=10 \
                           "${input_user}@${n}"; then
                _ok "  ${input_user}@${n} — clé installée"
            else
                _warn "  ${input_user}@${n} — échec (vérifier IP, user et que sshd est démarré)"
                ssh_ok=false
            fi
        fi
    done
    echo ""

    # Écrire l'identité SSH dans cluster.conf pour que deploy.sh et les tests l'utilisent
    if ! grep -q "SSH_KEY=" "$CONF_FILE" 2>/dev/null; then
        echo "" >> "$CONF_FILE"
        echo "# Clé SSH dédiée omega (générée par omega-lab.sh)" >> "$CONF_FILE"
        echo "SSH_KEY=\"${SSH_KEY}\"" >> "$CONF_FILE"
    else
        sed -i "s|^SSH_KEY=.*|SSH_KEY=\"${SSH_KEY}\"|" "$CONF_FILE"
    fi

    # ── Test de connectivité finale ───────────────────────────────────────────
    _info "Test de connectivité SSH finale..."
    local ok=true
    for n in "${NODES_ARR[@]}"; do
        if ssh -o ConnectTimeout=5 -o BatchMode=yes \
               -i "$SSH_KEY" "${input_user}@${n}" "hostname" &>/dev/null; then
            _ok "  ${input_user}@${n} — OK"
        else
            _warn "  ${input_user}@${n} — ÉCHEC"
            ok=false
        fi
    done
    echo ""
    $ok && _ok "Tous les nœuds sont joignables sans mot de passe" || \
          _warn "Certains nœuds sont injoignables — l'installation peut échouer"

    # Recharger pour que SSH_OPTS soit à jour pour le reste de la session
    _load_config
}

# ── Opérations d'installation ─────────────────────────────────────────────────
_write_or_replace_conf_var() {
    local key="$1" value="$2"
    if grep -q "^${key}=" "$CONF_FILE" 2>/dev/null; then
        sed -i "s|^${key}=.*|${key}=\"${value}\"|" "$CONF_FILE"
    else
        echo "${key}=\"${value}\"" >> "$CONF_FILE"
    fi
}

do_provision_vms() {
    _need_config || return
    _hdr "══ Provisioning VMs physiques Omega ══"
    echo -e "  Cette étape crée de vraies VMs Proxmox sur le cluster."
    echo -e "  Elle utilise ${CYAN}${CONTROLLER_NODE}${RESET} comme nœud contrôleur et ${CYAN}${OMEGA_PROVISION_VMIDS}${RESET} comme VMIDs."
    echo ""

    local provision_vmids="${OMEGA_PROVISION_VMIDS:-$OMEGA_TEST_VMIDS}"
    local storage="${OMEGA_VM_STORAGE}"
    local bridge="${OMEGA_VM_BRIDGE}"
    local image_remote="${OMEGA_VM_IMAGE_REMOTE}"
    local image_local="${OMEGA_VM_IMAGE_LOCAL}"
    local sshkey_remote="${OMEGA_VM_SSHKEY_REMOTE}"
    local memory="${OMEGA_VM_MEMORY}"
    local balloon="${OMEGA_VM_BALLOON}"
    local cores="${OMEGA_VM_CORES}"
    local sockets="${OMEGA_VM_SOCKETS}"
    local vcpus="${OMEGA_VM_VCPUS}"
    local disk_max_gib="${OMEGA_VM_DISK_MAX_GIB}"
    local gpu_vram_mib="${OMEGA_VM_GPU_VRAM_MIB}"

    if ! $AUTO; then
        _ask "VMIDs à créer [${provision_vmids}]"
        read -r input
        provision_vmids="${input:-$provision_vmids}"
        if ! provision_vmids="$(_normalize_vmids_spec "$provision_vmids")"; then
            _err "Format VMIDs invalide. Exemples valides: 9001,9002,9003 ou 9001-9150"
            return 1
        fi

        _ask "Storage Proxmox cible [${storage}]"
        read -r input
        storage="${input:-$storage}"

        _ask "Bridge VM [${bridge}]"
        read -r input
        bridge="${input:-$bridge}"

        _ask "Image qcow2 distante [${image_remote}]"
        read -r input
        image_remote="${input:-$image_remote}"

        echo -e "  ${DIM}Si l'image existe déjà sur le cluster, laissez vide.${RESET}"
        _ask "Image qcow2 locale à copier [${image_local:-aucune}]"
        read -r input
        image_local="${input:-$image_local}"

        _ask "Clé publique distante injectée par cloud-init [${sshkey_remote}]"
        read -r input
        sshkey_remote="${input:-$sshkey_remote}"

        _ask "RAM max VM MiB [${memory}]"
        read -r input
        memory="${input:-$memory}"

        _ask "RAM initiale/balloon MiB [${balloon}]"
        read -r input
        balloon="${input:-$balloon}"

        _ask "Max vCPU hotpluggable / cores [${cores}]"
        read -r input
        cores="${input:-$cores}"

        _ask "Sockets QEMU [${sockets}]"
        read -r input
        sockets="${input:-$sockets}"

        _ask "vCPU au boot [${vcpus}]"
        read -r input
        vcpus="${input:-$vcpus}"

        _ask "Budget disque logique GiB [${disk_max_gib}]"
        read -r input
        disk_max_gib="${input:-$disk_max_gib}"

        _ask "Budget VRAM MiB [${gpu_vram_mib}]"
        read -r input
        gpu_vram_mib="${input:-$gpu_vram_mib}"
    fi

    [[ -n "$storage" ]] || { _err "Storage vide"; return 1; }
    [[ -n "$image_remote" ]] || { _err "Image distante vide"; return 1; }
    [[ -n "$provision_vmids" ]] || { _err "VMIDs provisioning vides"; return 1; }
    [[ "$cores" =~ ^[0-9]+$ && "$sockets" =~ ^[0-9]+$ && "$vcpus" =~ ^[0-9]+$ ]] || { _err "cores/sockets/vcpus doivent être numériques"; return 1; }
    local max_vcpus=$(( cores * sockets ))
    if [[ "$max_vcpus" -le 1 ]]; then
        _err "cores*sockets=$max_vcpus invalide pour les VMs Omega: il faut au moins 2 vCPU max pour tester le scale-up"
        return 1
    fi
    if [[ "$vcpus" -ge "$max_vcpus" ]]; then
        _err "vcpus=$vcpus >= cores*sockets=$max_vcpus: la VM démarrerait déjà au plafond et ne pourrait pas tester le scale-up"
        return 1
    fi

    _write_or_replace_conf_var "OMEGA_PROVISION_VMIDS" "$provision_vmids"
    _write_or_replace_conf_var "OMEGA_VM_STORAGE" "$storage"
    _write_or_replace_conf_var "OMEGA_VM_BRIDGE" "$bridge"
    _write_or_replace_conf_var "OMEGA_VM_IMAGE_REMOTE" "$image_remote"
    _write_or_replace_conf_var "OMEGA_VM_IMAGE_LOCAL" "$image_local"
    _write_or_replace_conf_var "OMEGA_VM_SSHKEY_REMOTE" "$sshkey_remote"
    _write_or_replace_conf_var "OMEGA_VM_MEMORY" "$memory"
    _write_or_replace_conf_var "OMEGA_VM_BALLOON" "$balloon"
    _write_or_replace_conf_var "OMEGA_VM_CORES" "$cores"
    _write_or_replace_conf_var "OMEGA_VM_SOCKETS" "$sockets"
    _write_or_replace_conf_var "OMEGA_VM_VCPUS" "$vcpus"
    _write_or_replace_conf_var "OMEGA_VM_DISK_MAX_GIB" "$disk_max_gib"
    _write_or_replace_conf_var "OMEGA_VM_GPU_VRAM_MIB" "$gpu_vram_mib"
    _load_config

    echo ""
    echo -e "  ${BOLD}Résumé provisioning${RESET}"
    echo -e "    Contrôleur : ${CYAN}${CONTROLLER_NODE}${RESET}"
    echo -e "    VMIDs      : ${CYAN}${provision_vmids}${RESET}"
    echo -e "    Storage    : ${CYAN}${storage}${RESET}"
    echo -e "    Bridge     : ${CYAN}${bridge}${RESET}"
    echo -e "    Image dist : ${CYAN}${image_remote}${RESET}"
    [[ -n "$image_local" ]] && echo -e "    Image loc  : ${CYAN}${image_local}${RESET}"
    echo -e "    vCPU       : ${CYAN}${vcpus}/${max_vcpus}${RESET} ${DIM}(cores=${cores}, sockets=${sockets})${RESET}"
    echo -e "    RAM        : ${CYAN}${balloon}/${memory} MiB${RESET}"
    echo ""

    if ! $AUTO; then
        read -rp "  Créer ces VMs réelles maintenant ? [oui/N] " confirm
        [[ "$confirm" =~ ^[Oo]([Uu][Ii])?$ ]] || { _info "Provisioning annulé."; return; }
    fi

    local args=(
        --controller "$CONTROLLER_NODE"
        --user "$DEPLOY_USER"
        --vmids "$provision_vmids"
        --storage "$storage"
        --bridge "$bridge"
        --image-remote "$image_remote"
        --sshkey-remote "$sshkey_remote"
        --memory "$memory"
        --balloon "$balloon"
        --cores "$cores"
        --sockets "$sockets"
        --vcpus "$vcpus"
        --disk-max-gib "$disk_max_gib"
        --gpu-vram-mib "$gpu_vram_mib"
    )
    [[ -n "$image_local" ]] && args+=(--image-local "$image_local")
    [[ -f "${SSH_KEY:-}" ]] && args+=(--ssh-key "$SSH_KEY")

    bash "${SCRIPT_DIR}/provision-omega-vms-remote.sh" "${args[@]}"
    _ok "Provisioning VM terminé"
}

do_build() {
    _hdr "══ Build ══"
    cd "$ROOT_DIR"
    _info "make build-bridge"
    make build-bridge
    _info "cargo build --release --workspace"
    cargo build --release --workspace
    _ok "Build terminé"
    if [[ -f "${ROOT_DIR}/omega-uffd-bridge/omega-uffd-bridge.so" ]]; then
        _ok "  omega-uffd-bridge.so  $(ls -lh "${ROOT_DIR}/omega-uffd-bridge/omega-uffd-bridge.so" | awk '{print $5}')"
    else
        _warn "  omega-uffd-bridge.so absent"
    fi
    for bin in omega-daemon node-a-agent node-bc-store omega-qemu-launcher omega-gpu-proxy; do
        local b="${ROOT_DIR}/target/release/${bin}"
        [[ -x "$b" ]] && _ok "  ${bin}  $(ls -lh "$b" | awk '{print $5}')" || \
                          _warn "  ${bin} absent"
    done
    cd - &>/dev/null
}

do_uninstall() {
    _need_config || return
    _hdr "══ Désinstallation ══"
    _warn "Arrêt des services et suppression des fichiers sur :"
    for n in "${NODES_ARR[@]}"; do echo -e "  ${CYAN}${n}${RESET}"; done
    echo ""
    if ! $AUTO; then
        read -rp "  Confirmer la désinstallation ? [oui/N] " confirm
        [[ "$confirm" =~ ^[Oo]([Uu][Ii])?$ ]] || { _info "Annulé."; return; }
    fi
    OMEGA_NODES="$OMEGA_NODES" \
    OMEGA_CONTROLLER="$CONTROLLER_NODE" \
    DEPLOY_USER="$DEPLOY_USER" \
    SSH_KEY="${SSH_KEY:-}" \
    bash "${SCRIPT_DIR}/uninstall.sh"
    _ok "Désinstallation terminée"
}

do_deploy() {
    _need_config || return
    _hdr "══ Déploiement ══"
    _info "Déploiement sur ${#NODES_ARR[@]} nœud(s) : ${OMEGA_NODES}"
    OMEGA_NODES="$OMEGA_NODES" \
    OMEGA_CONTROLLER="$CONTROLLER_NODE" \
    DEPLOY_USER="$DEPLOY_USER" \
    SSH_KEY="${SSH_KEY:-}" \
    OMEGA_GPU_PROXY_ENABLED="$($DO_GPU && echo 1 || echo 0)" \
    OMEGA_GPU_PROXY_LISTEN="${OMEGA_GPU_PROXY_LISTEN}" \
    OMEGA_GPU_PROXY_MAX_CONCURRENT="${OMEGA_GPU_PROXY_MAX_CONCURRENT}" \
    OMEGA_GPU_PROXY_TOTAL_VRAM_MIB="${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}" \
    OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS="${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS}" \
    OMEGA_GPU_NODES="${OMEGA_GPU_NODES:-}" \
    OMEGA_GPU_PRIMARY_NODE="${OMEGA_GPU_PRIMARY_NODE:-}" \
    OMEGA_GPU_PROXY_URL="${OMEGA_GPU_PROXY_URL:-}" \
    OMEGA_GPU_PROXY_API_TOKEN_FILE="${OMEGA_GPU_PROXY_API_TOKEN_FILE:-/etc/omega/gpu-proxy.token}" \
    OMEGA_GPU_PROXY_API_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}" \
    OMEGA_GPU_MIGRATE_TO_GPU_NODE="${OMEGA_GPU_MIGRATE_TO_GPU_NODE}" \
    OMEGA_GPU_FALLBACK_NETWORK="${OMEGA_GPU_FALLBACK_NETWORK}" \
    bash "${SCRIPT_DIR}/deploy.sh"
    _ok "Déploiement terminé"
    _sync
}

do_install_full() {
    _need_config || return
    _hdr "══ Installation complète ══"
    echo -e "  Étapes : ${CYAN}désinstallation → build → déploiement${RESET}"
    echo -e "  Nœuds  : ${CYAN}${OMEGA_NODES}${RESET} (${#NODES_ARR[@]} nœud(s))"
    echo ""
    if ! $AUTO; then
        read -rp "  Lancer l'installation complète ? [oui/N] " confirm
        [[ "$confirm" =~ ^[Oo]([Uu][Ii])?$ ]] || { _info "Annulé."; return; }
    fi
    do_uninstall
    do_build
    do_deploy
}

# ── Sections de tests ─────────────────────────────────────────────────────────
run_section_1() {
    _need_config || return
    _hdr "══ Section 1 — Tests isolés (smoke, réplication, failover, éviction) ══"
    _sync
    _run_local    "00" "Tests unitaires Rust"  "00-unit-tests.sh"
    _run_isolated "01" "Smoke test"            "01-smoke-test.sh"
    _run_isolated "02" "Réplication 2 stores"  "02-replication.sh"
    _run_isolated "03" "Failover store"        "03-failover.sh"
    _run_isolated "04" "Éviction daemon"       "04-eviction-daemon.sh" 20
    _run_isolated "10" "Multi-VM 3 agents"     "10-multi-vm.sh"
}

run_section_2() {
    _need_config || return
    _hdr "══ Section 2 — Fonctionnalités avancées store ══"
    _sync
    _run_isolated "18" "Recall LIFO"         "18-recall-lifo.sh"
    _run_isolated "20" "Prefetch stride"     "20-prefetch-stride.sh"
    _run_isolated "21" "TLS TOFU"            "21-tls-tofu.sh"
    _run_local    "23" "Disk I/O scheduler"  "23-disk-io-scheduler.sh"
}

run_section_3() {
    _need_config || return
    _hdr "══ Section 3 — Tests cluster (vCPU, migration, balloon, compaction) ══"
    _sync
    _run_cluster "05" "vCPU élastique"            "05-vcpu-elastic.sh"         "${TEST_VMIDS_ARR[0]}"
    _run_cluster "08" "Migration RAM"             "08-migration-ram.sh"        "${TEST_VMIDS_ARR[0]}"
    _run_cluster "09" "Orphan cleaner"            "09-orphan-cleaner.sh"
    _run_cluster "19" "Compaction cluster"        "19-compaction.sh"           "${TEST_VMIDS_ARR[0]}"
    _run_cluster "22" "Balloon thin-provisioning" "22-balloon-thinprov.sh"     "${TEST_VMIDS_ARR[0]}"
    _run_cluster "23C" "Disk I/O scheduler cluster" "23-disk-io-scheduler.sh"  "${TEST_VMIDS_ARR[0]}"
}

run_section_4() {
    _need_config || return
    _hdr "══ Section 4 — Tests GPU ══"
    if ! $DO_GPU; then
        _warn "GPU non activé — relancez avec --gpu pour activer ces tests"
        for t in 06 07 32 34 35 36; do RESULTS["$t"]="SKIP"; ((TOTAL_SKIP++)) || true; done
        return
    fi
    _sync
    _run_cluster "06" "GPU placement"  "06-gpu-placement.sh"  "${TEST_VMIDS_ARR[0]}"
    if [[ -n "${TEST_VMIDS_ARR[1]:-}" ]]; then
        _run_cluster "07" "GPU scheduler"  "07-gpu-scheduler.sh"  "${TEST_VMIDS_ARR[0]}" "${TEST_VMIDS_ARR[1]}"
    else
        _warn "Test 07 ignoré — OMEGA_TEST_VMIDS doit contenir au moins 2 VMs"
        RESULTS["07"]="SKIP"; ((TOTAL_SKIP++)) || true
    fi
    _run_cluster "32" "GPU proxy applicatif" "32-gpu-proxy.sh" "${TEST_VMIDS_ARR[0]}"
    _run_cluster "34" "GPU placement global" "34-gpu-placement-global.sh" "${TEST_VMIDS_ARR[0]}"
    _run_cluster "35" "GPU fallback réseau"  "35-gpu-network-fallback.sh" "${TEST_VMIDS_ARR[0]}"
    _run_cluster "36" "GPU concurrence CUDA" "36-gpu-concurrency-cuda.sh" "${TEST_VMIDS_ARR[0]}"
}

run_section_5() {
    _need_config || return
    _hdr "══ Section 5 — Tests mixtes (stress, pression cluster) ══"
    _sync
    local nc="${#NODES_ARR[@]}"
    _run_cluster "M1" "RAM + CPU simultanés"        "11-mixed-ram-cpu.sh"               "${TEST_VMIDS_ARR[0]}"
    _run_cluster "M2" "CPU+RAM → migration"         "12-mixed-cpu-ram-migration.sh"     "${TEST_VMIDS_ARR[0]}"
    if $DO_GPU; then
        _run_cluster "M3" "GPU+CPU multi"           "13-mixed-gpu-cpu.sh"               "${TEST_VMIDS_ARR[0]}"
    else
        RESULTS["M3"]="SKIP"; ((TOTAL_SKIP++)) || true
    fi
    _run_cluster "M4" "Stress cluster complet"      "14-mixed-cluster-pressure.sh"      "$nc" 60
    _run_cluster "M5" "Live migration pression"     "15-mixed-live-migration-pressure.sh" "${TEST_VMIDS_ARR[0]}"
    _run_cluster "M6" "Rafale démarrages"           "16-mixed-burst-starts.sh"          6
    _run_cluster "M7" "Drain nœud"                  "17-mixed-drain-node.sh"            "${CONTROLLER_NODE}"
    if $DO_GPU; then
        _run_cluster "M8" "Placement GPU intelligent" "34-gpu-placement-global.sh"       "${TEST_VMIDS_ARR[0]}"
        _run_cluster "M9" "Fallback GPU réseau"       "35-gpu-network-fallback.sh"       "${TEST_VMIDS_ARR[0]}"
    else
        for t in M8 M9; do RESULTS["$t"]="SKIP"; ((TOTAL_SKIP++)) || true; done
    fi
}

run_section_6() {
    _need_config || return
    _hdr "══ Section 6 — Production physique (install, réseau, Ceph/GPU réel, panne, soak) ══"
    _sync
    _run_cluster "30" "Conformité VMs Omega"      "30-vm-conformity.sh" "${OMEGA_TEST_VMIDS}"
    _run_cluster "24" "Installation doctor"       "24-install-doctor.sh"
    _run_cluster "25" "Réseau VM invitée"         "25-vm-network.sh"       "${TEST_VMIDS_ARR[0]}"
    if $DO_CEPH; then
        _run_cluster "26" "Ceph réel"             "26-ceph-real.sh"
    else
        _warn "Test 26 ignoré — relancer avec --ceph"
        RESULTS["26"]="SKIP"; ((TOTAL_SKIP++)) || true
    fi
    if $DO_GPU; then
        _run_cluster "27" "GPU réel / rendu"      "27-gpu-real-render.sh"  "${TEST_VMIDS_ARR[0]}"
        _run_cluster "32" "GPU proxy applicatif"  "32-gpu-proxy.sh"        "${TEST_VMIDS_ARR[0]}"
        _run_cluster "34" "GPU placement global"  "34-gpu-placement-global.sh" "${TEST_VMIDS_ARR[0]}"
        _run_cluster "35" "GPU fallback réseau"   "35-gpu-network-fallback.sh" "${TEST_VMIDS_ARR[0]}"
        _run_cluster "36" "GPU concurrence CUDA"  "36-gpu-concurrency-cuda.sh" "${TEST_VMIDS_ARR[0]}"
    else
        _warn "Tests 27/32/34/35/36 ignorés — relancer avec --gpu"
        for t in 27 32 34 35 36; do RESULTS["$t"]="SKIP"; ((TOTAL_SKIP++)) || true; done
    fi
    if $DO_DESTRUCTIVE; then
        _run_cluster "28" "Partition réseau"      "28-network-partition.sh" "${NODES_ARR[1]:-}"
    else
        _warn "Test 28 ignoré — relancer avec --destructive"
        RESULTS["28"]="SKIP"; ((TOTAL_SKIP++)) || true
    fi
    if $DO_LONG; then
        _run_cluster "29" "Soak long physique"    "29-long-run-soak.sh"    "${TEST_VMIDS_ARR[0]}" "${OMEGA_SOAK_SECS:-1800}"
    else
        _warn "Test 29 ignoré — relancer avec --long"
        RESULTS["29"]="SKIP"; ((TOTAL_SKIP++)) || true
    fi
    if $DO_SCALE; then
        _run_cluster "31" "Scalabilité VMs physiques" "31-scale-vms.sh" \
            "${OMEGA_SCALE_VMIDS:-${OMEGA_PROVISION_VMIDS:-${OMEGA_TEST_VMIDS}}}" \
            "${OMEGA_SCALE_TARGET:-500}" \
            "${OMEGA_SCALE_BATCH_SIZE:-20}" \
            "${OMEGA_SCALE_SOAK_SECS:-1800}"
    else
        _warn "Test 31 ignoré — relancer avec --scale"
        RESULTS["31"]="SKIP"; ((TOTAL_SKIP++)) || true
    fi
    _run_cluster "33" "Métriques production" "33-production-metrics.sh"
}

# ── Pause inter-section ───────────────────────────────────────────────────────
_pause_section() {
    local name="$1"
    $AUTO && return 0
    echo ""
    _sep
    _results_line
    _sep
    echo -e "\n  Section ${BOLD}${name}${RESET} terminée."
    echo -e "  ${BOLD}[Entrée]${RESET} Section suivante   ${BOLD}[m]${RESET} Menu   ${BOLD}[r]${RESET} Résumé   ${BOLD}[q]${RESET} Quitter\n"
    read -rp "  Choix : " c
    case "${c,,}" in
        q) echo "Au revoir."; exit 0 ;;
        m) return 1 ;;
        r) show_results; read -rp "  [Entrée] " _ ;;
    esac
    return 0
}

run_all() {
    _need_config || return
    run_section_1; _pause_section "1 — Isolés"          || return
    run_section_2; _pause_section "2 — Store avancé"    || return
    run_section_3; _pause_section "3 — Cluster"         || return
    run_section_4; _pause_section "4 — GPU"             || return
    run_section_5; _pause_section "5 — Mixtes"          || return
    run_section_6
    show_results
}

run_production_full() {
    _need_config || return
    if ! $AUTO; then
        echo ""
        _warn "Validation production stricte: GPU, Ceph, longs, scale et destructifs seront activés."
        _warn "Aucun SKIP n'est accepté. Les tests destructifs peuvent perturber temporairement réseau/services."
        read -rp "  Taper OUI pour lancer : " confirm
        [[ "$confirm" == "OUI" ]] || { _warn "Validation production annulée"; return 1; }
    fi

    reset_results
    DO_GPU=true
    DO_CEPH=true
    DO_LONG=true
    DO_SCALE=true
    DO_DESTRUCTIVE=true
    export OMEGA_GPU_PROXY_REQUIRE_CUDA=1
    export OMEGA_GPU_PROXY_REQUIRE_PARALLEL=1
    export OMEGA_STRICT_FULL=1
    export OMEGA_REQUIRE_NO_SKIP=1

    local prev_auto="$AUTO"
    AUTO=true
    _hdr "Mode production strict — toutes les sections, aucun skip"
    run_all
    AUTO="$prev_auto"

    if [[ "$TOTAL_SKIP" -ne 0 ]]; then
        _err "Validation production refusée: $TOTAL_SKIP test(s) ignoré(s)."
        return 1
    fi
    if [[ "$TOTAL_FAIL" -ne 0 ]]; then
        _err "Validation production refusée: $TOTAL_FAIL test(s) en échec."
        return 1
    fi
    _ok "Validation production complète OK: aucun échec, aucun skip."
}

# ── Test individuel ───────────────────────────────────────────────────────────
run_one() {
    _need_config || return
    _sync
    case "${1^^}" in
        00) _run_local    "00" "Tests unitaires Rust"       "00-unit-tests.sh" ;;
        01) _run_isolated "01" "Smoke test"                 "01-smoke-test.sh" ;;
        02) _run_isolated "02" "Réplication 2 stores"       "02-replication.sh" ;;
        03) _run_isolated "03" "Failover store"             "03-failover.sh" ;;
        04) _run_isolated "04" "Éviction daemon"            "04-eviction-daemon.sh" 20 ;;
        10) _run_isolated "10" "Multi-VM 3 agents"          "10-multi-vm.sh" ;;
        18) _run_isolated "18" "Recall LIFO"                "18-recall-lifo.sh" ;;
        20) _run_isolated "20" "Prefetch stride"            "20-prefetch-stride.sh" ;;
        21) _run_isolated "21" "TLS TOFU"                   "21-tls-tofu.sh" ;;
        23) _run_local    "23" "Disk I/O scheduler"          "23-disk-io-scheduler.sh" ;;
        30) _run_cluster  "30" "Conformité VMs Omega"         "30-vm-conformity.sh"      "${OMEGA_TEST_VMIDS}" ;;
        05) _run_cluster  "05" "vCPU élastique"             "05-vcpu-elastic.sh"          "${TEST_VMIDS_ARR[0]}" ;;
        06) _run_cluster  "06" "GPU placement"              "06-gpu-placement.sh"         "${TEST_VMIDS_ARR[0]}" ;;
        07) if [[ -n "${TEST_VMIDS_ARR[1]:-}" ]]; then
                _run_cluster  "07" "GPU scheduler"          "07-gpu-scheduler.sh"         "${TEST_VMIDS_ARR[0]}" "${TEST_VMIDS_ARR[1]}"
            else
                _warn "Test 07 ignoré — OMEGA_TEST_VMIDS doit contenir au moins 2 VMs"
                RESULTS["07"]="SKIP"; ((TOTAL_SKIP++)) || true
            fi ;;
        08) _run_cluster  "08" "Migration RAM"              "08-migration-ram.sh"         "${TEST_VMIDS_ARR[0]}" ;;
        09) _run_cluster  "09" "Orphan cleaner"             "09-orphan-cleaner.sh" ;;
        19) _run_cluster  "19" "Compaction cluster"         "19-compaction.sh"            "${TEST_VMIDS_ARR[0]}" ;;
        22) _run_cluster  "22" "Balloon thin-provisioning"  "22-balloon-thinprov.sh"      "${TEST_VMIDS_ARR[0]}" ;;
        24) _run_cluster  "24" "Installation doctor"        "24-install-doctor.sh" ;;
        25) _run_cluster  "25" "Réseau VM invitée"          "25-vm-network.sh"            "${TEST_VMIDS_ARR[0]}" ;;
        26) _run_cluster  "26" "Ceph réel"                  "26-ceph-real.sh" ;;
        27) _run_cluster  "27" "GPU réel / rendu"           "27-gpu-real-render.sh"       "${TEST_VMIDS_ARR[0]}" ;;
        32) _run_cluster  "32" "GPU proxy applicatif"        "32-gpu-proxy.sh"             "${TEST_VMIDS_ARR[0]}" ;;
        33) _run_cluster  "33" "Métriques production"        "33-production-metrics.sh" ;;
        34) _run_cluster  "34" "GPU placement global"        "34-gpu-placement-global.sh"  "${TEST_VMIDS_ARR[0]}" ;;
        35) _run_cluster  "35" "GPU fallback réseau"         "35-gpu-network-fallback.sh"  "${TEST_VMIDS_ARR[0]}" ;;
        36) _run_cluster  "36" "GPU concurrence CUDA"        "36-gpu-concurrency-cuda.sh"  "${TEST_VMIDS_ARR[0]}" ;;
        28) _run_cluster  "28" "Partition réseau"           "28-network-partition.sh"     "${NODES_ARR[1]:-}" ;;
        29) _run_cluster  "29" "Soak long physique"         "29-long-run-soak.sh"         "${TEST_VMIDS_ARR[0]}" "${OMEGA_SOAK_SECS:-1800}" ;;
        31) _run_cluster  "31" "Scalabilité VMs physiques"   "31-scale-vms.sh"            "${OMEGA_SCALE_VMIDS:-${OMEGA_PROVISION_VMIDS:-${OMEGA_TEST_VMIDS}}}" "${OMEGA_SCALE_TARGET:-500}" "${OMEGA_SCALE_BATCH_SIZE:-20}" "${OMEGA_SCALE_SOAK_SECS:-1800}" ;;
        M1) _run_cluster  "M1" "RAM + CPU simultanés"       "11-mixed-ram-cpu.sh"         "${TEST_VMIDS_ARR[0]}" ;;
        M2) _run_cluster  "M2" "CPU+RAM → migration"        "12-mixed-cpu-ram-migration.sh" "${TEST_VMIDS_ARR[0]}" ;;
        M3) _run_cluster  "M3" "GPU+CPU multi"              "13-mixed-gpu-cpu.sh"         "${TEST_VMIDS_ARR[0]}" ;;
        M4) _run_cluster  "M4" "Stress cluster"             "14-mixed-cluster-pressure.sh" "${#NODES_ARR[@]}" 60 ;;
        M5) _run_cluster  "M5" "Live migration pression"    "15-mixed-live-migration-pressure.sh" "${TEST_VMIDS_ARR[0]}" ;;
        M6) _run_cluster  "M6" "Rafale démarrages"          "16-mixed-burst-starts.sh"        6 ;;
        M7) _run_cluster  "M7" "Drain nœud"                 "17-mixed-drain-node.sh"          "${CONTROLLER_NODE}" ;;
        M8) _run_cluster  "M8" "Placement GPU intelligent"  "34-gpu-placement-global.sh"      "${TEST_VMIDS_ARR[0]}" ;;
        M9) _run_cluster  "M9" "Fallback GPU réseau"        "35-gpu-network-fallback.sh"      "${TEST_VMIDS_ARR[0]}" ;;
        *)  _warn "ID de test inconnu : $1" ;;
    esac
}

# ── Menu principal ────────────────────────────────────────────────────────────
show_menu() {
    clear
    echo ""
    echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}${BLUE}║          omega-remote-paging — Lab interactif                    ║${RESET}"
    echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════╝${RESET}"
    echo ""

    # ── Statut cluster ────────────────────────────────────────────────────────
    if $CONFIGURED; then
        echo -e "  ${GREEN}●${RESET} Cluster configuré"
        echo -e "    Nœuds (${#NODES_ARR[@]}) : ${CYAN}${OMEGA_NODES}${RESET}"
        echo -e "    Contrôleur        : ${CYAN}${CONTROLLER_NODE}${RESET}"
        echo -e "    VMs test          : ${CYAN}${OMEGA_TEST_VMIDS}${RESET}  ${DIM}(${#TEST_VMIDS_ARR[@]} VM(s))${RESET}"
        echo -e "    VMs provisioning  : ${CYAN}${OMEGA_PROVISION_VMIDS}${RESET}"
        echo -e "    VM provisioning   : ${CYAN}${OMEGA_VM_STORAGE}${RESET} · ${CYAN}${OMEGA_VM_BRIDGE}${RESET} · ${DIM}${OMEGA_VM_IMAGE_REMOTE}${RESET}"
        echo -e "    GPU               : $(${DO_GPU} && echo "${GREEN}activé${RESET}" || echo "${DIM}non (--gpu)${RESET}") · nodes=${CYAN}${OMEGA_GPU_NODES:-auto}${RESET} · primary=${CYAN}${OMEGA_GPU_PRIMARY_NODE:-auto}${RESET} · proxy=${CYAN}${OMEGA_GPU_PROXY_URL:-auto}${RESET} · vram=${CYAN}${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-0}${RESET}MiB"
        echo -e "    Long/scale        : $(${DO_LONG} && echo "${GREEN}long${RESET}" || echo "${DIM}long off${RESET}") / $(${DO_SCALE} && echo "${GREEN}scale${RESET}" || echo "${DIM}scale off${RESET}")"
        echo -e "    Scale config      : ${CYAN}${OMEGA_SCALE_STEPS:-auto 50..target}${RESET} · cleanup=${CYAN}${OMEGA_SCALE_CLEANUP_SCOPE}${RESET} · stop=${CYAN}${OMEGA_SCALE_STOP_AFTER}${RESET} · destroy=${CYAN}${OMEGA_SCALE_DESTROY_AFTER}${RESET}"
        echo -e "    Destructif        : $(${DO_DESTRUCTIVE} && echo "${RED}activé${RESET}" || echo "${DIM}off${RESET}")"
    else
        echo -e "  ${RED}●${RESET} ${BOLD}Cluster non configuré${RESET} — entrez ${BOLD}[c]${RESET} pour définir les nœuds"
    fi

    # ── Binaires ──────────────────────────────────────────────────────────────
    local bin_ok=true
    for b in node-a-agent node-bc-store; do
        [[ -x "${ROOT_DIR}/target/release/${b}" ]] || { bin_ok=false; break; }
    done
    if $bin_ok; then
        local ts; ts=$(stat -c %y "${ROOT_DIR}/target/release/node-a-agent" 2>/dev/null | cut -d. -f1 || echo "?")
        echo -e "    Binaires          : ${GREEN}compilés${RESET}  ${DIM}(${ts})${RESET}"
    else
        echo -e "    Binaires          : ${YELLOW}non compilés${RESET} — ${DIM}entrez [b] pour builder${RESET}"
    fi

    echo ""
    _sep
    _results_line
    _sep
    echo ""

    # ── Configuration ─────────────────────────────────────────────────────────
    echo -e "  ${BOLD}${MAG}── Configuration ────────────────────────────────────────────${RESET}"
    echo -e "   ${BOLD}[c]${RESET}  Configurer les nœuds du cluster (IPs, VM test, user SSH)"
    echo ""

    # ── Installation ──────────────────────────────────────────────────────────
    echo -e "  ${BOLD}${MAG}── Installation ─────────────────────────────────────────────${RESET}"
    echo -e "   ${BOLD}[I]${RESET}  Installation complète  (désinstaller → build → déployer)"
    echo -e "   ${BOLD}[u]${RESET}  Désinstaller           (arrêter services + supprimer fichiers)"
    echo -e "   ${BOLD}[b]${RESET}  Build                  (cargo build --release --workspace)"
    echo -e "   ${BOLD}[d]${RESET}  Déployer               (copier binaires + démarrer services)"
    echo -e "   ${BOLD}[p]${RESET}  Créer les VMs physiques Omega sur Proxmox"
    echo ""

    # ── Tests par section ─────────────────────────────────────────────────────
    echo -e "  ${BOLD}${MAG}── Tests ─────────────────────────────────────────────────────${RESET}"
    echo -e "   ${BOLD}[A]${RESET}  Tout — sections 1→6 avec pause entre chaque"
    echo -e "   ${BOLD}[P]${RESET}  Production stricte — tout le projet, aucun skip, GPU/Ceph/long/scale/destructif"
    echo -e "   ${BOLD}[1]${RESET}  Section 1 — Isolés    : smoke · réplication · failover · éviction"
    echo -e "   ${BOLD}[2]${RESET}  Section 2 — Store+    : recall LIFO · prefetch · TLS TOFU"
    echo -e "   ${BOLD}[3]${RESET}  Section 3 — Cluster   : vCPU · migration · balloon · compaction"
    echo -e "   ${BOLD}[4]${RESET}  Section 4 — GPU       : placement · scheduler · proxy · CUDA$(${DO_GPU} && echo '' || echo '  (--gpu requis)')"
    echo -e "   ${BOLD}[5]${RESET}  Section 5 — Mixtes    : stress · live migration · drain · GPU"
    echo -e "   ${BOLD}[6]${RESET}  Section 6 — Physique  : install · réseau VM · Ceph/GPU réel · panne · soak"
    echo ""

    # ── Tests individuels ─────────────────────────────────────────────────────
    echo -e "  ${BOLD}${MAG}── Test individuel (entrer le numéro) ───────────────────────${RESET}"
    echo -e "   ${DIM}Isolés  :${RESET}  00  01  02  03  04  10  18  20  21  23"
    echo -e "   ${DIM}Cluster :${RESET}  05  06  07  08  09  19  22  24  25  26  27  28  29  30  31  32  33  34  35  36"
    echo -e "   ${DIM}Mixtes  :${RESET}  M1  M2  M3  M4  M5  M6  M7  M8  M9"
    echo ""

    echo -e "   ${BOLD}[g]${RESET}  GPU tests : $(${DO_GPU} && echo "${GREEN}activé  ${RESET}→ [g] pour désactiver" || echo "${YELLOW}désactivé${RESET} → [g] pour activer")"
    echo -e "   ${BOLD}[l]${RESET}  Long tests : $(${DO_LONG} && echo "${GREEN}activé  ${RESET}→ [l] pour désactiver" || echo "${YELLOW}désactivé${RESET} → [l] pour activer")"
    echo -e "   ${BOLD}[s]${RESET}  Scale 500 : $(${DO_SCALE} && echo "${GREEN}activé  ${RESET}→ [s] pour désactiver" || echo "${YELLOW}désactivé${RESET} → [s] pour activer")"
    echo -e "   ${BOLD}[x]${RESET}  Destructif : $(${DO_DESTRUCTIVE} && echo "${RED}activé  ${RESET}→ [x] pour désactiver" || echo "${YELLOW}désactivé${RESET} → [x] pour activer")"
    echo -e "   ${BOLD}[r]${RESET}  Résumé détaillé des résultats       ${BOLD}[q]${RESET}  Quitter"
    echo ""
    _sep
    echo -ne "  Choix : "
}

# ── Boucle principale ─────────────────────────────────────────────────────────
main_loop() {
    while true; do
        show_menu
        read -r choice || choice="q"
        echo ""
        case "${choice}" in
            c|C) do_configure ;;
            I)   do_install_full ;;
            u|U) do_uninstall ;;
            b|B) do_build ;;
            d|D) do_deploy ;;
            p)   do_provision_vms ;;
            P)   run_production_full ;;
            A)   run_all ;;
            1)   run_section_1 ;;
            2)   run_section_2 ;;
            3)   run_section_3 ;;
            4)   run_section_4 ;;
            5)   run_section_5 ;;
            6)   run_section_6 ;;
            g|G) if $DO_GPU; then DO_GPU=false; _info "Tests GPU désactivés"
                 else DO_GPU=true; _info "Tests GPU activés"
                 fi ;;
            l|L) if $DO_LONG; then DO_LONG=false; _info "Tests longue durée désactivés"
                 else DO_LONG=true; _info "Tests longue durée activés"
                 fi ;;
            s|S) if $DO_SCALE; then DO_SCALE=false; _info "Tests scalabilité désactivés"
                 else DO_SCALE=true; _info "Tests scalabilité activés"
                 fi ;;
            x|X) if $DO_DESTRUCTIVE; then DO_DESTRUCTIVE=false; _info "Tests destructifs désactivés"
                 else DO_DESTRUCTIVE=true; _warn "Tests destructifs activés"
                 fi ;;
            r|R) show_results ;;
            q|Q) echo "Au revoir."; exit 0 ;;
            "")  continue ;;
            # Tests individuels : 00-22 ou M1-M7 (insensible casse)
            [0-9][0-9]|[0-9]|M[1-9]|m[1-9]) run_one "$choice" ;;
            *)   _warn "Choix inconnu : '${choice}'" ;;
        esac

        if ! $AUTO && [[ "${choice}" != "" ]]; then
            echo ""
            read -rp "  [Entrée] revenir au menu  [q] quitter : " back
            [[ "${back,,}" == "q" ]] && { echo "Au revoir."; exit 0; }
        fi
    done
}

# ── Mode auto (CI) ────────────────────────────────────────────────────────────
if $AUTO; then
    if ! $CONFIGURED; then
        echo "Mode --auto : OMEGA_NODES requis (cluster.conf ou variable d'environnement)"
        exit 1
    fi
    _hdr "Mode automatique — installation + toutes les sections"
    do_install_full
    $DO_PROVISION && do_provision_vms
    if $DO_PRODUCTION; then
        run_production_full
        [[ $TOTAL_FAIL -eq 0 && $TOTAL_SKIP -eq 0 ]] && exit 0 || exit 1
    else
        run_all
        show_results
        [[ $TOTAL_FAIL -eq 0 ]] && exit 0 || exit 1
    fi
fi

main_loop
