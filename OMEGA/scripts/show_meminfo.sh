#!/usr/bin/env bash
# show_meminfo.sh — Affichage formaté de /proc/meminfo avec highlighting.
#
# Usage : ./scripts/show_meminfo.sh [--watch N]   (N = intervalle en secondes)

set -euo pipefail

WATCH_INTERVAL="${1:-}"

show() {
    local MEM_TOTAL FREE AVAILABLE CACHED DIRTY SWAP_TOTAL SWAP_FREE SWAP_USED
    MEM_TOTAL=$(awk '/^MemTotal:/    {print $2}' /proc/meminfo)
    FREE=$(awk       '/^MemFree:/    {print $2}' /proc/meminfo)
    AVAILABLE=$(awk  '/^MemAvailable:/{print $2}' /proc/meminfo)
    CACHED=$(awk     '/^Cached:/     {print $2}' /proc/meminfo)
    DIRTY=$(awk      '/^Dirty:/      {print $2}' /proc/meminfo)
    SWAP_TOTAL=$(awk '/^SwapTotal:/  {print $2}' /proc/meminfo)
    SWAP_FREE=$(awk  '/^SwapFree:/   {print $2}' /proc/meminfo)
    SWAP_USED=$((SWAP_TOTAL - SWAP_FREE))

    MEM_USED=$((MEM_TOTAL - AVAILABLE))
    MEM_PCT=0
    [[ $MEM_TOTAL -gt 0 ]] && MEM_PCT=$(( MEM_USED * 100 / MEM_TOTAL ))

    SWAP_PCT=0
    [[ $SWAP_TOTAL -gt 0 ]] && SWAP_PCT=$(( SWAP_USED * 100 / SWAP_TOTAL ))

    echo "═══════════════════════════════════════════════════════"
    echo "  /proc/meminfo — $(date '+%Y-%m-%d %H:%M:%S')  [$(hostname)]"
    echo "═══════════════════════════════════════════════════════"
    printf "  %-18s : %10s Ko  (%s Gio)\n" "Mémoire totale"    "$MEM_TOTAL"  "$(( MEM_TOTAL / 1024 / 1024 ))"
    printf "  %-18s : %10s Ko  (%s Gio)\n" "Mémoire utilisée"  "$MEM_USED"   "$(( MEM_USED  / 1024 / 1024 ))"
    printf "  %-18s : %10s Ko  (%s Gio)\n" "Mémoire disponible" "$AVAILABLE" "$(( AVAILABLE / 1024 / 1024 ))"
    printf "  %-18s : %10s Ko\n"            "Cache"             "$CACHED"
    printf "  %-18s : %10s Ko\n"            "Dirty"             "$DIRTY"
    printf "  %-18s : %3d%%\n"              "Usage RAM"         "$MEM_PCT"
    echo "───────────────────────────────────────────────────────"
    printf "  %-18s : %10s Ko\n"            "Swap total"        "$SWAP_TOTAL"
    printf "  %-18s : %10s Ko\n"            "Swap utilisé"      "$SWAP_USED"
    printf "  %-18s : %3d%%\n"              "Usage swap"        "$SWAP_PCT"
    echo "═══════════════════════════════════════════════════════"
    echo
}

if [[ "${1:-}" == "--watch" ]]; then
    INTERVAL="${2:-5}"
    while true; do
        clear
        show
        sleep "$INTERVAL"
    done
else
    show
fi
