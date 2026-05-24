#!/usr/bin/env bash
# Provisionne des VMs Omega sur un cluster Proxmox depuis la machine de dev.
#
# Le script copie le createur de VM sur le noeud controleur, verifie l'image
# cloud distante, la copie si demande, puis lance la creation Proxmox.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/provision-omega-vms-remote.sh --controller HOST --vmids 9001,9002,9003 --storage ceph-vm --image-remote PATH [options]

Options:
  --controller HOST       Noeud Proxmox principal joignable en SSH.
  --user USER             Utilisateur SSH. Defaut: root.
  --ssh-key PATH          Cle SSH privee optionnelle.
  --vmids A,B,C           Liste de VMIDs a creer.
  --storage NAME          Stockage Proxmox cible. Exemple: ceph-vm.
  --bridge NAME           Bridge Proxmox. Defaut: vmbr0.
  --image-remote PATH     Chemin qcow2 sur le noeud Proxmox.
  --image-local PATH      Si l'image distante manque, copie ce qcow2 local.
  --sshkey-remote PATH    Cle publique a injecter dans les VMs. Defaut: /root/.ssh/id_rsa.pub.
  --name PREFIX           Prefixe nom VM. Defaut: omega-test.
  --memory MIB            RAM max VM. Defaut: 2048.
  --balloon MIB           RAM initiale/minimale. Defaut: 512.
  --cores N               Max vCPU hotpluggable. Defaut: 4.
  --sockets N             Sockets QEMU. Defaut: 1.
  --vcpus N               vCPU au boot. Defaut: 1.
  --disk-max-gib N        Budget disque logique declare. Defaut: 20.
  --gpu-vram-mib N        Budget VRAM declare. Defaut: 0.
  --no-start              Ne demarre pas les VMs apres creation.
  -h, --help              Affiche cette aide.

Exemple:
  scripts/provision-omega-vms-remote.sh \
    --controller 192.168.123.100 \
    --vmids 9001,9002,9003 \
    --storage ceph-vm \
    --bridge vmbr0 \
    --image-local /home/me/images/debian-12-generic-amd64.qcow2 \
    --image-remote /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2
EOF
}

fail() {
    echo "ERREUR: $*" >&2
    exit 1
}

quote_args() {
    printf ' %q' "$@"
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CREATE_SCRIPT="${SCRIPT_DIR}/create-omega-vm.sh"

CONTROLLER=""
DEPLOY_USER="root"
SSH_KEY=""
VMIDS=""
STORAGE=""
BRIDGE="vmbr0"
IMAGE_REMOTE=""
IMAGE_LOCAL=""
SSHKEY_REMOTE="/root/.ssh/id_rsa.pub"
NAME_PREFIX="omega-test"
MEMORY=2048
BALLOON=512
CORES=4
SOCKETS=1
VCPUS=1
DISK_MAX_GIB=20
GPU_VRAM_MIB=0
START=true

while [[ $# -gt 0 ]]; do
    case "$1" in
        --controller) CONTROLLER="$2"; shift 2 ;;
        --user) DEPLOY_USER="$2"; shift 2 ;;
        --ssh-key) SSH_KEY="$2"; shift 2 ;;
        --vmids) VMIDS="$2"; shift 2 ;;
        --storage) STORAGE="$2"; shift 2 ;;
        --bridge) BRIDGE="$2"; shift 2 ;;
        --image-remote) IMAGE_REMOTE="$2"; shift 2 ;;
        --image-local) IMAGE_LOCAL="$2"; shift 2 ;;
        --sshkey-remote) SSHKEY_REMOTE="$2"; shift 2 ;;
        --name) NAME_PREFIX="$2"; shift 2 ;;
        --memory) MEMORY="$2"; shift 2 ;;
        --balloon) BALLOON="$2"; shift 2 ;;
        --cores) CORES="$2"; shift 2 ;;
        --sockets) SOCKETS="$2"; shift 2 ;;
        --vcpus) VCPUS="$2"; shift 2 ;;
        --disk-max-gib) DISK_MAX_GIB="$2"; shift 2 ;;
        --gpu-vram-mib) GPU_VRAM_MIB="$2"; shift 2 ;;
        --no-start) START=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "option inconnue: $1" ;;
    esac
done

[[ -x "$CREATE_SCRIPT" ]] || fail "script introuvable ou non executable: $CREATE_SCRIPT"
[[ -n "$CONTROLLER" ]] || fail "--controller requis"
[[ -n "$VMIDS" ]] || fail "--vmids requis"
[[ -n "$STORAGE" ]] || fail "--storage requis"
[[ -n "$IMAGE_REMOTE" ]] || fail "--image-remote requis"
[[ "$CORES" =~ ^[0-9]+$ && "$SOCKETS" =~ ^[0-9]+$ && "$VCPUS" =~ ^[0-9]+$ ]] || fail "cores/sockets/vcpus doivent etre numeriques"
MAX_VCPUS=$((CORES * SOCKETS))
[[ "$MAX_VCPUS" -ge 2 ]] || fail "cores*sockets doit etre >= 2"
[[ "$VCPUS" -lt "$MAX_VCPUS" ]] || fail "vcpus doit etre < cores*sockets (${MAX_VCPUS})"

SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
SCP_OPTS=(-o StrictHostKeyChecking=accept-new)
if [[ -n "$SSH_KEY" ]]; then
    [[ -f "$SSH_KEY" ]] || fail "cle SSH introuvable: $SSH_KEY"
    SSH_OPTS=(-i "$SSH_KEY" "${SSH_OPTS[@]}")
    SCP_OPTS=(-i "$SSH_KEY" "${SCP_OPTS[@]}")
