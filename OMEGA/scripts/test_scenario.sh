#!/usr/bin/env bash
# test_scenario.sh — Scénario de test minimal de la V1 (3 nœuds simulés localement).
#
# Ce script permet de valider le prototype sur une seule machine en simulant
# les 3 nœuds dans des processus distincts sur localhost.
#
# Architecture simulée :
#   - node-bc-store sur :${OMEGA_STORE_PORT:-9100} (simule nœud B)
#   - node-bc-store sur :${OMEGA_STORE_PORT:-9100}+1 (simule nœud C)
#   - node-a-agent en mode demo
#
# Usage :
#   ./scripts/test_scenario.sh [--build] [--verbose]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

BUILD=0
VERBOSE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --build)   BUILD=1;   shift ;;
        --verbose) VERBOSE=1; shift ;;
        -h|--help)
            echo "Usage: $0 [--build] [--verbose]"
            exit 0 ;;
        *) echo "Argument inconnu : $1"; exit 1 ;;
    esac
done

info()    { echo -e "\033[32m[INFO]\033[0m  $*"; }
warn()    { echo -e "\033[33m[WARN]\033[0m  $*"; }
success() { echo -e "\033[32m[OK]\033[0m    $*"; }
fail()    { echo -e "\033[31m[FAIL]\033[0m  $*"; exit 1; }

# ─── Pids des processus lancés ────────────────────────────────────────────────

PIDS=()

cleanup() {
    info "Nettoyage des processus..."
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    info "Nettoyage terminé"
}
trap cleanup EXIT INT TERM

# ─── Compilation ──────────────────────────────────────────────────────────────

if [[ $BUILD -eq 1 ]]; then
    info "Compilation du workspace Rust..."
    cd "$ROOT_DIR"
    cargo build --release 2>&1 | (if [[ $VERBOSE -eq 1 ]]; then cat; else grep -E "^(error|warning:|Compiling|Finished)"; fi)
    success "Compilation terminée"
fi

STORE_BIN="${ROOT_DIR}/target/release/node-bc-store"
AGENT_BIN="${ROOT_DIR}/target/release/node-a-agent"
STORE_PORT="${OMEGA_STORE_PORT:-9100}"
STATUS_PORT="${OMEGA_STATUS_PORT:-9200}"
VMID="${OMEGA_TEST_VMID:-1}"
REGION_MIB="${OMEGA_TEST_REGION_MIB:-16}"

[[ -x "$STORE_BIN" ]] || fail "node-bc-store non compilé — lancez avec --build ou 'make build'"
[[ -x "$AGENT_BIN" ]] || fail "node-a-agent non compilé — lancez avec --build ou 'make build'"

# ─── Vérification userfaultfd ─────────────────────────────────────────────────

info "Vérification de userfaultfd..."
UFFD_UNPRIV=$(cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null || echo "1")
if [[ "$UFFD_UNPRIV" == "0" ]]; then
    warn "unprivileged_userfaultfd=0 — l'agent doit tourner en root"
    warn "Pour autoriser sans root : echo 1 | sudo tee /proc/sys/vm/unprivileged_userfaultfd"
fi

# ─── Lancement des stores ─────────────────────────────────────────────────────

info "Démarrage node-bc-store sur :${STORE_PORT} (simule nœud B)..."
LOG_LEVEL=info "$STORE_BIN" \
    --listen "127.0.0.1:${STORE_PORT}" \
    --status-listen "127.0.0.1:${STATUS_PORT}" \
    --node-id node-b &
PIDS+=($!)
STORE_B_PID=${!}

info "Démarrage node-bc-store sur :$((STORE_PORT + 1)) (simule nœud C)..."
LOG_LEVEL=info "$STORE_BIN" \
    --listen "127.0.0.1:$((STORE_PORT + 1))" \
    --status-listen "127.0.0.1:$((STATUS_PORT + 1))" \
    --node-id node-c &
PIDS+=($!)
STORE_C_PID=${!}

# Attente que les stores soient prêts
info "Attente du démarrage des stores..."
sleep 1

# Vérification de la connectivité avec nc
for port in "$STORE_PORT" "$((STORE_PORT + 1))"; do
    if nc -z -w2 127.0.0.1 "$port" 2>/dev/null; then
        success "Store :${port} accessible"
    else
        fail "Store :${port} ne répond pas"
    fi
done

# ─── Baseline mémoire ─────────────────────────────────────────────────────────

info "Baseline mémoire avant le test..."
bash "${SCRIPT_DIR}/collect_baseline.sh" --output /tmp/omega-baselines --tag "pre-test" 2>/dev/null || true

# ─── Lancement de l'agent en mode demo ───────────────────────────────────────

info "Démarrage node-a-agent en mode demo..."
info "(les page faults vont être interceptés — attendez le résultat)"
echo

RUST_LOG="${RUST_LOG:-info}" \
    "$AGENT_BIN" \
    --stores "127.0.0.1:${STORE_PORT},127.0.0.1:$((STORE_PORT + 1))" \
    --status-addrs "127.0.0.1:${STATUS_PORT},127.0.0.1:$((STATUS_PORT + 1))" \
    --vm-id "$VMID" \
    --vm-requested-mib "$REGION_MIB" \
    --region-mib "$REGION_MIB" \
    --mode demo

AGENT_EXIT=$?

echo
if [[ $AGENT_EXIT -eq 0 ]]; then
    success "=== SCÉNARIO DE TEST RÉUSSI === (exit 0)"
else
    fail "=== SCÉNARIO DE TEST ÉCHOUÉ === (exit ${AGENT_EXIT})"
fi

# ─── Baseline post-test ───────────────────────────────────────────────────────

info "Baseline mémoire après le test..."
bash "${SCRIPT_DIR}/collect_baseline.sh" --output /tmp/omega-baselines --tag "post-test" 2>/dev/null || true

info "Baselines sauvegardées dans /tmp/omega-baselines/"
ls /tmp/omega-baselines/ 2>/dev/null || true
