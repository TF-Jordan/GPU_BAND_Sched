#!/usr/bin/env bash
# Verifie que les VMs de test sont conformes aux prerequis Omega.

set -euo pipefail
source "$(dirname "$0")/lib.sh"

header "Test 30 — Conformite VMs Omega"

require_bin qm
require_bin pvesh

vmids=()
if [[ $# -gt 0 ]]; then
    IFS=',' read -ra vmids <<< "$1"
else
    vmids=("${TEST_VMIDS_ARR[@]}")
fi

[[ ${#vmids[@]} -gt 0 ]] || fail "aucune VM a verifier"

failures=0

cfg_value() {
    local cfg="$1" key="$2"
    awk -F': ' -v k="$key" '$1 == k {print $2; exit}' <<< "$cfg"
}

mark_fail() {
    warn "$*"
    ((failures++)) || true
}

check_contains() {
    local text="$1" needle="$2" msg="$3"
    if [[ "$text" != *"$needle"* ]]; then
        mark_fail "$msg"
    fi
}

desc_value() {
    local description="$1" key="$2"
    tr ' ' '\n' <<< "$description" | awk -F= -v k="$key" '$1 == k {print $2; exit}'
}

check_desc_number() {
    local vmid="$1" description="$2" key="$3" expected="$4"
    local actual
    actual="$(desc_value "$description" "$key")"
    if [[ -z "$actual" ]]; then
        mark_fail "VM $vmid: description sans ${key}"
        return
    fi
    if [[ ! "$actual" =~ ^[0-9]+$ ]]; then
        mark_fail "VM $vmid: description ${key}='$actual' non numerique"
        return
    fi
    if [[ "$actual" -ne "$expected" ]]; then
        mark_fail "VM $vmid: description ${key}=$actual incoherent avec la config Proxmox, attendu $expected"
    fi
}

for vmid in "${vmids[@]}"; do
    [[ -n "$vmid" ]] || continue
    step "VM $vmid"

    info_line="$(_cluster_vm_info "$vmid")"
    if [[ -z "$info_line" ]]; then
        mark_fail "VM $vmid introuvable dans /cluster/resources"
        continue
    fi
    pve_node="${info_line%% *}"
    node_ip="$(_pve_node_to_ip "$pve_node")"
    _VM_NODE_CACHE[$vmid]="$node_ip"
    info "VM $vmid localisée sur $pve_node ($node_ip)"

    if qm config "$vmid" >/dev/null 2>&1; then
        ensure_omega_vcpu_profile "$vmid"
    fi

    cfg="$(qm config "$vmid" 2>/dev/null || true)"
    if [[ -z "$cfg" ]]; then
        mark_fail "VM $vmid visible dans le cluster mais config illisible sur $pve_node ($node_ip)"
        continue
    fi

    cores="$(cfg_value "$cfg" cores)"
    sockets="$(cfg_value "$cfg" sockets)"
    vcpus="$(cfg_value "$cfg" vcpus)"
    hotplug="$(cfg_value "$cfg" hotplug)"
    numa="$(cfg_value "$cfg" numa)"
    agent="$(cfg_value "$cfg" agent)"
    net0="$(cfg_value "$cfg" net0)"
    scsihw="$(cfg_value "$cfg" scsihw)"
    description="$(cfg_value "$cfg" description)"
    memory="$(cfg_value "$cfg" memory)"
    balloon="$(cfg_value "$cfg" balloon)"

    cores="${cores:-1}"
    sockets="${sockets:-1}"
    max_vcpus=$((cores * sockets))

    [[ -n "$vcpus" ]] || mark_fail "VM $vmid: champ 'vcpus' absent; Proxmox demarrera tous les cores et Omega ne pourra pas monter progressivement"
    if [[ -n "$vcpus" && "$vcpus" -gt "$max_vcpus" ]]; then
        mark_fail "VM $vmid: vcpus=$vcpus > cores*sockets=$max_vcpus"
    fi
    if [[ "$max_vcpus" -le 1 ]]; then
        mark_fail "VM $vmid: cores*sockets=$max_vcpus; les tests Omega exigent au moins 2 vCPU max pour valider le scale-up"
    fi
    if [[ -n "$vcpus" && "$vcpus" -ge "$max_vcpus" ]]; then
        mark_fail "VM $vmid: vcpus=$vcpus >= max_vcpus=$max_vcpus; elle demarre deja au plafond et ne peut pas tester le scale-up"
    fi
    [[ "$hotplug" == *cpu* ]] || mark_fail "VM $vmid: hotplug CPU absent (qm set $vmid --hotplug cpu,disk,network)"
    if [[ "$hotplug" == *memory* ]]; then
        mark_fail "VM $vmid: hotplug mémoire Proxmox actif; les VMs Omega doivent utiliser balloon + backend Omega, pas hotplug memory"
    fi
    [[ "$agent" == *enabled=1* || "$agent" == "1" ]] || mark_fail "VM $vmid: qemu-guest-agent non active (qm set $vmid --agent enabled=1)"
    [[ "$net0" == virtio* ]] || mark_fail "VM $vmid: net0 doit utiliser virtio"
    [[ "$net0" == *bridge=* ]] || mark_fail "VM $vmid: net0 doit declarer un bridge"
    [[ "$scsihw" == virtio-scsi* ]] || mark_fail "VM $vmid: scsihw doit etre virtio-scsi-*"
    [[ -n "$memory" ]] || mark_fail "VM $vmid: memory absent"
    [[ -n "$balloon" ]] || mark_fail "VM $vmid: balloon absent; les VMs Omega doivent avoir une RAM initiale/min via balloon"
    if [[ -n "$memory" && -n "$balloon" ]]; then
        if [[ "$balloon" -le 0 ]]; then
            mark_fail "VM $vmid: balloon=$balloon invalide; attendu > 0"
        fi
        if [[ "$balloon" -ge "$memory" ]]; then
            mark_fail "VM $vmid: balloon=$balloon >= memory=$memory; la VM demarre deja avec toute sa RAM et ne teste pas le thin-provisioning"
        fi
    fi

    if [[ -n "$vcpus" ]]; then
        check_desc_number "$vmid" "$description" "omega_min_vcpus" "$vcpus"
    else
        check_contains "$description" "omega_min_vcpus=" "VM $vmid: description sans omega_min_vcpus"
    fi
    check_desc_number "$vmid" "$description" "omega_max_vcpus" "$max_vcpus"
    if [[ -n "$balloon" ]]; then
        check_desc_number "$vmid" "$description" "omega_memory_min_mib" "$balloon"
    else
        check_contains "$description" "omega_memory_min_mib=" "VM $vmid: description sans omega_memory_min_mib"
    fi
    if [[ -n "$memory" ]]; then
        check_desc_number "$vmid" "$description" "omega_memory_max_mib" "$memory"
    else
        check_contains "$description" "omega_memory_max_mib=" "VM $vmid: description sans omega_memory_max_mib"
    fi
    check_contains "$description" "omega_disk_max_gib=" "VM $vmid: description sans omega_disk_max_gib"
    check_contains "$description" "omega_gpu_vram_mib=" "VM $vmid: description sans omega_gpu_vram_mib"

    smp="$(qm showcmd "$vmid" --pretty 2>/dev/null | grep -- '-smp' || true)"
    if [[ -z "$smp" ]]; then
        mark_fail "VM $vmid: impossible de lire qm showcmd"
    else
        echo "$smp"
        [[ "$smp" == *"maxcpus=${max_vcpus}"* ]] || mark_fail "VM $vmid: -smp ne contient pas maxcpus=${max_vcpus}; appliquer la correction puis stop/start la VM pour regenerer la ligne QEMU"
        if [[ -n "$vcpus" ]]; then
            [[ "$smp" == *"-smp '${vcpus},"* || "$smp" == *"-smp ${vcpus},"* ]] || warn "VM $vmid: -smp ne demarre pas visiblement a vcpus=$vcpus"
        fi
    fi

    status="$(_cluster_vm_status "$vmid")"
    if [[ "$status" == "running" ]]; then
        if qm guest cmd "$vmid" ping >/dev/null 2>&1; then
            pass "VM $vmid: agent invite joignable"
        else
            warn "VM $vmid: qemu-guest-agent configure mais non joignable; verifier service qemu-guest-agent dans l'invite"
        fi
    else
        warn "VM $vmid: statut '$status'; la conformite statique est testee mais pas l'agent invite"
    fi
done

[[ "$failures" -eq 0 ]] || fail "$failures erreur(s) de conformite VM"
pass "VMs conformes aux prerequis Omega"
