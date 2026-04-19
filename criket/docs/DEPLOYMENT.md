# Cricket GPU Remoting — Cahier de conception et de déploiement

Document unique couvrant la conception, les dépendances, l'installation,
la configuration, l'utilisation et le dépannage de Cricket pour Proxmox
VE 9.1.1.

- **Version des paquets** : `0.0.1`
- **Plateforme cible serveur** : Proxmox VE 9.1.1 (Debian 13 Trixie, kernel 6.14)
- **Plateforme cliente** : Debian 12+/Ubuntu 22.04+/Proxmox LXC ou VM KVM
- **Transport** : ONC RPC TCP (port fixe `58648`)

---

## Table des matières

1. [Vue d'ensemble](#1-vue-densemble)
2. [Architecture](#2-architecture)
3. [Dépendances — côté serveur](#3-dépendances--côté-serveur)
4. [Dépendances — côté client](#4-dépendances--côté-client)
5. [Installation du serveur depuis zéro](#5-installation-du-serveur-depuis-zéro)
6. [Installation du client](#6-installation-du-client)
7. [Compilation depuis les sources](#7-compilation-depuis-les-sources)
8. [Construction des paquets `.deb`](#8-construction-des-paquets-deb)
9. [Déploiement via paquets](#9-déploiement-via-paquets)
10. [Configuration](#10-configuration)
11. [Commandes utiles](#11-commandes-utiles)
12. [Dépannage](#12-dépannage)
13. [Limitations connues](#13-limitations-connues)

---

## 1. Vue d'ensemble

Cricket est une couche de virtualisation CUDA basée sur ONC RPC permettant
d'exécuter des applications CUDA sur une machine ne disposant pas de GPU, en
déportant les appels vers un serveur équipé d'un GPU NVIDIA.

- **Pas de module noyau** requis
- **LD_PRELOAD** côté client intercepte les appels CUDA
- Compatible avec **Proxmox VE 9.1.1** (Debian 13, kernel 6.14, glibc 2.41)
- Testé avec **CUDA 12.4** et **driver NVIDIA 550+**

---

## 2. Architecture

```
┌──────────────────────────┐                ┌────────────────────────────┐
│    Client (VM/LXC)       │                │   Serveur (Proxmox PVE)    │
│                          │                │                            │
│  Application CUDA        │                │   cricket-rpc-server       │
│       │                  │                │            │               │
│       ▼                  │   TCP 58648    │            ▼               │
│  cricket-client.so  ─────┼────────────────┼──► libtirpc (ONC RPC)       │
│  (LD_PRELOAD)            │                │            │               │
│                          │                │            ▼               │
│                          │                │   libcudart / libcuda      │
│                          │                │            │               │
│                          │                │            ▼               │
│                          │                │   NVIDIA driver + GPU      │
└──────────────────────────┘                └────────────────────────────┘
```

- **Serveur** : exécute réellement les kernels CUDA sur le GPU.
- **Client** : intercepte les appels CUDA via `LD_PRELOAD` et les transmet
  au serveur via TCP.
- **Transport** : ONC RPC (libtirpc), portmapper sur port 111, port RPC fixe
  `58648`.

### Composants logiciels produits par le build

| Binaire              | Rôle                              | Installé sur |
|----------------------|-----------------------------------|--------------|
| `cricket-rpc-server` | Démon RPC qui appelle CUDA        | Serveur      |
| `cricket-client.so`  | Bibliothèque LD_PRELOAD           | Client       |
| `libtirpc.so.3`      | Transport ONC RPC (submodule)     | Serveur et client |
| `cricket-run`        | Wrapper shell (simplifie l'usage) | Client       |

---

## 3. Dépendances — côté serveur

### 3.1 Système d'exploitation

- **Proxmox VE 9.1.1** (ou Debian 13 Trixie équivalent)
- Kernel Linux 6.14 (fourni par PVE)
- glibc 2.41 (fourni par Trixie)

### 3.2 Paquets système (runtime)

```bash
apt install -y \
    libc6 \
    libssl3 \
    libelf1 \
    libtirpc-common \
    rpcbind
```

| Paquet             | Rôle                                              |
|--------------------|---------------------------------------------------|
| `libc6`            | glibc 2.41                                        |
| `libssl3`          | OpenSSL pour chiffrement RPC                      |
| `libelf1`          | Parsing ELF (découverte de kernels CUDA)          |
| `libtirpc-common`  | Fichiers communs pour libtirpc                    |
| `rpcbind`          | Portmapper ONC RPC (port 111)                     |

### 3.3 Paquets pour la compilation (uniquement si build from source)

```bash
apt install -y \
    build-essential \
    git \
    make \
    gcc \
    g++ \
    pkg-config \
    autoconf \
    automake \
    libtool \
    m4 \
    libssl-dev \
    libelf-dev \
    libtirpc-dev \
    linux-headers-$(uname -r)
```

### 3.4 Driver NVIDIA

- **Version minimale** : 550.xx
- Installation via runfile (recommandé sur Proxmox) :

```bash
# Blacklister nouveau
echo -e "blacklist nouveau\noptions nouveau modeset=0" > /etc/modprobe.d/blacklist-nouveau.conf
update-initramfs -u

# Installer le driver
wget https://us.download.nvidia.com/XFree86/Linux-x86_64/550.127.05/NVIDIA-Linux-x86_64-550.127.05.run
chmod +x NVIDIA-Linux-x86_64-550.127.05.run
./NVIDIA-Linux-x86_64-550.127.05.run --dkms --silent

# Vérifier
nvidia-smi
```

### 3.5 Toolkit CUDA

- **Version recommandée** : CUDA 12.4 (testé)
- **Installation via runfile** (évite les problèmes de signature du repo APT
  sur Debian 13) :

```bash
cd /tmp
wget https://developer.download.nvidia.com/compute/cuda/12.4.1/local_installers/cuda_12.4.1_550.54.15_linux.run
sh cuda_12.4.1_550.54.15_linux.run --toolkit --silent --override --toolkitpath=/usr/local/cuda-12.4

# Symlink requis par Cricket
ln -sf /usr/local/cuda-12.4 /usr/local/cuda

# PATH
echo 'export PATH=/usr/local/cuda/bin:$PATH' >> /etc/profile.d/cuda.sh
echo 'export LD_LIBRARY_PATH=/usr/local/cuda/lib64:$LD_LIBRARY_PATH' >> /etc/profile.d/cuda.sh
source /etc/profile.d/cuda.sh

# Vérifier
nvcc --version
```

### 3.6 Services système

```bash
systemctl enable --now rpcbind
```

### 3.7 Matériel

- Au moins un GPU NVIDIA avec compute capability ≥ 6.1
- RAM recommandée : ≥ 8 Go
- Accès réseau vers les clients (port `58648/tcp` + `111/tcp,udp`)

---

## 4. Dépendances — côté client

### 4.1 Système d'exploitation

N'importe quelle distribution Linux récente :

- Debian 12 / 13
- Ubuntu 22.04 / 24.04
- Conteneur LXC Proxmox
- VM KVM

### 4.2 Paquets système (runtime)

```bash
sudo apt install -y \
    libc6 \
    libssl3 \
    libelf1 \
    libtirpc3 \
    rpcbind
```

| Paquet       | Rôle                                               |
|--------------|----------------------------------------------------|
| `libc6`      | glibc                                              |
| `libssl3`    | OpenSSL                                            |
| `libelf1`    | Parsing ELF                                        |
| `libtirpc3`  | Client ONC RPC                                     |
| `rpcbind`    | Nécessaire pour la résolution de ports RPC         |

### 4.3 Pas de driver NVIDIA requis

Le client **n'a pas besoin** du driver NVIDIA ni du toolkit CUDA.
La compilation du binaire CUDA de l'utilisateur final nécessite `nvcc` sur
la machine qui **compile l'application**, mais pas à l'exécution via Cricket.

### 4.4 Utilitaires recommandés

```bash
sudo apt install -y netcat-openbsd   # pour tester la connectivité
```

### 4.5 Réseau

- Accès TCP au serveur sur le port `58648`
- Accès TCP au serveur sur le port `111` (rpcbind, si utilisé pour découverte)

---

## 5. Installation du serveur depuis zéro

### 5.1 Préparation Proxmox

```bash
# Désactiver le repo enterprise (si pas d'abonnement)
sed -i 's|^deb |# deb |' /etc/apt/sources.list.d/pve-enterprise.list 2>/dev/null
sed -i 's|^deb |# deb |' /etc/apt/sources.list.d/ceph.list 2>/dev/null

# Activer no-subscription
echo "deb http://download.proxmox.com/debian/pve trixie pve-no-subscription" \
    > /etc/apt/sources.list.d/pve-no-subscription.list

apt update
apt upgrade -y
```

### 5.2 Installation des dépendances

```bash
apt install -y build-essential git make gcc g++ pkg-config \
    autoconf automake libtool m4 \
    libssl-dev libelf-dev libtirpc-dev rpcbind \
    linux-headers-$(uname -r) dkms curl wget netcat-openbsd
```

### 5.3 Installation du driver NVIDIA

Voir section [3.4](#34-driver-nvidia).

### 5.4 Installation du toolkit CUDA

Voir section [3.5](#35-toolkit-cuda).

### 5.5 Activation de rpcbind

```bash
systemctl enable --now rpcbind
systemctl status rpcbind
```

### 5.6 Firewall (si client distant)

```bash
iptables -A INPUT -p tcp --dport 111   -j ACCEPT
iptables -A INPUT -p udp --dport 111   -j ACCEPT
iptables -A INPUT -p tcp --dport 58648 -j ACCEPT
```

Sur Proxmox, adapter via l'interface `Datacenter → Firewall`.

---

## 6. Installation du client

### 6.1 Dépendances

```bash
sudo apt update
sudo apt install -y libc6 libssl3 libelf1 libtirpc3 rpcbind netcat-openbsd
```

### 6.2 Test de connectivité au serveur

```bash
nc -zv <IP_SERVEUR_PVE> 58648
```

---

## 7. Compilation depuis les sources

### 7.1 Cloner le dépôt

```bash
cd /opt
git clone --recurse-submodules https://github.com/TF-Jordan/GPU_BAND_Sched.git
cd /opt/GPU_BAND_Sched/criket
git submodule update --init --recursive
```

### 7.2 Build (uniquement libtirpc + cpu)

Les tests `tests/test_apps/` peuvent échouer avec CUDA 12.4 + glibc 2.41 (voir
[section 13](#13-limitations-connues)). On se limite à ce qui est nécessaire.

```bash
cd /opt/GPU_BAND_Sched/criket
make libtirpc
make cpu

# Récupérer les binaires dans bin/
mkdir -p bin
cp cpu/cricket-client.so bin/
cp cpu/cricket-rpc-server bin/
cp submodules/libtirpc/install/lib/libtirpc.so.3 bin/
ls -la bin/
```

### 7.3 Build avec tous les tests (optionnel, nécessite CUDA 12.6+)

```bash
cd /opt/GPU_BAND_Sched/criket
make
```

---

## 8. Construction des paquets `.deb`

### 8.1 Pré-requis

- `dpkg-deb` (`apt install dpkg`)
- Binaires disponibles dans l'un des répertoires :
  - `criket/docs/`
  - `criket/bin/`
  - `criket/cpu/` + `criket/submodules/libtirpc/install/lib/`

### 8.2 Construction

```bash
cd /opt/GPU_BAND_Sched/criket/packaging
./build-packages.sh
```

Résultat dans `packaging/dist/` :

```
cricket-server_0.0.1_amd64.deb
cricket-client_0.0.1_amd64.deb
```

### 8.3 Override de version

```bash
./build-packages.sh 0.0.2
```

---

## 9. Déploiement via paquets

### 9.1 Sur le serveur Proxmox

```bash
scp cricket-server_0.0.1_amd64.deb root@<IP_PVE>:/tmp/
ssh root@<IP_PVE>
apt install -y /tmp/cricket-server_0.0.1_amd64.deb
systemctl status cricket-rpc
```

Le `postinst` effectue automatiquement :

1. Création de l'utilisateur système `cricket` (groupes `video`, `render`)
2. Activation de `rpcbind.service`
3. Symlink `/usr/local/cuda` vers la première installation détectée
4. `systemctl daemon-reload` + `enable --now cricket-rpc`
5. Affichage du port d'écoute

### 9.2 Sur le client

```bash
scp cricket-client_0.0.1_amd64.deb user@<IP_CLIENT>:/tmp/
ssh user@<IP_CLIENT>
sudo apt install -y /tmp/cricket-client_0.0.1_amd64.deb

# Éditer la config
sudoedit /etc/cricket/client.conf
# Ajuster REMOTE_GPU_ADDRESS et REMOTE_GPU_PORT
```

---

## 10. Configuration

### 10.1 Serveur — `/etc/cricket/server.conf`

```ini
TRANSPORT=tcp        # tcp | ib | shm
RPC_PORT=58648       # port fixe d'écoute
LOG_LEVEL=info       # debug | info | warn | error
CUDA_HOME=/usr/local/cuda
```

Appliquer un changement :

```bash
systemctl restart cricket-rpc
```

### 10.2 Client — `/etc/cricket/client.conf`

```ini
REMOTE_GPU_ADDRESS=192.168.123.101
REMOTE_GPU_PORT=58648
TRANSPORT=tcp
AUTO_PRELOAD=false   # true = LD_PRELOAD automatique pour tous les shells
```

### 10.3 Unité systemd — `/etc/systemd/system/cricket-rpc.service`

- `User=cricket`, `Group=cricket`
- `SupplementaryGroups=video render` (accès GPU)
- `Restart=on-failure`
- `EnvironmentFile=/etc/cricket/server.conf`

---

## 11. Commandes utiles

### 11.1 Serveur

```bash
# Démarrer/arrêter le service
systemctl start cricket-rpc
systemctl stop cricket-rpc
systemctl restart cricket-rpc
systemctl status cricket-rpc

# Journaux (temps réel)
journalctl -u cricket-rpc -f

# Vérifier l'enregistrement RPC
rpcinfo -p localhost

# Vérifier le port d'écoute
ss -tlnp | grep 58648
# ou
netstat -tlnp | grep 58648

# Vérifier le GPU
nvidia-smi

# Recharger la config après modification
systemctl daemon-reload
systemctl restart cricket-rpc
```

### 11.2 Client

```bash
# Lancement simple
cricket-run nvidia-smi
cricket-run python3 train.py
cricket-run ./mon_app_cuda

# Mode debug Cricket
CRICKET_DEBUG=1 cricket-run python3 train.py

# Utilisation manuelle (sans wrapper)
export REMOTE_GPU_ADDRESS=192.168.123.101:58648
export LD_PRELOAD=/usr/local/lib/cricket/cricket-client.so
export LD_LIBRARY_PATH=/usr/local/lib/cricket:$LD_LIBRARY_PATH
./mon_app_cuda

# Tester la connectivité
nc -zv 192.168.123.101 58648
```

### 11.3 Validation end-to-end

```bash
# Côté serveur — doit afficher "listening on port 58648"
journalctl -u cricket-rpc --since "5 min ago"

# Côté client
cricket-run nvidia-smi
# Doit renvoyer les infos du GPU distant via RPC
```

### 11.4 Désinstallation

```bash
# Serveur
apt remove cricket-server          # conserve /etc/cricket
apt purge  cricket-server          # supprime tout

# Client
apt remove cricket-client
apt purge  cricket-client
```

---

## 12. Dépannage

### 12.1 Le service `cricket-rpc` ne démarre pas

```bash
journalctl -u cricket-rpc -n 50
```

Causes fréquentes :

- `nvidia-smi` introuvable → installer le driver NVIDIA
- `/usr/local/cuda` absent → `ln -sf /usr/local/cuda-12.4 /usr/local/cuda`
- `rpcbind` non démarré → `systemctl start rpcbind`

### 12.2 Le client ne se connecte pas

```bash
# Vérifier la config
cat /etc/cricket/client.conf

# Tester la connectivité réseau
nc -zv <IP_SERVEUR> 58648

# Vérifier que le serveur écoute
ssh root@<IP_SERVEUR> 'ss -tlnp | grep 58648'

# Vérifier le firewall
ssh root@<IP_SERVEUR> 'iptables -L INPUT -n | grep 58648'
```

### 12.3 Erreur `libtirpc.so.3: cannot open shared object file`

```bash
# Ajouter le répertoire au cache ld
echo "/usr/local/lib/cricket" > /etc/ld.so.conf.d/cricket.conf
ldconfig

# Ou via variable d'environnement
export LD_LIBRARY_PATH=/usr/local/lib/cricket:$LD_LIBRARY_PATH
```

### 12.4 L'application CUDA se fige

- Augmenter le timeout RPC (`TRANSPORT=tcp` est le seul transport supporté
  actuellement)
- Vérifier les logs serveur : `journalctl -u cricket-rpc -f`
- Activer le debug côté client : `CRICKET_DEBUG=1 cricket-run ...`

### 12.5 Erreur de version CUDA

Si le client compile avec une version CUDA incompatible avec celle du serveur,
les signatures RPC peuvent ne pas correspondre. S'assurer que :

- Le toolkit CUDA côté **développeur** (machine qui compile l'app) est
  compatible avec la version CUDA côté **serveur**.

---

## 13. Limitations connues

### 13.1 Build des tests CUDA samples

Avec CUDA 12.4 + glibc 2.41 (Debian 13), la compilation des applications de
test `tests/test_apps/` échoue avec :

```
linkage specification is incompatible with previous "memset"
```

**Contournement** : build uniquement `libtirpc` et `cpu` (voir
[section 7.2](#72-build-uniquement-libtirpc--cpu)). Les binaires Cricket
eux-mêmes sont correctement produits.

**Résolution permanente** : utiliser CUDA 12.6 ou 13.x, compatibles avec
glibc 2.41.

### 13.2 Transport

Seul **TCP** est testé et activé par défaut. Les transports Infiniband et
Shared Memory existent dans le code mais ne sont pas supportés par le
packaging actuel.

### 13.3 Port RPC

Par défaut, Cricket choisit un port dynamique. Le packaging force `RPC_PORT=58648`
via le fichier de configuration, mais si le code Cricket ignore ce paramètre
lors d'une version future, il faudra patcher `cpu/cpu-server.c`.

### 13.4 Sécurité

- Aucune authentification entre client et serveur par défaut
- **Recommandation** : déployer Cricket sur un réseau privé isolé (VLAN,
  bridge Proxmox dédié) ou via un tunnel VPN/Wireguard.

### 13.5 Compatibilité

- Driver NVIDIA ≥ 550 requis pour CUDA 12.4
- Certaines APIs driver non documentées peuvent casser avec des versions
  majeures du driver NVIDIA (fichier `cpu/cpu-client-driver-hidden.c`)

---

## Annexe A — Arborescence après installation

### Serveur

```
/usr/local/bin/cricket-rpc-server
/usr/local/lib/cricket/libtirpc.so.3
/etc/cricket/server.conf
/etc/systemd/system/cricket-rpc.service
/etc/profile.d/cricket-server.sh
/var/lib/cricket/                   # home de l'utilisateur cricket
```

### Client

```
/usr/local/bin/cricket-run
/usr/local/lib/cricket/cricket-client.so
/usr/local/lib/cricket/libtirpc.so.3
/etc/cricket/client.conf
/etc/profile.d/cricket-client.sh
```

---

## Annexe B — Références

- Dépôt : <https://github.com/TF-Jordan/GPU_BAND_Sched>
- Upstream Cricket : <https://github.com/RWTH-ACS/cricket>
- Plan de packaging : [docs/PACKAGING_PLAN.md](PACKAGING_PLAN.md)
- README packaging : [packaging/README.md](../packaging/README.md)
