#!/usr/bin/env bash
# Cree une VM Proxmox conforme aux tests omega-remote-paging.
#
# Exemple:
#   ./scripts/create-omega-vm.sh \
#     --vmids 9001,9002,9003 \
#     --storage ceph-vms \
#     --bridge vmbr0 \
#     --image /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2 \
#     --sshkey /root/.ssh/id_rsa.pub \
#     --start

set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/create-omega-vm.sh --vmid 9001 --storage ceph-vms --bridge vmbr0 --image IMAGE.qcow2 [options]
  scripts/create-omega-vm.sh --vmids 9001,9002,9003 --storage ceph-vms --bridge vmbr0 --image IMAGE.qcow2 [options]

Options:
  --vmid ID             VMID unique.
  --vmids A,B,C         Liste de VMIDs a creer.
  --name PREFIX         Prefixe de nom. Defaut: omega-test.
  --storage NAME        Stockage Proxmox cible, idealement Ceph RBD.
  --bridge NAME         Bridge reseau Proxmox. Defaut: vmbr0.
  --image PATH          Image cloud qcow2 a importer.
  --sshkey PATH         Cle publique injectee par cloud-init.
  --ciuser USER         Utilisateur cloud-init. Defaut: root.
  --memory MIB          RAM max demandee par la VM. Defaut: 2048.
  --balloon MIB         RAM locale initiale/minimale. Defaut: 512.
  --cores N             Cores QEMU. Avec sockets, definit le max vCPU hotpluggable. Defaut: 4.
  --vcpus N             vCPU au boot. Defaut: 1.
  --sockets N           Sockets QEMU. max_vcpus = cores * sockets. Defaut: 1.
  --disk-max-gib N      Budget disque logique documente. Defaut: 20.
  --gpu-vram-mib N      Budget VRAM documente. Defaut: 0.
  --cpu TYPE            Type CPU QEMU. Defaut: kvm64.
  --ipconfig0 VALUE     Config IP cloud-init. Defaut: ip=dhcp.
  --nameserver VALUE    DNS cloud-init. Defaut: 8.8.8.8.
  --start               Demarre la VM apres creation.
  -h, --help            Affiche cette aide.

La VM creee est conforme au projet:
  - boot avec --vcpus <min> et --cores/--sockets <max>, donc showcmd expose maxcpus
  - hotplug CPU/disk/network actif
  - RAM elastique via balloon + backend Omega, pas via memory hotplug Proxmox
  - qemu-guest-agent active
  - disque virtio-scsi avec iothread/discard
  - net0 virtio sur bridge Proxmox
  - description contenant les quotas Omega lus par le controller
EOF
}

fail() {
    echo "ERREUR: $*" >&2
    exit 1
}

cfg_value() {
    local cfg="$1" key="$2"
    awk -F': ' -v k="$key" '$1 == k {print $2; exit}' <<< "$cfg"
}

require_cfg_value() {
    local cfg="$1" key="$2" expected="$3" vmid="$4"
    local actual
    actual="$(cfg_value "$cfg" "$key")"
    [[ "$actual" == "$expected" ]] || fail "VM $vmid non conforme: $key='$actual', attendu '$expected'"
}

