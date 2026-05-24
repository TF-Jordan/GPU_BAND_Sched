#!/usr/bin/env bash
# Fonctions communes — omega-remote-paging
# Supporte N nœuds (pas seulement 3)
#
# Variables d'environnement clés :
#   OMEGA_NODES         — liste IPs/hostnames séparés par virgule (lu depuis scripts/cluster.conf si absent)
#   OMEGA_CONTROLLER    — IP du nœud contrôleur (défaut: premier de OMEGA_NODES)
#   OMEGA_STORE_PORT    — port TCP store    (défaut: 9100)
#   OMEGA_STATUS_PORT   — port HTTP status  (défaut: 9200)
#   OMEGA_METRICS_PORT  — port HTTP metrics (défaut: 9300)
#   OMEGA_BIN_DIR       — répertoire des binaires (défaut: target/release/)
#   OMEGA_TEST_VMID     — vmid de la VM de test (défaut: 9001)
#   OMEGA_SKIP          — tests à ignorer (ex: "06,07")

set -euo pipefail

# Désactive les codes ANSI dans les binaires Rust (tracing-subscriber respecte NO_COLOR)
export NO_COLOR=1

# ── Couleurs ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; RESET='\033[0m'

# ── Chemins ───────────────────────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN_DIR="${OMEGA_BIN_DIR:-$REPO_ROOT/target/release}"
AGENT_BIN="$BIN_DIR/node-a-agent"
STORE_BIN="$BIN_DIR/node-bc-store"

# ── Ports ─────────────────────────────────────────────────────────────────────
STORE_PORT="${OMEGA_STORE_PORT:-${STORE_PORT:-9100}}"
STATUS_PORT="${OMEGA_STATUS_PORT:-${STATUS_PORT:-9200}}"
# 9300 est réservé à omega-daemon (contrôle HTTP) — les agents de test utilisent 9310
METRICS_PORT="${OMEGA_METRICS_PORT:-${METRICS_PORT:-9310}}"

# ── Nœuds : liste complète, contrôleur, stores ───────────────────────────────
# Source cluster.conf si OMEGA_NODES n'est pas défini dans l'environnement
if [[ -z "${OMEGA_NODES:-}" ]]; then
    _conf="$(dirname "${BASH_SOURCE[0]}")/../cluster.conf"
    [[ -f "$_conf" ]] && source "$_conf"
fi
# En cluster réel, l'installation écrit la configuration effective du nœud
# dans /etc/omega/cluster.env (ex: nœud GPU détecté, URL proxy, token file).
# On ne s'en sert que comme fallback pour éviter de figer des noms de cluster
# dans les tests; les variables explicitement passées par omega-lab.sh gardent
# la priorité.
if [[ -f /etc/omega/cluster.env ]]; then
    _env_OMEGA_NODES="${OMEGA_NODES:-}"
    _env_OMEGA_CONTROLLER="${OMEGA_CONTROLLER:-}"
    _env_OMEGA_GPU_NODES="${OMEGA_GPU_NODES:-}"
    _env_OMEGA_GPU_PRIMARY_NODE="${OMEGA_GPU_PRIMARY_NODE:-}"
    _env_OMEGA_GPU_PROXY_URL="${OMEGA_GPU_PROXY_URL:-}"
    _env_OMEGA_GPU_PROXY_TOTAL_VRAM_MIB="${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:-}"
    _env_OMEGA_GPU_PROXY_API_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
    _env_OMEGA_GPU_PROXY_API_TOKEN_FILE="${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}"
    # shellcheck disable=SC1091
    source /etc/omega/cluster.env
    [[ -n "$_env_OMEGA_NODES" ]] && OMEGA_NODES="$_env_OMEGA_NODES"
    [[ -n "$_env_OMEGA_CONTROLLER" ]] && OMEGA_CONTROLLER="$_env_OMEGA_CONTROLLER"
    [[ -n "$_env_OMEGA_GPU_NODES" ]] && OMEGA_GPU_NODES="$_env_OMEGA_GPU_NODES"
    [[ -n "$_env_OMEGA_GPU_PRIMARY_NODE" ]] && OMEGA_GPU_PRIMARY_NODE="$_env_OMEGA_GPU_PRIMARY_NODE"
    [[ -n "$_env_OMEGA_GPU_PROXY_URL" ]] && OMEGA_GPU_PROXY_URL="$_env_OMEGA_GPU_PROXY_URL"
    [[ -n "$_env_OMEGA_GPU_PROXY_TOTAL_VRAM_MIB" ]] && OMEGA_GPU_PROXY_TOTAL_VRAM_MIB="$_env_OMEGA_GPU_PROXY_TOTAL_VRAM_MIB"
    [[ -n "$_env_OMEGA_GPU_PROXY_API_TOKEN" ]] && OMEGA_GPU_PROXY_API_TOKEN="$_env_OMEGA_GPU_PROXY_API_TOKEN"
    [[ -n "$_env_OMEGA_GPU_PROXY_API_TOKEN_FILE" ]] && OMEGA_GPU_PROXY_API_TOKEN_FILE="$_env_OMEGA_GPU_PROXY_API_TOKEN_FILE"
    unset _env_OMEGA_NODES _env_OMEGA_CONTROLLER _env_OMEGA_GPU_NODES _env_OMEGA_GPU_PRIMARY_NODE \
        _env_OMEGA_GPU_PROXY_URL _env_OMEGA_GPU_PROXY_TOTAL_VRAM_MIB \
        _env_OMEGA_GPU_PROXY_API_TOKEN _env_OMEGA_GPU_PROXY_API_TOKEN_FILE
fi
STORE_PORT="${OMEGA_STORE_PORT:-${STORE_PORT:-9100}}"
STATUS_PORT="${OMEGA_STATUS_PORT:-${STATUS_PORT:-9200}}"
METRICS_PORT="${OMEGA_METRICS_PORT:-${METRICS_PORT:-9310}}"
[[ -n "${OMEGA_NODES:-}" ]] || { echo "ERREUR: OMEGA_NODES non défini (ex: export OMEGA_NODES=192.168.1.1,192.168.1.2,192.168.1.3)" >&2; exit 1; }

