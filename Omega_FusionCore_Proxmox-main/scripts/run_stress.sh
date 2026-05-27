#!/usr/bin/env bash
# run_stress.sh — Lance stress-ng pour simuler une pression mémoire.
#
# Utilisation :
#   ./scripts/run_stress.sh [--vm N] [--vm-bytes SIZE] [--timeout SECS]
#
# Exemples :
#   ./scripts/run_stress.sh                         # défauts : 2 workers, 80% RAM, 60s
#   ./scripts/run_stress.sh --vm 4 --timeout 120
#   ./scripts/run_stress.sh --vm-bytes 4G --timeout 30

set -euo pipefail

die()  { echo "[ERREUR] $*" >&2; exit 1; }
info() { echo "[INFO]  $*"; }

command -v stress-ng &>/dev/null || die "stress-ng non installé (apt install stress-ng)"

# ─── Paramètres ───────────────────────────────────────────────────────────────

VM_WORKERS="${STRESS_VM:-2}"
VM_BYTES="${STRESS_VM_BYTES:-80%}"
TIMEOUT="${STRESS_TIMEOUT:-60}"
VM_HANG="${STRESS_VM_HANG:-0}"        # 0 = pas de pause entre accès

# Parsing des arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --vm)       VM_WORKERS="$2"; shift 2 ;;
        --vm-bytes) VM_BYTES="$2";   shift 2 ;;
        --timeout)  TIMEOUT="$2";    shift 2 ;;
        --vm-hang)  VM_HANG="$2";    shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--vm N] [--vm-bytes SIZE] [--timeout SECS]"
            exit 0 ;;
        *) die "argument inconnu : $1" ;;
    esac
done

# ─── Baseline mémoire avant ───────────────────────────────────────────────────

info "=== Baseline mémoire AVANT stress ==="
free -h
echo

MEM_AVAIL_BEFORE=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
info "Mémoire disponible avant : ${MEM_AVAIL_BEFORE} Ko"

# ─── Lancement stress-ng ──────────────────────────────────────────────────────

info "Démarrage stress-ng : ${VM_WORKERS} workers × ${VM_BYTES} RAM pendant ${TIMEOUT}s"
info "PID : $$"
echo

# --vm-method all : alterne les patterns d'accès (random, seq, stride, etc.)
# --metrics-brief : affichage résumé à la fin
stress-ng \
    --vm            "$VM_WORKERS"  \
    --vm-bytes      "$VM_BYTES"    \
    --vm-method     all            \
    --vm-hang       "$VM_HANG"     \
    --timeout       "${TIMEOUT}s"  \
    --metrics-brief                \
    --log-brief

echo
info "=== Baseline mémoire APRÈS stress ==="
free -h
MEM_AVAIL_AFTER=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
info "Mémoire disponible après : ${MEM_AVAIL_AFTER} Ko"

DELTA=$((MEM_AVAIL_BEFORE - MEM_AVAIL_AFTER))
info "Delta disponible : ${DELTA} Ko (positif = plus utilisé qu'avant)"
