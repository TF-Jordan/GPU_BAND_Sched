# Créer un cluster Proxmox VE à 3 nœuds sur KVM

Ce guide décrit la création d'un cluster Proxmox VE de 3 nœuds entièrement virtualisés sur une machine hôte Linux. Aucun matériel supplémentaire n'est requis. Chaque nœud Proxmox tourne dans une VM KVM.

---

## Prérequis de la machine hôte

| Ressource | Minimum recommandé |
|-----------|-------------------|
| CPU | 8 cœurs physiques (virtualisation imbriquée activée) |
| RAM | 32 Go (12 Go par nœud Proxmox + marge hôte) |
| Disque | 500 Go SSD (200 Go par nœud) |
| OS hôte | Ubuntu 22.04 / Debian 12 / Fedora 38+ |
| Kernel | ≥ 5.15 (support KVM + userfaultfd) |

---

## 1. Préparer la machine hôte

### 1.1 Vérifier le support KVM

```bash
# Vérifier que le CPU supporte la virtualisation matérielle
grep -Ec '(vmx|svm)' /proc/cpuinfo
# Résultat > 0 = OK

# Vérifier que KVM est chargé
lsmod | grep kvm
# Attendu : kvm_intel (Intel) ou kvm_amd (AMD)
```

### 1.2 Installer QEMU/KVM et libvirt

```bash
# Ubuntu / Debian
sudo apt update
sudo apt install -y \
    qemu-kvm \
    libvirt-daemon-system \
    libvirt-clients \
    virtinst \
    virt-manager \
    bridge-utils \
    ovmf

# Activer et démarrer libvirt
sudo systemctl enable --now libvirtd

# Ajouter votre utilisateur au groupe libvirt
sudo usermod -aG libvirt,kvm $USER
newgrp libvirt
```

### 1.3 Activer la virtualisation imbriquée (nested virtualization)

Proxmox a besoin de virtualiser des VMs — il faut que KVM transmette les extensions de virtualisation.

```bash
# Intel
echo "options kvm_intel nested=1" | sudo tee /etc/modprobe.d/kvm-intel.conf
sudo modprobe -r kvm_intel && sudo modprobe kvm_intel

# AMD
echo "options kvm_amd nested=1" | sudo tee /etc/modprobe.d/kvm-amd.conf
sudo modprobe -r kvm_amd && sudo modprobe kvm_amd

# Vérifier
cat /sys/module/kvm_intel/parameters/nested   # doit afficher Y ou 1
# ou
cat /sys/module/kvm_amd/parameters/nested
```

### 1.4 Configurer le réseau pont (bridge)

Les 3 nœuds Proxmox doivent être sur le même réseau L2 pour former le cluster.

```bash
# Créer un bridge dédié au cluster lab
sudo ip link add name br-proxmox type bridge
sudo ip link set br-proxmox up
sudo ip addr add 10.10.0.1/24 dev br-proxmox

# Rendre la configuration persistante (NetworkManager)
nmcli connection add type bridge ifname br-proxmox con-name br-proxmox \
    ipv4.addresses "10.10.0.1/24" \
    ipv4.method manual \
    connection.autoconnect yes
```

---

## 2. Télécharger l'ISO Proxmox VE

```bash
# Créer un répertoire pour les ISOs
mkdir -p ~/iso

# Télécharger Proxmox VE (vérifier la dernière version sur proxmox.com/downloads)
# PVE 9.x (recommandé)
wget -O ~/iso/proxmox-ve.iso \
    https://enterprise.proxmox.com/iso/proxmox-ve_9.1-1.iso

# PVE 8.x (alternative)
# wget -O ~/iso/proxmox-ve.iso \
#     https://enterprise.proxmox.com/iso/proxmox-ve_8.2-1.iso

# Vérifier l'intégrité (SHA256 disponible sur le site Proxmox)
sha256sum ~/iso/proxmox-ve.iso
```

---

## 3. Créer les 3 VMs Proxmox

Les commandes suivantes créent 3 VMs identiques. Répéter pour `pve1`, `pve2`, `pve3`.

### 3.1 Créer les disques virtuels

```bash
# Répertoire de stockage
sudo mkdir -p /var/lib/libvirt/images/proxmox-lab

# Créer un disque de 80 Go par nœud (format qcow2 = allocation dynamique)
for node in pve1 pve2 pve3; do
    sudo qemu-img create -f qcow2 \
        /var/lib/libvirt/images/proxmox-lab/${node}.qcow2 80G
done
```

### 3.2 Créer et démarrer les VMs (virt-install)

