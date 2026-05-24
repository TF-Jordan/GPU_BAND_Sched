#!/usr/bin/env bash
# Test 1 — Smoke test local : cycle éviction → fault → recall (sans Proxmox)
# Usage : ./01-smoke-test.sh
# Prérequis : binaires compilés, userfaultfd autorisé

source "$(dirname "$0")/lib.sh"

VMID="${TEST_VMIDS_ARR[0]:-$TEST_VMID}"
VM_RAM_MIB=$(vm_ram_mib "$VMID" 2>/dev/null || echo ""); VM_RAM_MIB="${VM_RAM_MIB:-512}"

header "Test 1 — Smoke test (VM $VMID, ${VM_RAM_MIB} MiB)"

require_omega_bins

step "Vérification userfaultfd"
uffd=$(sysctl -n vm.unprivileged_userfaultfd 2>/dev/null || echo "0")
if [[ "$uffd" != "1" ]]; then
    warn "vm.unprivileged_userfaultfd=0 — tentative en root ou activation"
    if [[ $EUID -ne 0 ]]; then
        sysctl -w vm.unprivileged_userfaultfd=1 2>/dev/null || \
            fail "userfaultfd non autorisé — lancer en root ou: sysctl -w vm.unprivileged_userfaultfd=1"
    fi
fi
pass "userfaultfd OK"

step "Démarrage du store"
start_store "smoke" "$STORE_PORT" "$STATUS_PORT"

step "Exécution agent en mode demo"
LOG="/tmp/omega-agent-smoke.log"
_TMPFILES+=("$LOG")
t0=$SECONDS

output=$("$AGENT_BIN" \
    --stores "127.0.0.1:$STORE_PORT" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --mode demo 2>&1) || true

echo "$output"

step "Vérification résultats"
echo "$output" | grep -q "étape 1/5" || fail "étape 1 manquante dans la sortie"
echo "$output" | grep -q "étape 5/5" || fail "étape 5 manquante — le scénario n'est pas allé au bout"
echo "$output" | grep -q "integrity_ok=true" || fail "intégrité des données non confirmée"
echo "$output" | grep -q "SUCCÈS" || fail "message de succès absent"

errors=$(echo "$output" | grep -oP 'errors=\K[0-9]+' | head -1)
[[ "${errors:-0}" -eq 0 ]] || fail "$errors erreurs d'intégrité détectées"

step "Vérification status store"
wait_http "http://127.0.0.1:$STATUS_PORT/status" 5
pages=$(curl -sf "http://127.0.0.1:$STATUS_PORT/status" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('page_count',0))")
info "pages stockées dans le store : $pages"

pass "smoke test OK ($(elapsed $t0)s) — integrity_ok=true, errors=0"