verify_vm_profile() {
    local vmid="$1"
    local cfg smp hotplug agent net0 scsihw description

    cfg="$(qm config "$vmid" 2>/dev/null)" || fail "impossible de relire la config de la VM $vmid"
    require_cfg_value "$cfg" cores "$CORES" "$vmid"
    require_cfg_value "$cfg" sockets "$SOCKETS" "$vmid"
    require_cfg_value "$cfg" vcpus "$VCPUS" "$vmid"
    require_cfg_value "$cfg" memory "$MEMORY" "$vmid"
    require_cfg_value "$cfg" balloon "$BALLOON" "$vmid"

    hotplug="$(cfg_value "$cfg" hotplug)"
    [[ "$hotplug" == *cpu* && "$hotplug" == *disk* && "$hotplug" == *network* ]] || fail "VM $vmid non conforme: hotplug='$hotplug', attendu cpu,disk,network"
    [[ "$hotplug" != *memory* ]] || fail "VM $vmid non conforme: hotplug memory interdit pour Omega"

    agent="$(cfg_value "$cfg" agent)"
    [[ "$agent" == *enabled=1* || "$agent" == "1" ]] || fail "VM $vmid non conforme: qemu-guest-agent non active"

    net0="$(cfg_value "$cfg" net0)"
    [[ "$net0" == virtio=* && "$net0" == *"bridge=${BRIDGE}"* ]] || fail "VM $vmid non conforme: net0='$net0', attendu virtio sur ${BRIDGE}"

    scsihw="$(cfg_value "$cfg" scsihw)"
    [[ "$scsihw" == virtio-scsi* ]] || fail "VM $vmid non conforme: scsihw='$scsihw', attendu virtio-scsi-*"

    description="$(cfg_value "$cfg" description)"
    [[ "$description" == *"omega_min_vcpus=${VCPUS}"* ]] || fail "VM $vmid non conforme: description sans omega_min_vcpus=${VCPUS}"
    [[ "$description" == *"omega_max_vcpus=${MAX_VCPUS}"* ]] || fail "VM $vmid non conforme: description sans omega_max_vcpus=${MAX_VCPUS}"
    [[ "$description" == *"omega_memory_min_mib=${BALLOON}"* ]] || fail "VM $vmid non conforme: description sans omega_memory_min_mib=${BALLOON}"
    [[ "$description" == *"omega_memory_max_mib=${MEMORY}"* ]] || fail "VM $vmid non conforme: description sans omega_memory_max_mib=${MEMORY}"
    [[ "$description" == *"omega_disk_max_gib=${DISK_MAX_GIB}"* ]] || fail "VM $vmid non conforme: description sans omega_disk_max_gib=${DISK_MAX_GIB}"
    [[ "$description" == *"omega_gpu_vram_mib=${GPU_VRAM_MIB}"* ]] || fail "VM $vmid non conforme: description sans omega_gpu_vram_mib=${GPU_VRAM_MIB}"

    smp="$(qm showcmd "$vmid" --pretty 2>/dev/null | grep -- '-smp' || true)"
    [[ -n "$smp" ]] || fail "VM $vmid non conforme: qm showcmd ne retourne pas -smp"
    echo "$smp"
    [[ "$smp" == *"maxcpus=${MAX_VCPUS}"* ]] || fail "VM $vmid non conforme: -smp ne contient pas maxcpus=${MAX_VCPUS}"
    [[ "$smp" == *"-smp '${VCPUS},"* || "$smp" == *"-smp ${VCPUS},"* ]] || fail "VM $vmid non conforme: -smp ne demarre pas a ${VCPUS} vCPU"

    echo "Profil Omega VM $vmid valide: vcpus=${VCPUS}, maxcpus=${MAX_VCPUS}, memory=${MEMORY}, balloon=${BALLOON}, hotplug=cpu,disk,network"
}

VMIDS=""
NAME_PREFIX="omega-test"
STORAGE=""
BRIDGE="vmbr0"
IMAGE=""
SSHKEY=""
CIUSER="root"
MEMORY=2048
BALLOON=512
CORES=4
VCPUS=1
SOCKETS=1
DISK_MAX_GIB=20
GPU_VRAM_MIB=0
CPU_TYPE="kvm64"
IPCONFIG0="ip=dhcp"
NAMESERVER="8.8.8.8"
START=false
OSTYPE="l26"
MACHINE="q35"
TAGS="omega,omega-test"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --vmid) VMIDS="$2"; shift 2 ;;
        --vmids) VMIDS="$2"; shift 2 ;;
        --name) NAME_PREFIX="$2"; shift 2 ;;
        --storage) STORAGE="$2"; shift 2 ;;
        --bridge) BRIDGE="$2"; shift 2 ;;
        --image) IMAGE="$2"; shift 2 ;;
        --sshkey) SSHKEY="$2"; shift 2 ;;
        --ciuser) CIUSER="$2"; shift 2 ;;
        --memory) MEMORY="$2"; shift 2 ;;
        --balloon) BALLOON="$2"; shift 2 ;;
        --cores) CORES="$2"; shift 2 ;;
        --vcpus) VCPUS="$2"; shift 2 ;;
        --sockets) SOCKETS="$2"; shift 2 ;;
        --disk-max-gib) DISK_MAX_GIB="$2"; shift 2 ;;
        --gpu-vram-mib) GPU_VRAM_MIB="$2"; shift 2 ;;
        --cpu) CPU_TYPE="$2"; shift 2 ;;
        --ipconfig0) IPCONFIG0="$2"; shift 2 ;;
        --nameserver) NAMESERVER="$2"; shift 2 ;;
        --start) START=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "option inconnue: $1" ;;
    esac
done

