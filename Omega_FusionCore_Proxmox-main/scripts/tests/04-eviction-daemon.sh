#!/usr/bin/env bash
# Test 4 — Éviction continue en mode daemon (pression simulée)
# Usage : ./04-eviction-daemon.sh [durée_secondes]
# Prérequis : binaires compilés, userfaultfd autorisé

source "$(dirname "$0")/lib.sh"

DUREE="${1:-20}"
VMID="${TEST_VMIDS_ARR[0]:-$TEST_VMID}"
VM_RAM_MIB=$(vm_ram_mib "$VMID" 2>/dev/null || echo ""); VM_RAM_MIB="${VM_RAM_MIB:-512}"

header "Test 4 — Éviction daemon (VM $VMID, ${VM_RAM_MIB} MiB, ${DUREE}s)"

require_omega_bins

step "Démarrage store"
start_store "evict" "$STORE_PORT" "$STATUS_PORT"

step "Démarrage agent daemon (éviction forcée toutes les 2s)"
LOG_AGENT="/tmp/omega-agent-evict.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "127.0.0.1:$STORE_PORT" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --eviction-threshold-mib 999999 \
    --eviction-batch-size 16 \
    --eviction-interval-secs 2 \
    --recall-threshold-mib 0 \
    --metrics-listen "127.0.0.1:$METRICS_PORT" \
    --mode daemon >"$LOG_AGENT" 2>&1 &
_PIDS+=($!)
AGENT_PID=$!

wait_http "http://127.0.0.1:$METRICS_PORT/metrics" 15

step "Observation des métriques pendant ${DUREE}s"
t0=$SECONDS
evicted_start=$(curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pages_evicted',0))" 2>/dev/null || echo 0)

while [[ $(elapsed $t0) -lt $DUREE ]]; do
    evicted=$(curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pages_evicted',0))" 2>/dev/null || echo 0)
    pages_store=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" | python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo "?")
    printf "\r  [%3ds] pages_evicted=%-6s  store.pages=%-6s" "$(elapsed $t0)" "$evicted" "$pages_store"
    sleep 2
done
echo ""

evicted_end=$(curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pages_evicted',0))" 2>/dev/null || echo 0)
delta=$(( ${evicted_end%.*} - ${evicted_start%.*} ))
info "pages évincées pendant le test : $delta"

step "Vérification métriques"
[[ "$delta" -gt 0 ]] || fail "aucune page évincée en ${DUREE}s — éviction non fonctionnelle"

recalls=$(curl -sf "http://127.0.0.1:$METRICS_PORT/metrics" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pages_recalled',0))" 2>/dev/null || echo 0)
info "pages rappelées (attendu ~0) : $recalls"

step "Arrêt agent"
kill "$AGENT_PID" 2>/dev/null || true
sleep 1

step "Logs agent (dernières lignes)"
tail -10 "$LOG_AGENT"

pass "éviction daemon OK — $delta pages évincées en ${DUREE}s"
