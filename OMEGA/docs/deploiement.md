# Guide de déploiement — omega-remote-paging

## De zéro à un cluster Proxmox avec paging distant

---

> **Public visé :** quelqu'un qui n'a jamais touché à Proxmox, pas de machine dédiée disponible,
> et veut tester ou déployer omega-remote-paging de bout en bout.

---

## Table des matières

1. [Vue d'ensemble de la solution](#1-vue-densemble)
2. [Prérequis machine](#2-prérequis-machine)
3. [Option A — Tester en local (lab virtuel, recommandé pour débuter)](#3-option-a--lab-virtuel)
4. [Option B — Déploiement sur machines physiques](#4-option-b--machines-physiques)
5. [Option C — ISO Proxmox personnalisé](#5-option-c--iso-proxmox-personnalisé)
6. [Construire le code du projet](#6-construire-le-code)
7. [Déployer omega-daemon sur chaque nœud](#7-déployer-omega-daemon)
8. [Déployer le controller Python](#8-déployer-le-controller-python)
9. [Configurer le hook Proxmox sur les VMs](#9-configurer-le-hook)
10. [Vérifier que tout fonctionne](#10-vérification)
11. [Avantages de la solution](#11-avantages)
12. [Limites et solutions possibles](#12-limites-et-solutions)

> Retour d'expérience réel et historique détaillé des problèmes rencontrés :
> voir [retour-experience-cluster-reel.md](retour-experience-cluster-reel.md).

---

## 1. Vue d'ensemble

```
┌─────────────────────────────────────────────────────┐
│                  Cluster Proxmox                    │
│                                                     │
│  ┌──────────────┐   ┌──────────────┐  ┌──────────┐ │
│  │  Nœud A      │   │  Nœud B      │  │ Nœud C   │ │
│  │  (compute)   │   │  (store B)   │  │ (store C)│ │
│  │              │   │              │  │          │ │
│  │  VMs QEMU    │──▶│  PageStore   │  │ PageStore│ │
│  │  + omega-    │   │  :9100       │  │ :9100    │ │
│  │    daemon    │   │  + omega-    │  │ + omega- │ │
│  │              │   │    daemon    │  │   daemon │ │
│  └──────────────┘   └──────────────┘  └──────────┘ │
│         │                  ▲                 ▲      │
│         └──────────────────┴─────────────────┘      │
│              réseau interne 10.0.0.0/24              │
│                                                     │
│  ┌─────────────────────────────────────────────┐    │
│  │  Controller Python (sur nœud A ou séparé)   │    │
│  │  - surveille la RAM du cluster              │    │
│  │  - décide qui migre où                      │    │
│  │  - déclenche les migrations Proxmox         │    │
│  └─────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────┘
```

**Ce que fait la solution :**
quand une VM sur le nœud A manque de RAM, ses pages mémoire sont déportées sur B ou C
via le réseau (TCP, pages de 4 Ko), évitant le swap disque. Si la pression persiste,
la VM entière est migrée live vers le nœud qui a le plus de RAM libre.

---

## 2. Prérequis machine

### Pour le lab virtuel (Option A)

| Ressource | Minimum | Recommandé |
|-----------|---------|------------|
| RAM hôte  | 16 Go   | 32 Go      |
| CPU hôte  | 4 cœurs avec VT-x/AMD-V | 8 cœurs |
| Disque    | 100 Go libres | 200 Go SSD |
| OS hôte   | Ubuntu 22.04 / Debian 12 / Fedora 38+ | idem |

> **Comment savoir si votre CPU supporte la virtualisation imbriquée :**
> ```bash
> grep -E 'vmx|svm' /proc/cpuinfo | head -1
> # vmx = Intel VT-x   svm = AMD-V
> # Si la commande retourne quelque chose, vous pouvez continuer.
> ```

### Pour machines physiques (Option B)

- 3 machines x86_64 (peuvent être des vieux PC, des mini-PC, des serveurs)
- Chacune avec au minimum 8 Go RAM, 60 Go disque
- Un switch réseau ou routeur avec 3 ports libres
- Câbles ethernet (ou Wi-Fi, mais ethernet fortement recommandé pour la latence)

---

## 3. Option A — Lab virtuel

Cette option crée **3 VMs Proxmox** sur votre machine, reliées par un réseau interne.
C'est la façon la plus simple de tout tester sans matériel dédié.

### 3.1 Installer les outils sur votre machine hôte

```bash
# Ubuntu / Debian
sudo apt update
sudo apt install -y qemu-kvm libvirt-daemon-system virt-manager \
                    bridge-utils wget curl git python3 python3-pip

# Fedora / RHEL
sudo dnf install -y qemu-kvm libvirt virt-manager bridge-utils \
                    wget curl git python3 python3-pip

# Activer libvirt
sudo systemctl enable --now libvirtd
sudo usermod -aG libvirt,kvm $USER
# Déconnectez-vous et reconnectez-vous pour que les groupes soient actifs
```

### 3.2 Activer la virtualisation imbriquée (nested virt)

```bash
# Intel
echo "options kvm-intel nested=1" | sudo tee /etc/modprobe.d/kvm-intel.conf
sudo modprobe -r kvm-intel && sudo modprobe kvm-intel

# AMD
echo "options kvm-amd nested=1" | sudo tee /etc/modprobe.d/kvm-amd.conf
sudo modprobe -r kvm-amd && sudo modprobe kvm-amd

# Vérification
cat /sys/module/kvm_intel/parameters/nested   # doit afficher Y ou 1
# ou
cat /sys/module/kvm_amd/parameters/nested
```

### 3.3 Télécharger l'ISO Proxmox

```bash
mkdir -p ~/lab-proxmox && cd ~/lab-proxmox
wget https://enterprise.proxmox.com/iso/proxmox-ve_8.2-1.iso
# Taille ~1.2 Go — prenez un café
```

> L'ISO est toujours disponible sur https://www.proxmox.com/en/downloads (section Proxmox VE).

### 3.4 Créer un réseau interne pour le cluster

```bash
# Réseau NAT isolé nommé "omega-net" en 10.10.10.0/24
cat > /tmp/omega-net.xml << 'EOF'
<network>
  <name>omega-net</name>
  <forward mode='nat'/>
  <bridge name='virbr-omega' stp='on' delay='0'/>
  <ip address='10.10.10.1' netmask='255.255.255.0'>
    <dhcp>
      <range start='10.10.10.10' end='10.10.10.50'/>
    </dhcp>
  </ip>
</network>
EOF

virsh net-define /tmp/omega-net.xml
virsh net-start omega-net
virsh net-autostart omega-net
```

### 3.5 Créer les 3 disques pour les VMs Proxmox

```bash
cd ~/lab-proxmox

# Nœud A (compute) — 6 Go RAM, 50 Go disque
qemu-img create -f qcow2 pve-a.qcow2 50G

# Nœud B (store primaire) — 4 Go RAM, 50 Go disque
qemu-img create -f qcow2 pve-b.qcow2 50G

# Nœud C (store secondaire) — 4 Go RAM, 50 Go disque
qemu-img create -f qcow2 pve-c.qcow2 50G
```

### 3.6 Installer Proxmox sur chaque VM

**Répéter les étapes 3.6.1 à 3.6.4 pour chaque nœud** (pve-a, pve-b, pve-c).

#### 3.6.1 Démarrer l'installation du nœud A

```bash
virt-install \
  --name pve-a \
  --ram 6144 \
  --vcpus 2 \
  --cpu host \
  --disk path=~/lab-proxmox/pve-a.qcow2,format=qcow2,bus=virtio \
  --cdrom ~/lab-proxmox/proxmox-ve_8.2-1.iso \
  --network network=omega-net,model=virtio \
  --os-variant debian11 \
  --graphics vnc,listen=127.0.0.1,port=5901 \
  --boot cdrom,hd \
  --noautoconsole
```

#### 3.6.2 Se connecter à l'interface graphique d'installation

```bash
# Ouvrir un client VNC sur 127.0.0.1:5901
# Sur Ubuntu : vncviewer 127.0.0.1:5901
# Ou utiliser Remmina (inclus dans Ubuntu)
vncviewer 127.0.0.1:5901
```

#### 3.6.3 Suivre l'installateur Proxmox (écrans successifs)

1. **"Install Proxmox VE"** → Entrée
2. **EULA** → Accept
3. **Disque cible** → le seul disque disponible → Next
4. **Localisation** : Country = France, Timezone = Europe/Paris, Keyboard = fr → Next
5. **Mot de passe root** : choisissez un mot de passe (ex: `omega2026!`) → Next
6. **Réseau** :
   - Hostname : `pve-a.local` (pour le nœud A)
   - IP : `10.10.10.11` (A), `10.10.10.12` (B), `10.10.10.13` (C)
   - Netmask : `255.255.255.0`
   - Gateway : `10.10.10.1`
   - DNS : `8.8.8.8`
   → Next
7. **Résumé** → Install
8. Attendre ~10 min, la VM redémarre automatiquement.

#### 3.6.4 Répéter pour pve-b et pve-c

```bash
# pve-b (port VNC 5902)
virt-install \
  --name pve-b \
  --ram 4096 \
  --vcpus 2 \
  --cpu host \
  --disk path=~/lab-proxmox/pve-b.qcow2,format=qcow2,bus=virtio \
  --cdrom ~/lab-proxmox/proxmox-ve_8.2-1.iso \
  --network network=omega-net,model=virtio \
  --os-variant debian11 \
  --graphics vnc,listen=127.0.0.1,port=5902 \
  --boot cdrom,hd \
  --noautoconsole

# pve-c (port VNC 5903)
virt-install \
  --name pve-c \
  --ram 4096 \
  --vcpus 2 \
  --cpu host \
  --disk path=~/lab-proxmox/pve-c.qcow2,format=qcow2,bus=virtio \
  --cdrom ~/lab-proxmox/proxmox-ve_8.2-1.iso \
  --network network=omega-net,model=virtio \
  --os-variant debian11 \
  --graphics vnc,listen=127.0.0.1,port=5903 \
  --boot cdrom,hd \
  --noautoconsole
```

Mêmes étapes d'installation avec :
- pve-b → hostname `pve-b.local`, IP `10.10.10.12`
- pve-c → hostname `pve-c.local`, IP `10.10.10.13`

### 3.7 Former le cluster Proxmox

Une fois les 3 nœuds installés et démarrés :

```bash
# Depuis votre machine hôte, se connecter en SSH à pve-a
ssh root@10.10.10.11
# Mot de passe : celui choisi à l'installation
```

```bash
# Sur pve-a : créer le cluster
pvecm create omega-cluster

# Vérification
pvecm status
```

```bash
# Sur pve-b : rejoindre le cluster
ssh root@10.10.10.12
pvecm add 10.10.10.11
# Il demandera le mot de passe root de pve-a
```

```bash
# Sur pve-c : rejoindre le cluster
ssh root@10.10.10.13
pvecm add 10.10.10.11
```

```bash
# Retour sur pve-a : vérifier que les 3 nœuds sont là
pvecm status
# Doit afficher : Quorum information, 3 nodes
```

### 3.8 Accéder à l'interface web Proxmox

Depuis votre navigateur : **https://10.10.10.11:8006**

Login : `root` / votre mot de passe.

> Ignorez l'avertissement SSL (certificat auto-signé). Cliquez "Advanced" → "Proceed".

---

## 4. Option B — Machines physiques

Si vous avez 3 machines physiques, le processus est identique mais plus simple :

### 4.1 Graver l'ISO sur une clé USB

```bash
# Sur Linux (remplacez /dev/sdX par votre clé USB — ATTENTION à ne pas écraser votre disque)
lsblk  # identifier la clé USB
sudo dd if=proxmox-ve_8.2-1.iso of=/dev/sdX bs=4M status=progress oflag=sync
```

Ou utilisez **Balena Etcher** (GUI, plus simple) : https://etcher.balena.io/

### 4.2 Installer sur chaque machine

- Brancher la clé USB, démarrer sur la clé (touche F12, F2 ou Del selon le BIOS)
- Suivre les mêmes étapes que 3.6.3
- Adresses IP selon votre réseau local (ex: 192.168.1.11, 192.168.1.12, 192.168.1.13)

### 4.3 Former le cluster

Même procédure que 3.7, avec vos adresses IP réelles.

---

## 5. Option C — ISO Proxmox personnalisé

Cette option crée un ISO qui contient omega-daemon pré-installé et qui se configure
automatiquement au premier démarrage. Utile pour déployer sur beaucoup de machines.

### 5.1 Prérequis

```bash
# Sur Debian/Ubuntu
sudo apt install -y git make build-essential xorriso isolinux \
                    squashfs-tools genisoimage wget curl \
                    cargo rustup

# Rust (si pas déjà installé)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
```

### 5.2 Télécharger et extraire l'ISO Proxmox

```bash
mkdir -p ~/custom-pve/work && cd ~/custom-pve

# Télécharger l'ISO
wget https://enterprise.proxmox.com/iso/proxmox-ve_8.2-1.iso

# Monter l'ISO
sudo mkdir /mnt/pve-iso
sudo mount -o loop proxmox-ve_8.2-1.iso /mnt/pve-iso

# Copier le contenu (en lecture seule → copie modifiable)
cp -a /mnt/pve-iso/. work/
sudo umount /mnt/pve-iso

# Extraire le système de fichiers squashfs
mkdir work/extract
sudo unsquashfs -d work/extract/squashfs-root work/live/filesystem.squashfs
```

### 5.3 Compiler omega-daemon pour l'inclusion dans l'ISO

```bash
# Dans le répertoire du projet omega-remote-paging
cd /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging

# Compiler en mode release (binaire optimisé)
cargo build --release --workspace

# Les binaires produits :
ls -lh target/release/
# omega-daemon    → daemon principal (tous les rôles)
# node-bc-store   → store standalone (si déployé séparément)
# node-a-agent    → agent uffd (si déployé séparément)
```

### 5.4 Injecter les fichiers dans le squashfs

```bash
cd ~/custom-pve

# Créer les répertoires cibles dans le système extrait
sudo mkdir -p work/extract/squashfs-root/opt/omega-remote-paging/bin
sudo mkdir -p work/extract/squashfs-root/etc/systemd/system
sudo mkdir -p work/extract/squashfs-root/var/lib/vz/snippets

# Copier le binaire principal
sudo cp /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging/target/release/omega-daemon \
        work/extract/squashfs-root/opt/omega-remote-paging/bin/

sudo chmod +x work/extract/squashfs-root/opt/omega-remote-paging/bin/omega-daemon

# Copier le hook Proxmox
sudo cp /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging/scripts/proxmox_hook.pl \
        work/extract/squashfs-root/var/lib/vz/snippets/omega-agent-hook.pl

sudo chmod +x work/extract/squashfs-root/var/lib/vz/snippets/omega-agent-hook.pl
```

### 5.5 Créer le service systemd

```bash
sudo tee work/extract/squashfs-root/etc/systemd/system/omega-daemon.service > /dev/null << 'EOF'
[Unit]
Description=omega-daemon — Remote Memory Paging for Proxmox
After=network-online.target pve-cluster.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/omega-remote-paging/bin/omega-daemon \
    --node-id ${HOSTNAME} \
    --node-addr ${NODE_ADDR:-0.0.0.0} \
    --store-port 9100 \
    --api-port 9200 \
    --monitor-vms \
    --log-format json
Restart=on-failure
RestartSec=5s
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF
```

### 5.6 Script de premier démarrage (auto-configuration)

```bash
sudo tee work/extract/squashfs-root/opt/omega-remote-paging/first-boot.sh > /dev/null << 'SCRIPT'
#!/bin/bash
# Exécuté une seule fois au premier démarrage pour auto-configurer omega-daemon

CONF=/etc/omega-daemon.env
[ -f "$CONF" ] && exit 0   # déjà configuré

# Détecter l'IP principale du nœud
NODE_ADDR=$(hostname -I | awk '{print $1}')

cat > "$CONF" << EOF
NODE_ADDR=${NODE_ADDR}
RUST_LOG=info
EOF

# Activer le service
systemctl daemon-reload
systemctl enable omega-daemon
systemctl start omega-daemon

# Marquer comme configuré
touch /etc/omega-daemon.configured
SCRIPT

sudo chmod +x work/extract/squashfs-root/opt/omega-remote-paging/first-boot.sh

# Créer un service one-shot pour le premier démarrage
sudo tee work/extract/squashfs-root/etc/systemd/system/omega-first-boot.service > /dev/null << 'EOF'
[Unit]
Description=omega-daemon first boot configuration
After=network-online.target
ConditionPathExists=!/etc/omega-daemon.configured

[Service]
Type=oneshot
ExecStart=/opt/omega-remote-paging/first-boot.sh
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

# Activer le service dans le squashfs
sudo chroot work/extract/squashfs-root systemctl enable omega-first-boot.service
```

### 5.7 Reconstruire le squashfs et l'ISO

```bash
cd ~/custom-pve

# Reconstruire le squashfs (compressé)
sudo mksquashfs work/extract/squashfs-root work/live/filesystem.squashfs \
    -comp xz -b 1M -no-progress -noappend

# Mettre à jour le checksum
cd work
sudo sh -c 'find . -type f ! -name "md5sum.txt" | sort | xargs md5sum > md5sum.txt'
cd ..

# Reconstruire l'ISO bootable
sudo xorriso \
    -as mkisofs \
    -r -J \
    -b isolinux/isolinux.bin \
    -c isolinux/boot.cat \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    -eltorito-alt-boot \
    -e boot/grub/efi.img \
    -no-emul-boot \
    -isohybrid-gpt-basdat \
    -o ~/proxmox-ve_8.2-omega.iso \
    work/

echo "ISO créé : ~/proxmox-ve_8.2-omega.iso"
```

Vous pouvez maintenant graver cet ISO (section 4.1) ou l'utiliser dans virt-install
(section 3.6) à la place de l'ISO officiel.

---

## 6. Construire le code

Que vous utilisiez l'Option A, B ou C, il faut compiler le code Rust.

### 6.1 Installer Rust sur votre machine de développement

```bash
# Téléchargement et installation de rustup (gestionnaire de toolchain Rust)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Appuyer sur 1 (installation standard) puis Entrée
# Recharger le shell
source ~/.cargo/env

# Vérification
rustc --version   # doit afficher rustc 1.7x.x
cargo --version   # doit afficher cargo 1.7x.x
```

### 6.2 Compiler le projet

```bash
cd /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging

# Build complet (mode debug — plus rapide à compiler)
cargo build --workspace

# Build production (optimisé — binaires plus petits et rapides)
cargo build --release --workspace

# Binaires produits dans target/release/ :
#   omega-daemon    (~15 Mo) — daemon unifié (store + API + control + balloon)
#   node-bc-store   (~8 Mo)  — store standalone (déploiement sans le daemon complet)
#   node-a-agent    (~10 Mo) — agent uffd (si déployé séparément de omega-daemon)
```

### 6.3 Lancer les tests

```bash
# Tests Rust
cargo test --workspace

# Tests Python (controller + protocole)
cd /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging
pip3 install -e controller/
python3 -m pytest controller/tests/ tests/protocol/ -v
# Résultat attendu : 67 passed
```

---

## 7. Déployer omega-daemon

Cette section s'applique que vous soyez en lab virtuel ou sur machines physiques.
**À répéter sur chaque nœud (pve-a, pve-b, pve-c).**

### 7.1 Copier le binaire sur les nœuds

```bash
# Depuis votre machine de développement
# (remplacez 10.10.10.11 par l'IP de chaque nœud)

scp target/release/omega-daemon root@10.10.10.11:/opt/omega-remote-paging/bin/
scp target/release/omega-daemon root@10.10.10.12:/opt/omega-remote-paging/bin/
scp target/release/omega-daemon root@10.10.10.13:/opt/omega-remote-paging/bin/
```

Ou utiliser le script de déploiement inclus :

```bash
# Éditer scripts/deploy.sh pour renseigner les adresses IP de vos nœuds
nano scripts/deploy.sh

# Puis déployer
bash scripts/deploy.sh
```

### 7.2 Créer le service systemd sur chaque nœud

Se connecter en SSH à chaque nœud et exécuter :

```bash
ssh root@10.10.10.11   # pve-a

# Créer le répertoire si absent
mkdir -p /opt/omega-remote-paging/bin
chmod +x /opt/omega-remote-paging/bin/omega-daemon

# Créer le service
cat > /etc/systemd/system/omega-daemon.service << 'EOF'
[Unit]
Description=omega-daemon — Remote Memory Paging for Proxmox
After=network-online.target pve-cluster.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/omega-remote-paging/bin/omega-daemon \
    --node-id pve-a \
    --node-addr 10.10.10.11 \
    --peers 10.10.10.12:9200,10.10.10.13:9200 \
    --store-port 9100 \
    --api-port 9200 \
    --monitor-vms \
    --log-format json
Restart=on-failure
RestartSec=5s
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

# Activer et démarrer
systemctl daemon-reload
systemctl enable omega-daemon
systemctl start omega-daemon

# Vérifier que ça tourne
systemctl status omega-daemon
journalctl -u omega-daemon -f --no-pager   # logs en temps réel (Ctrl+C pour quitter)
```

**Adapter pour pve-b (10.10.10.12) et pve-c (10.10.10.13)** :
- Changer `--node-id` (pve-b, pve-c)
- Changer `--node-addr` (10.10.10.12, 10.10.10.13)
- Les `--peers` pointent vers les deux autres nœuds

### 7.3 Vérifier que les ports sont ouverts

```bash
# Depuis pve-a, tester que pve-b répond sur le port store (9100) et API (9200)
curl http://10.10.10.12:9200/api/status
# Doit retourner un JSON avec l'état du nœud B

# Tester le port de contrôle
curl http://10.10.10.11:9300/control/status
# Doit retourner un JSON avec les métriques du daemon A
```

### 7.4 Ouvrir les ports dans le pare-feu Proxmox (si actif)

```bash
# Sur chaque nœud
# Port store TCP (paging)
iptables -I INPUT -p tcp --dport 9100 -j ACCEPT
# Port API HTTP (cluster state)
iptables -I INPUT -p tcp --dport 9200 -j ACCEPT
# Port contrôle HTTP (controller → daemon)
iptables -I INPUT -p tcp --dport 9300 -j ACCEPT

# Rendre persistant
apt install -y iptables-persistent
netfilter-persistent save
```

---

## 8. Déployer le controller Python

Le controller tourne idéalement sur pve-a (ou sur une machine séparée).

### 8.1 Installer Python et les dépendances

```bash
ssh root@10.10.10.11

apt install -y python3 python3-pip python3-venv

# Copier le controller (depuis votre machine de dev)
# (depuis la machine hôte)
scp -r controller/ root@10.10.10.11:/opt/omega-remote-paging/controller/
```

```bash
# Sur pve-a
cd /opt/omega-remote-paging/controller
python3 -m venv .venv
source .venv/bin/activate
pip install -e .
```

### 8.2 Lancer le controller en monitoring

```bash
# Sur pve-a
cd /opt/omega-remote-paging/controller
source .venv/bin/activate

# Mode surveillance (affiche l'état du cluster toutes les 10s)
python3 -m controller.main \
    --peers 10.10.10.11:9200,10.10.10.12:9200,10.10.10.13:9200 \
    --mode monitor \
    --interval 10

# Exemple de sortie :
# [10:15:02] pve-a : RAM 78% (1638/2048 Mio), 0 pages distantes, 2 VMs
# [10:15:02] pve-b : RAM 31% (639/2048 Mio), 450 pages stockées
# [10:15:02] pve-c : RAM 28% (573/2048 Mio), 0 pages stockées
# [10:15:02] DÉCISION : enable_remote (pve-a sous pression)
```

### 8.3 Lancer le daemon de migration automatique

```bash
python3 -m controller.main \
    --peers 10.10.10.11:9200,10.10.10.12:9200,10.10.10.13:9200 \
    --proxmox-url https://10.10.10.11:8006 \
    --proxmox-token root@pam!omega=VOTRE_TOKEN \
    --mode daemon \
    --min-remote-pages 256 \
    --strategy best_fit
```

> **Créer le token API Proxmox :**
> Dans l'interface web → Datacenter → API Tokens → Add
> - User: root@pam
> - Token ID: omega
> - Cocher "Privilege Separation" = Non (pour simplifier)
> - Copier le token affiché (visible une seule fois)

### 8.4 Service systemd pour le controller

```bash
cat > /etc/systemd/system/omega-controller.service << 'EOF'
[Unit]
Description=omega-controller — Migration Policy Engine
After=omega-daemon.service
Requires=omega-daemon.service

[Service]
Type=simple
WorkingDirectory=/opt/omega-remote-paging/controller
ExecStart=/opt/omega-remote-paging/controller/.venv/bin/python \
    -m controller.main \
    --peers 10.10.10.11:9200,10.10.10.12:9200,10.10.10.13:9200 \
    --proxmox-url https://10.10.10.11:8006 \
    --proxmox-token root@pam!omega=VOTRE_TOKEN \
    --mode daemon
Restart=on-failure
RestartSec=10s

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable omega-controller
systemctl start omega-controller
```

---

## 9. Configurer le hook

Le hook Proxmox permet à omega-daemon de savoir quand une VM démarre/s'arrête.

### 9.1 Installer le hook

```bash
# Sur pve-a (le nœud compute)
cp /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging/scripts/proxmox_hook.pl \
   /var/lib/vz/snippets/omega-agent-hook.pl
chmod +x /var/lib/vz/snippets/omega-agent-hook.pl
```

### 9.2 Créer une VM de test

Dans l'interface web Proxmox (https://10.10.10.11:8006) :

1. Cliquer **"Create VM"** (bouton bleu en haut à droite)
2. Remplir :
   - VM ID: `100`
   - Name: `vm-test-paging`
3. OS → `Do not use any media` (ou utiliser une ISO Linux légère comme Alpine)
4. System → par défaut
5. Disks → 10 Go
6. CPU → 1 socket, 2 cores
7. Memory → **512 Mo** (petit intentionnellement pour déclencher la pression mémoire)
8. Network → `vmbr0`
9. Finish

### 9.3 Attacher le hook à la VM

```bash
# Sur pve-a, en SSH
qm set 100 --hookscript local:snippets/omega-agent-hook.pl

# Vérification
qm config 100 | grep hookscript
# hookscript: local:snippets/omega-agent-hook.pl
```

### 9.4 Démarrer la VM et observer les logs

```bash
# Démarrer la VM
qm start 100

# Observer les logs du hook
tail -f /var/log/omega-hook.log

# Observer les logs du daemon
journalctl -u omega-daemon -f
```

---

## 10. Vérification

### 10.1 Vérifier l'état du cluster via l'API

```bash
# État du nœud A
curl -s http://10.10.10.11:9200/api/status | python3 -m json.tool

# Métriques Prometheus du nœud A
curl http://10.10.10.11:9300/control/metrics

# État complet du contrôle
curl -s http://10.10.10.11:9300/control/status | python3 -m json.tool
```

### 10.2 Test de charge mémoire (déclencher le paging)

```bash
# Dans la VM de test (via console Proxmox ou SSH)
# Installer un outil de stress
apt install -y stress-ng

# Consommer 450 Mo de RAM (sur une VM de 512 Mo → ~88% d'usage)
stress-ng --vm 1 --vm-bytes 450M --timeout 60s &

# Observer depuis pve-a les métriques du daemon
watch -n 2 'curl -s http://10.10.10.11:9300/control/status | python3 -m json.tool'
```

### 10.3 Tester l'éviction manuelle

```bash
# Demander au daemon de pve-a d'évincer 100 pages de la VM 100
curl -s -X POST http://10.10.10.11:9300/control/evict/100 \
     -H 'Content-Type: application/json' \
     -d '{"count": 100}'
```

### 10.4 Vérifier les pages sur le store distant

```bash
# État du store de pve-b
curl -s http://10.10.10.12:9200/api/status | python3 -m json.tool
# "pages_stored" doit augmenter quand pve-a est sous pression
```

### 10.5 Tester la migration automatique

```bash
# Depuis pve-a, simuler une forte pression (plusieurs VMs)
# Observer les logs du controller
journalctl -u omega-controller -f

# Quand le seuil est dépassé, vous devriez voir :
# MIGRATION : vmid=100 pve-a → pve-b (512 Mio RAM, 1024 pages distantes, confiance=0.85)
# migration vmid=100 en cours (UPID=...)
# migration vmid=100 RÉUSSIE en 45s
```

---

## 11. Avantages

| Avantage | Détail |
|----------|--------|
| **Latence réseau < swap disque** | Un GET_PAGE sur réseau 1 Gbps ≈ 0.1–1 ms. Un accès disque rotatif ≈ 5–15 ms. SSD NVMe ≈ 0.1 ms (mais partagé). |
| **Zéro modification QEMU/kernel** | Tout fonctionne avec un Proxmox standard. Le hook et le daemon s'installent en post-install. |
| **Migration transparente** | Les VMs ne voient rien. Pas de coupure réseau, pas d'arrêt. |
| **Politique configurable** | Seuils, stratégie (best_fit/first_fit/most_free), marge de sécurité, délai entre cycles. |
| **Métriques Prometheus prêtes** | `/control/metrics` expose les données dans un format compatible Grafana. |
| **Multi-store déterministe** | `page_id % num_stores` → pas de métadonnées, pas de coordination, distribution automatique. |
| **Balloon driver intégré** | Surveillance de la RAM guest (pas seulement host) pour décider plus tôt. |

---

## 12. Limites et solutions

### L1 — L'agent uffd n'est pas encore branché au daemon

**Description :** `omega-daemon` gère le store et la politique, mais l'interception des
page faults (userfaultfd) est dans `node-a-agent` qui est un binaire séparé.
En V4, les deux coexistent mais ne se parlent pas directement en temps réel.

**Impact :** Le paging distant n'est pas encore transparent au niveau kernel.
Les pages ne sont actuellement déplacées que sur décision explicite (via `/control/evict`),
pas automatiquement à chaque page fault.

**Solution possible :** Intégrer `node-a-agent` comme module dans `omega-daemon` via
un canal Tokio (`tokio::sync::mpsc`). Le handler uffd envoie les événements
page-fault au moteur d'éviction qui décide de fetch en local ou depuis le store distant.
Effort estimé : 2–3 semaines.

---

### L2 — Pas de persistance des pages après arrêt du daemon

**Description :** Le store est entièrement en mémoire (`HashMap` dans `PageStore`).
Si `omega-daemon` crash ou est redémarré, toutes les pages distantes sont perdues.

**Impact :** Les VMs dont les pages étaient sur le store distant peuvent rencontrer
des erreurs si leur agent essaie de les récupérer après un redémarrage du store.

**Solution possible :** Ajouter un journal d'écriture (write-ahead log) avec `sled`
ou `RocksDB`. Les pages sont d'abord écrites sur disque, puis servies depuis le cache
mémoire. Un `sync` au démarrage reconstruit le cache. Effort : 1 semaine.

---

### L3 — Pas de chiffrement sur le canal de paging

**Description :** Les pages mémoire transitent en clair sur le réseau interne via TCP.
Quiconque peut sniffer le réseau peut lire les données des VMs.

**Impact :** Problème de sécurité si le réseau de stockage n'est pas isolé (VLAN dédié
non configuré, Wi-Fi partagé, etc.).

**Solution possible :**
- **Court terme :** Isoler le trafic de paging sur un VLAN dédié (switch manageable requis).
- **Long terme :** Encapsuler le canal TCP dans TLS avec `rustls`. Chaque nœud présente
  un certificat auto-signé, vérification par empreinte fixée (TOFU).
  Surcoût CPU estimé : 5–10% pour AES-GCM sur hardware moderne avec AES-NI.

---

### L4 — Le controller Python ne résiste pas à sa propre panne

**Description :** Si le process Python du controller s'arrête, les décisions de migration
ne sont plus prises. Le daemon continue de stocker des pages, mais personne ne commande
les migrations.

**Impact :** En cas de crash du controller, les nœuds surchargés restent surchargés.
Pas de perte de données, mais dégradation des performances.

**Solution possible :**
- **Immédiat :** Systemd avec `Restart=always` (déjà configuré dans la section 8.4).
- **Robuste :** Déployer le controller en actif/passif avec `keepalived` ou dans un pod
  Kubernetes avec `replicaCount: 2` et leader election via un lock etcd/Redis.

---

### L5 — Ceph RBD requis pour la migration live

**Description :** La migration live de VMs Proxmox (`qm migrate ... --online`) nécessite
que le disque de la VM soit sur un stockage partagé. Ce projet utilise **Ceph RBD**.
Avec Ceph, le disque est déjà accessible depuis tous les nœuds : seule la RAM est
transférée pendant la migration, pas le disque.

**Setup :** Ceph est déployé via `pveceph` sur les 3 nœuds (voir `cluster-physique.md`
section 7 ou `cluster-kvm.md` section 8).
- Chaque nœud fournit un disque dédié en OSD (séparé du disque système).
- Pool `vm-pool` avec réplication x3 (prod) ou x2 (lab).
- Les VMs sont créées avec leurs disques sur le storage `ceph-vms` (RBD).

---

### L6 — Pas de QoS réseau entre paging et trafic VM

**Description :** Le trafic de paging (TCP 9100) partage la même interface réseau que
le trafic des VMs et de l'administration Proxmox. En cas de burst de paging intense,
il peut saturer la bande passante et dégrader les performances réseau des VMs.

**Impact :** Pendant une éviction massive, latence réseau des VMs augmentée.

**Solution possible :**
- **Dédier une interface réseau** au trafic de paging (une carte réseau par nœud,
  ou un VLAN séparé avec QoS sur le switch).
- **tc/qdisc Linux :** Appliquer une limite de bande passante sur le port 9100 avec
  `tc qdisc add dev eth0 root handle 1: htb` pour éviter de saturer l'interface principale.

---

### L7 — La solution ne fonctionne qu'avec QEMU/KVM

**Description :** Le hook Proxmox, le client QMP balloon, et l'agent uffd sont
spécifiques à QEMU/KVM. Les containers LXC de Proxmox ne sont pas supportés.

**Impact :** Les workloads containerisés sur Proxmox (LXC) ne bénéficient pas du paging distant.

**Solution possible :** Pour LXC, le paging distant n'a pas de sens au niveau mémoire
(les containers partagent le kernel de l'hôte). La vraie solution serait d'intercepter
les cgroup memory events (`memory.pressure`) et d'agir au niveau du nœud hôte plutôt
qu'au niveau VM.

---

### L8 — Le placement ne tient pas compte des affinités CPU/NUMA

**Description :** Le `PlacementEngine` optimise uniquement sur la RAM libre.
Il ne tient pas compte des contraintes NUMA, des affinités CPU, de la topologie réseau
(distance entre nœuds), ni des SLA de latence des applications.

**Impact :** Une VM migrée peut se retrouver sur un nœud physiquement éloigné
(différent rack, différent datacenter) avec une latence réseau plus élevée.

**Solution possible :** Ajouter des **tags de contrainte** par nœud dans la config
(`rack`, `zone`, `max_latency_ms`) et les intégrer comme poids dans le score du
`PlacementEngine`. La décision devient multi-critères (RAM + topologie + latence).

---

*Document généré le 8 avril 2026 — omega-remote-paging V4*
