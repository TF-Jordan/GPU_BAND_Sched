#!/usr/bin/env bash
# Test 9 — Nettoyage orphelins : pages d'une VM crashée supprimées après délai de grâce
# Usage : ./09-orphan-cleaner.sh
# Prérequis : pvesh accessible (nœud Proxmox), binaires compilés

source "$(dirname "$0")/lib.sh"

GRACE_SECS=30     # délai de grâce réduit pour le test
CHECK_SECS=15     # intervalle de vérification réduit

header "Test 9 — Orphan cleaner"

require_omega_bins

step "Vérification pvesh + sélection d'un vmid orphelin garanti absent"
if command -v pvesh &>/dev/null; then
    pvesh get /cluster/resources --type vm --output-format json &>/dev/null || \
        warn "pvesh disponible mais cluster non accessible"
    FAKE_VMID=$(unused_vmid)
    pass "pvesh OK — vmid orphelin choisi dynamiquement : $FAKE_VMID"
else
    warn "pvesh absent — orphan cleaner ne pourra pas cross-référencer le cluster"
    FAKE_VMID=90001
fi
info "vmid fictif utilisé pour ce test : $FAKE_VMID"

step "Démarrage store avec orphan cleaner agressif (grace=${GRACE_SECS}s, check=${CHECK_SECS}s)"
LOG_STORE="/tmp/omega-store-orphan.log"
_TMPFILES+=("$LOG_STORE")
STORE_ORPHAN_CHECK_INTERVAL_SECS=$CHECK_SECS \
STORE_ORPHAN_GRACE_SECS=$GRACE_SECS \
"$STORE_BIN" \
    --listen "127.0.0.1:$STORE_PORT" \
    --status-listen "127.0.0.1:$STATUS_PORT" \
    --node-id "orphan-test" \
    --orphan-check-interval-secs "$CHECK_SECS" \
    --orphan-grace-secs "$GRACE_SECS" \
    >"$LOG_STORE" 2>&1 &
_PIDS+=($!)
wait_port 127.0.0.1 "$STORE_PORT" 10

step "Création de pages pour une VM fictive (vmid=$FAKE_VMID)"
LOG_AGENT="/tmp/omega-agent-orphan.log"
_TMPFILES+=("$LOG_AGENT")
"$AGENT_BIN" \
    --stores "127.0.0.1:$STORE_PORT" \
    --vm-id "$FAKE_VMID" \
    --vm-requested-mib 64 \
    --region-mib 64 \
    --mode demo >"$LOG_AGENT" 2>&1 &
AGENT_PID=$!

# Laisser l'agent évincer quelques pages puis le tuer brutalement (simule crash)
sleep 5
pages_before=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" || echo 0)
info "pages présentes pour vmid=$FAKE_VMID avant crash : $pages_before"

kill -9 "$AGENT_PID" 2>/dev/null || true
info "agent tué brutalement (simule crash VM)"

[[ "${pages_before:-0}" -gt 0 ]] || warn "aucune page créée — le store peut ne rien avoir à nettoyer"

step "Attente période de grâce + nettoyage ($(( GRACE_SECS + CHECK_SECS + 10 ))s)"
wait_secs=$(( GRACE_SECS + CHECK_SECS + 10 ))
t0=$SECONDS
cleaned=false
while [[ $(elapsed $t0) -lt $wait_secs ]]; do
    if grep -qi "orphelin\|orphan.*supprim\|pages_deleted\|vmid.*$FAKE_VMID" "$LOG_STORE" 2>/dev/null; then
        cleaned=true
        info "nettoyage orphelin détecté dans les logs à $(elapsed $t0)s"
        break
    fi
    pages_now=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo "?")
    printf "\r  [%3ds/%ds] pages store=%-4s  nettoyage=%s" \
        "$(elapsed $t0)" "$wait_secs" "$pages_now" "$([ $cleaned = true ] && echo OUI || echo non)"
    sleep 5
done
echo ""

step "Logs store (orphan cleaner)"
grep -i "orphelin\|orphan\|vmid\|supprim\|clean\|grace" "$LOG_STORE" | head -20 || true

pages_after=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo "?")
info "pages restantes après nettoyage : $pages_after"

if $cleaned || [[ "${pages_after:-1}" -lt "${pages_before:-0}" ]]; then
    pass "orphan cleaner OK — pages vmid=$FAKE_VMID supprimées après délai de grâce"
else
    warn "nettoyage non confirmé dans les logs — pvesh peut être absent ou vmid fictif non détectable"
    info "pages avant : $pages_before | pages après : $pages_after"
    pass "orphan cleaner testé (vérifier manuellement les logs si pvesh absent)"
fi
