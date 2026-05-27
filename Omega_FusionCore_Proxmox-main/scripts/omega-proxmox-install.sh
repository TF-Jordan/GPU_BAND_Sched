#!/usr/bin/env bash
# omega-proxmox-install.sh — Installation complète d'Omega sur un nœud Proxmox A
#
# Ce script :
#   1. Installe les binaires Rust compilés dans INSTALL_DIR
#   2. Remplace /usr/bin/kvm par un wrapper omega-qemu-launcher (intercept QEMU)
#   3. Installe le hookscript Proxmox dans /var/lib/vz/snippets/
#   4. Installe les services systemd omega-daemon et node-bc-store
#   5. Enregistre le hookscript sur les VMs locales, ou sur OMEGA_VMIDS si fourni
#   6. Installe optionnellement omega-gpu-proxy et ses workers applicatifs
#
# Prérequis :
#   - Binaires compilés : make build
#   - Proxmox VE 7+ sur le nœud courant
#   - accès root
#
# Variables d'environnement :
#   OMEGA_STORES     : adresses stores B et C (défaut : 127.0.0.1:9100,127.0.0.1:9101)
#   OMEGA_RUN_DIR    : répertoire d'état runtime   (défaut : /var/lib/omega-qemu)
#   OMEGA_LOG_DIR    : répertoire de logs           (défaut : /var/log/omega)
#   INSTALL_DIR      : répertoire d'installation    (défaut : /usr/local/bin)
#   OMEGA_VMIDS      : VMIDs séparés par virgule pour enregistrer le hookscript
#                      (ex: "100,101,102") — vide = toutes les VMs locales
#   OMEGA_REAL_KVM   : chemin vers kvm réel (défaut : /usr/bin/kvm.real)
#   OMEGA_AGENT_BIN  : chemin vers node-a-agent (défaut : $INSTALL_DIR/node-a-agent)
#   OMEGA_GPU_PROXY_ENABLED : 1 pour démarrer le proxy GPU applicatif
#   OMEGA_GPU_PROXY_LISTEN  : adresse d'écoute du proxy GPU (défaut : 0.0.0.0:9400)
#   OMEGA_GPU_PROXY_BACKEND_COMMAND : worker GPU applicatif
#   OMEGA_GPU_PYTHON : Python/venv CUDA à utiliser par le worker applicatif
#   OMEGA_GPU_PRIMARY_NODE  : nœud GPU principal du cluster (hostname ou IP)
#   OMEGA_GPU_PROXY_API_TOKEN_FILE : fichier token API proxy GPU

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

: "${OMEGA_STORES:=127.0.0.1:9100,127.0.0.1:9101}"
: "${OMEGA_RUN_DIR:=/var/lib/omega-qemu}"
: "${OMEGA_LOG_DIR:=/var/log/omega}"
: "${INSTALL_DIR:=/usr/local/bin}"
: "${OMEGA_REAL_KVM:=/usr/bin/kvm.real}"
: "${OMEGA_VMIDS:=}"
: "${SNIPPETS_DIR:=/var/lib/vz/snippets}"
: "${HOOKSCRIPT_NAME:=omega-hook.pl}"
: "${OMEGA_OBJECT_ID:=ram0}"
: "${OMEGA_START_TIMEOUT:=30}"
: "${OMEGA_STORE_TIMEOUT_MS:=2000}"
: "${BRIDGE_LIB_DIR:=/usr/local/lib}"
: "${BRIDGE_LIB:=${BRIDGE_LIB_DIR}/omega-uffd-bridge.so}"
: "${OMEGA_GPU_PROXY_ENABLED:=0}"
: "${OMEGA_GPU_PROXY_LISTEN:=0.0.0.0:9400}"
: "${OMEGA_GPU_PROXY_MAX_CONCURRENT:=1}"
: "${OMEGA_GPU_PROXY_TOTAL_VRAM_MIB:=0}"
: "${OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS:=900}"
: "${OMEGA_GPU_PYTHON:=}"
: "${OMEGA_GPU_WORKER_DIR:=/opt/omega-remote-paging/workers}"
: "${OMEGA_GPU_PROXY_API_TOKEN_FILE:=/etc/omega/gpu-proxy.token}"
: "${OMEGA_GPU_PROXY_API_TOKEN:=}"
: "${OMEGA_GPU_NODES:=}"
: "${OMEGA_GPU_PRIMARY_NODE:=}"
: "${OMEGA_GPU_PROXY_URL:=}"
: "${OMEGA_GPU_MIGRATE_TO_GPU_NODE:=1}"
: "${OMEGA_GPU_FALLBACK_NETWORK:=1}"