```bash
# Nœud 1 — pve1
sudo virt-install \
    --name pve1 \
    --memory 8192 \
    --vcpus 4 \
    --cpu host-passthrough \
    --disk /var/lib/libvirt/images/proxmox-lab/pve1.qcow2,bus=virtio \
    --cdrom ~/iso/proxmox-ve.iso \
    --network bridge=br-proxmox,model=virtio \
    --os-variant debian13 \
    --boot uefi \
    --graphics vnc,listen=127.0.0.1,port=5901 \
    --noautoconsole

# Nœud 2 — pve2
sudo virt-install \
    --name pve2 \
    --memory 8192 \
    --vcpus 4 \
    --cpu host-passthrough \
    --disk /var/lib/libvirt/images/proxmox-lab/pve2.qcow2,bus=virtio \
    --cdrom ~/iso/proxmox-ve.iso \
    --network bridge=br-proxmox,model=virtio \
    --os-variant debian13 \
    --boot uefi \
    --graphics vnc,listen=127.0.0.1,port=5902 \
    --noautoconsole

# Nœud 3 — pve3
sudo virt-install \
    --name pve3 \
    --memory 8192 \
    --vcpus 4 \
    --cpu host-passthrough \
    --disk /var/lib/libvirt/images/proxmox-lab/pve3.qcow2,bus=virtio \
    --cdrom ~/iso/proxmox-ve.iso \
    --network bridge=br-proxmox,model=virtio \
    --os-variant debian13 \
    --boot uefi \
    --graphics vnc,listen=127.0.0.1,port=5903 \
    --noautoconsole

# Note : si debian13 n'est pas reconnu par votre osinfo-db, utiliser debian12
# ou mettre à jour : sudo apt install osinfo-db osinfo-db-tools
```

L'option `--cpu host-passthrough` transmet les instructions CPU au complet — nécessaire pour que Proxmox puisse démarrer des VMs invitées.

---

## 4. Installer Proxmox VE sur chaque nœud

Se connecter à chaque VM via VNC :

```bash
# Sur la machine hôte, se connecter à pve1
vncviewer 127.0.0.1:5901
# pve2 : port 5902, pve3 : port 5903
```

### 4.1 Procédure d'installation (identique sur chaque nœud)

Dans l'installeur graphique Proxmox :

1. **Accepter la licence** → `I agree`
2. **Disque cible** → sélectionner `/dev/vda` (disque virtio)
   - Filesystem : `ext4` (plus simple pour un lab)
3. **Pays / Fuseau horaire / Clavier** → configurer selon votre localisation
4. **Mot de passe root** → choisir un mot de passe (ex : `proxmox123`)
5. **Email** → peut être fictif pour un lab (ex : `admin@lab.local`)
6. **Configuration réseau** :

| Paramètre | pve1 | pve2 | pve3 |
|-----------|------|------|------|
| Hostname | `pve1.lab.local` | `pve2.lab.local` | `pve3.lab.local` |
| IP | `10.10.0.11/24` | `10.10.0.12/24` | `10.10.0.13/24` |
| Gateway | `10.10.0.1` | `10.10.0.1` | `10.10.0.1` |
| DNS | `8.8.8.8` | `8.8.8.8` | `8.8.8.8` |

7. **Résumé** → vérifier, puis `Install`
8. **Redémarrage** → retirer l'ISO (libvirt le fera automatiquement)

---

## 5. Configuration post-installation sur chaque nœud

Se connecter en SSH sur chaque nœud une fois installé :

```bash
ssh root@10.10.0.11   # pve1
ssh root@10.10.0.12   # pve2
ssh root@10.10.0.13   # pve3
```

### 5.1 Désactiver le dépôt Enterprise (pas de licence)

```bash
# Sur chaque nœud
sed -i 's|^deb|#deb|' /etc/apt/sources.list.d/pve-enterprise.list
sed -i 's|^deb|#deb|' /etc/apt/sources.list.d/ceph.list 2>/dev/null || true

# Ajouter le dépôt communautaire (no-subscription)
cat > /etc/apt/sources.list.d/pve-no-subscription.list << 'EOF'
deb http://download.proxmox.com/debian/pve bookworm pve-no-subscription
EOF

apt update && apt full-upgrade -y
```

### 5.2 Configurer /etc/hosts sur chaque nœud

```bash
# Sur pve1
cat >> /etc/hosts << 'EOF'
10.10.0.11 pve1 pve1.lab.local
10.10.0.12 pve2 pve2.lab.local
10.10.0.13 pve3 pve3.lab.local
EOF

# Répéter le même bloc sur pve2 et pve3
```

### 5.3 Vérifier la résolution DNS

```bash
ping -c2 pve2    # depuis pve1
ping -c2 pve3    # depuis pve1
```

---

## 6. Créer le cluster Proxmox

### 6.1 Initialiser le cluster sur pve1

```bash
# Sur pve1 uniquement
pvecm create lab-cluster

# Vérifier l'état
pvecm status
```

Résultat attendu :
```
Cluster information
-------------------
Name:             lab-cluster
Config Version:   1
Transport:        knet
Nodeid:           1
...
```

### 6.2 Rejoindre le cluster depuis pve2 et pve3

```bash
# Sur pve2
pvecm add 10.10.0.11 --use_ssh

# Sur pve3
pvecm add 10.10.0.11 --use_ssh
```

