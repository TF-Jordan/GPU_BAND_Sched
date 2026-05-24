#!/usr/bin/env bash
# setup_qos.sh — Limite de bande passante sur le trafic de paging (correction L6)
#
# Ce script applique une QoS réseau avec tc (Traffic Control) pour :
#   1. Limiter la bande passante du trafic de paging (TCP 9100) à MAX_PAGING_MBPS
#   2. Garantir que le trafic des VMs (Proxmox vmbr0) n'est pas saturé
#   3. Prioriser le trafic Proxmox cluster (corosync UDP 5405) sur le paging
#
# Usage :
#   ./setup_qos.sh [interface] [max_paging_mbps] [max_total_mbps]
#
# Exemples :
#   ./setup_qos.sh eth0 500 1000    # Limit paging à 500 Mbps sur lien 1 Gbps
#   ./setup_qos.sh eth1 200 10000   # Limit paging à 200 Mbps sur lien 10 Gbps
#   ./setup_qos.sh                  # Valeurs par défaut : eth0, 500, 1000
#
# Pour supprimer les règles QoS :
#   ./setup_qos.sh --remove [interface]
#
# Architecture HTB (Hierarchical Token Bucket) :
#
#   1: root  (MAX_TOTAL_MBPS)
#    ├── 1:10 (Priorité haute — corosync/proxmox)  [garanti 100 Mbps]
#    ├── 1:20 (Priorité normale — trafic VMs)       [garanti 400 Mbps]
#    └── 1:30 (Basse priorité — paging TCP 9100)    [max MAX_PAGING_MBPS]
#
# Vérification après application :
#   tc -s class show dev eth0          # statistiques par classe
#   tc -s qdisc show dev eth0          # état des qdiscs
#   iptables -t mangle -L -n -v        # règles de marquage

set -euo pipefail

# ─── Paramètres ────────────────────────────────────────────────────────────────

IFACE="${1:-eth0}"
MAX_PAGING_MBPS="${2:-500}"
MAX_TOTAL_MBPS="${3:-1000}"

PAGING_PORT="9100"
COROSYNC_PORT="5405"

# Classes HTB
CLASS_ROOT="1:"
CLASS_HIGH="1:10"    # Corosync + Proxmox API
CLASS_NORMAL="1:20"  # Trafic VMs
CLASS_PAGING="1:30"  # Paging omega

# Marques iptables (DSCP/fwmark)
MARK_PAGING=0x30

# ─── Fonctions ─────────────────────────────────────────────────────────────────

log() { echo "[$(date -u +%H:%M:%S)] $*"; }

check_root() {
    if [[ $EUID -ne 0 ]]; then
        echo "ERREUR : ce script doit être exécuté en root (sudo)" >&2
        exit 1
    fi
}

check_deps() {
    for cmd in tc iptables ip; do
        if ! command -v "$cmd" &>/dev/null; then
            echo "ERREUR : '$cmd' non trouvé — installer iproute2 et iptables" >&2
            exit 1
        fi
    done
}

check_interface() {
    if ! ip link show "$IFACE" &>/dev/null; then
        echo "ERREUR : interface '$IFACE' non trouvée" >&2
        echo "Interfaces disponibles :"
        ip link show | grep -oP '^\d+: \K\w+' | grep -v '^lo$'
        exit 1
    fi
}

remove_qos() {
    local iface="${2:-eth0}"
    log "Suppression des règles QoS sur $iface..."

    tc qdisc del dev "$iface" root 2>/dev/null || true

    iptables -t mangle -D OUTPUT  -p tcp --dport "$PAGING_PORT" -j MARK --set-mark "$MARK_PAGING" 2>/dev/null || true
    iptables -t mangle -D FORWARD -p tcp --dport "$PAGING_PORT" -j MARK --set-mark "$MARK_PAGING" 2>/dev/null || true

    log "Règles QoS supprimées."
    exit 0
}

# ─── Main ──────────────────────────────────────────────────────────────────────

check_root
check_deps

# Gestion de --remove
if [[ "${1:-}" == "--remove" ]]; then
    remove_qos "${@}"
fi

check_interface

log "=== Configuration QoS réseau pour omega-remote-paging ==="
log "Interface      : $IFACE"
log "Bande passante : $MAX_PAGING_MBPS Mbps (paging) / $MAX_TOTAL_MBPS Mbps (total)"
log "Port paging    : TCP $PAGING_PORT"

# ── 1. Nettoyer les règles existantes ──────────────────────────────────────────
log "Nettoyage des règles existantes..."
tc qdisc del dev "$IFACE" root 2>/dev/null || true
iptables -t mangle -F OUTPUT  2>/dev/null || true
iptables -t mangle -F FORWARD 2>/dev/null || true

# ── 2. Créer la discipline racine HTB ──────────────────────────────────────────
log "Création de la discipline HTB racine..."
tc qdisc add dev "$IFACE" root handle 1: htb default 20

