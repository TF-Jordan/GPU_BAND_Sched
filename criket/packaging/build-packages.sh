#!/bin/bash
# build-packages.sh - assemble cricket-server and cricket-client .deb packages.
#
# Usage: ./build-packages.sh [VERSION]
#   VERSION: package version (default: read from DEBIAN/control)
#
# Binaries are taken from ../docs/ (cricket-client.so, cricket-rpc-server,
# libtirpc.so.3) or from ../bin/ if the former is absent.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="${OUT_DIR:-$SCRIPT_DIR/dist}"

SERVER_ROOT="$SCRIPT_DIR/cricket-server"
CLIENT_ROOT="$SCRIPT_DIR/cricket-client"

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    VERSION=$(awk '/^Version:/ {print $2}' "$SERVER_ROOT/DEBIAN/control")
fi

if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "error: dpkg-deb is required (apt install dpkg)" >&2
    exit 1
fi

find_binary() {
    local name="$1"
    for dir in "$REPO_ROOT/docs" "$REPO_ROOT/bin" "$REPO_ROOT/cpu" "$REPO_ROOT/submodules/libtirpc/install/lib"; do
        if [ -f "$dir/$name" ]; then
            echo "$dir/$name"
            return 0
        fi
    done
    return 1
}

copy_binary() {
    local name="$1"
    local dest="$2"
    local src
    src=$(find_binary "$name") || {
        echo "error: cannot locate $name (looked in docs/, bin/, cpu/, submodules/libtirpc/install/lib/)" >&2
        exit 1
    }
    install -D -m 0755 "$src" "$dest"
    echo "  $src -> $dest"
}

echo "==> Staging server binaries"
copy_binary "cricket-rpc-server" "$SERVER_ROOT/usr/local/bin/cricket-rpc-server"
copy_binary "libtirpc.so.3"      "$SERVER_ROOT/usr/local/lib/cricket/libtirpc.so.3"

echo "==> Staging client binaries"
copy_binary "cricket-client.so"  "$CLIENT_ROOT/usr/local/lib/cricket/cricket-client.so"
copy_binary "libtirpc.so.3"      "$CLIENT_ROOT/usr/local/lib/cricket/libtirpc.so.3"

echo "==> Fixing permissions"
chmod 0755 \
    "$SERVER_ROOT/DEBIAN/postinst" \
    "$SERVER_ROOT/DEBIAN/prerm" \
    "$SERVER_ROOT/DEBIAN/postrm" \
    "$CLIENT_ROOT/DEBIAN/postinst" \
    "$CLIENT_ROOT/DEBIAN/prerm" \
    "$CLIENT_ROOT/DEBIAN/postrm" \
    "$CLIENT_ROOT/usr/local/bin/cricket-run"
chmod 0644 \
    "$SERVER_ROOT/DEBIAN/control" \
    "$SERVER_ROOT/DEBIAN/conffiles" \
    "$CLIENT_ROOT/DEBIAN/control" \
    "$CLIENT_ROOT/DEBIAN/conffiles" \
    "$SERVER_ROOT/etc/cricket/server.conf" \
    "$SERVER_ROOT/etc/systemd/system/cricket-rpc.service" \
    "$SERVER_ROOT/etc/profile.d/cricket-server.sh" \
    "$CLIENT_ROOT/etc/cricket/client.conf" \
    "$CLIENT_ROOT/etc/profile.d/cricket-client.sh"

sed -i "s/^Version: .*/Version: $VERSION/" \
    "$SERVER_ROOT/DEBIAN/control" \
    "$CLIENT_ROOT/DEBIAN/control"

mkdir -p "$OUT_DIR"

echo "==> Building cricket-server_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group --build "$SERVER_ROOT" "$OUT_DIR/cricket-server_${VERSION}_amd64.deb"

echo "==> Building cricket-client_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group --build "$CLIENT_ROOT" "$OUT_DIR/cricket-client_${VERSION}_amd64.deb"

echo
echo "Done. Packages in $OUT_DIR:"
ls -1 "$OUT_DIR"/*.deb
