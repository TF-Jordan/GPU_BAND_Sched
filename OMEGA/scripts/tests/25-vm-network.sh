#!/usr/bin/env bash
# Test 25 — Réseau invité VM : interface, route, DNS, Internet.
# Usage : ./25-vm-network.sh [vmid]

set -euo pipefail
source "$(dirname "$0")/lib.sh"

VMID="${1:-$TEST_VMID}"
require_vm_running "$VMID"
VMID="$SELECTED_VMID"

header "Test 25 — Réseau VM invitée (VM $VMID)"

step "Vérification qemu-guest-agent"
if ! qm guest cmd "$VMID" ping &>/dev/null; then
    cfg="$(qm config "$VMID" 2>/dev/null || true)"
    if ! printf '%s\n' "$cfg" | grep -q '^agent:.*enabled=1'; then
        warn "agent Proxmox non activé dans la config — activation côté Proxmox"
        qm set "$VMID" --agent enabled=1 >/dev/null || true
        fail "qemu-guest-agent activé côté Proxmox mais la VM doit être redémarrée, puis installer/démarrer qemu-guest-agent dans l'invité"
    fi
    fail "qemu-guest-agent indisponible dans la VM $VMID — dans l'invité exécuter: apt-get update && apt-get install -y qemu-guest-agent && systemctl start qemu-guest-agent; puis redémarrer la VM si Proxmox ne répond toujours pas"
fi
pass "qemu-guest-agent OK"

guest_exec() {
    local cmd="$1"
    local started pid status
    started=$(qm guest exec "$VMID" -- bash -lc "$cmd" 2>/dev/null || true)
    pid=$(printf '%s' "$started" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("pid",""))' 2>/dev/null || true)
    [[ -n "$pid" ]] || return 1
    for _ in $(seq 1 30); do
        status=$(qm guest exec-status "$VMID" "$pid" 2>/dev/null || true)
        if printf '%s' "$status" | python3 -c 'import sys,json; sys.exit(0 if json.load(sys.stdin).get("exited") else 1)' 2>/dev/null; then
            printf '%s' "$status" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("out-data","") + d.get("err-data",""), end="")' 2>/dev/null
            return 0
        fi
        sleep 1
    done
    return 1
}

step "Interfaces et route"
ip_out=$(guest_exec "ip -br addr; echo ---; ip route")
echo "$ip_out"
echo "$ip_out" | grep -Eq "UP|UNKNOWN" || fail "aucune interface réseau visible dans la VM"
echo "$ip_out" | grep -q "default" || fail "pas de route par défaut dans la VM"
pass "interface + route par défaut OK"

step "Connectivité IP"
if guest_exec "ping -c1 -W3 8.8.8.8" | grep -qi "1 received\|1 packets received"; then
    pass "ping 8.8.8.8 OK"
else
    fail "ping 8.8.8.8 échoue — vérifier bridge Proxmox, gateway, NAT ou routage LAN"
fi

step "DNS"
if guest_exec "getent hosts deb.debian.org || nslookup deb.debian.org" | grep -qi "debian"; then
    pass "résolution DNS OK"
else
    fail "DNS invité KO — vérifier /etc/resolv.conf, DHCP, gateway ou NAT"
fi

step "HTTP apt repository"
if guest_exec "timeout 10 bash -lc 'command -v apt-get >/dev/null && apt-get update -o Acquire::Retries=0 >/tmp/omega-apt.log 2>&1; tail -40 /tmp/omega-apt.log || true'" \
    | grep -Eqi "Reading package lists|All packages are up to date|Hit:|Get:"; then
    pass "apt update communique avec les dépôts"
else
    warn "apt update non concluant — VM peut être non Debian ou sources APT invalides"
fi

pass "Réseau invité validé pour VM $VMID"