Le mot de passe root de **pve1** sera demandé à chaque fois.

### 6.3 Vérifier l'état du cluster

```bash
# Sur n'importe quel nœud
pvecm status
pvecm nodes
```

Résultat attendu :
```
Membership information
----------------------
    Nodeid      Votes Name
         1          1 pve1 (local)
         2          1 pve2
         3          1 pve3
```

---

## 7. Accéder à l'interface web

Depuis votre navigateur sur la machine hôte :

```
https://10.10.0.11:8006
https://10.10.0.12:8006
https://10.10.0.13:8006
```

Identifiants : `root` / mot de passe choisi à l'installation.

Accepter le certificat auto-signé. Les 3 nœuds doivent apparaître dans le panneau gauche sous `Datacenter`.

---

## 8. Configurer le stockage partagé — Ceph dans les VMs KVM

En production on utilise Ceph RBD natif Proxmox. Dans le lab KVM, Ceph tourne **à l'intérieur** des VMs Proxmox (nested). Cela nécessite un disque virtuel supplémentaire par VM pour l'OSD Ceph.

> **Ressources minimales pour le lab avec Ceph** : 12 Go RAM hôte (4 Go/nœud), 3 disques virtuels supplémentaires de 20 Go pour les OSDs.

### 8.1 Ajouter un disque OSD à chaque VM Proxmox

```bash
# Sur la machine hôte — ajouter un disque de 20 Go à chaque VM
for node in pve1 pve2 pve3; do
    sudo qemu-img create -f qcow2 \
        /var/lib/libvirt/images/proxmox-lab/${node}-ceph.qcow2 20G

    # Attacher à la VM (VM doit être arrêtée ou utiliser virsh attach-disk à chaud)
    sudo virsh attach-disk $node \
        /var/lib/libvirt/images/proxmox-lab/${node}-ceph.qcow2 \
        vdb \
        --driver qemu --subdriver qcow2 \
        --targetbus virtio \
        --persistent
done
```

### 8.2 Installer et configurer Ceph sur le cluster Proxmox lab

```bash
# Sur pve1 uniquement
pveceph install --repository no-subscription

# Initialiser avec le réseau du lab
pveceph init --network 10.10.0.0/24

# Créer les moniteurs (sur chaque nœud — se connecter à chacun)
# pve1 :
pveceph createmon
# pve2 :
pveceph createmon
# pve3 :
pveceph createmon

# Créer les OSDs — /dev/vdb est le disque ajouté à l'étape 8.1
# pve1 :
pveceph createosd /dev/vdb
# pve2 :
pveceph createosd /dev/vdb
# pve3 :
pveceph createosd /dev/vdb

# Créer la pool (réplication x2 suffit pour le lab)
pveceph createpool vm-pool --pg_num 32 --min_size 1 --size 2 --application rbd

# Vérifier
ceph -s
# → health: HEALTH_OK (ou HEALTH_WARN : dégradé, mais fonctionnel pour le lab)
```

### 8.3 Activer dans Proxmox

Interface web → `Datacenter` → `Storage` → `Add` → `RBD` :

| Champ | Valeur |
|-------|--------|
| ID | `ceph-vms` |
| Pool | `vm-pool` |
| Content | `Disk image` |
| Nodes | `All` |

---

## 9. Tester le cluster

### 9.1 Créer une VM de test

Dans l'interface web sur `pve1` → `Create VM` :
- OS : utiliser une image cloud (Debian, Ubuntu cloud-init)
- RAM : 512 Mo
- CPU : 1 vCPU
- Disque : 8 Go sur `ceph-vms` (pool Ceph)

### 9.2 Tester la migration live

Clic droit sur la VM → `Migrate` → sélectionner `pve2` → `Migrate`.

La VM doit continuer à fonctionner pendant la migration.

---

## 10. Arrêter et reprendre le lab

```bash
# Suspendre les VMs (économise la RAM hôte)
sudo virsh suspend pve1
sudo virsh suspend pve2
sudo virsh suspend pve3

# Reprendre
sudo virsh resume pve1
sudo virsh resume pve2
sudo virsh resume pve3

# Arrêt propre
sudo virsh shutdown pve1
sudo virsh shutdown pve2
sudo virsh shutdown pve3

# Redémarrage
sudo virsh start pve1
sudo virsh start pve2
sudo virsh start pve3
```

---

## Récapitulatif des adresses

| Rôle | Hostname | IP | Interface web |
|------|----------|----|---------------|
| Nœud 1 | pve1.lab.local | 10.10.0.11 | https://10.10.0.11:8006 |
| Nœud 2 | pve2.lab.local | 10.10.0.12 | https://10.10.0.12:8006 |
| Nœud 3 | pve3.lab.local | 10.10.0.13 | https://10.10.0.13:8006 |
| Hôte (hyperviseur) | — | 10.10.0.1 | — |