IFS=',' read -ra OMEGA_NODES_ARR <<< "$OMEGA_NODES"
CONTROLLER_NODE="${OMEGA_CONTROLLER:-${OMEGA_NODES_ARR[0]}}"

# Tous les nœuds sont des stores (architecture symétrique)
STORE_NODES_ARR=("${OMEGA_NODES_ARR[@]}")

# Chaînes CSV utilisées par les binaires
STORES_CSV=""
STATUS_CSV=""
for n in "${STORE_NODES_ARR[@]}"; do
    STORES_CSV="${STORES_CSV:+$STORES_CSV,}${n}:${STORE_PORT}"
    STATUS_CSV="${STATUS_CSV:+$STATUS_CSV,}${n}:${STATUS_PORT}"
done

# Compat legacy (COMPUTE_NODE pour les scripts qui l'utilisent encore)
PVE1="${OMEGA_NODES_ARR[0]}"
PVE2="${OMEGA_NODES_ARR[1]:-}"
PVE3="${OMEGA_NODES_ARR[2]:-}"
COMPUTE_NODE="$CONTROLLER_NODE"
TEST_VMID="${OMEGA_TEST_VMID:-9001}"
IFS=',' read -ra TEST_VMIDS_ARR <<< "${OMEGA_TEST_VMIDS:-$TEST_VMID}"
DEPLOY_USER="${DEPLOY_USER:-root}"
SSH_KEY="${SSH_KEY:-}"
# Les tableaux Bash ne traversent pas l'environnement. On reconstruit donc les
# options SSH depuis cluster.conf quand les tests sont lancés directement.
if ! declare -p SSH_OPTS &>/dev/null; then
    if [[ -n "$SSH_KEY" && -f "$SSH_KEY" ]]; then
        SSH_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=accept-new)
    else
        SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
    fi
fi

# ── PIDs / fichiers temporaires à nettoyer ────────────────────────────────────
_PIDS=()
_TMPFILES=()

cleanup() {
    for pid in "${_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    for f in "${_TMPFILES[@]}"; do
        [[ -e "$f" ]] || continue
        if [[ -d "$f" ]]; then
            rm -rf "$f" 2>/dev/null || true
        else
            rm -f "$f" 2>/dev/null || true
        fi
    done
    _PIDS=(); _TMPFILES=()
}
trap cleanup EXIT INT TERM

# ── Affichage ─────────────────────────────────────────────────────────────────
pass()   { echo -e "${GREEN}[PASS]${RESET} $*"; }
fail()   { echo -e "${RED}[FAIL]${RESET} $*"; exit 1; }
info()   { echo -e "${BLUE}[INFO]${RESET} $*"; }
warn()   { echo -e "${YELLOW}[WARN]${RESET} $*"; }
step()   { echo -e "\n${BOLD}▶ $*${RESET}"; }
header() {
    echo -e "\n${BOLD}${BLUE}══════════════════════════════════════════${RESET}"
    echo -e "${BOLD}${BLUE}  $*${RESET}"
    echo -e "${BOLD}${BLUE}══════════════════════════════════════════${RESET}"
}

# ── Prérequis ─────────────────────────────────────────────────────────────────
require_bin() {
    command -v "$1" &>/dev/null || fail "binaire manquant : $1"
}

require_omega_bins() {
    [[ -x "$AGENT_BIN" ]] || \
        fail "node-a-agent introuvable : $AGENT_BIN — lancer: cargo build --release --workspace"
    [[ -x "$STORE_BIN" ]] || fail "node-bc-store introuvable : $STORE_BIN"
}

ensure_local_packages() {
    local packages=("$@")
    local missing=()
    local pkg
    for pkg in "${packages[@]}"; do
        command -v "$pkg" >/dev/null 2>&1 || missing+=("$pkg")
    done
    [[ "${#missing[@]}" -eq 0 ]] && return 0

    [[ "$(id -u)" -eq 0 ]] || fail "paquets manquants sur le noeud local: ${missing[*]} — relancer en root ou installer: apt install -y ${missing[*]}"
    command -v apt-get >/dev/null 2>&1 || fail "apt-get introuvable; installer manuellement: ${missing[*]}"

    info "installation automatique locale: ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}"
}

ensure_node_packages() {
    local node="$1"; shift
    local packages=("$@")
    [[ "${#packages[@]}" -gt 0 ]] || return 0

    if _is_local_node "$node"; then
        ensure_local_packages "${packages[@]}"
        return
    fi

    local pkg_list="${packages[*]}"
    info "vérification paquets sur $node: ${pkg_list}"
    ssh_run "$node" "missing=''; for p in ${pkg_list}; do command -v \"\$p\" >/dev/null 2>&1 || missing=\"\$missing \$p\"; done; if [ -n \"\$missing\" ]; then export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y \$missing; fi"
}

ensure_node_packages_optional() {
    local node="$1"; shift
    local packages=("$@")
    [[ "${#packages[@]}" -gt 0 ]] || return 0

    local pkg_list="${packages[*]}"
    info "vérification paquets sur $node: ${pkg_list}"

    if _is_local_node "$node"; then
        local missing=() pkg
        for pkg in "${packages[@]}"; do
            command -v "$pkg" >/dev/null 2>&1 || missing+=("$pkg")
        done
        [[ "${#missing[@]}" -eq 0 ]] && return 0
        if [[ "$(id -u)" -ne 0 ]] || ! command -v apt-get >/dev/null 2>&1; then
            warn "paquets manquants sur le noeud local: ${missing[*]} — installation automatique impossible"
            return 1
        fi
        info "installation automatique locale optionnelle: ${missing[*]}"
        DEBIAN_FRONTEND=noninteractive apt-get update && \
            DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}" && return 0
        warn "installation automatique locale échouée: ${missing[*]}"
        return 1
    fi

    ssh_run "$node" "missing=''; for p in ${pkg_list}; do command -v \"\$p\" >/dev/null 2>&1 || missing=\"\$missing \$p\"; done; if [ -z \"\$missing\" ]; then exit 0; fi; export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y \$missing" && return 0
    warn "paquets indisponibles sur $node: ${pkg_list}; ce noeud sera ignoré pour la charge hôte qui en dépend"
    return 1
}

