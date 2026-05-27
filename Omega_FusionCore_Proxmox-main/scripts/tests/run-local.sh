#!/usr/bin/env bash
# Lance tous les tests locaux (sans Proxmox) dans l'ordre
# Usage : ./run-local.sh [--skip 03,04]
#
# Variables d'environnement :
#   OMEGA_SKIP=03,04     — tests à ignorer

source "$(dirname "$0")/lib.sh"

SKIP_LIST="${OMEGA_SKIP:-}"
for arg in "$@"; do
    case $arg in --skip=*) SKIP_LIST="${arg#--skip=}" ;; esac
done

should_skip() { [[ ",$SKIP_LIST," == *",$1,"* ]]; }

TESTS_DIR="$(dirname "$0")"
PASS=0; FAIL=0; SKIP=0
RESULTS=()

run_test() {
    local num="$1" name="$2" script="$3"; shift 3
    if should_skip "$num"; then
        warn "Test $num ($name) — ignoré"
        RESULTS+=("SKIP  $num $name")
        ((SKIP++)) || true
        return
    fi
    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}  Test $num — $name${RESET}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    t0=$SECONDS
    if bash "$script" "$@"; then
        RESULTS+=("${GREEN}PASS${RESET}  $num $name ($(( SECONDS - t0 ))s)")
        ((PASS++)) || true
    else
        RESULTS+=("${RED}FAIL${RESET}  $num $name ($(( SECONDS - t0 ))s)")
        ((FAIL++)) || true
    fi
    sleep 1
}

header "Tests locaux — omega-remote-paging"
info "Binaires : $BIN_DIR"
info "Skip : ${SKIP_LIST:-aucun}"

# ── Tests unitaires ───────────────────────────────────────────────────────────
run_test "00" "Tests unitaires"              "$TESTS_DIR/00-unit-tests.sh"

# ── Tests store + agent locaux ────────────────────────────────────────────────
run_test "01" "Smoke test"                   "$TESTS_DIR/01-smoke-test.sh"
run_test "02" "Réplication 2 stores"         "$TESTS_DIR/02-replication.sh"
run_test "03" "Failover store"               "$TESTS_DIR/03-failover.sh"
run_test "04" "Éviction daemon"              "$TESTS_DIR/04-eviction-daemon.sh" 20

# ── Tests charge ─────────────────────────────────────────────────────────────
run_test "10" "Multi-VM 3 agents"            "$TESTS_DIR/10-multi-vm.sh"

# ── Tests mixtes (locaux — pas besoin de cluster) ────────────────────────────
run_test "M6" "Rafale démarrages simultanés" "$TESTS_DIR/16-mixed-burst-starts.sh" 6 0

# ── Résumé ────────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
echo -e "${BOLD}  Résumé — tests locaux${RESET}"
echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
for r in "${RESULTS[@]}"; do echo -e "  $r"; done
echo ""
echo -e "  ${GREEN}PASS${RESET}: $PASS  ${RED}FAIL${RESET}: $FAIL  ${YELLOW}SKIP${RESET}: $SKIP"

[[ $FAIL -eq 0 ]] && \
    echo -e "\n${GREEN}${BOLD}  ✓ Tous les tests locaux passent${RESET}" || \
    { echo -e "\n${RED}${BOLD}  ✗ $FAIL test(s) échoué(s)${RESET}"; exit 1; }
