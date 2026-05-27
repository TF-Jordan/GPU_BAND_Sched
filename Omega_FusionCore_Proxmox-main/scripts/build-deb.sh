#!/usr/bin/env bash
# build-deb.sh — construit un paquet Debian Omega installable sur Proxmox.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

PACKAGE_NAME="${PACKAGE_NAME:-omega-remote-paging}"
VERSION="${VERSION:-$(grep -m1 '^version' "${ROOT_DIR}/omega-daemon/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')}"
ARCH="${ARCH:-amd64}"
BUILD_DIR="${ROOT_DIR}/target/deb"
PKG_DIR="${BUILD_DIR}/${PACKAGE_NAME}_${VERSION}_${ARCH}"
INSTALL_ROOT="${PKG_DIR}/opt/omega-remote-paging"

info() { echo "[INFO] $*"; }
fail() { echo "[FAIL] $*" >&2; exit 1; }

command -v dpkg-deb >/dev/null 2>&1 || fail "dpkg-deb introuvable"

info "Compilation release"
cargo build --release --workspace

info "Préparation staging ${PKG_DIR}"
rm -rf "$PKG_DIR"
mkdir -p \
    "${PKG_DIR}/DEBIAN" \
    "${INSTALL_ROOT}/bin" \
    "${INSTALL_ROOT}/scripts" \
    "${INSTALL_ROOT}/workers" \
    "${INSTALL_ROOT}/docs" \
    "${PKG_DIR}/usr/share/doc/${PACKAGE_NAME}" \
    "${PKG_DIR}/usr/sbin"

copy_bin() {
    local name="$1"
    local src="${ROOT_DIR}/target/release/${name}"
    [[ -x "$src" ]] || fail "binaire manquant: ${src}"
    install -m 755 "$src" "${INSTALL_ROOT}/bin/${name}"
}

copy_bin omega-daemon
copy_bin node-a-agent
copy_bin node-bc-store
copy_bin omega-qemu-launcher
copy_bin omega-gpu-proxy

if [[ -f "${ROOT_DIR}/omega-uffd-bridge/omega-uffd-bridge.so" ]]; then
    install -m 644 "${ROOT_DIR}/omega-uffd-bridge/omega-uffd-bridge.so" "${INSTALL_ROOT}/bin/omega-uffd-bridge.so"
fi

install -m 755 "${SCRIPT_DIR}/omega-proxmox-install.sh" "${INSTALL_ROOT}/scripts/omega-proxmox-install.sh"
install -m 755 "${SCRIPT_DIR}/deploy.sh" "${INSTALL_ROOT}/scripts/deploy.sh"
install -m 755 "${SCRIPT_DIR}/uninstall.sh" "${INSTALL_ROOT}/scripts/uninstall.sh"
install -m 755 "${SCRIPT_DIR}/omega-lab.sh" "${INSTALL_ROOT}/scripts/omega-lab.sh"
install -m 755 "${SCRIPT_DIR}/omega-gpu-client.sh" "${INSTALL_ROOT}/scripts/omega-gpu-client.sh"
install -m 755 "${SCRIPT_DIR}/create-omega-vm.sh" "${INSTALL_ROOT}/scripts/create-omega-vm.sh"
install -m 755 "${SCRIPT_DIR}/provision-omega-vms-remote.sh" "${INSTALL_ROOT}/scripts/provision-omega-vms-remote.sh"
install -m 644 "${SCRIPT_DIR}/proxmox_hook.pl" "${INSTALL_ROOT}/scripts/proxmox_hook.pl"

install -m 755 "${SCRIPT_DIR}/omega-gpu-worker-cpu.py" "${INSTALL_ROOT}/workers/omega-gpu-worker-cpu.py"
install -m 755 "${SCRIPT_DIR}/omega-gpu-worker-app.py" "${INSTALL_ROOT}/workers/omega-gpu-worker-app.py"
cat > "${INSTALL_ROOT}/workers/omega-gpu-worker-app-cuda" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
INSTALL_DIR="${OMEGA_INSTALL_DIR:-/opt/omega-remote-paging}"
WORKER_DIR="${OMEGA_GPU_WORKER_DIR:-${INSTALL_DIR}/workers}"
if [[ -n "${OMEGA_GPU_PYTHON:-}" && -x "${OMEGA_GPU_PYTHON}" ]]; then
    exec "${OMEGA_GPU_PYTHON}" "${WORKER_DIR}/omega-gpu-worker-app.py"
fi
for py in \
    "${INSTALL_DIR}/gpu-venv/bin/python" \
    "/opt/omega-gpu-venv/bin/python" \
    "/opt/omega-cuda-venv/bin/python"
do
    if [[ -x "$py" ]]; then
        exec "$py" "${WORKER_DIR}/omega-gpu-worker-app.py"
    fi