info()    { echo -e "\033[32m[INFO]\033[0m   $*"; }
warn()    { echo -e "\033[33m[WARN]\033[0m   $*"; }
success() { echo -e "\033[32m[OK]\033[0m     $*"; }
fail()    { echo -e "\033[31m[ERROR]\033[0m  $*" >&2; exit 1; }
step()    { echo; echo -e "\033[34m──── $* ────\033[0m"; }

set_env_var() {
    local key="$1" value="$2" file="${3:-/etc/omega/cluster.env}"
    mkdir -p "$(dirname "$file")"
    touch "$file"
    if grep -q "^${key}=" "$file"; then
        sed -i "s|^${key}=.*|${key}=${value}|" "$file"
    else
        echo "${key}=${value}" >> "$file"
    fi
}

ensure_token_file() {
    local file="$1"
    if [[ ! -s "$file" ]]; then
        mkdir -p "$(dirname "$file")"
        umask 077
        if [[ -n "$OMEGA_GPU_PROXY_API_TOKEN" ]]; then
            printf '%s\n' "$OMEGA_GPU_PROXY_API_TOKEN" > "$file"
        else
            head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$file"
            echo >> "$file"
        fi
    fi
    chmod 600 "$file"
}

# ─── Vérifications ────────────────────────────────────────────────────────────

[[ "$(id -u)" == "0" ]] || fail "Ce script doit être exécuté en root"

# Les chemins sources peuvent être surchargés via env vars (utile quand les
# binaires sont déjà copiés sur le nœud par deploy.sh)
LAUNCHER_SRC="${LAUNCHER_SRC:-${ROOT_DIR}/target/release/omega-qemu-launcher}"
AGENT_SRC="${AGENT_SRC:-${ROOT_DIR}/target/release/node-a-agent}"
DAEMON_SRC="${DAEMON_SRC:-${ROOT_DIR}/target/release/omega-daemon}"
GPU_PROXY_SRC="${GPU_PROXY_SRC:-${ROOT_DIR}/target/release/omega-gpu-proxy}"
BRIDGE_SRC="${BRIDGE_SRC:-${ROOT_DIR}/omega-uffd-bridge/omega-uffd-bridge.so}"
GPU_WORKER_CPU_SRC="${GPU_WORKER_CPU_SRC:-${ROOT_DIR}/scripts/omega-gpu-worker-cpu.py}"
GPU_WORKER_APP_SRC="${GPU_WORKER_APP_SRC:-${ROOT_DIR}/scripts/omega-gpu-worker-app.py}"

[[ -x "$LAUNCHER_SRC" ]] || fail "omega-qemu-launcher introuvable : ${LAUNCHER_SRC}"
[[ -x "$AGENT_SRC"    ]] || fail "node-a-agent introuvable : ${AGENT_SRC}"

# ─── 1. Installer les binaires ────────────────────────────────────────────────

step "Installation des binaires dans ${INSTALL_DIR}"

# Copier uniquement si la source diffère de la destination
_install_bin() {
    local src="$1" dst="$2"
    [[ "$(realpath "$src" 2>/dev/null)" == "$(realpath "$dst" 2>/dev/null)" ]] \
        && { success "$(basename "$dst") déjà en place"; return; }
    install -m 755 "$src" "$dst"
}

_install_bin "$LAUNCHER_SRC" "${INSTALL_DIR}/omega-qemu-launcher"
_install_bin "$AGENT_SRC"    "${INSTALL_DIR}/node-a-agent"
[[ -x "$DAEMON_SRC" ]] && _install_bin "$DAEMON_SRC" "${INSTALL_DIR}/omega-daemon" || true
[[ -x "$GPU_PROXY_SRC" ]] && _install_bin "$GPU_PROXY_SRC" "${INSTALL_DIR}/omega-gpu-proxy" || warn "omega-gpu-proxy non trouvé (${GPU_PROXY_SRC}) — proxy GPU applicatif non installé"

