#!/usr/bin/env bash
# show_pressure.sh — Affiche les métriques PSI (Pressure Stall Information).
#
# PSI mesure le temps que les processus passent à attendre des ressources.
# "some" = au moins un processus en attente, "full" = tous les processus en attente.
#
# Usage :
#   ./scripts/show_pressure.sh              # affichage unique
#   ./scripts/show_pressure.sh --watch 2   # rafraîchissement toutes les 2s

set -euo pipefail

PRESSURE_DIR="/proc/pressure"

check_psi() {
    if [[ ! -d "$PRESSURE_DIR" ]]; then
        echo "[ERREUR] PSI non disponible sur ce système."
        echo "  Requis : kernel ≥ 4.20 avec CONFIG_PSI=y"
        echo "  Activation runtime : echo 1 > /proc/sys/kernel/pressure_stall_information"
        exit 1
    fi
}

parse_psi_line() {
    # Entrée : "some avg10=3.50 avg60=1.20 avg300=0.40 total=12345678"
    # Sortie : avg10 avg60 avg300
    local line="$1"
    echo "$line" | awk '{
        for (i=1; i<=NF; i++) {
            if ($i ~ /^avg10=/)  { sub(/avg10=/,  "", $i); a10=$i }
            if ($i ~ /^avg60=/)  { sub(/avg60=/,  "", $i); a60=$i }
            if ($i ~ /^avg300=/) { sub(/avg300=/, "", $i); a300=$i }
        }
        printf "%6.2f  %6.2f  %6.2f", a10, a60, a300
    }'
}

show_resource() {
    local name="$1"
    local file="${PRESSURE_DIR}/${name}"

    [[ -f "$file" ]] || return 0

    local some_line full_line
    some_line=$(grep '^some' "$file")
    full_line=$(grep '^full' "$file" 2>/dev/null || echo "full avg10=0.00 avg60=0.00 avg300=0.00 total=0")

    local some_vals full_vals
    some_vals=$(parse_psi_line "$some_line")
    full_vals=$(parse_psi_line "$full_line")

    printf "  %-8s some : %s  (avg10 / avg60 / avg300 %%)\n" "[$name]" "$some_vals"
    printf "  %-8s full : %s\n"                               ""        "$full_vals"
    echo
}

show() {
    echo "═══════════════════════════════════════════════════════"
    echo "  PSI — Pressure Stall Information — $(date '+%H:%M:%S')"
    echo "  Valeurs en % du temps — avg10/avg60/avg300"
    echo "═══════════════════════════════════════════════════════"
    show_resource "memory"
    show_resource "cpu"
    show_resource "io"
    echo "  Interprétation :"
    echo "    < 1%  : aucune pression significative"
    echo "    1-10% : pression modérée"
    echo "    > 10% : pression élevée (intervenir)"
    echo "═══════════════════════════════════════════════════════"
}

check_psi

if [[ "${1:-}" == "--watch" ]]; then
    INTERVAL="${2:-3}"
    while true; do
        clear
        show
        sleep "$INTERVAL"
    done
else
    show
fi