done
if [[ "${OMEGA_GPU_WORKER_REQUIRE_CUDA:-0}" == "1" ]]; then
    echo "CUDA obligatoire mais aucun Python CUDA trouvé. Définir OMEGA_GPU_PYTHON=/chemin/venv/bin/python" >&2
    exit 127
fi
exec /usr/bin/env python3 "${WORKER_DIR}/omega-gpu-worker-app.py"
EOF
chmod 755 "${INSTALL_ROOT}/workers/omega-gpu-worker-app-cuda"

if [[ -d "${ROOT_DIR}/docs" ]]; then
    find "${ROOT_DIR}/docs" -maxdepth 1 -type f -name '*.md' -exec install -m 644 {} "${INSTALL_ROOT}/docs/" \;
fi
install -m 644 "${ROOT_DIR}/README.md" "${PKG_DIR}/usr/share/doc/${PACKAGE_NAME}/README.md" 2>/dev/null || true

cat > "${PKG_DIR}/usr/sbin/omega-node-install" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
exec /opt/omega-remote-paging/scripts/omega-proxmox-install.sh "$@"
EOF
chmod 755 "${PKG_DIR}/usr/sbin/omega-node-install"

cat > "${PKG_DIR}/DEBIAN/control" <<EOF
Package: ${PACKAGE_NAME}
Version: ${VERSION}
Section: admin
Priority: optional
Architecture: ${ARCH}
Maintainer: omega-remote-paging contributors
Depends: bash, systemd, qemu-server, pve-manager, python3, curl
Recommends: stress-ng, qemu-guest-agent, python3-venv
Description: Omega remote paging, elastic VM resources and GPU application proxy for Proxmox
 Omega installs the node daemon, QEMU launcher wrapper, remote paging agents,
 cluster test tooling and the application-level GPU proxy.
EOF

cat > "${PKG_DIR}/DEBIAN/postinst" <<'EOF'
#!/usr/bin/env bash
set -e
mkdir -p /etc/omega /var/log/omega /var/lib/omega-qemu
if [ ! -f /etc/omega/cluster.env ]; then
    {
        echo "OMEGA_NODE_ID=$(hostname -s)"
        echo "OMEGA_NODE_ADDR=$(hostname -I | awk '{print $1}')"
        echo "OMEGA_STORE_PORT=9100"
        echo "OMEGA_API_PORT=9200"
        echo "OMEGA_GPU_NODES="
        echo "OMEGA_GPU_PRIMARY_NODE="
        echo "OMEGA_GPU_PROXY_URL="
        echo "OMEGA_GPU_PROXY_TOTAL_VRAM_MIB=0"
        echo "OMEGA_GPU_PYTHON="
        echo "OMEGA_GPU_PROXY_BACKEND_COMMAND=/opt/omega-remote-paging/workers/omega-gpu-worker-app-cuda"
        echo "OMEGA_GPU_PROXY_API_TOKEN_FILE=/etc/omega/gpu-proxy.token"
        echo "OMEGA_GPU_MIGRATE_TO_GPU_NODE=1"
        echo "OMEGA_GPU_FALLBACK_NETWORK=1"
    } > /etc/omega/cluster.env
fi
if [ ! -s /etc/omega/gpu-proxy.token ]; then
    umask 077
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > /etc/omega/gpu-proxy.token
    echo >> /etc/omega/gpu-proxy.token
fi
chmod 600 /etc/omega/gpu-proxy.token
echo "Omega installé. Pour activer le noeud Proxmox: omega-node-install"
EOF

cat > "${PKG_DIR}/DEBIAN/prerm" <<'EOF'
#!/usr/bin/env bash
set -e
systemctl stop omega-gpu-proxy.service 2>/dev/null || true
systemctl stop omega-daemon.service 2>/dev/null || true
systemctl stop omega-hookscript-watcher.service 2>/dev/null || true
EOF

cat > "${PKG_DIR}/DEBIAN/postrm" <<'EOF'
#!/usr/bin/env bash
set -e
if [ "$1" = "purge" ]; then
    rm -f /etc/systemd/system/omega-gpu-proxy.service
    rm -f /etc/systemd/system/omega-daemon.service
    rm -f /etc/systemd/system/omega-hookscript-watcher.service
    systemctl daemon-reload 2>/dev/null || true
fi
EOF

chmod 755 "${PKG_DIR}/DEBIAN/postinst" "${PKG_DIR}/DEBIAN/prerm" "${PKG_DIR}/DEBIAN/postrm"

info "Construction paquet"
dpkg-deb --build --root-owner-group "$PKG_DIR" "${BUILD_DIR}/${PACKAGE_NAME}_${VERSION}_${ARCH}.deb"
info "Paquet créé: ${BUILD_DIR}/${PACKAGE_NAME}_${VERSION}_${ARCH}.deb"