mkdir -p "$OMEGA_GPU_WORKER_DIR"
if [[ -f "$GPU_WORKER_CPU_SRC" ]]; then
    install -m 755 "$GPU_WORKER_CPU_SRC" "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-cpu.py"
fi
if [[ -f "$GPU_WORKER_APP_SRC" ]]; then
    install -m 755 "$GPU_WORKER_APP_SRC" "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app.py"
fi
cat > "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app-cuda" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ -n "\${OMEGA_GPU_PYTHON:-}" && -x "\${OMEGA_GPU_PYTHON}" ]]; then
    exec "\${OMEGA_GPU_PYTHON}" "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app.py"
fi
for py in \
    "${INSTALL_DIR}/gpu-venv/bin/python" \
    "/opt/omega-gpu-venv/bin/python" \
    "/opt/omega-cuda-venv/bin/python"
do
    if [[ -x "\$py" ]]; then
        exec "\$py" "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app.py"
    fi
done
if [[ "\${OMEGA_GPU_WORKER_REQUIRE_CUDA:-0}" == "1" ]]; then
    echo "CUDA obligatoire mais aucun Python CUDA trouvé. Définir OMEGA_GPU_PYTHON=/chemin/venv/bin/python" >&2
    exit 127
fi
exec /usr/bin/env python3 "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app.py"
EOF
chmod 755 "${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app-cuda"
OMEGA_GPU_PROXY_BACKEND_COMMAND="${OMEGA_GPU_PROXY_BACKEND_COMMAND:-${OMEGA_GPU_WORKER_DIR}/omega-gpu-worker-app-cuda}"

# Bridge LD_PRELOAD (optionnel — absence = pas d'interception uffd QEMU)
if [[ -f "$BRIDGE_SRC" ]]; then
    mkdir -p "$BRIDGE_LIB_DIR"
    install -m 644 "$BRIDGE_SRC" "$BRIDGE_LIB"
    ldconfig "$BRIDGE_LIB_DIR" 2>/dev/null || true
    success "Bridge LD_PRELOAD installé : ${BRIDGE_LIB}"
else
    warn "omega-uffd-bridge.so non trouvé (${BRIDGE_SRC}) — compilez avec 'make build-bridge'"
fi

success "Binaires installés"

# ─── 2. Wrapper QEMU (intercept /usr/bin/kvm) ─────────────────────────────────

step "Installation du wrapper QEMU omega"

WRAPPER_PATH="${INSTALL_DIR}/kvm-omega"
OMEGA_AGENT_BIN="${OMEGA_AGENT_BIN:-${INSTALL_DIR}/node-a-agent}"

find_real_qemu() {
    local candidate
    for candidate in \
        /usr/bin/qemu-system-x86_64 \
        /usr/libexec/qemu-kvm \
        /usr/bin/qemu-kvm
    do
        [[ -x "$candidate" ]] && { printf '%s\n' "$candidate"; return 0; }
    done
    command -v qemu-system-x86_64 2>/dev/null || true
}

# Sauvegarder le kvm réel si ce n'est pas déjà fait. L'installation doit rester
# idempotente: après une mise à jour Proxmox, /usr/bin/kvm peut redevenir un
# binaire normal alors que /usr/bin/kvm.real existe déjà.
if [[ -L "/usr/bin/kvm" ]]; then
    current_kvm_target="$(readlink -f /usr/bin/kvm || true)"
    if [[ ! "$current_kvm_target" =~ omega ]]; then
        ln -sfn "$current_kvm_target" "$OMEGA_REAL_KVM"
        info "cible QEMU réelle détectée depuis /usr/bin/kvm: ${current_kvm_target}"
    else
        info "/usr/bin/kvm pointe déjà vers Omega — il sera repointé si nécessaire"
    fi
elif [[ -e "/usr/bin/kvm" && ! -e "$OMEGA_REAL_KVM" ]]; then
    mv /usr/bin/kvm "$OMEGA_REAL_KVM"
    info "kvm réel sauvegardé dans ${OMEGA_REAL_KVM}"
