#!/usr/bin/env bash
# collect_baseline.sh — Collecte une baseline mémoire complète et la sauvegarde.
#
# Collecte :
#   - /proc/meminfo
#   - /proc/pressure/memory (PSI)
#   - /proc/vmstat (statistiques VM du kernel)
#   - free -h
#   - Résumé JSON de sortie
#
# Usage :
#   ./scripts/collect_baseline.sh [--output DIR] [--tag label]
#
# Sortie :
#   baseline-<timestamp>-<tag>/
#     meminfo.txt
#     pressure.txt
#     vmstat.txt
#     free.txt
#     summary.json

set -euo pipefail

info() { echo "[INFO]  $*"; }

# ─── Arguments ────────────────────────────────────────────────────────────────

OUTPUT_DIR="${BASELINE_DIR:-./baselines}"
TAG="${BASELINE_TAG:-default}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) OUTPUT_DIR="$2"; shift 2 ;;
        --tag)    TAG="$2";        shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--output DIR] [--tag LABEL]"
            exit 0 ;;
        *) echo "Argument inconnu : $1"; exit 1 ;;
    esac
done

# ─── Création du répertoire de sortie ────────────────────────────────────────

TS=$(date +%Y%m%d-%H%M%S)
DEST="${OUTPUT_DIR}/${TS}-${TAG}"
mkdir -p "$DEST"
info "Répertoire de sortie : $DEST"

# ─── Collecte ─────────────────────────────────────────────────────────────────

HOSTNAME=$(hostname)
TS_ISO=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

info "Collecte /proc/meminfo..."
cp /proc/meminfo "${DEST}/meminfo.txt"

info "Collecte /proc/pressure/memory (PSI)..."
if [[ -f /proc/pressure/memory ]]; then
    cp /proc/pressure/memory "${DEST}/pressure.txt"
else
    echo "PSI non disponible (kernel < 4.20 ou CONFIG_PSI=n)" > "${DEST}/pressure.txt"
fi

info "Collecte /proc/vmstat..."
cp /proc/vmstat "${DEST}/vmstat.txt"

info "Collecte free..."
free -k > "${DEST}/free.txt"
free -h >> "${DEST}/free.txt"

info "Collecte /proc/buddyinfo (fragmentation mémoire)..."
cp /proc/buddyinfo "${DEST}/buddyinfo.txt" 2>/dev/null || true

info "Collecte /proc/slabinfo..."
cp /proc/slabinfo "${DEST}/slabinfo.txt" 2>/dev/null || true

# ─── Extraction valeurs clés pour le JSON de résumé ──────────────────────────

extract_meminfo() {
    awk -v key="$1" '$1 == key":" {print $2}' "${DEST}/meminfo.txt"
}

MEM_TOTAL=$(extract_meminfo MemTotal)
MEM_FREE=$(extract_meminfo MemFree)
MEM_AVAILABLE=$(extract_meminfo MemAvailable)
MEM_CACHED=$(extract_meminfo Cached)
SWAP_TOTAL=$(extract_meminfo SwapTotal)
SWAP_FREE=$(extract_meminfo SwapFree)

MEM_USED=$((MEM_TOTAL - MEM_AVAILABLE))
MEM_USAGE_PCT=0
[[ $MEM_TOTAL -gt 0 ]] && MEM_USAGE_PCT=$(( MEM_USED * 100 / MEM_TOTAL ))

SWAP_USED=$((SWAP_TOTAL - SWAP_FREE))
SWAP_USAGE_PCT=0
[[ $SWAP_TOTAL -gt 0 ]] && SWAP_USAGE_PCT=$(( SWAP_USED * 100 / SWAP_TOTAL ))

PSI_SOME_AVG10="null"
if [[ -f /proc/pressure/memory ]]; then
    PSI_SOME_AVG10=$(awk '/^some/ {for(i=1;i<=NF;i++) if($i~/^avg10=/) {sub(/avg10=/,"",$i); print $i}}' /proc/pressure/memory)
fi

# ─── Écriture du JSON de résumé ───────────────────────────────────────────────

cat > "${DEST}/summary.json" <<EOF
{
  "timestamp":       "${TS_ISO}",
  "hostname":        "${HOSTNAME}",
  "tag":             "${TAG}",
  "mem_total_kb":    ${MEM_TOTAL},
  "mem_available_kb":${MEM_AVAILABLE},
  "mem_used_kb":     ${MEM_USED},
  "mem_usage_pct":   ${MEM_USAGE_PCT},
  "mem_cached_kb":   ${MEM_CACHED},
  "swap_total_kb":   ${SWAP_TOTAL},
  "swap_used_kb":    ${SWAP_USED},
  "swap_usage_pct":  ${SWAP_USAGE_PCT},
  "psi_some_avg10":  ${PSI_SOME_AVG10}
}
EOF

info "Résumé :"
cat "${DEST}/summary.json"
echo
info "Baseline sauvegardée dans : $DEST"
