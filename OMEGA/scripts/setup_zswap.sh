#!/usr/bin/env bash
# setup_zswap.sh — Active et configure zswap sur le nœud courant.
#
# zswap est un compresseur de swap en RAM : avant d'écrire sur le swap disque,
# Linux compresse les pages en RAM. Cela réduit la pression I/O et peut coexister
# avec notre prototype de paging distant.
#
# Usage :
#   sudo ./scripts/setup_zswap.sh [--disable]
#
# Prérequis : root, kernel ≥ 3.11 avec CONFIG_ZSWAP=y

set -euo pipefail

ZSWAP_BASE="/sys/module/zswap/parameters"
ZSWAP_ENABLED="${ZSWAP_BASE}/enabled"

# Valeurs par défaut (modifiables via variables d'environnement)
: "${ZSWAP_COMPRESSOR:=lz4}"       # lz4 = meilleur compromis latence/ratio
: "${ZSWAP_POOL:=z3fold}"           # z3fold = meilleur ratio mémoire
: "${ZSWAP_MAX_POOL_PCT:=20}"       # % de RAM max pour le pool zswap

# ─── Fonctions ────────────────────────────────────────────────────────────────

die() { echo "[ERREUR] $*" >&2; exit 1; }
info() { echo "[INFO]  $*"; }

check_root() {
    [[ $EUID -eq 0 ]] || die "ce script doit être exécuté en root (sudo)"
}

check_zswap_available() {
    [[ -d "$ZSWAP_BASE" ]] || die "zswap non disponible — kernel compilé avec CONFIG_ZSWAP=y ?"
}

show_status() {
    info "=== État actuel de zswap ==="
    for f in enabled compressor zpool max_pool_percent accept_threshold_percent; do
        val_file="${ZSWAP_BASE}/${f}"
        if [[ -r "$val_file" ]]; then
            printf "  %-35s = %s\n" "$f" "$(cat "$val_file")"
        fi
    done
    echo

    info "=== Statistiques zswap (debugfs) ==="
    DEBUGFS="/sys/kernel/debug/zswap"
    if [[ -d "$DEBUGFS" ]]; then
        for f in pool_total_size stored_pages written_back_pages duplicate_entry same_filled_pages; do
            val_file="${DEBUGFS}/${f}"
            [[ -r "$val_file" ]] && printf "  %-35s = %s\n" "$f" "$(cat "$val_file")"
        done
    else
        info "(debugfs non monté — pas de stats détaillées)"
    fi
}

enable_zswap() {
    info "Activation de zswap..."

    # Chargement des modules si nécessaire
    for mod in zswap "${ZSWAP_COMPRESSOR}" "${ZSWAP_POOL}"; do
        if ! lsmod | grep -q "^${mod}"; then
            modprobe "$mod" 2>/dev/null || info "module $mod non chargeable (peut être intégré au kernel)"
        fi
    done

    # Compresseur
    echo "$ZSWAP_COMPRESSOR" > "${ZSWAP_BASE}/compressor" \
        && info "compresseur : $ZSWAP_COMPRESSOR" \
        || info "avertissement : impossible de changer le compresseur"

    # Pool allocateur
    echo "$ZSWAP_POOL" > "${ZSWAP_BASE}/zpool" 2>/dev/null \
        && info "pool : $ZSWAP_POOL" \
        || info "avertissement : impossible de changer le pool"

    # Taille max du pool
    echo "$ZSWAP_MAX_POOL_PCT" > "${ZSWAP_BASE}/max_pool_percent" \
        && info "max_pool_percent : ${ZSWAP_MAX_POOL_PCT}%"

    # Activation
    echo "Y" > "$ZSWAP_ENABLED"
    info "zswap activé ✓"

    show_status
}

disable_zswap() {
    info "Désactivation de zswap..."
    echo "N" > "$ZSWAP_ENABLED"
    info "zswap désactivé"
}

# ─── Main ─────────────────────────────────────────────────────────────────────

check_root
check_zswap_available

case "${1:-enable}" in
    --disable | disable)
        disable_zswap
        ;;
    --status | status)
        show_status
        ;;
    --enable | enable | "")
        enable_zswap
        ;;
    *)
        echo "Usage: $0 [enable|disable|status]"
        exit 1
        ;;
esac