elif [[ -e "/usr/bin/kvm" && -e "$OMEGA_REAL_KVM" ]]; then
    KVM_BACKUP="/usr/bin/kvm.pre-omega.$(date +%Y%m%d%H%M%S)"
    mv /usr/bin/kvm "$KVM_BACKUP"
    warn "/usr/bin/kvm était un fichier normal malgré ${OMEGA_REAL_KVM}; backup créé: ${KVM_BACKUP}"
fi

if [[ ! -x "$OMEGA_REAL_KVM" ]]; then
    REAL_QEMU_CANDIDATE="$(find_real_qemu)"
    if [[ -n "$REAL_QEMU_CANDIDATE" && -x "$REAL_QEMU_CANDIDATE" ]]; then
        ln -sfn "$REAL_QEMU_CANDIDATE" "$OMEGA_REAL_KVM"
        info "kvm réel initialisé: ${OMEGA_REAL_KVM} -> ${REAL_QEMU_CANDIDATE}"
    fi
fi

[[ -x "$OMEGA_REAL_KVM" ]] || fail "kvm réel introuvable (${OMEGA_REAL_KVM}) — installer qemu-system-x86 ou définir OMEGA_REAL_KVM"

# Générer le wrapper shell via omega-qemu-launcher write-proxmox-wrapper
BRIDGE_ARG=()
if [[ -f "$BRIDGE_LIB" ]]; then
    BRIDGE_ARG=(--bridge-lib "$BRIDGE_LIB")
fi

"${INSTALL_DIR}/omega-qemu-launcher" write-proxmox-wrapper \
    --stores         "$OMEGA_STORES" \
    --qemu-bin       "$OMEGA_REAL_KVM" \
    --agent-bin      "$OMEGA_AGENT_BIN" \
    --run-dir        "$OMEGA_RUN_DIR" \
    --object-id      "$OMEGA_OBJECT_ID" \
    --start-timeout-secs "$OMEGA_START_TIMEOUT" \
    --store-timeout-ms   "$OMEGA_STORE_TIMEOUT_MS" \
    "${BRIDGE_ARG[@]}" \
    --output         "$WRAPPER_PATH"

success "Wrapper écrit dans ${WRAPPER_PATH}"

step "Diagnostic Omega QEMU local"

DOCTOR_BRIDGE_ARG=()
if [[ -f "$BRIDGE_LIB" ]]; then
    DOCTOR_BRIDGE_ARG=(--bridge-lib "$BRIDGE_LIB")
fi

if "${INSTALL_DIR}/omega-qemu-launcher" doctor \
    --qemu-bin  "$OMEGA_REAL_KVM" \
    --agent-bin "$OMEGA_AGENT_BIN" \
    --run-dir   "$OMEGA_RUN_DIR" \
    "${DOCTOR_BRIDGE_ARG[@]}"
then
    success "Diagnostic omega-qemu-launcher OK"
else
    warn "Diagnostic omega-qemu-launcher en échec — vérifier userfaultfd, bridge et chemins binaires"
fi

# Créer/mettre à jour le symlink /usr/bin/kvm → wrapper.
ln -sfn "$WRAPPER_PATH" /usr/bin/kvm
success "Symlink /usr/bin/kvm → ${WRAPPER_PATH} actif"

# ─── 3. Hookscript Proxmox ────────────────────────────────────────────────────

step "Installation du hookscript Proxmox"

mkdir -p "$SNIPPETS_DIR"
HOOKSCRIPT_DEST="${SNIPPETS_DIR}/${HOOKSCRIPT_NAME}"

# Copier le hookscript en injectant les valeurs de configuration
sed \
    -e "s|/usr/local/bin/omega-qemu-launcher|${INSTALL_DIR}/omega-qemu-launcher|g" \
    -e "s|/var/lib/omega-qemu|${OMEGA_RUN_DIR}|g" \
    -e "s|127.0.0.1:9100,127.0.0.1:9101|${OMEGA_STORES}|g" \
    "${SCRIPT_DIR}/proxmox_hook.pl" > "$HOOKSCRIPT_DEST"
chmod +x "$HOOKSCRIPT_DEST"

success "Hookscript installé dans ${HOOKSCRIPT_DEST}"

# ─── 4. Service systemd omega-daemon ─────────────────────────────────────────

