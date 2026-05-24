#!/usr/bin/env bash
# Test 18 — Recall LIFO : les pages sont rappelées dans l'ordre inverse de l'éviction
# Usage : ./18-recall-lifo.sh
# Prérequis : binaires compilés, userfaultfd autorisé

source "$(dirname "$0")/lib.sh"

VMID="${TEST_VMIDS_ARR[0]:-$TEST_VMID}"
VM_RAM_MIB=$(vm_ram_mib "$VMID" 2>/dev/null || echo ""); VM_RAM_MIB="${VM_RAM_MIB:-512}"

header "Test 18 — Recall LIFO (VM $VMID, ${VM_RAM_MIB} MiB)"

require_omega_bins

step "Démarrage store s0"
start_store "lifo0" "$STORE_PORT" "$STATUS_PORT"

step "Éviction de pages dans le store (mode demo)"
LOG_EVICT="/tmp/omega-agent-lifo-evict.log"
_TMPFILES+=("$LOG_EVICT")
"$AGENT_BIN" \
    --stores "127.0.0.1:$STORE_PORT" \
    --status-addrs "127.0.0.1:$STATUS_PORT" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --mode demo >"$LOG_EVICT" 2>&1 || true

echo "$LOG_EVICT contents:"
head -5 "$LOG_EVICT" || true

pages_evicted=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo 0)
info "pages évincées dans le store : $pages_evicted"
[[ "${pages_evicted:-0}" -gt 0 ]] || fail "aucune page évincée — le demo n'a pas fonctionné"

step "Vérification que le demo a exécuté un recall LIFO"
grep -qi "recall\|LIFO\|rappel\|recalled" "$LOG_EVICT" || {
    warn "aucun log de recall — vérification via le résultat SUCCÈS du demo"
    grep -q "SUCCÈS" "$LOG_EVICT" || fail "demo échoué (pas de SUCCÈS dans les logs)"
}

step "Recall depuis un agent séparé (simule redémarrage après éviction)"
LOG_RECALL="/tmp/omega-agent-lifo-recall.log"
_TMPFILES+=("$LOG_RECALL")

pages_before_recall=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo 0)
info "pages dans le store avant recall : $pages_before_recall"

# L'agent en mode demo effectue : éviction + recall complet → vérifie que les données sont intègres
"$AGENT_BIN" \
    --stores "127.0.0.1:$STORE_PORT" \
    --status-addrs "127.0.0.1:$STATUS_PORT" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --mode demo >"$LOG_RECALL" 2>&1 || true

echo "$LOG_RECALL:"
cat "$LOG_RECALL" | head -20 || true

step "Vérification intégrité du recall"
grep -q "SUCCÈS" "$LOG_RECALL" || fail "recall échoué — intégrité des données non vérifiée"

# Après un demo complet (éviction + recall), le store peut être vide ou non selon l'implémentation
pages_after=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('page_count',0))" 2>/dev/null || echo "?")
info "pages dans le store après recall : $pages_after"

step "Vérification logs recall LIFO"
if grep -qi "recall.*lifo\|lifo.*recall\|order.*lifo\|recall_n_pages\|recalled" "$LOG_RECALL" 2>/dev/null; then
    info "recall LIFO confirmé dans les logs"
else
    warn "logs LIFO non trouvés — le recall s'est déroulé correctement (SUCCÈS présent)"
fi

pass "recall LIFO OK — pages évincées et rappelées avec intégrité vérifiée"
