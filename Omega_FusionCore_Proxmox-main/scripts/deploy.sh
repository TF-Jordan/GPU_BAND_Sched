#!/usr/bin/env bash
# deploy.sh — Déploie les binaires compilés sur tous les nœuds du cluster via SSH.
#
# Variables d'environnement :
#   OMEGA_NODES      : nœuds séparés par virgule (défaut : lire cluster.conf)
#   OMEGA_CONTROLLER : nœud qui active le contrôleur (défaut : lire cluster.conf)
#   DEPLOY_USER      : utilisateur SSH (défaut : root)
#   DEPLOY_DIR       : répertoire de déploiement sur les nœuds (défaut : /opt/omega-remote-paging)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Sourcer cluster.conf si les variables ne sont pas déjà définies
if [[ -z "${OMEGA_NODES:-}" ]] || [[ -z "${OMEGA_CONTROLLER:-}" ]]; then
    source "${SCRIPT_DIR}/cluster.conf"
fi

: "${OMEGA_NODES:?Variable OMEGA_NODES requise}"
: "${OMEGA_CONTROLLER:?Variable OMEGA_CONTROLLER requise}"
: "${DEPLOY_USER:=root}"
: "${DEPLOY_DIR:=/opt/omega-remote-paging}"
: "${STORE_PORT:=9100}"
: "${STATUS_PORT:=9200}"
: "${OMEGA_INSTALL_VMIDS:=${OMEGA_TEST_VMIDS:-}}"
: "${SSH_KEY:=}"
: "${OMEGA_GPU_PROXY_API_TOKEN_FILE:=/etc/omega/gpu-proxy.token}"
: "${OMEGA_GPU_PROXY_API_TOKEN:=}"
: "${OMEGA_GPU_NODES:=}"
: "${OMEGA_GPU_PRIMARY_NODE:=}"
: "${OMEGA_GPU_PROXY_URL:=}"
: "${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:=0}"
: "${OMEGA_GPU_MIGRATE_TO_GPU_NODE:=1}"
: "${OMEGA_GPU_FALLBACK_NETWORK:=1}"

SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
SCP_OPTS=(-o StrictHostKeyChecking=accept-new)
if [[ -n "$SSH_KEY" && -f "$SSH_KEY" ]]; then
    SSH_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=accept-new)
    SCP_OPTS=(-i "$SSH_KEY" -o StrictHostKeyChecking=accept-new)
fi

DAEMON_BIN="${ROOT_DIR}/target/release/omega-daemon"
AGENT_BIN="${ROOT_DIR}/target/release/node-a-agent"
LAUNCHER_BIN="${ROOT_DIR}/target/release/omega-qemu-launcher"
GPU_PROXY_BIN="${ROOT_DIR}/target/release/omega-gpu-proxy"
BRIDGE_LIB="${ROOT_DIR}/omega-uffd-bridge/omega-uffd-bridge.so"
GPU_WORKER_CPU="${SCRIPT_DIR}/omega-gpu-worker-cpu.py"
GPU_WORKER_APP="${SCRIPT_DIR}/omega-gpu-worker-app.py"

info()    { echo -e "\033[32m[INFO]\033[0m  $*"; }
success() { echo -e "\033[32m[OK]\033[0m    $*"; }
fail()    { echo -e "\033[31m[FAIL]\033[0m  $*" >&2; exit 1; }

[[ -x "$DAEMON_BIN"   ]] || fail "omega-daemon non compilé — lancez 'make build' d'abord"
[[ -x "$AGENT_BIN"    ]] || fail "node-a-agent non compilé — lancez 'make build' d'abord"
[[ -x "$LAUNCHER_BIN" ]] || fail "omega-qemu-launcher non compilé — lancez 'make build' d'abord"
[[ -x "$GPU_PROXY_BIN" ]] || info "omega-gpu-proxy non compilé — proxy GPU applicatif ignoré"
[[ -f "$BRIDGE_LIB" ]] || fail "omega-uffd-bridge.so absent — lancez 'make build-bridge' ou l'option [b]/[I]"

IFS=',' read -ra NODES_ARR <<< "$OMEGA_NODES"

node_in_csv() {
    local needle="$1" csv="$2" item
    IFS=',' read -ra _csv_items <<< "$csv"
    for item in "${_csv_items[@]}"; do
        [[ "$item" == "$needle" ]] && return 0
    done
    return 1
}

if [[ -z "$OMEGA_GPU_NODES" ]]; then
    for candidate in "${NODES_ARR[@]}"; do
        if ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${candidate}" "command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1" >/dev/null 2>&1; then
            OMEGA_GPU_NODES="${OMEGA_GPU_NODES:+$OMEGA_GPU_NODES,}${candidate}"
        fi
    done
fi

if [[ -z "$OMEGA_GPU_PRIMARY_NODE" ]]; then
    OMEGA_GPU_PRIMARY_NODE="${OMEGA_GPU_NODES%%,*}"
    [[ -n "$OMEGA_GPU_PRIMARY_NODE" ]] || OMEGA_GPU_PRIMARY_NODE="$OMEGA_CONTROLLER"
