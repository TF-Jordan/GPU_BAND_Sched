#!/usr/bin/env bash
# Test 29 — Soak physique long : pression mixte + surveillance services.
# Usage : ./29-long-run-soak.sh [vmid] [duration_secs]

set -euo pipefail
source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
DURATION="${2:-${OMEGA_SOAK_SECS:-1800}}"
INTERVAL="${OMEGA_SOAK_INTERVAL:-30}"

require_vm_running "$VMID"
VMID="$SELECTED_VMID"
ensure_omega_vcpu_profile "$VMID"

header "Test 29 — Soak long physique (VM $VMID, ${DURATION}s)"

require_cluster

step "Démarrage charge CPU légère"
vm_cpu_stress "$VMID" "$DURATION" || true

step "Surveillance cluster"
t0=$SECONDS
failures=0
while [[ $(elapsed "$t0") -lt "$DURATION" ]]; do
    printf "\n[%4ss/%ss]\n" "$(elapsed "$t0")" "$DURATION"
    for node in "${OMEGA_NODES_ARR[@]}"; do
        active=$(ssh_run "$node" "systemctl is-active omega-daemon 2>/dev/null || true" || echo "ssh-error")
        if [[ "$active" != "active" ]]; then
            warn "$node : omega-daemon status=$active"
            failures=$((failures + 1))
        fi

        status=$(curl -sf --max-time 5 "http://${node}:${STATUS_PORT}/status" 2>/dev/null \
            || curl -sf --max-time 5 "http://${node}:${STATUS_PORT}/api/status" 2>/dev/null \
            || echo "{}")
        printf '  %s daemon=%s status=' "$node" "$active"
        printf '%s' "$status" | python3 -c 'import sys,json
d=json.load(sys.stdin)
print("pages=%s ram=%s ceph=%s" % (d.get("page_count", d.get("pages_stored","?")), d.get("available_mib","?"), d.get("ceph_enabled","?")))' 2>/dev/null || echo "invalid"
    done
    sleep "$INTERVAL"
done

step "État final"
all_stores_status

if [[ "$failures" -eq 0 ]]; then
    pass "Soak terminé sans panne daemon détectée"
else
    fail "Soak terminé avec $failures anomalie(s)"
fi
