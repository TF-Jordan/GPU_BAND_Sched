# Créer un cluster Proxmox VE à 3 machines physiques

Ce guide couvre l'installation et la configuration d'un cluster Proxmox VE sur 3 machines physiques réelles, de zéro jusqu'à un cluster opérationnel avec stockage partagé et migration live.

---

## Prérequis matériel

### Par machine

| Composant | Minimum | Recommandé |
|-----------|---------|------------|
| CPU | x86-64 avec VT-x / AMD-V | Xeon E5 / Ryzen 5 ou mieux |
| RAM | 16 Go | 32 Go ou plus |
| Disque système | 60 Go SSD | 120 Go SSD NVMe |
| Disque données | 200 Go | 1 To SSD ou HDD |
| Réseau | 1 Gbps (1 carte) | 2 × 1 Gbps ou 1 × 10 Gbps |

### Infrastructure réseau

| Élément | Requis |
|---------|--------|
| Switch | 1 switch L2 avec au moins 8 ports |
| Câbles | Câbles Ethernet RJ45 Cat6 |
| IP fixes | 3 adresses IP fixes sur le même sous-réseau |
| Routeur/Gateway | Accès optionnel à Internet pour les mises à jour |

### Clé USB (pour l'installation)

- 1 clé USB de 8 Go minimum
- Sera formatée : sauvegarder les données au préalable

---

## 1. Planifier le réseau

Définir les adresses IP avant de commencer. Exemple utilisé dans ce guide :

| Machine | Hostname | IP cluster | Gateway |
|---------|----------|------------|---------|
| Machine 1 | `pve1.monlab.local` | `192.168.1.11/24` | `192.168.1.1` |
| Machine 2 | `pve2.monlab.local` | `192.168.1.12/24` | `192.168.1.1` |
| Machine 3 | `pve3.monlab.local` | `192.168.1.13/24` | `192.168.1.1` |

Adapter ces valeurs à votre réseau existant.

---

## 2. Préparer l'ISO Proxmox VE

### 2.1 Télécharger l'ISO

Depuis un ordinateur avec accès internet :

```bash
# Proxmox VE 9.x recommandé (vérifier la dernière version sur proxmox.com/downloads)
wget https://enterprise.proxmox.com/iso/proxmox-ve_9.1-1.iso

# Alternative PVE 8.x
# wget https://enterprise.proxmox.com/iso/proxmox-ve_8.2-1.iso

# Vérifier le hash SHA256 (affiché sur la page de téléchargement)
sha256sum proxmox-ve_9.1-1.iso
```

### 2.2 Créer la clé USB bootable

**Sous Linux :**

```bash
# Identifier la clé USB (ne pas se tromper de disque !)
lsblk
# Exemple : /dev/sdb

# Écrire l'ISO (remplacer /dev/sdb par votre clé)
sudo dd if=proxmox-ve_8.2-1.iso of=/dev/sdb bs=4M status=progress conv=fsync
sync
```

**Sous Windows :**
- Utiliser Rufus (rufus.ie) → sélectionner l'ISO → mode DD Image → écrire

---

## 3. Configurer le BIOS/UEFI sur chaque machine

Démarrer sur chaque machine et entrer dans le BIOS (touche `Del`, `F2`, ou `F10` selon le fabricant).

### Paramètres obligatoires

| Paramètre | Valeur requise |
|-----------|---------------|
| Virtualisation (VT-x / AMD-V) | **Activé** |
| VT-d / AMD-Vi (IOMMU) | Activé si disponible |
| Secure Boot | **Désactivé** |
| Boot order | USB en premier |
| AHCI (pour les disques SATA) | **Activé** (pas IDE) |
| Hyperthreading | Activé (optionnel mais recommandé) |

Sauvegarder et redémarrer avec la clé USB insérée.

---

## 4. Installer Proxmox VE sur chaque machine

Cette procédure est **identique sur les 3 machines**. Seuls le hostname et l'IP changent.

### 4.1 Démarrer l'installeur

Au boot sur la clé USB : sélectionner `Install Proxmox VE (Graphical)`.

### 4.2 Étapes de l'installation

**Étape 1 — Licence**
→ Lire et accepter la licence EULA : `I agree`

**Étape 2 — Disque cible**
→ Sélectionner le disque système (ex : `/dev/sda`)

Options du système de fichiers :
- `ext4` : simple, stable, recommandé pour un premier cluster
- `zfs (RAID-1)` : si 2 disques disponibles, offre la redondance locale

Cliquer sur `Options` pour ajuster les partitions si nécessaire.

**Étape 3 — Localisation**
- Pays : votre pays
- Fuseau horaire : votre timezone (ex : `Europe/Paris`)
- Clavier : votre disposition (ex : `fr`)