# Classe racine : limite totale de la liaison
tc class add dev "$IFACE" parent 1: classid 1:1 htb \
    rate "${MAX_TOTAL_MBPS}mbit" \
    burst "$((MAX_TOTAL_MBPS / 8))k"

# ── 3. Sous-classes de priorité ────────────────────────────────────────────────

# Priorité haute : corosync, Proxmox API (8006), SSH (22)
# Garantie : 10% du lien, burst jusqu'à 100%
HIGH_GUARANTEED=$((MAX_TOTAL_MBPS / 10))
tc class add dev "$IFACE" parent 1:1 classid "$CLASS_HIGH" htb \
    rate "${HIGH_GUARANTEED}mbit" \
    ceil "${MAX_TOTAL_MBPS}mbit" \
    burst "${HIGH_GUARANTEED}k" \
    prio 1

# Priorité normale : trafic VMs, trafic général
# Garantie : 50% du lien, burst jusqu'à 100%
NORMAL_GUARANTEED=$((MAX_TOTAL_MBPS / 2))
tc class add dev "$IFACE" parent 1:1 classid "$CLASS_NORMAL" htb \
    rate "${NORMAL_GUARANTEED}mbit" \
    ceil "${MAX_TOTAL_MBPS}mbit" \
    burst "${NORMAL_GUARANTEED}k" \
    prio 2

# Basse priorité : paging omega (TCP 9100)
# Garantie : minimum, plafonné à MAX_PAGING_MBPS
tc class add dev "$IFACE" parent 1:1 classid "$CLASS_PAGING" htb \
    rate "10mbit" \
    ceil "${MAX_PAGING_MBPS}mbit" \
    burst "$((MAX_PAGING_MBPS / 8))k" \
    prio 3

# ── 4. SFQ (Stochastic Fair Queuing) dans chaque classe ────────────────────────
# Évite qu'un seul flux monopolise une classe
for class in "$CLASS_HIGH" "$CLASS_NORMAL" "$CLASS_PAGING"; do
    handle="${class/1:/1}0:"  # "1:10" → "110:"
    tc qdisc add dev "$IFACE" parent "$class" handle "$(echo $class | tr -d '1:')0:" sfq perturb 10 2>/dev/null || true
done

# ── 5. Filtres de classification ────────────────────────────────────────────────
log "Application des filtres de classification..."

# Filtrer le trafic paging par fwmark (marqué par iptables)
tc filter add dev "$IFACE" parent 1:0 protocol ip prio 3 \
    handle "$MARK_PAGING" fw flowid "$CLASS_PAGING"

# Priorité haute : corosync (UDP 5405), Proxmox web (TCP 8006), SSH (TCP 22)
for port in 5404 5405 8006 22; do
    tc filter add dev "$IFACE" parent 1:0 protocol ip prio 1 u32 \
        match ip dport "$port" 0xffff flowid "$CLASS_HIGH" 2>/dev/null || true
    tc filter add dev "$IFACE" parent 1:0 protocol ip prio 1 u32 \
        match ip sport "$port" 0xffff flowid "$CLASS_HIGH" 2>/dev/null || true
done

# ── 6. Marquage iptables pour le trafic paging ─────────────────────────────────
log "Marquage iptables du trafic paging (TCP $PAGING_PORT)..."

iptables -t mangle -A OUTPUT -p tcp --dport "$PAGING_PORT" \
    -j MARK --set-mark "$MARK_PAGING"
iptables -t mangle -A FORWARD -p tcp --dport "$PAGING_PORT" \
    -j MARK --set-mark "$MARK_PAGING"

# ── 7. Vérification ────────────────────────────────────────────────────────────
log ""
log "=== Configuration appliquée — résumé ==="
tc -s class show dev "$IFACE"

log ""
log "=== Règles iptables mangle ==="
iptables -t mangle -L OUTPUT -n -v | head -20

log ""
log "✓ QoS opérationnelle sur $IFACE"
log "  Paging TCP $PAGING_PORT : limité à ${MAX_PAGING_MBPS} Mbps"
log "  Trafic VMs  : garanti ${NORMAL_GUARANTEED} Mbps, burst jusqu'à ${MAX_TOTAL_MBPS} Mbps"
log "  Corosync    : garanti ${HIGH_GUARANTEED} Mbps, priorité maximale"
log ""
log "Pour supprimer : $0 --remove $IFACE"
log "Pour surveiller : watch -n2 'tc -s class show dev $IFACE'"

# ── 8. Persistance après reboot ────────────────────────────────────────────────
PERSIST_SCRIPT="/etc/network/if-up.d/omega-qos"
cat > "$PERSIST_SCRIPT" << EOF
#!/bin/bash
# Auto-généré par setup_qos.sh — relance la QoS au démarrage réseau
if [[ "\$IFACE" == "$IFACE" ]]; then
    $(realpath "$0") "$IFACE" "$MAX_PAGING_MBPS" "$MAX_TOTAL_MBPS"
fi
EOF
chmod +x "$PERSIST_SCRIPT"
log "Script de persistance installé : $PERSIST_SCRIPT"