command -v qm >/dev/null 2>&1 || fail "qm introuvable: lancer ce script sur un noeud Proxmox"
[[ -n "$VMIDS" ]] || fail "--vmid ou --vmids requis"
[[ -n "$STORAGE" ]] || fail "--storage requis"
[[ -n "$IMAGE" ]] || fail "--image requis"
[[ -f "$IMAGE" ]] || fail "image introuvable: $IMAGE"
[[ "$VCPUS" =~ ^[0-9]+$ && "$CORES" =~ ^[0-9]+$ && "$SOCKETS" =~ ^[0-9]+$ ]] || fail "vcpus/cores/sockets doivent etre numeriques"
[[ "$MEMORY" =~ ^[0-9]+$ && "$BALLOON" =~ ^[0-9]+$ ]] || fail "memory/balloon doivent etre numeriques"
[[ "$DISK_MAX_GIB" =~ ^[0-9]+$ && "$GPU_VRAM_MIB" =~ ^[0-9]+$ ]] || fail "disk-max-gib/gpu-vram-mib doivent etre numeriques"
[[ "$VCPUS" -ge 1 ]] || fail "--vcpus doit etre >= 1"
[[ "$BALLOON" -gt 0 ]] || fail "--balloon doit etre > 0 pour la RAM initiale/minimale Omega"
[[ "$MEMORY" -gt "$BALLOON" ]] || fail "--memory doit etre strictement superieur a --balloon pour tester le thin-provisioning RAM"

MAX_VCPUS=$((CORES * SOCKETS))
[[ "$MAX_VCPUS" -ge 2 ]] || fail "--cores x --sockets doit etre >= 2 pour tester le vCPU elastique"
[[ "$VCPUS" -lt "$MAX_VCPUS" ]] || fail "--vcpus doit etre strictement inferieur a --cores x --sockets (${MAX_VCPUS}) pour permettre le scale-up"
HOST_MAX_VCPUS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 0)"
if [[ "$HOST_MAX_VCPUS" =~ ^[0-9]+$ && "$HOST_MAX_VCPUS" -gt 0 && "$MAX_VCPUS" -gt "$HOST_MAX_VCPUS" ]]; then
    fail "--cores x --sockets = ${MAX_VCPUS}, mais ce noeud Proxmox n'autorise que ${HOST_MAX_VCPUS} vCPU par VM. Reduire --cores ou choisir un noeud plus grand."
fi

IFS=',' read -ra VMID_ARR <<< "$VMIDS"

for vmid in "${VMID_ARR[@]}"; do
    [[ "$vmid" =~ ^[0-9]+$ ]] || fail "VMID invalide: $vmid"
    if qm status "$vmid" >/dev/null 2>&1; then
        fail "VM $vmid existe deja; supprimer ou choisir un autre VMID"
    fi

    name="${NAME_PREFIX}-${vmid}"
    desc="omega_min_vcpus=${VCPUS} omega_max_vcpus=${MAX_VCPUS} omega_memory_min_mib=${BALLOON} omega_memory_max_mib=${MEMORY} omega_disk_max_gib=${DISK_MAX_GIB} omega_gpu_vram_mib=${GPU_VRAM_MIB}"

    echo "Creation VM $vmid ($name)"
    qm create "$vmid" \
        --name "$name" \
        --ostype "$OSTYPE" \
        --machine "$MACHINE" \
        --agent enabled=1 \
        --cpu "$CPU_TYPE" \
        --sockets "$SOCKETS" \
        --cores "$CORES" \
        --vcpus "$VCPUS" \
        --memory "$MEMORY" \
        --balloon "$BALLOON" \
        --hotplug cpu,disk,network \
        --scsihw virtio-scsi-single \
        --net0 "virtio,bridge=${BRIDGE},firewall=0" \
        --serial0 socket \
        --vga serial0 \
        --tags "$TAGS" \
        --description "$desc"

    qm importdisk "$vmid" "$IMAGE" "$STORAGE"
    qm set "$vmid" \
        --scsi0 "${STORAGE}:vm-${vmid}-disk-0,discard=on,iothread=1,ssd=1" \
        --boot order=scsi0 \
        --ide2 "${STORAGE}:cloudinit" \
        --ipconfig0 "$IPCONFIG0" \
        --nameserver "$NAMESERVER" \
        --ciuser "$CIUSER"

    if [[ -n "$SSHKEY" ]]; then
        [[ -f "$SSHKEY" ]] || fail "cle SSH introuvable: $SSHKEY"
        qm set "$vmid" --sshkeys "$SSHKEY"
    fi

    echo "Verification profil Omega VM $vmid"
    verify_vm_profile "$vmid"

    if $START; then
        qm start "$vmid"
        echo "Verification profil Omega VM $vmid apres demarrage"
        verify_vm_profile "$vmid"
        echo "VM $vmid demarree"
    fi
done