**Étape 4 — Mot de passe**
- Mot de passe root : choisir un mot de passe fort, **identique sur les 3 machines** facilite la gestion
- Email : une adresse valide (pour les alertes système)

**Étape 5 — Réseau**

Sur **machine 1** :
```
Management Interface : eth0  (ou eno1, enp3s0 — selon la machine)
Hostname            : pve1.monlab.local
IP Address          : 192.168.1.11/24
Gateway             : 192.168.1.1
DNS Server          : 8.8.8.8
```

Sur **machine 2** :
```
Hostname : pve2.monlab.local
IP       : 192.168.1.12/24
```

Sur **machine 3** :
```
Hostname : pve3.monlab.local
IP       : 192.168.1.13/24
```

**Étape 6 — Résumé**
→ Vérifier toutes les valeurs → `Install`

L'installation dure environ 3 à 5 minutes. La machine redémarre automatiquement.

**Retirer la clé USB** avant le redémarrage (ou rapidement après le POST).

---

## 5. Configuration post-installation sur chaque nœud

Se connecter en SSH depuis votre poste de travail :

```bash
ssh root@192.168.1.11   # pve1
ssh root@192.168.1.12   # pve2
ssh root@192.168.1.13   # pve3
```

Effectuer les étapes suivantes **sur les 3 nœuds**.

### 5.1 Désactiver le dépôt Enterprise

Sans abonnement Proxmox, le dépôt enterprise bloque les mises à jour :

```bash
# Désactiver enterprise
sed -i 's|^deb|#deb|' /etc/apt/sources.list.d/pve-enterprise.list
sed -i 's|^deb|#deb|' /etc/apt/sources.list.d/ceph.list 2>/dev/null || true

# Activer le dépôt communautaire (gratuit)
cat > /etc/apt/sources.list.d/pve-no-subscription.list << 'EOF'
deb http://download.proxmox.com/debian/pve bookworm pve-no-subscription
EOF

# Mettre à jour
apt update && apt full-upgrade -y
```

### 5.2 Configurer /etc/hosts

```bash
# Ajouter les 3 nœuds dans /etc/hosts (sur chaque nœud)
cat >> /etc/hosts << 'EOF'
192.168.1.11 pve1 pve1.monlab.local
192.168.1.12 pve2 pve2.monlab.local
192.168.1.13 pve3 pve3.monlab.local
EOF
```

### 5.3 Vérifier la résolution et la connectivité

```bash
# Depuis pve1
ping -c2 pve2
ping -c2 pve3
ssh root@pve2 hostname
ssh root@pve3 hostname
```

### 5.4 Synchroniser les horloges (NTP)

Le cluster Proxmox est sensible aux désynchronisations d'horloge :

```bash
# Sur chaque nœud
systemctl enable --now chrony
chronyc tracking
# Vérifier que "System time offset" < 1 seconde
```

---

## 6. Créer le cluster Proxmox

### 6.1 Initialiser depuis pve1

```bash
# Sur pve1 uniquement
pvecm create monlab-cluster

# Vérifier
pvecm status
```

Résultat attendu :
```
Cluster information
-------------------
Name:             monlab-cluster
Config Version:   1
Transport:        knet
Nodeid:           1
Nodes:            1
Expected votes:   1
Quorum:           1
```

### 6.2 Joindre depuis pve2 et pve3

```bash
# Sur pve2
pvecm add 192.168.1.11 --use_ssh

# Sur pve3
pvecm add 192.168.1.11 --use_ssh
```

Chaque commande demande le mot de passe root de pve1.

### 6.3 Vérifier le cluster complet

```bash
# Sur n'importe quel nœud
pvecm status
pvecm nodes
```

Résultat attendu avec 3 nœuds :
```
Membership information
----------------------
    Nodeid      Votes Name
         1          1 pve1 (local)
         2          1 pve2
         3          1 pve3
```

---

## 7. Configurer le stockage — Ceph RBD (obligatoire)

Ceph est le stockage partagé utilisé dans ce projet. Il distribue les disques VMs sur les 3 nœuds avec réplication, sans point de défaillance unique, et permet les migrations live sans copie de disque.

> **Prérequis** : chaque nœud doit avoir un disque **dédié** pour Ceph, séparé du disque système. Exemple : `/dev/sda` = système Proxmox, `/dev/sdb` = OSD Ceph.

### 7.1 Installer Ceph sur le cluster

```bash
# Sur pve1 uniquement — s'applique à tous les nœuds via le cluster
pveceph install --repository no-subscription
```

### 7.2 Initialiser Ceph

```bash
# Sur pve1 — utiliser le réseau dédié cluster (interface séparée recommandée)
pveceph init --network 192.168.1.0/24
```

### 7.3 Créer les moniteurs (MON) — sur chaque nœud

