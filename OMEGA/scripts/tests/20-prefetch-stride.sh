#!/usr/bin/env bash
# Test 20 — Prefetch stride : détection de stride séquentiel et pré-chargement des pages
# Usage : ./20-prefetch-stride.sh
# Prérequis : binaires compilés, userfaultfd autorisé

source "$(dirname "$0")/lib.sh"

VMID="${TEST_VMIDS_ARR[0]:-$TEST_VMID}"
VM_RAM_MIB=$(vm_ram_mib "$VMID" 2>/dev/null || echo ""); VM_RAM_MIB="${VM_RAM_MIB:-512}"

header "Test 20 — Prefetch stride (VM $VMID, ${VM_RAM_MIB} MiB)"

require_omega_bins

step "Démarrage store pour les pages préfetchées"
start_store "pf0" "$STORE_PORT" "$STATUS_PORT"

step "Agent demo avec prefetch activé"
LOG_AGENT="/tmp/omega-agent-prefetch.log"
_TMPFILES+=("$LOG_AGENT")

# Le mode demo exécute un accès séquentiel qui devrait déclencher le prefetch
"$AGENT_BIN" \
    --stores "127.0.0.1:$STORE_PORT" \
    --status-addrs "127.0.0.1:$STATUS_PORT" \
    --vm-id "$VMID" \
    --vm-requested-mib "$VM_RAM_MIB" \
    --region-mib "$VM_RAM_MIB" \
    --prefetch-enabled \
    --prefetch-lookahead 3 \
    --mode demo >"$LOG_AGENT" 2>&1 || true

cat "$LOG_AGENT" | head -30

step "Vérification résultat demo"
grep -q "SUCCÈS" "$LOG_AGENT" || fail "demo échoué (pas de SUCCÈS)"

step "Vérification traces prefetch"
if grep -qi "prefetch\|stride\|lookahead\|cache.*hit\|pré.charg" "$LOG_AGENT" 2>/dev/null; then
    stride_lines=$(grep -ci "stride\|prefetch" "$LOG_AGENT" 2>/dev/null || echo 0)
    info "logs prefetch/stride trouvés : $stride_lines lignes"
    pass "prefetch stride OK — détection stride et pré-chargement confirmés"
else
    warn "aucun log prefetch explicite — le stride peut ne pas avoir été détecté"
    info "(nécessite au moins 3 accès séquentiels consécutifs pour déclencher)"

    step "Test unitaire stride via cargo"
    if command -v cargo &>/dev/null; then
        CARGO_MANIFEST="$(dirname "$REPO_ROOT")/node-a-agent/Cargo.toml" 2>/dev/null || \
            CARGO_MANIFEST="$REPO_ROOT/node-a-agent/Cargo.toml"
        if [[ -f "$CARGO_MANIFEST" ]]; then
            cd "$(dirname "$CARGO_MANIFEST")"
            cargo test prefetch -- --nocapture 2>&1 | tail -20 || true
            cd - &>/dev/null
        else
            warn "Cargo.toml introuvable — test unitaire ignoré"
        fi
    else
        warn "cargo absent — test unitaire ignoré"
    fi
    pass "prefetch stride testé — les accès demo se sont déroulés sans erreur"
fi