fi

if [[ -z "$OMEGA_GPU_PROXY_API_TOKEN" ]]; then
    OMEGA_GPU_PROXY_API_TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
fi
if [[ -z "$OMEGA_GPU_PROXY_URL" ]]; then
    OMEGA_GPU_PROXY_URL="http://${OMEGA_GPU_PRIMARY_NODE}:9400"
fi

# Construire OMEGA_STORES = tous les nœuds avec leur port store
OMEGA_STORES=""
for n in "${NODES_ARR[@]}"; do
    OMEGA_STORES="${OMEGA_STORES:+$OMEGA_STORES,}${n}:${STORE_PORT}"
done

info "=== Déploiement omega-remote-paging ==="
info "Nœuds      : ${NODES_ARR[*]}"
info "Contrôleur : ${OMEGA_CONTROLLER}"
info "GPU nodes  : ${OMEGA_GPU_NODES:-aucun détecté}"
info "GPU primary: ${OMEGA_GPU_PRIMARY_NODE}"
info "Répertoire : ${DEPLOY_DIR}"
if [[ -n "$OMEGA_INSTALL_VMIDS" ]]; then
    info "VMIDs hookscript ciblés : ${OMEGA_INSTALL_VMIDS}"
else
    info "VMIDs hookscript ciblés : auto local par nœud (qm list)"
fi
echo

# ─── Déploiement sur chaque nœud ─────────────────────────────────────────────

for node in "${NODES_ARR[@]}"; do
    info "── Nœud ${node} ──"

    ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "mkdir -p ${DEPLOY_DIR}/bin ${DEPLOY_DIR}/logs"

    # Arrêter le daemon avant de remplacer les binaires (on ne peut pas écraser un exécutable en cours)
    ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "systemctl stop omega-daemon 2>/dev/null || true"

    scp "${SCP_OPTS[@]}" "$DAEMON_BIN"    "${DEPLOY_USER}@${node}:${DEPLOY_DIR}/bin/omega-daemon"
    scp "${SCP_OPTS[@]}" "$AGENT_BIN"     "${DEPLOY_USER}@${node}:${DEPLOY_DIR}/bin/node-a-agent"
    scp "${SCP_OPTS[@]}" "$LAUNCHER_BIN"  "${DEPLOY_USER}@${node}:${DEPLOY_DIR}/bin/omega-qemu-launcher"
    [[ -x "$GPU_PROXY_BIN" ]] && scp "${SCP_OPTS[@]}" "$GPU_PROXY_BIN" "${DEPLOY_USER}@${node}:${DEPLOY_DIR}/bin/omega-gpu-proxy"
    scp "${SCP_OPTS[@]}" "$BRIDGE_LIB" "${DEPLOY_USER}@${node}:${DEPLOY_DIR}/bin/omega-uffd-bridge.so"

    scp "${SCP_OPTS[@]}" "${SCRIPT_DIR}/proxmox_hook.pl"          "${DEPLOY_USER}@${node}:/tmp/proxmox_hook.pl"
    scp "${SCP_OPTS[@]}" "${SCRIPT_DIR}/omega-proxmox-install.sh" "${DEPLOY_USER}@${node}:/tmp/omega-proxmox-install.sh"
    scp "${SCP_OPTS[@]}" "$GPU_WORKER_CPU" "${DEPLOY_USER}@${node}:/tmp/omega-gpu-worker-cpu.py" 2>/dev/null || true
    scp "${SCP_OPTS[@]}" "$GPU_WORKER_APP" "${DEPLOY_USER}@${node}:/tmp/omega-gpu-worker-app.py" 2>/dev/null || true

    # Stores distants = tous les autres nœuds
    node_stores=""
    for s in "${NODES_ARR[@]}"; do
        [[ "$s" == "$node" ]] && continue
        node_stores="${node_stores:+$node_stores,}${s}:${STORE_PORT}"
    done

    node_api_port="${STATUS_PORT:-9200}"
    node_peers=""
    for peer in "${NODES_ARR[@]}"; do
        [[ "$peer" == "$node" ]] && continue
        node_peers="${node_peers:+$node_peers,}${peer}:${node_api_port}"
    done
    node_id="$(ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "hostname -s" | tr -d '\r' | head -1)"
    [[ -n "$node_id" ]] || node_id="$node"

    info "Activation userfaultfd sur ${node}..."
    ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "
        sysctl -w vm.unprivileged_userfaultfd=1
        grep -q 'unprivileged_userfaultfd' /etc/sysctl.conf \
            || echo 'vm.unprivileged_userfaultfd=1' >> /etc/sysctl.conf
    "

    info "Création /etc/omega/cluster.env sur ${node}..."
    ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "
        mkdir -p /etc/omega
        cat > /etc/omega/cluster.env <<'ENVEOF'