fi

REMOTE="${DEPLOY_USER}@${CONTROLLER}"
REMOTE_DIR="/opt/omega-remote-paging/scripts"
REMOTE_CREATE="${REMOTE_DIR}/create-omega-vm.sh"

echo "[1/5] Verification SSH vers ${REMOTE}"
ssh "${SSH_OPTS[@]}" -o ConnectTimeout=10 "$REMOTE" "hostname; command -v qm >/dev/null; command -v pvesm >/dev/null"

echo "[2/5] Verification stockage Proxmox: ${STORAGE}"
ssh "${SSH_OPTS[@]}" "$REMOTE" "pvesm status | awk 'NR==1 || \$1==\"${STORAGE}\" {print}'; pvesm status | awk 'NR>1 {print \$1}' | grep -qx '${STORAGE}'"

echo "[3/5] Synchronisation du script de creation"
ssh "${SSH_OPTS[@]}" "$REMOTE" "mkdir -p '$REMOTE_DIR'"
scp "${SCP_OPTS[@]}" "$CREATE_SCRIPT" "${REMOTE}:${REMOTE_CREATE}"
ssh "${SSH_OPTS[@]}" "$REMOTE" "chmod +x '$REMOTE_CREATE'"

echo "[4/5] Verification image distante"
if ! ssh "${SSH_OPTS[@]}" "$REMOTE" "test -f '$IMAGE_REMOTE'"; then
    [[ -n "$IMAGE_LOCAL" ]] || fail "image distante absente et --image-local non fourni: $IMAGE_REMOTE"
    [[ -f "$IMAGE_LOCAL" ]] || fail "image locale introuvable: $IMAGE_LOCAL"
    remote_image_dir="$(dirname "$IMAGE_REMOTE")"
    echo "Image absente sur le cluster; copie de $IMAGE_LOCAL vers ${REMOTE}:${IMAGE_REMOTE}"
    ssh "${SSH_OPTS[@]}" "$REMOTE" "mkdir -p '$remote_image_dir'"
    scp "${SCP_OPTS[@]}" "$IMAGE_LOCAL" "${REMOTE}:${IMAGE_REMOTE}"
fi
ssh "${SSH_OPTS[@]}" "$REMOTE" "ls -lh '$IMAGE_REMOTE'"

create_args=(
    --vmids "$VMIDS"
    --name "$NAME_PREFIX"
    --storage "$STORAGE"
    --bridge "$BRIDGE"
    --image "$IMAGE_REMOTE"
    --sshkey "$SSHKEY_REMOTE"
    --memory "$MEMORY"
    --balloon "$BALLOON"
    --cores "$CORES"
    --sockets "$SOCKETS"
    --vcpus "$VCPUS"
    --disk-max-gib "$DISK_MAX_GIB"
    --gpu-vram-mib "$GPU_VRAM_MIB"
)
$START && create_args+=(--start)

echo "[5/5] Creation distante des VMs Omega"
ssh "${SSH_OPTS[@]}" "$REMOTE" "cd /opt/omega-remote-paging && '$REMOTE_CREATE'$(quote_args "${create_args[@]}")"

echo "Provisioning termine. Verification stricte du profil Omega:"
ssh "${SSH_OPTS[@]}" "$REMOTE" "for vmid in \$(echo '$VMIDS' | tr ',' ' '); do
    echo \"=== VM \$vmid ===\"
    cfg=\$(qm config \"\$vmid\") || exit 1
    printf '%s\n' \"\$cfg\" | egrep '^(name|agent|cores|sockets|vcpus|hotplug|memory|balloon|scsihw|scsi0|net0|description):'
    smp=\$(qm showcmd \"\$vmid\" --pretty | grep -- '-smp' || true)
    printf '%s\n' \"\$smp\"
    printf '%s\n' \"\$cfg\" | grep -qx 'cores: ${CORES}' || { echo \"VM \$vmid non conforme: cores attendu ${CORES}\" >&2; exit 1; }
    printf '%s\n' \"\$cfg\" | grep -qx 'sockets: ${SOCKETS}' || { echo \"VM \$vmid non conforme: sockets attendu ${SOCKETS}\" >&2; exit 1; }
    printf '%s\n' \"\$cfg\" | grep -qx 'vcpus: ${VCPUS}' || { echo \"VM \$vmid non conforme: vcpus attendu ${VCPUS}\" >&2; exit 1; }
    printf '%s\n' \"\$cfg\" | grep -qx 'memory: ${MEMORY}' || { echo \"VM \$vmid non conforme: memory attendu ${MEMORY}\" >&2; exit 1; }
    printf '%s\n' \"\$cfg\" | grep -qx 'balloon: ${BALLOON}' || { echo \"VM \$vmid non conforme: balloon attendu ${BALLOON}\" >&2; exit 1; }
    printf '%s\n' \"\$cfg\" | grep -q '^hotplug: .*cpu' || { echo \"VM \$vmid non conforme: hotplug cpu absent\" >&2; exit 1; }
    printf '%s\n' \"\$smp\" | grep -q 'maxcpus=${MAX_VCPUS}' || { echo \"VM \$vmid non conforme: showcmd sans maxcpus=${MAX_VCPUS}\" >&2; exit 1; }
done"
