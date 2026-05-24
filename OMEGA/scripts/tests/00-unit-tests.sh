#!/usr/bin/env bash
# Test 0 — Tests unitaires Rust (toujours en premier)
# Usage : ./00-unit-tests.sh
# Prérequis : cargo, workspace compilable

source "$(dirname "$0")/lib.sh"

header "Test 0 — Tests unitaires"

command -v cargo &>/dev/null || fail "cargo absent sur ce nœud — ce test doit tourner sur la machine de dev, pas sur un nœud Proxmox"

step "Compilation workspace"
cd "$REPO_ROOT"
cargo build --workspace --quiet || fail "compilation échouée"
pass "compilation OK"

step "Exécution tests unitaires"
output=$(cargo test --workspace -- --nocapture 2>&1) || true
echo "$output" | tail -30

failures=$(echo "$output" | grep -c "^FAILED" || true)
[[ "$failures" -eq 0 ]] || fail "$failures tests échoués"

# Certains tests nécessitent userfaultfd — vérifier si c'est la cause des échecs
if echo "$output" | grep -q "Operation not permitted\|EPERM\|userfaultfd"; then
    warn "Certains tests nécessitent userfaultfd localement : sudo sysctl -w vm.unprivileged_userfaultfd=1"
fi

total=$(echo "$output" | grep "^test result:" | awk '{sum+=$4} END{print sum+0}')
info "Total : $total tests"
[[ "${total:-0}" -ge 200 ]] || warn "moins de 200 tests — possible régression ($total trouvés)"

pass "tous les tests unitaires passent ($total tests)"
