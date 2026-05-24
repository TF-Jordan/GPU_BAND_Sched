#!/usr/bin/env bash
# tests/integration/test_full_scenario.sh
#
# Test d'intégration : lance les stores, envoie des pages via le protocole
# brut Python, et vérifie les réponses.
#
# Ce test ne nécessite PAS l'agent (pas de userfaultfd) — il valide uniquement
# la couche réseau store.
#
# Usage :
#   bash tests/integration/test_full_scenario.sh
#   bash tests/integration/test_full_scenario.sh --keep-stores

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
KEEP_STORES=0

[[ "${1:-}" == "--keep-stores" ]] && KEEP_STORES=1

PASS=0
FAIL=0
PIDS=()

info()    { echo -e "\033[36m[TEST]\033[0m  $*"; }
ok()      { echo -e "\033[32m[PASS]\033[0m  $*"; PASS=$((PASS + 1)); }
fail()    { echo -e "\033[31m[FAIL]\033[0m  $*"; FAIL=$((FAIL + 1)); }

cleanup() {
    if [[ $KEEP_STORES -eq 0 ]]; then
        for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
        wait 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

STORE_BIN="${ROOT_DIR}/target/release/node-bc-store"
[[ -x "$STORE_BIN" ]] || { echo "[ERREUR] node-bc-store non compilé"; exit 1; }

# ─── Démarrage des stores ─────────────────────────────────────────────────────

info "Démarrage des stores de test..."
"$STORE_BIN" --listen 127.0.0.1:19100 --node-id test-b &> /tmp/store-b-test.log &
PIDS+=($!)
"$STORE_BIN" --listen 127.0.0.1:19101 --node-id test-c &> /tmp/store-c-test.log &
PIDS+=($!)
sleep 0.8

# ─── Tests via le client Python ───────────────────────────────────────────────

PYTHON=$(command -v python3 || command -v python || echo "")
[[ -z "$PYTHON" ]] && { echo "[ERREUR] python3 non trouvé"; exit 1; }

run_test() {
    local name="$1"
    local script="$2"
    if echo "$script" | "$PYTHON" - 2>&1; then
        ok "$name"
    else
        fail "$name"
    fi
}

# Test 1 : PING
run_test "PING store-B" "
import sys
sys.path.insert(0, '${ROOT_DIR}/controller')
from controller.store_client import StoreClient
c = StoreClient('127.0.0.1', 19100, timeout=2.0)
c.connect()
assert c.ping(), 'PING échoué'
c.disconnect()
print('PONG reçu')
"

# Test 2 : PING store-C
run_test "PING store-C" "
import sys
sys.path.insert(0, '${ROOT_DIR}/controller')
from controller.store_client import StoreClient
c = StoreClient('127.0.0.1', 19101, timeout=2.0)
c.connect()
assert c.ping(), 'PING échoué'
c.disconnect()
print('PONG reçu')
"

# Test 3 : PUT + GET page
run_test "PUT puis GET page (intégrité)" "
import sys, struct, socket
sys.path.insert(0, '${ROOT_DIR}/controller')
from controller.store_client import StoreClient, _build_frame, _parse_header, Opcode, PAGE_SIZE, HEADER_SIZE

PAGE = bytes(range(256)) * 16  # 4096 octets avec pattern connu

def send_recv(sock, frame):
    sock.sendall(frame)
    hdr = b''
    while len(hdr) < HEADER_SIZE:
        hdr += sock.recv(HEADER_SIZE - len(hdr))
    opcode, flags, vm_id, page_id, plen = _parse_header(hdr)
    payload = b''
    while len(payload) < plen:
        payload += sock.recv(plen - len(payload))
    return opcode, vm_id, page_id, payload

sock = socket.create_connection(('127.0.0.1', 19100), timeout=3.0)
sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

# PUT
frame = _build_frame(Opcode.PUT_PAGE, vm_id=42, page_id=999, payload=PAGE)
opcode, vm_id, page_id, payload = send_recv(sock, frame)
assert opcode == Opcode.OK, f'PUT attendait OK, reçu 0x{opcode:02x}'
assert vm_id == 42 and page_id == 999

# GET
frame = _build_frame(Opcode.GET_PAGE, vm_id=42, page_id=999)
opcode, vm_id, page_id, payload = send_recv(sock, frame)
assert opcode == Opcode.OK, f'GET attendait OK, reçu 0x{opcode:02x}'
assert len(payload) == PAGE_SIZE, f'payload size {len(payload)}'
assert payload == PAGE, 'données corrompues !'

sock.close()
print('intégrité OK')
"

# Test 4 : GET page inexistante → NOT_FOUND
run_test "GET page inexistante → NOT_FOUND" "
import sys, socket
sys.path.insert(0, '${ROOT_DIR}/controller')
from controller.store_client import _build_frame, _parse_header, Opcode, HEADER_SIZE

def send_recv(sock, frame):
    sock.sendall(frame)
    hdr = b''
    while len(hdr) < HEADER_SIZE:
        hdr += sock.recv(HEADER_SIZE - len(hdr))
    opcode, _, vm_id, page_id, plen = _parse_header(hdr)
    payload = sock.recv(plen) if plen > 0 else b''
    return opcode

sock = socket.create_connection(('127.0.0.1', 19100), timeout=3.0)
opcode = send_recv(sock, _build_frame(Opcode.GET_PAGE, vm_id=99, page_id=9999))
assert opcode == Opcode.NOT_FOUND, f'attendait NOT_FOUND (0x{Opcode.NOT_FOUND:02x}), reçu 0x{opcode:02x}'
sock.close()
print('NOT_FOUND reçu correctement')
"

# Test 5 : STATS
run_test "STATS retourne JSON valide" "
import sys, json
sys.path.insert(0, '${ROOT_DIR}/controller')
from controller.store_client import StoreClient
c = StoreClient('127.0.0.1', 19100, timeout=2.0)
c.connect()
stats = c.get_stats()
assert stats is not None, 'stats est None'
assert 'pages_stored' in stats, f'clé manquante dans {stats}'
c.disconnect()
print(f'stats ok: {stats}')
"

# Test 6 : DELETE page
run_test "DELETE page existante" "
import sys, socket
sys.path.insert(0, '${ROOT_DIR}/controller')
from controller.store_client import _build_frame, _parse_header, Opcode, HEADER_SIZE

PAGE = b'X' * 4096

def send_recv(sock, frame):
    sock.sendall(frame)
    hdr = b''
    while len(hdr) < HEADER_SIZE:
        hdr += sock.recv(HEADER_SIZE - len(hdr))
    opcode, _, vm_id, page_id, plen = _parse_header(hdr)
    if plen > 0: sock.recv(plen)
    return opcode

sock = socket.create_connection(('127.0.0.1', 19100), timeout=3.0)
# PUT
assert send_recv(sock, _build_frame(Opcode.PUT_PAGE, vm_id=1, page_id=1, payload=PAGE)) == Opcode.OK
# DELETE
assert send_recv(sock, _build_frame(Opcode.DELETE_PAGE, vm_id=1, page_id=1)) == Opcode.OK
# GET après DELETE → NOT_FOUND
assert send_recv(sock, _build_frame(Opcode.GET_PAGE, vm_id=1, page_id=1)) == Opcode.NOT_FOUND
sock.close()
print('DELETE + GET→NOT_FOUND ok')
"

# ─── Résultat global ──────────────────────────────────────────────────────────

echo
echo "═══════════════════════════════════════"
echo "  Résultats : ${PASS} passés, ${FAIL} échoués"
echo "═══════════════════════════════════════"

[[ $FAIL -eq 0 ]] && exit 0 || exit 1
