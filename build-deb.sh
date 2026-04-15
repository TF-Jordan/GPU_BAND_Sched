#!/bin/bash
# build-deb.sh — Build the pve-gpu-sched .deb packages
#
# Usage:
#   ./build-deb.sh          Build packages
#   ./build-deb.sh clean    Clean build artifacts
#
# Prerequisites:
#   apt-get install build-essential debhelper dkms devscripts
#
# Output:
#   ../pve-gpu-sched-dkms_0.2.0-1_all.deb
#   ../pve-gpu-sched-utils_0.2.0-1_all.deb

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

PKG_NAME="pve-gpu-sched"
PKG_VERSION="0.2.0"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info() { echo -e "${GREEN}[INFO]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }

# --- Clean ---
if [ "${1}" = "clean" ]; then
    info "Cleaning build artifacts..."
    rm -rf debian/.debhelper debian/pve-gpu-sched-dkms debian/pve-gpu-sched-utils
    rm -rf debian/files debian/debhelper-build-stamp
    rm -f debian/*.debhelper debian/*.debhelper.log debian/*.substvars
    rm -f ../${PKG_NAME}-dkms_*.deb ../${PKG_NAME}-utils_*.deb
    rm -f ../${PKG_NAME}_*.buildinfo ../${PKG_NAME}_*.changes
    info "Clean done."
    exit 0
fi

# --- Check prerequisites ---
info "Checking build prerequisites..."

for cmd in dpkg-buildpackage debhelper dh; do
    if ! command -v dpkg-buildpackage &>/dev/null; then
        warn "dpkg-buildpackage not found. Installing build dependencies..."
        apt-get update -qq && apt-get install -y -qq \
            build-essential debhelper dkms devscripts 2>/dev/null || \
            error "Failed to install build dependencies.\n  Run: apt-get install build-essential debhelper dkms devscripts"
        break
    fi
done

# --- Verify source files ---
info "Verifying source files..."
for f in BAND_Sched/pve_gpu_sched.h \
         BAND_Sched/pve_gpu_sched_core.c \
         BAND_Sched/pve_gpu_sched_band.c \
         BAND_Sched/pve_gpu_sched_mdev.c \
         BAND_Sched/pve_gpu_sched_bar.c \
         BAND_Sched/Makefile \
         BAND_Sched/dkms.conf \
         scripts/pve-gpu-sched \
         conf/pve-gpu-sched.conf \
         conf/pve-gpu-sched.service \
         debian/control \
         debian/rules \
         debian/changelog; do
    [ -f "$f" ] || error "Missing file: $f"
done

info "All source files present."

# --- Build ---
info "Building .deb packages..."
info "Package: ${PKG_NAME} v${PKG_VERSION}"
echo

dpkg-buildpackage -us -uc -b --no-sign 2>&1

echo
info "Build complete!"
echo

# --- Show results ---
info "Generated packages:"
ls -lh ../${PKG_NAME}*.deb 2>/dev/null || warn "No .deb files found in parent directory"

echo
info "Installation:"
echo "  sudo dpkg -i ../${PKG_NAME}-dkms_${PKG_VERSION}-1_all.deb"
echo "  sudo dpkg -i ../${PKG_NAME}-utils_${PKG_VERSION}-1_all.deb"
echo
info "Or install both at once:"
echo "  sudo dpkg -i ../${PKG_NAME}-*.deb"
echo
info "Usage after installation:"
echo "  pve-gpu-sched detect      # Detect compatible GPUs"
echo "  pve-gpu-sched load        # Load kernel module"
echo "  pve-gpu-sched create 100  # Create GPU instance for VM 100"
echo "  pve-gpu-sched status      # Show system status"
