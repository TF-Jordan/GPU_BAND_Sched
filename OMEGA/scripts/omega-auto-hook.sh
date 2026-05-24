#!/usr/bin/env bash
# omega-auto-hook.sh — Enregistre automatiquement le hookscript omega sur toutes
# les VMs du nœud local qui ne l'ont pas encore.
# Prévu pour tourner en cron ou via systemd timer.

HOOKSCRIPT="local:snippets/omega-hook.pl"

for vmid in $(qm list 2>/dev/null | awk 'NR>1 {print $1}'); do
    current=$(qm config "$vmid" 2>/dev/null | grep '^hookscript:' | awk '{print $2}')
    if [[ "$current" != "$HOOKSCRIPT" ]]; then
        qm set "$vmid" --hookscript "$HOOKSCRIPT" && \
            echo "$(date -Iseconds) hookscript enregistré sur VM ${vmid}" || \
            echo "$(date -Iseconds) ERREUR VM ${vmid}" >&2
    fi
done