```bash
# Sur pve1
pveceph createmon
# Sur pve2
pveceph createmon
# Sur pve3
pveceph createmon

# Vérifier (depuis n'importe quel nœud)
ceph -s
# → health: HEALTH_OK (ou HEALTH_WARN avant les OSDs)
```

### 7.4 Créer les OSDs — un disque dédié par nœud

```bash
# Sur pve1 (/dev/sdb = disque dédié Ceph, sera EFFACÉ)
pveceph createosd /dev/sdb

# Sur pve2
pveceph createosd /dev/sdb

# Sur pve3
pveceph createosd /dev/sdb

# Vérifier les OSDs
ceph osd tree
# → 3 OSDs up, in
```

### 7.5 Créer la pool de stockage VM

```bash
# Pool pour les disques VMs (réplication x3, disponibilité même si 1 nœud tombe)
pveceph createpool vm-pool \
    --pg_num 64 \
    --min_size 2 \
    --size 3 \
    --application rbd

# Vérifier
ceph df
rados lspools
```

### 7.6 Activer dans Proxmox

Interface web → `Datacenter` → `Storage` → `Add` → `RBD` :

| Champ | Valeur |
|-------|--------|
| ID | `ceph-vms` |
| Pool | `vm-pool` |
| Content | `Disk image, Container volume` |
| Nodes | `All` |
| Username | `admin` |

Ou via CLI :

```bash
pvesm add rbd ceph-vms \
    --pool vm-pool \
    --content images \
    --nodes pve1,pve2,pve3
```

### 7.7 Vérifier le stockage

```bash
# L'espace Ceph doit apparaître dans l'interface web
pvesm status
# → ceph-vms  rbd    active  ...
```

---

## 8. Configurer la Haute Disponibilité (HA)

Avec Ceph et 3 nœuds, Proxmox peut redémarrer automatiquement les VMs d'un nœud défaillant. Le disque étant sur Ceph, il est accessible depuis n'importe quel nœud survivant.

```bash
# Interface web → Datacenter → HA → Groups → Add
# Nom : ha-group, Nœuds : pve1, pve2, pve3

# Ajouter une VM au groupe HA (clic droit sur la VM → More → Manage HA)
# Group : ha-group, Max restart : 3, Max relocate : 3
```

---

## 9. Accéder à l'interface web

```
https://192.168.1.11:8006   (pve1)
https://192.168.1.12:8006   (pve2)
https://192.168.1.13:8006   (pve3)
```

Chaque interface montre le cluster complet. Les 3 nœuds apparaissent dans le panneau gauche.

---

## 10. Tester le cluster

### 10.1 Créer une VM de test

`pve1` → `Create VM` :
- ISO : charger une image cloud Debian ou Ubuntu
- RAM : 1 Go, CPU : 2 vCPU
- Disque : 20 Go sur `ceph-vms` (pool Ceph)

### 10.2 Test de migration live

Clic droit sur la VM → `Migrate` → sélectionner `pve2` → `Migrate`

La VM continue de fonctionner pendant la migration. Seule la RAM est transférée — le disque RBD bascule instantanément sur pve2. Interruption < 1s.

### 10.3 Test de basculement HA

Couper physiquement pve2 (ou `systemctl poweroff`). Les VMs HA redémarrent automatiquement sur pve1 ou pve3 dans les 60 secondes, avec accès immédiat au disque via Ceph.

---

## 11. Dépannage courant

| Symptôme | Cause probable | Solution |
|----------|---------------|----------|
| `pvecm add` échoue | Résolution DNS incorrecte | Vérifier `/etc/hosts` sur les 2 nœuds |
| Quorum perdu | Un nœud hors ligne | `pvecm expected 1` (temporaire, perd HA) |
| `pveceph createosd` échoue | Disque déjà partitionné | `wipefs -a /dev/sdb && sgdisk -Z /dev/sdb` |
| Ceph HEALTH_WARN après install | OSDs pas encore up | Attendre 30s, relancer `ceph -s` |
| Migrations lentes | Réseau 100 Mbps | Upgrade vers 1 Gbps minimum |
| VM ne démarre pas après migration | Disque pas sur Ceph | Migrer le disque vers `ceph-vms` d'abord |
| Horloge désynchronisée | NTP non configuré | `systemctl restart chrony` |
| Interface web inaccessible | pveproxy arrêté | `systemctl restart pveproxy` |

---

## Récapitulatif

| Nœud | IP | Interface web | Rôle |
|------|----|---------------|------|
| pve1 | 192.168.1.11 | https://192.168.1.11:8006 | Premier nœud (initialisateur) |
| pve2 | 192.168.1.12 | https://192.168.1.12:8006 | Deuxième nœud |
| pve3 | 192.168.1.13 | https://192.168.1.13:8006 | Troisième nœud |