step "Installation du service systemd omega-daemon"

mkdir -p "$OMEGA_LOG_DIR" "$OMEGA_RUN_DIR"
mkdir -p /etc/omega
NODE_ADDR="$(hostname -I | awk '{print $1}')"
NODE_ID="$(hostname -s)"
[[ -n "$OMEGA_GPU_PRIMARY_NODE" ]] || OMEGA_GPU_PRIMARY_NODE="$NODE_ID"
[[ -n "$OMEGA_GPU_PROXY_URL" ]] || OMEGA_GPU_PROXY_URL="http://${OMEGA_GPU_PRIMARY_NODE}:9400"
ensure_token_file "$OMEGA_GPU_PROXY_API_TOKEN_FILE"

set_env_var OMEGA_NODE_ID "$NODE_ID"
set_env_var OMEGA_NODE_ADDR "$NODE_ADDR"
set_env_var OMEGA_STORE_PORT "9100"
set_env_var OMEGA_API_PORT "9200"
set_env_var OMEGA_GPU_NODES "$OMEGA_GPU_NODES"
set_env_var OMEGA_GPU_PRIMARY_NODE "$OMEGA_GPU_PRIMARY_NODE"
set_env_var OMEGA_GPU_PROXY_URL "$OMEGA_GPU_PROXY_URL"
set_env_var OMEGA_GPU_PROXY_LISTEN "$OMEGA_GPU_PROXY_LISTEN"
set_env_var OMEGA_GPU_PROXY_MAX_CONCURRENT "$OMEGA_GPU_PROXY_MAX_CONCURRENT"
set_env_var OMEGA_GPU_PROXY_TOTAL_VRAM_MIB "$OMEGA_GPU_PROXY_TOTAL_VRAM_MIB"
set_env_var OMEGA_GPU_PROXY_BACKEND_COMMAND "$OMEGA_GPU_PROXY_BACKEND_COMMAND"
set_env_var OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS "$OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS"
set_env_var OMEGA_GPU_PYTHON "$OMEGA_GPU_PYTHON"
set_env_var OMEGA_GPU_PROXY_API_TOKEN_FILE "$OMEGA_GPU_PROXY_API_TOKEN_FILE"
set_env_var OMEGA_GPU_MIGRATE_TO_GPU_NODE "$OMEGA_GPU_MIGRATE_TO_GPU_NODE"
set_env_var OMEGA_GPU_FALLBACK_NETWORK "$OMEGA_GPU_FALLBACK_NETWORK"
success "Environnement daemon : /etc/omega/cluster.env"

cat > /etc/systemd/system/omega-daemon.service <<EOF
[Unit]
Description=Omega Remote Paging Daemon
After=network.target

[Service]
Type=simple
EnvironmentFile=-/etc/omega/cluster.env
ExecStart=${INSTALL_DIR}/omega-daemon
Restart=always
RestartSec=5
Environment=RUST_LOG=info
RuntimeDirectory=omega
LogsDirectory=omega
StateDirectory=omega

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable omega-daemon.service
success "Service omega-daemon installé (démarrage manuel : systemctl start omega-daemon)"

# ─── 4b. Service systemd omega-gpu-proxy ─────────────────────────────────────

if [[ -x "${INSTALL_DIR}/omega-gpu-proxy" ]]; then
    step "Installation du service systemd omega-gpu-proxy"

    cat > /etc/systemd/system/omega-gpu-proxy.service <<EOF
[Unit]
Description=Omega GPU Application Proxy
After=network.target

[Service]
Type=simple
EnvironmentFile=-/etc/omega/cluster.env
Environment=RUST_LOG=info
ExecStart=${INSTALL_DIR}/omega-gpu-proxy
Restart=always
RestartSec=5
RuntimeDirectory=omega
LogsDirectory=omega
StateDirectory=omega

[Install]
WantedBy=multi-user.target
EOF

    systemctl daemon-reload
    systemctl enable omega-gpu-proxy.service
    if [[ "$OMEGA_GPU_PROXY_ENABLED" == "1" ]]; then
        systemctl restart omega-gpu-proxy.service
        success "Service omega-gpu-proxy actif (${OMEGA_GPU_PROXY_LISTEN})"
    else
        success "Service omega-gpu-proxy installé (démarrage manuel : systemctl start omega-gpu-proxy)"
    fi