require_cluster() {
    require_bin qm
    require_bin pvesh
    [[ ${#OMEGA_NODES_ARR[@]} -ge 2 ]] || fail "cluster requires at least 2 nodes (OMEGA_NODES)"
    for n in "${STORE_NODES_ARR[@]}"; do
        [[ -z "$n" ]] && continue
        nc -z "$n" "$STORE_PORT" 2>/dev/null || \
            fail "store $n:$STORE_PORT inaccessible — démarrer omega-daemon sur $n"
    done
}

# ── Routage qm cluster ────────────────────────────────────────────────────────
# Cache vmid → IP du nœud hôte, alimenté par require_vm_running et vm_node.
declare -A _VM_NODE_CACHE=()

_vm_node_cached() {
    local vmid="$1"
    if [[ -z "${_VM_NODE_CACHE[$vmid]:-}" ]]; then
        _VM_NODE_CACHE[$vmid]=$(vm_node "$vmid" 2>/dev/null || echo "")
    fi
    echo "${_VM_NODE_CACHE[$vmid]}"
}

# Retourne 0 si le nœud donné (IP ou hostname) est la machine locale.
_is_local_node() {
    local node="$1"
    [[ -z "$node" ]] && return 0  # vide → on suppose local par sécurité
    # Vérifier les IPs locales
    hostname -I 2>/dev/null | tr ' ' '\n' | grep -qxF "$node" && return 0
    # Vérifier le nom d'hôte local (court et FQDN)
    local h
    for h in "$(hostname 2>/dev/null || true)" "$(hostname -s 2>/dev/null || true)"; do
        [[ -n "$h" && "$node" == "$h" ]] && return 0
    done
    return 1
}

_refresh_vm_node_cache() {
    local vmid="$1" node
    unset "_VM_NODE_CACHE[$vmid]"
    node="$(vm_node "$vmid" 2>/dev/null || true)"
    [[ -n "$node" ]] && _VM_NODE_CACHE[$vmid]="$node"
    echo "$node"
}

_qm_stale_owner_error() {
    local vmid="$1"
    grep -qiE "Configuration file 'nodes/.*/qemu-server/${vmid}\.conf' does not exist|unable to find configuration file|no such cluster node|VM ${vmid} does not exist"
}

_qm_capture_on_node() {
    local node="$1"; shift
    if [[ -n "$node" ]] && ! _is_local_node "$node"; then
        ssh "${SSH_OPTS[@]+"${SSH_OPTS[@]}"}" -o ConnectTimeout=10 \
            "${DEPLOY_USER:-root}@${node}" \
            "qm $(printf '%q ' "$@")"
        return $?
    fi
    command qm "$@"
}

# Surcharge de qm : route automatiquement les commandes vers le nœud hôte de la VM.
# Détecte le vmid dynamiquement : premier argument numérique en position 2 ou 3
# ($2 pour "qm config <vmid>", $3 pour "qm guest exec <vmid>").
qm() {
    local vmid=""
    [[ "${2:-}" =~ ^[0-9]+$ ]] && vmid="${2}"
    [[ -z "$vmid" && "${3:-}" =~ ^[0-9]+$ ]] && vmid="${3}"

    if [[ -n "$vmid" ]]; then
        local node refreshed out rc
        node=$(_vm_node_cached "$vmid")
        out="$(_qm_capture_on_node "$node" "$@" 2>&1)"
        rc=$?

        if [[ $rc -ne 0 ]] && printf '%s\n' "$out" | _qm_stale_owner_error "$vmid"; then
            refreshed="$(_refresh_vm_node_cache "$vmid")"
            if [[ -n "$refreshed" && "$refreshed" != "$node" ]]; then
                warn "VM $vmid relocalisée après échec qm : ${node:-local} -> $refreshed"
                out="$(_qm_capture_on_node "$refreshed" "$@" 2>&1)"
                rc=$?
            fi
        fi

        [[ -n "$out" ]] && printf '%s\n' "$out"
        return "$rc"
    fi
    command qm "$@"
}

qm_monitor_cmd() {
    local vmid="$1" cmd="$2"
    local node_ip
    node_ip="$(vm_node "$vmid" 2>/dev/null || true)"
    [[ -n "$node_ip" ]] || node_ip="$CONTROLLER_NODE"

    # qm monitor must run on the node currently owning the VM. The qm wrapper
    # routes subcommands, but not stdin pipes, so route this explicitly.
    if _is_local_node "$node_ip"; then
        printf '%s\nquit\n' "$cmd" | command qm monitor "$vmid" 2>/dev/null || true
    else
        ssh_run "$node_ip" "printf '%s\nquit\n' \"$cmd\" | qm monitor \"$vmid\" 2>/dev/null || true" 2>/dev/null || true
    fi
}

vm_runtime_vcpus() {
    local vmid="$1" out count
    out="$(qm_monitor_cmd "$vmid" "info cpus")"
    count="$(printf '%s\n' "$out" | awk '/(^|[[:space:]])\*?[[:space:]]*CPU #[0-9]+/{c++} END{print c+0}')"
    if [[ "$count" =~ ^[0-9]+$ && "$count" -gt 0 ]]; then
        printf '%s\n' "$count"
        return 0
    fi
    return 1
}

_cluster_vm_info() {
    local vmid="$1"
    pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys, json
vms = json.load(sys.stdin)
for v in vms:
    if str(v.get('vmid','')) == '$vmid':
        print(v.get('node',''), v.get('status',''))
        sys.exit(0)
" 2>/dev/null || true
}

_cluster_vm_status() {
    local vmid="$1"
    pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys, json
vms = json.load(sys.stdin)
for v in vms:
    if str(v.get('vmid','')) == '$vmid':
        print(v.get('status',''))
        sys.exit(0)
" 2>/dev/null || echo ""
}

_try_start_vm() {
    local vmid="$1"
    local info node status node_ip

    info="$(_cluster_vm_info "$vmid")"
    if [[ -z "$info" ]]; then
        warn "VM $vmid introuvable dans /cluster/resources — fallback vers une autre VM"
        unset "_VM_NODE_CACHE[$vmid]"
        return 2
    fi
    node="${info%% *}"
    status="${info##* }"
    node_ip="$(_pve_node_to_ip "$node")"

    # Alimenter le cache pour que qm() n'ait pas besoin de refaire pvesh
    _VM_NODE_CACHE[$vmid]="$node_ip"

    if [[ "$status" == "running" ]]; then
        info "VM $vmid running sur $node"
        return 0
    fi

    info "VM $vmid est '$status' sur $node — démarrage..."
    sanitize_vm_passthrough_for_node "$vmid" "$node_ip" || true
    if ! start_vm_with_hostpci_repair "$vmid"; then
        warn "échec démarrage VM $vmid sur $node"
        unset "_VM_NODE_CACHE[$vmid]"
        return 1
    fi

    local i=0
    while true; do
        status="$(_cluster_vm_status "$vmid")"
        [[ "$status" == "running" ]] && break
        ((i++)) || true
        if [[ $i -ge 60 ]]; then
            warn "VM $vmid non démarrée après 60s (statut='$status')"
            return 1
        fi
        sleep 1
    done
    info "VM $vmid démarrée sur $node"
    return 0
}

delete_vm_hostpci() {
    local vmid="$1" cfg keys key
    cfg="$(qm config "$vmid" 2>/dev/null || true)"
    [[ -n "$cfg" ]] || return 0
    keys="$(printf '%s\n' "$cfg" | awk -F': ' '/^hostpci[0-9]+:/{print $1}')"
    [[ -n "$keys" ]] || return 0
    while IFS= read -r key; do
        [[ -n "$key" ]] || continue
        warn "VM $vmid: suppression $key pour éviter un passthrough GPU invalide sur ce nœud"
        qm set "$vmid" --delete "$key" >/dev/null 2>&1 || true
    done <<< "$keys"
}

start_vm_with_hostpci_repair() {
    local vmid="$1" out
    if out="$(qm start "$vmid" 2>&1)"; then
        wait_vm_status "$vmid" running 90 || return 1
        return 0
    fi

    if printf '%s\n' "$out" | grep -qi "no PCI device found"; then
        warn "VM $vmid: démarrage bloqué par hostpci invalide — nettoyage puis nouvel essai"
        delete_vm_hostpci "$vmid"
        if out="$(qm start "$vmid" 2>&1)"; then
            wait_vm_status "$vmid" running 90 || return 1
            return 0
        fi
    fi

    [[ -n "$out" ]] && warn "qm start $vmid a échoué: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-220)"
    return 1
}

wait_vm_status() {
    local vmid="$1" expected="$2" timeout="${3:-60}"
    local i status
    for ((i=0; i<timeout; i++)); do
        status="$(qm status "$vmid" 2>/dev/null | awk '{print $2}' || true)"
        [[ "$status" == "$expected" ]] && return 0
        sleep 1
    done
    warn "VM $vmid: statut attendu '$expected' non atteint après ${timeout}s (dernier='${status:-inconnu}')"
    return 1
}

stop_vm_for_reconfig() {
    local vmid="$1" status
    status="$(qm status "$vmid" 2>/dev/null | awk '{print $2}' || true)"
    [[ "$status" == "stopped" ]] && return 0
    qm stop "$vmid" >/dev/null
    wait_vm_status "$vmid" stopped 90
}

sanitize_vm_passthrough_for_node() {
    local vmid="$1" node_ip="${2:-}"
    local node="${node_ip:-$(_vm_node_cached "$vmid")}"
    local remote_cmd

    [[ "${OMEGA_TEST_REMOVE_INVALID_HOSTPCI:-1}" == "1" ]] || return 0
    [[ -n "$node" ]] || return 0

    remote_cmd=$(cat <<'EOF'
vmid="$1"
cfg=$(qm config "$vmid" 2>/dev/null || true)
[ -n "$cfg" ] || exit 0
printf '%s\n' "$cfg" | awk -F': ' '/^hostpci[0-9]+:/{print $1 "|" $2}' | while IFS='|' read -r key val; do
    pci=${val%%,*}
    case "$pci" in
        *:*) ;;
        *) pci="0000:$pci" ;;
    esac
    if [ ! -e "/sys/bus/pci/devices/$pci" ]; then
        echo "remove $key $pci"
        qm set "$vmid" --delete "$key" >/dev/null
    fi
done
EOF
)

    if _is_local_node "$node"; then
        bash -c "$remote_cmd" _ "$vmid" 2>/dev/null | while read -r line; do warn "VM $vmid: hostpci invalide nettoyé ($line)"; done || true
    else
        ssh "${SSH_OPTS[@]+"${SSH_OPTS[@]}"}" -o ConnectTimeout=10 "${DEPLOY_USER:-root}@${node}" \
            "bash -c $(printf '%q' "$remote_cmd") _ $(printf '%q' "$vmid")" 2>/dev/null | \
            while read -r line; do warn "VM $vmid: hostpci invalide nettoyé ($line)"; done || true
    fi
}

vm_fallback_candidates() {
    local requested="${1:-$TEST_VMID}"
    local seen=",$requested,"
    printf '%s\n' "$requested"
    for vmid in "${TEST_VMIDS_ARR[@]}"; do
        [[ -z "$vmid" ]] && continue
        [[ "$seen" == *",$vmid,"* ]] && continue
        printf '%s\n' "$vmid"
        seen="${seen}${vmid},"
    done
}

# S'assure qu'une VM est running. Si la VM demandée échoue, bascule sur une autre
# VM de OMEGA_TEST_VMIDS. La fonction affiche toujours le VMID réellement retenu.
require_vm_running() {
    local requested="${1:-$TEST_VMID}"
    local vmid

    while IFS= read -r vmid; do
        [[ -n "$vmid" ]] || continue
        if _try_start_vm "$vmid"; then
            SELECTED_VMID="$vmid"
            if [[ "$vmid" != "$requested" ]]; then
                warn "VM $requested indisponible — bascule automatique vers VM $vmid"
            fi
            info "VM retenue pour le test : $vmid"
            return 0
        fi
    done < <(vm_fallback_candidates "$requested")

    fail "aucune VM de test utilisable — vérifier OMEGA_TEST_VMIDS (${TEST_VMIDS_ARR[*]:-vide})"
}

# Retourne un couple de VMs running distinctes, avec fallback automatique.
require_two_vms_running() {
    local requested1="${1:-${TEST_VMIDS_ARR[0]:-$TEST_VMID}}"
    local requested2="${2:-${TEST_VMIDS_ARR[1]:-}}"
    local first second vmid

    require_vm_running "$requested1"
    first="$SELECTED_VMID"
    while IFS= read -r vmid; do
        [[ -n "$vmid" && "$vmid" != "$first" ]] || continue
        if _try_start_vm "$vmid"; then
            second="$vmid"
            break
        fi
    done < <(vm_fallback_candidates "${requested2:-$requested1}")

    [[ -n "${second:-}" ]] || fail "impossible de trouver 2 VMs running distinctes dans OMEGA_TEST_VMIDS"
    SELECTED_VMIDS=("$first" "$second")
    info "VMs retenues pour le test : $first $second"
}

# ── Réseau ────────────────────────────────────────────────────────────────────
wait_port() {
    local host="$1" port="$2" timeout="${3:-15}"
    local i=0
    while ! nc -z "$host" "$port" 2>/dev/null; do
        ((i++)) || true
        [[ $i -ge $timeout ]] && fail "port $host:$port non disponible après ${timeout}s"
        sleep 1
    done
}

wait_http() {
    local url="$1" timeout="${2:-15}"
    local i=0
    while ! curl -sf "$url" &>/dev/null; do
        ((i++)) || true
        [[ $i -ge $timeout ]] && fail "URL $url non disponible après ${timeout}s"
        sleep 1
    done
}

# ── Store local (pour tests sans cluster) ─────────────────────────────────────
# Usage : start_store <id> <port> <status_port>
start_store() {
    local id="$1" port="$2" status_port="$3"
    local log="/tmp/omega-store-${id}.log"
    local datadir="/tmp/omega-store-data-${id}"
    _TMPFILES+=("$log")
    mkdir -p "$datadir"
    "$STORE_BIN" \
        --listen "127.0.0.1:$port" \
        --status-listen "127.0.0.1:$status_port" \
        --node-id "test-store-$id" \
        --store-data-path "$datadir" \
        >"$log" 2>&1 &
    _PIDS+=($!)
    wait_port 127.0.0.1 "$port" 15
    info "store $id démarré sur :$port (log: $log)"
}

# ── Store distant via SSH ─────────────────────────────────────────────────────
# Usage : start_store_remote <node_ip> [store_port] [status_port]
# Retourne le PID distant dans REMOTE_STORE_PID (non fiable pour cleanup, utiliser stop_store_remote)
start_store_remote() {
    local node="$1"
    local port="${2:-$STORE_PORT}"
    local status_port="${3:-$STATUS_PORT}"
    local remote_bin="${OMEGA_REMOTE_BIN_DIR:-/usr/local/bin}/node-bc-store"
    local log="/tmp/omega-remote-store-${node}.log"
    _TMPFILES+=("$log")

    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${DEPLOY_USER}@${node}" \
        "nohup $remote_bin \
            --listen 0.0.0.0:$port \
            --status-listen 0.0.0.0:$status_port \
            --node-id ${node} \
            --store-data-path /tmp/omega-store-remote \
        >/tmp/omega-store.log 2>&1 & echo \$!" >"$log" 2>&1
    REMOTE_STORE_PID=$(cat "$log" | tail -1)
    info "store distant démarré sur $node:$port (pid=$REMOTE_STORE_PID)"
    # Attendre que le port soit joignable
    wait_port "$node" "$port" 20
}

stop_store_remote() {
    local node="$1" pid="${2:-}"
    if [[ -n "$pid" ]]; then
        ssh "${SSH_OPTS[@]}" -o ConnectTimeout=3 "${DEPLOY_USER}@${node}" \
            "kill $pid 2>/dev/null || true" || true
    else
        ssh "${SSH_OPTS[@]}" -o ConnectTimeout=3 "${DEPLOY_USER}@${node}" \
            "pkill -f node-bc-store 2>/dev/null || true" || true
    fi
}

# ── SSH helpers ───────────────────────────────────────────────────────────────
ssh_run() {
    local node="$1"; shift
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes "${DEPLOY_USER}@${node}" "$@"
}

ssh_run_bg() {
    local node="$1"; shift
    ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 -o BatchMode=yes "${DEPLOY_USER}@${node}" "$@" &
    _PIDS+=($!)
}

# ── Assertions ────────────────────────────────────────────────────────────────
assert_json_field() {
    local url="$1" field="$2" expected="$3"
    local actual
    actual=$(curl -sf "$url" | \
        python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('$field','MISSING'))")
    [[ "$actual" == "$expected" ]] || \
        fail "champ '$field' = '$actual', attendu '$expected' (url: $url)"
}

assert_metric_gt() {
    local url="$1" metric="$2" min="$3"
    local val
    val=$(curl -sf "$url" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('${metric}',''))" 2>/dev/null || echo "")
    [[ -n "$val" ]] || fail "métrique '$metric' absente de $url"
    python3 -c "import sys; sys.exit(0 if float('$val') > float('$min') else 1)" || \
        fail "métrique '$metric' = $val, attendu > $min"
}

# ── Cluster state ─────────────────────────────────────────────────────────────

# pvesh retourne le nom d'hôte Proxmox (ex: "pve"), pas l'IP.
# Cette fonction traduit un nom d'hôte Proxmox en IP depuis OMEGA_NODES_ARR.
# Résultat mis en cache dans _PVE_IP_MAP (hostname→IP).
declare -A _PVE_IP_MAP=()   # hostname → IP
declare -A _PVE_NAME_MAP=() # IP → hostname
_build_node_maps() {
    [[ ${#_PVE_IP_MAP[@]} -gt 0 ]] && return  # déjà construit
    for n in "${OMEGA_NODES_ARR[@]}"; do
        [[ -z "$n" ]] && continue
        local hn
        hn=$(ssh "${SSH_OPTS[@]+"${SSH_OPTS[@]}"}" \
            -o ConnectTimeout=2 -o BatchMode=yes \
            "${DEPLOY_USER:-root}@${n}" "hostname" 2>/dev/null || echo "")
        [[ -n "$hn" ]] && _PVE_IP_MAP["$hn"]="$n" && _PVE_NAME_MAP["$n"]="$hn"
    done
}

# pvesh node name → IP
_pve_node_to_ip() {
    local pve_node="$1"
    _build_node_maps
    echo "${_PVE_IP_MAP[$pve_node]:-$pve_node}"
}

# IP → pvesh node name (for qm migrate etc.)
_ip_to_pve_node() {
    local ip="$1"
    _build_node_maps
    echo "${_PVE_NAME_MAP[$ip]:-$ip}"
}

# Nom Proxmox du nœud local (celui que pvesh/qm connaît, pas forcément hostname)
local_pve_node() {
    # pvesh retourne le nœud courant via /nodes — on cherche celui dont l'IP correspond à la nôtre
    _build_node_maps
    local my_ips
    my_ips=$(hostname -I 2>/dev/null | tr ' ' '\n')
    local n
    for n in "${!_PVE_NAME_MAP[@]}"; do
        if echo "$my_ips" | grep -qxF "$n"; then
            echo "${_PVE_NAME_MAP[$n]}"
            return
        fi
    done
    # Fallback : nom d'hôte court
    hostname -s 2>/dev/null || hostname
}

# Trouve un vmid qui n'existe pas dans le cluster (pour tests orphan cleaner etc.)
unused_vmid() {
    local max
    max=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys,json
vms=json.load(sys.stdin)
ids=[v.get('vmid',0) for v in vms]
print(max(ids)+1 if ids else 90001)
" 2>/dev/null || echo "90001")
    echo "$max"
}

# RAM configurée de la VM dans Proxmox (champ memory: en MiB)
vm_ram_mib() {
    local vmid="$1"
    local cfg
    cfg=$(qm config "$vmid" 2>/dev/null || true)
    printf '%s\n' "$cfg" | awk '/^memory:/{print $2}' | head -1
}

# Nombre total de cœurs CPU de la VM (cores × sockets)
vm_cores() {
    local vmid="$1"
    local cfg cores sockets smp smp_max
    cfg=$(qm config "$vmid" 2>/dev/null || true)
    cores=$(printf '%s\n' "$cfg" | awk '/^cores:/{print $2}' | head -1)
    sockets=$(printf '%s\n' "$cfg" | awk '/^sockets:/{print $2}' | head -1)
    smp="$(qm showcmd "$vmid" --pretty 2>/dev/null | grep -- '-smp' | head -1 || true)"
    smp_max="$(printf '%s\n' "$smp" | sed -n "s/.*maxcpus=\([0-9][0-9]*\).*/\1/p" | head -1)"
    if [[ "$smp_max" =~ ^[0-9]+$ && "$smp_max" -gt 1 ]]; then
        echo "$smp_max"
        return
    fi
    echo $(( ${cores:-1} * ${sockets:-1} ))
}

vm_desc_value() {
    local description="$1" key="$2"
    tr ' ' '\n' <<< "$description" | awk -F= -v k="$key" '$1 == k {print $2; exit}'
}

ensure_omega_vcpu_profile() {
    local vmid="$1"
    local cfg cores sockets vcpus hotplug memory balloon description max_vcpus desired_max desired_disk desired_gpu desc status smp smp_max

    cfg=$(qm config "$vmid" 2>/dev/null || true)
    if [[ -z "$cfg" ]]; then
        _refresh_vm_node_cache "$vmid" >/dev/null
        cfg=$(qm config "$vmid" 2>/dev/null || true)
    fi
    [[ -n "$cfg" ]] || fail "impossible de lire la config Proxmox de la VM $vmid"

    cores=$(printf '%s\n' "$cfg" | awk '/^cores:/{print $2}' | head -1)
    sockets=$(printf '%s\n' "$cfg" | awk '/^sockets:/{print $2}' | head -1)
    vcpus=$(printf '%s\n' "$cfg" | awk '/^vcpus:/{print $2}' | head -1)
    hotplug=$(printf '%s\n' "$cfg" | awk '/^hotplug:/{print $2}' | head -1)
    memory=$(printf '%s\n' "$cfg" | awk '/^memory:/{print $2}' | head -1)
    balloon=$(printf '%s\n' "$cfg" | awk '/^balloon:/{print $2}' | head -1)
    description=$(printf '%s\n' "$cfg" | awk -F': ' '$1 == "description" {print $2; exit}')

    cores="${cores:-1}"
    sockets="${sockets:-1}"
    vcpus="${vcpus:-1}"
    memory="${memory:-1024}"
    balloon="${balloon:-512}"
    max_vcpus=$(( cores * sockets ))
    smp="$(qm showcmd "$vmid" --pretty 2>/dev/null | grep -- '-smp' | head -1 || true)"
    smp_max="$(printf '%s\n' "$smp" | sed -n "s/.*maxcpus=\([0-9][0-9]*\).*/\1/p" | head -1)"
    if [[ "$smp_max" =~ ^[0-9]+$ && "$smp_max" -gt "$max_vcpus" ]]; then
        max_vcpus="$smp_max"
        cores="$smp_max"
        sockets=1
    fi
    if [[ "$max_vcpus" -gt 1 && "$vcpus" -lt "$max_vcpus" && ",${hotplug:-}," == *,cpu,* && "$smp" == *"maxcpus=${max_vcpus}"* ]]; then
        return 0
    fi

    desired_max="$(vm_desc_value "$description" omega_max_vcpus)"
    [[ "$desired_max" =~ ^[0-9]+$ && "$desired_max" -gt 1 ]] || desired_max="${OMEGA_VCPU_TEST_MAX_VCPUS:-4}"
    [[ "$desired_max" =~ ^[0-9]+$ && "$desired_max" -gt 1 ]] || desired_max=4
    desired_disk="$(vm_desc_value "$description" omega_disk_max_gib)"
    desired_gpu="$(vm_desc_value "$description" omega_gpu_vram_mib)"
    desired_disk="${desired_disk:-20}"
    desired_gpu="${desired_gpu:-0}"
    desc="omega_min_vcpus=1 omega_max_vcpus=${desired_max} omega_memory_min_mib=${balloon} omega_memory_max_mib=${memory} omega_disk_max_gib=${desired_disk} omega_gpu_vram_mib=${desired_gpu}"

    warn "VM $vmid non conforme CPU (vcpus=${vcpus}, max_config=${max_vcpus}, smp='${smp:-absent}') — réparation Omega: cores=${desired_max}, sockets=1, vcpus=1"
    status="$(qm status "$vmid" 2>/dev/null | awk '{print $2}' || true)"
    [[ "$status" == "running" ]] && stop_vm_for_reconfig "$vmid"
    qm set "$vmid" \
        --cores "$desired_max" \
        --sockets 1 \
        --vcpus 1 \
        --hotplug cpu,disk,network \
        --description "$desc" >/dev/null
    sanitize_vm_passthrough_for_node "$vmid" "$(_vm_node_cached "$vmid")" || true
    start_vm_with_hostpci_repair "$vmid" >/dev/null || fail "impossible de redémarrer la VM $vmid après réparation CPU"
    sleep 8
    unset "_VM_NODE_CACHE[$vmid]"

    smp="$(qm showcmd "$vmid" --pretty 2>/dev/null | grep -- '-smp' | head -1 || true)"
    [[ "$smp" == *"maxcpus=${desired_max}"* ]] || \
        fail "VM $vmid toujours non conforme après réparation: ${smp:-showcmd absent}"
}

ensure_omega_vcpu_profile_safe() {
    local vmid="$1" out rc
    out="$(ensure_omega_vcpu_profile "$vmid" 2>&1)"
    rc=$?
    [[ -n "$out" ]] && printf '%s\n' "$out"
    return "$rc"
}

vm_node() {
    local vmid="$1"
    local pve_node
    pve_node=$(pvesh get /cluster/resources --type vm --output-format json 2>/dev/null | \
        python3 -c "
import sys,json
vms = json.load(sys.stdin)
for v in vms:
    if v.get('vmid') == $vmid:
        print(v.get('node',''))
        break
" | head -1)
    _pve_node_to_ip "$pve_node"
}

cluster_free_mib() {
    local total=0
    for n in "${OMEGA_NODES_ARR[@]}"; do
        local free
        free=$(curl -sf "http://${n}:${STATUS_PORT}/status" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin).get('available_mib',0))" \
            2>/dev/null || echo 0)
        total=$((total + free))
    done
    echo "$total"
}

all_stores_status() {
    for n in "${STORE_NODES_ARR[@]}"; do
        local url="http://${n}:${STATUS_PORT}/status"
        echo -n "  store $n : "
        curl -sf "$url" | python3 -c "
import sys,json; d=json.load(sys.stdin)
print(f\"pages={d.get('page_count',0)}  ram_mib={d.get('available_mib','?')}  ceph={d.get('ceph_enabled',False)}\")
" 2>/dev/null || echo "inaccessible"
    done
}

# ── Commandes dans l'invité via qemu-guest-agent ─────────────────────────────
guest_agent_ready() {
    local vmid="$1"
    qm guest cmd "$vmid" ping >/dev/null 2>&1
}

guest_exec_wait() {
    local vmid="$1"; shift
    local started pid status exitcode
    started=$(qm guest exec "$vmid" -- "$@" 2>/dev/null || true)
    pid=$(printf '%s' "$started" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("pid",""))' 2>/dev/null || true)
    [[ -n "$pid" ]] || return 1
    for _ in $(seq 1 300); do
        status=$(qm guest exec-status "$vmid" "$pid" 2>/dev/null || true)
        if printf '%s' "$status" | python3 -c 'import sys,json; sys.exit(0 if json.load(sys.stdin).get("exited") else 1)' 2>/dev/null; then
            exitcode=$(printf '%s' "$status" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("exitcode", 1))' 2>/dev/null || echo 1)
            printf '%s' "$status" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("out-data","") + d.get("err-data",""), end="")' 2>/dev/null || true
            [[ "$exitcode" == "0" ]]
            return
        fi
        sleep 1
    done
    return 1
}

ensure_guest_packages() {
    local vmid="$1"; shift
    local packages=("$@")
    [[ "${#packages[@]}" -gt 0 ]] || return 0

    if ! guest_agent_ready "$vmid"; then
        warn "qemu-guest-agent indisponible dans VM $vmid — installation auto ignorée"
        return 1
    fi

    local pkg_list="${packages[*]}"
    local missing
    missing=$(guest_exec_wait "$vmid" bash -lc "for p in ${pkg_list}; do dpkg -s \"\$p\" >/dev/null 2>&1 || printf '%s ' \"\$p\"; done" 2>/dev/null || true)
    missing="${missing%"${missing##*[![:space:]]}"}"
    if [[ -z "$missing" ]]; then
        return 0
    fi

    info "installation automatique dans VM $vmid : ${missing}"
    guest_exec_wait "$vmid" bash -lc "export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y ${missing}"
}

host_cpu_stress() {
    local dur="${1:-60}"
    if command -v stress-ng >/dev/null 2>&1; then
        stress-ng --cpu 0 --timeout "${dur}s" &>/dev/null &
        _PIDS+=($!)
        info "stress-ng CPU hôte lancé (${dur}s, pid=$!)"
        return 0
    fi
    if command -v python3 >/dev/null 2>&1; then
        warn "stress-ng hôte absent — fallback Python CPU (${dur}s)"
        python3 -c '
import multiprocessing as mp, os, sys, time
duration = int(float(sys.argv[1]))
deadline = time.time() + duration
def burn():
    x = 0
    while time.time() < deadline:
        x = (x * 1664525 + 1013904223) & 0xffffffff
workers = [mp.Process(target=burn) for _ in range(max(1, os.cpu_count() or 1))]
[p.start() for p in workers]
[p.join() for p in workers]
' "$dur" &>/dev/null &
        _PIDS+=($!)
        info "fallback Python CPU hôte lancé (${dur}s, pid=$!)"
        return 0
    fi
    warn "aucun générateur CPU hôte disponible (stress-ng/python3 absents)"
    return 1
}

host_mem_stress() {
    local dur="${1:-60}" bytes="${2:-70%}"
    if command -v stress-ng >/dev/null 2>&1; then
        stress-ng --vm 1 --vm-bytes "$bytes" --timeout "${dur}s" &>/dev/null &
        _PIDS+=($!)
        info "stress-ng mémoire hôte lancé (${bytes}, ${dur}s, pid=$!)"
        return 0
    fi
    if command -v python3 >/dev/null 2>&1; then
        warn "stress-ng hôte absent — fallback Python mémoire (${bytes}, ${dur}s)"
        python3 - "$dur" "$bytes" <<'PY' &>/dev/null &
import re
import sys
import time

duration = int(float(sys.argv[1]))
spec = sys.argv[2].strip().lower()

def mem_available_mib():
    values = {}
    with open("/proc/meminfo", "r", encoding="utf-8") as fh:
        for line in fh:
            parts = line.split()
            if len(parts) >= 2:
                values[parts[0].rstrip(":")] = int(parts[1]) // 1024
    return values.get("MemAvailable") or values.get("MemFree") or 512

def parse_target_mib(raw, available):
    if raw.endswith("%"):
        pct = max(1.0, min(95.0, float(raw[:-1] or 0)))
        return int(available * pct / 100.0)
    match = re.match(r"^([0-9]+)([mMgG]?)$", raw)
    if not match:
        return int(available * 0.70)
    value = int(match.group(1))
    unit = match.group(2).lower()
    return value * 1024 if unit == "g" else value

available = mem_available_mib()
target_mib = parse_target_mib(spec, available)
target_mib = max(64, min(target_mib, int(available * 0.90)))
chunk_mib = 16
chunks = []
deadline = time.time() + duration
allocated = 0

while time.time() < deadline and allocated < target_mib:
    try:
        block = bytearray(chunk_mib * 1024 * 1024)
        for i in range(0, len(block), 4096):
            block[i] = 1
        chunks.append(block)
        allocated += chunk_mib
    except MemoryError:
        time.sleep(1)
    time.sleep(0.05)

while time.time() < deadline:
    time.sleep(1)
PY
        _PIDS+=($!)
        info "fallback Python mémoire hôte lancé (${bytes}, ${dur}s, pid=$!)"
        return 0
    fi
    warn "aucun générateur mémoire hôte disponible (stress-ng/python3 absents)"
    return 1
}

# ── Charge CPU via guest exec ou fallback cgroup ─────────────────────────────
# Tente qm guest exec ; si l'agent invité est absent, injecte stress-ng
# directement dans le cgroup QEMU de la VM (charge visible par l'agent omega).
# Usage : vm_cpu_stress <vmid> <duration_secs>
vm_cpu_stress() {
    local vmid="$1" dur="${2:-60}"
    ensure_guest_packages "$vmid" stress-ng >/dev/null 2>&1 || true
    if qm guest exec "$vmid" -- stress-ng --cpu 0 --timeout "${dur}s" &>/dev/null 2>&1; then
        info "stress-ng lancé dans la VM $vmid via qemu-guest-agent"
        return
    fi
    warn "qemu-guest-agent absent — stress-ng injecté dans le cgroup QEMU"
    # Proxmox 8 : /sys/fs/cgroup/qemu.slice/<vmid>.scope/
    # Proxmox 7 : /sys/fs/cgroup/machine.slice/qemu-<vmid>.scope/
    local cg=""
    [[ -d "/sys/fs/cgroup/qemu.slice/${vmid}.scope" ]] && \
        cg="/sys/fs/cgroup/qemu.slice/${vmid}.scope"
    [[ -z "$cg" && -d "/sys/fs/cgroup/machine.slice/qemu-${vmid}.scope" ]] && \
        cg="/sys/fs/cgroup/machine.slice/qemu-${vmid}.scope"
    if [[ -n "$cg" ]]; then
        (echo $BASHPID > "${cg}/cgroup.procs" 2>/dev/null; \
         stress-ng --cpu 0 --timeout "${dur}s") &>/dev/null &
        _PIDS+=($!)
        info "stress-ng (cgroup=$cg) lancé (pid=$!)"
    else
        warn "cgroup QEMU introuvable pour vmid=$vmid — pas de charge CPU simulée"
    fi
}

vm_mem_stress() {
    local vmid="$1" dur="${2:-60}" bytes="${3:-70%}"
    ensure_guest_packages "$vmid" stress-ng >/dev/null 2>&1 || true
    if qm guest exec "$vmid" -- stress-ng --vm 1 --vm-bytes "$bytes" --timeout "${dur}s" &>/dev/null 2>&1; then
        info "stress-ng mémoire lancé dans la VM $vmid via qemu-guest-agent (${bytes}, ${dur}s)"
        return 0
    fi
    warn "charge mémoire invitée impossible — qemu-guest-agent ou stress-ng indisponible"
    return 1
}

# ── Utilitaires ───────────────────────────────────────────────────────────────
elapsed() { echo $(( SECONDS - ${1:-0} )); }

node_count() { echo "${#OMEGA_NODES_ARR[@]}"; }
store_count() { echo "${#STORE_NODES_ARR[@]}"; }

print_cluster_config() {
    info "Nœuds       : ${OMEGA_NODES_ARR[*]}"
    info "Contrôleur  : $CONTROLLER_NODE"
    info "Stores      : ${STORE_NODES_ARR[*]} (${#STORE_NODES_ARR[@]} nœuds)"
    info "STORES_CSV  : $STORES_CSV"
    info "STATUS_CSV  : $STATUS_CSV"
}
