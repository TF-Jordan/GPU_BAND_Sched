#!/usr/bin/env bash
# Test 24 — Installation physique : services, wrapper QEMU, hookscript, doctor launcher.
# Usage : ./24-install-doctor.sh

set -euo pipefail
source "$(dirname "$0")/lib.sh"

header "Test 24 — Installation physique / doctor"

require_cluster

REMOTE_RUN_DIR="${OMEGA_RUN_DIR:-/var/lib/omega-qemu}"
REMOTE_BRIDGE_LIB="${OMEGA_BRIDGE_LIB:-/usr/local/lib/omega-uffd-bridge.so}"
REMOTE_REAL_KVM="${OMEGA_REAL_KVM:-/usr/bin/kvm.real}"
GPU_NODES="${OMEGA_GPU_NODES:-${OMEGA_GPU_PRIMARY_NODE:-}}"
GPU_PRIMARY="${OMEGA_GPU_PRIMARY_NODE:-}"
GPU_PROXY_URL="${OMEGA_GPU_PROXY_URL:-${GPU_PRIMARY:+http://${GPU_PRIMARY}:9400}}"

for node in "${OMEGA_NODES_ARR[@]}"; do
    node_real_kvm="$REMOTE_REAL_KVM"
    node_hostname="$(ssh_run "$node" "hostname -s" | tail -1 || true)"
    step "Nœud $node — services et binaires"

    REMOTE_BIN_DIR=$(ssh_run "$node" "
        for d in '${OMEGA_REMOTE_BIN_DIR:-}' /opt/omega-remote-paging/bin /usr/local/bin /tmp/omega-tests-bins; do
            [ -n \"\$d\" ] || continue
            [ -x \"\$d/omega-daemon\" ] && [ -x \"\$d/node-a-agent\" ] && [ -x \"\$d/omega-qemu-launcher\" ] && { echo \"\$d\"; exit 0; }
        done
        systemctl show omega-daemon -p ExecStart --value 2>/dev/null | awk '{print \$1}' | xargs -r dirname
    " | tail -1)
    [[ -n "$REMOTE_BIN_DIR" ]] || fail "$node : impossible de trouver les binaires Omega"

    ssh_run "$node" "test -x '${REMOTE_BIN_DIR}/omega-daemon' && test -x '${REMOTE_BIN_DIR}/node-a-agent' && test -x '${REMOTE_BIN_DIR}/omega-qemu-launcher'" \
        || fail "$node : binaires Omega incomplets dans ${REMOTE_BIN_DIR}"
    pass "$node : binaires Omega trouvés dans ${REMOTE_BIN_DIR}"

    active=$(ssh_run "$node" "systemctl is-active omega-daemon 2>/dev/null || true")
    [[ "$active" == "active" ]] || fail "$node : omega-daemon non actif (status=$active)"
    pass "$node : omega-daemon actif"

    step "Nœud $node — wrapper QEMU"
    ssh_run "$node" "test -L /usr/bin/kvm" \
        || fail "$node : /usr/bin/kvm n'est pas un symlink vers le wrapper"
    wrapper_target=$(ssh_run "$node" "readlink -f /usr/bin/kvm")
    case "$wrapper_target" in
        *omega-qemu-wrapper*|*omega-qemu-launcher*|*kvm-omega*) pass "$node : wrapper QEMU actif ($wrapper_target)" ;;
        *) fail "$node : /usr/bin/kvm pointe vers $wrapper_target, pas vers Omega" ;;
    esac

    if ! ssh_run "$node" "test -x '${node_real_kvm}'"; then
        fallback_real=$(ssh_run "$node" "command -v kvm.real || command -v qemu-system-x86_64 || true" | head -1)
        [[ -n "$fallback_real" ]] || fail "$node : QEMU réel introuvable/non exécutable: ${node_real_kvm}"
        node_real_kvm="$fallback_real"
        warn "$node : QEMU réel fallback utilisé: ${node_real_kvm}"
    fi

    step "Nœud $node — hookscript Proxmox"
    ssh_run "$node" "test -f /var/lib/vz/snippets/omega-hook.pl || test -f /var/lib/vz/snippets/proxmox_hook.pl" \
        || warn "$node : hookscript Omega non trouvé dans local:snippets"

    step "Nœud $node — omega-qemu-launcher doctor"
    bridge_arg=""
    if ssh_run "$node" "test -f '${REMOTE_BRIDGE_LIB}'"; then
        bridge_arg="--bridge-lib '${REMOTE_BRIDGE_LIB}'"
    else
        warn "$node : bridge UFFD absent (${REMOTE_BRIDGE_LIB}) — doctor sans bridge"
    fi

    if ssh_run "$node" "'${REMOTE_BIN_DIR}/omega-qemu-launcher' doctor \
            --qemu-bin '${node_real_kvm}' \
            --agent-bin '${REMOTE_BIN_DIR}/node-a-agent' \
            --run-dir '${REMOTE_RUN_DIR}' \
            ${bridge_arg}"; then
        pass "$node : doctor OK"
    else
        fail "$node : omega-qemu-launcher doctor en échec"
    fi

    if [[ ",${GPU_NODES}," == *",${node},"* || ",${GPU_NODES}," == *",${node_hostname},"* ]]; then
        step "Nœud $node — GPU proxy production"
        ssh_run "$node" "command -v nvidia-smi >/dev/null && nvidia-smi -L >/dev/null" \
            || fail "$node : nvidia-smi indisponible sur le nœud GPU principal"
        pass "$node : nvidia-smi OK"

        ssh_run "$node" "test -x '${REMOTE_BIN_DIR}/omega-gpu-proxy' && test -x /opt/omega-remote-paging/workers/omega-gpu-worker-app.py" \
            || fail "$node : omega-gpu-proxy ou worker applicatif absent"
        pass "$node : binaires GPU proxy OK"

        gpu_proxy_active=$(ssh_run "$node" "systemctl is-active omega-gpu-proxy 2>/dev/null || true")
        [[ "$gpu_proxy_active" == "active" ]] || fail "$node : omega-gpu-proxy non actif (status=$gpu_proxy_active)"
        pass "$node : omega-gpu-proxy actif"

        ssh_run "$node" "
            token_file='${OMEGA_GPU_PROXY_API_TOKEN_FILE:-/etc/omega/gpu-proxy.token}'
            token=''
            [ -r \"\$token_file\" ] && token=\$(tr -d ' \n\r\t' < \"\$token_file\")
            url='http://127.0.0.1:9400'
            curl -fsS \"\$url/health\" >/dev/null
            if [ -n \"\$token\" ]; then
                curl -fsS -H \"Authorization: Bearer \$token\" \"\$url/gpu/status\" >/dev/null
            else
                curl -fsS \"\$url/gpu/status\" >/dev/null
            fi
        " || fail "$node : API omega-gpu-proxy inaccessible"
        pass "$node : API omega-gpu-proxy OK"
    fi
done

pass "Installation physique validée sur ${#OMEGA_NODES_ARR[@]} nœud(s)"