fi

# ─── 5. Enregistrement hookscript sur les VMs locales ─────────────────────────

step "Enregistrement automatique du hookscript sur les VMs locales"

if [[ -n "$OMEGA_VMIDS" ]]; then
    info "VMs ciblées par OMEGA_VMIDS : ${OMEGA_VMIDS}"
    mapfile -t hook_vmids < <(tr ',' '\n' <<<"$OMEGA_VMIDS" | awk 'NF {gsub(/^[ \t]+|[ \t]+$/, ""); print}')
else
    info "OMEGA_VMIDS vide : scan des VMs locales visibles par qm list sur ce nœud"
    mapfile -t hook_vmids < <(qm list 2>/dev/null | awk 'NR>1 {print $1}')
fi

registered=0; skipped=0
for vmid in "${hook_vmids[@]}"; do
    [[ "$vmid" =~ ^[0-9]+$ ]] || continue
    if qm config "$vmid" 2>/dev/null | grep -q "omega-hook"; then
        info "VM ${vmid} : hookscript déjà configuré"
        skipped=$((skipped + 1))
    else
        qm set "$vmid" --hookscript "local:snippets/${HOOKSCRIPT_NAME}" 2>/dev/null \
            && { success "hookscript enregistré sur VM ${vmid}"; registered=$((registered + 1)); } \
            || warn "VM ${vmid} : qm set échoué ou VM non locale sur ce nœud"
    fi
done

info "VMs traitées sur ce nœud : ${#hook_vmids[@]}  enregistrées : ${registered}  déjà configurées : ${skipped}"
info "Pour couvrir tout le cluster, exécuter ce script sur chaque nœud Proxmox."

# ─── 6. Service systemd — auto-enregistrement hookscript toutes les 10s ───────

step "Installation service auto-enregistrement hookscript"

cat > /etc/systemd/system/omega-hookscript-watcher.service <<EOF
[Unit]
Description=Omega — auto-enregistrement hookscript sur les nouvelles VMs
After=pve-cluster.service

[Service]
Type=simple
ExecStart=/bin/bash -c 'while true; do for v in \$(qm list 2>/dev/null | awk "NR>1 {print \$1}"); do qm config \$v 2>/dev/null | grep -q omega-hook || qm set \$v --hookscript local:snippets/${HOOKSCRIPT_NAME} 2>/dev/null; done; sleep 10; done'
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable omega-hookscript-watcher.service
systemctl restart omega-hookscript-watcher.service
success "service omega-hookscript-watcher actif (toutes les 10s)"

# ─── Résumé ───────────────────────────────────────────────────────────────────

echo
echo -e "\033[32m╔══════════════════════════════════════════════════════════════╗\033[0m"
echo -e "\033[32m║          Omega Proxmox RAM — Installation terminée           ║\033[0m"
echo -e "\033[32m╚══════════════════════════════════════════════════════════════╝\033[0m"
echo
echo "  Binaires          : ${INSTALL_DIR}/omega-qemu-launcher"
echo "                      ${INSTALL_DIR}/node-a-agent"
echo "  Wrapper QEMU      : ${WRAPPER_PATH}"
echo "  kvm réel          : ${OMEGA_REAL_KVM}"
echo "  Hookscript        : ${HOOKSCRIPT_DEST}"
echo "  Service           : systemctl start omega-daemon"
echo "  Proxy GPU         : systemctl start omega-gpu-proxy"
echo "  Workers GPU       : ${OMEGA_GPU_WORKER_DIR}"
echo "  Config cluster    : /etc/omega/cluster.env"
echo "  Token proxy GPU   : ${OMEGA_GPU_PROXY_API_TOKEN_FILE}"
echo "  Stores B/C        : ${OMEGA_STORES}"
echo "  Répertoire état   : ${OMEGA_RUN_DIR}"
echo
echo "  Hookscript auto    : systemctl status omega-hookscript-watcher (toutes les 10s)"
echo
echo "  Pour vérifier le wrapper :"
echo "    ls -la /usr/bin/kvm"
echo "    ${INSTALL_DIR}/omega-qemu-launcher status --vm-id <vmid>"
echo