OMEGA_NODES=${OMEGA_NODES}
OMEGA_CONTROLLER=${OMEGA_CONTROLLER}
OMEGA_NODE_ID=${node_id}
OMEGA_NODE_ADDR=${node}
OMEGA_STORE_PORT=${STORE_PORT}
OMEGA_API_PORT=${node_api_port}
OMEGA_PEERS=${node_peers}
OMEGA_GPU_NODES=${OMEGA_GPU_NODES}
OMEGA_GPU_PRIMARY_NODE=${OMEGA_GPU_PRIMARY_NODE}
OMEGA_GPU_PROXY_URL=${OMEGA_GPU_PROXY_URL}
OMEGA_GPU_PROXY_TOTAL_VRAM_MIB=${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB}
OMEGA_GPU_PROXY_API_TOKEN_FILE=${OMEGA_GPU_PROXY_API_TOKEN_FILE}
OMEGA_GPU_MIGRATE_TO_GPU_NODE=${OMEGA_GPU_MIGRATE_TO_GPU_NODE}
OMEGA_GPU_FALLBACK_NETWORK=${OMEGA_GPU_FALLBACK_NETWORK}
ENVEOF
    "

    info "Installation wrapper QEMU sur ${node} (stores: ${node_stores})..."
    node_gpu_proxy_enabled=0
    if [[ "${OMEGA_GPU_PROXY_ENABLED:-0}" == "1" ]] && node_in_csv "$node" "$OMEGA_GPU_NODES"; then
        node_gpu_proxy_enabled=1
    fi
    ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "
        INSTALL_DIR='${DEPLOY_DIR}/bin' \
        LAUNCHER_SRC='${DEPLOY_DIR}/bin/omega-qemu-launcher' \
        AGENT_SRC='${DEPLOY_DIR}/bin/node-a-agent' \
        DAEMON_SRC='${DEPLOY_DIR}/bin/omega-daemon' \
        GPU_PROXY_SRC='${DEPLOY_DIR}/bin/omega-gpu-proxy' \
        BRIDGE_SRC='${DEPLOY_DIR}/bin/omega-uffd-bridge.so' \
        GPU_WORKER_CPU_SRC='/tmp/omega-gpu-worker-cpu.py' \
        GPU_WORKER_APP_SRC='/tmp/omega-gpu-worker-app.py' \
        OMEGA_STORES='${node_stores}' \
        OMEGA_VMIDS='${OMEGA_INSTALL_VMIDS}' \
        OMEGA_GPU_PROXY_ENABLED='${node_gpu_proxy_enabled}' \
        OMEGA_GPU_PROXY_LISTEN='${OMEGA_GPU_PROXY_LISTEN:-0.0.0.0:9400}' \
        OMEGA_GPU_PROXY_MAX_CONCURRENT='${OMEGA_GPU_PROXY_MAX_CONCURRENT:-1}' \
        OMEGA_GPU_PROXY_TOTAL_VRAM_MIB='${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB}' \
        OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS='${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS:-900}' \
        OMEGA_GPU_PROXY_API_TOKEN_FILE='${OMEGA_GPU_PROXY_API_TOKEN_FILE}' \
        OMEGA_GPU_PROXY_API_TOKEN='${OMEGA_GPU_PROXY_API_TOKEN}' \
        OMEGA_GPU_NODES='${OMEGA_GPU_NODES}' \
        OMEGA_GPU_PRIMARY_NODE='${OMEGA_GPU_PRIMARY_NODE}' \
        OMEGA_GPU_PROXY_URL='${OMEGA_GPU_PROXY_URL}' \
        OMEGA_GPU_MIGRATE_TO_GPU_NODE='${OMEGA_GPU_MIGRATE_TO_GPU_NODE}' \
        OMEGA_GPU_FALLBACK_NETWORK='${OMEGA_GPU_FALLBACK_NETWORK}' \
        bash /tmp/omega-proxmox-install.sh
    "

    # Démarrer omega-daemon sur tous les nœuds
    info "Démarrage omega-daemon sur ${node}..."
    ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "
        systemctl daemon-reload
        systemctl enable omega-daemon.service
        systemctl restart omega-daemon.service
        sleep 1
        systemctl is-active omega-daemon.service || true
    "
    success "omega-daemon actif sur ${node}"

    success "Nœud ${node} déployé"
    echo
done

success "=== Déploiement terminé ==="
echo
echo "Récapitulatif :"
for node in "${NODES_ARR[@]}"; do
    if [[ "$node" == "$OMEGA_CONTROLLER" ]]; then
        echo "  ${node} : binaires + wrapper QEMU + contrôleur actif"
    else
        echo "  ${node} : binaires + wrapper QEMU"
    fi
done
echo
echo "Pour enregistrer le hookscript sur une VM :"
echo "  qm set <vmid> --hookscript local:snippets/omega-hook.pl"
echo
echo "Note : le hookscript est enregistré par nœud. Par défaut deploy.sh utilise"
echo "OMEGA_INSTALL_VMIDS=OMEGA_TEST_VMIDS si défini, sinon chaque nœud scanne ses"
echo "VMs locales avec qm list."
