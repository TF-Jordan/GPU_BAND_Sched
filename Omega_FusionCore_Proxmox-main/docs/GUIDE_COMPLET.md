# Omega Remote Paging — Guide Complet

> Cluster Proxmox homogène — toutes les machines font tourner Proxmox et des VMs.
> Axes : RAM · CPU · GPU · Disque

---

## Table des matières

1. [Ce que fait Omega](#1-ce-que-fait-omega)
2. [Architecture réelle](#2-architecture-réelle)
3. [Ce qui tourne sur chaque machine](#3-ce-qui-tourne-sur-chaque-machine)
4. [Prérequis](#4-prérequis)
5. [Étape 1 — Compilation](#5-étape-1--compilation)
6. [Étape 2 — Installation sur chaque nœud](#6-étape-2--installation-sur-chaque-nœud)
7. [Étape 3 — Démarrer les services](#7-étape-3--démarrer-les-services)
8. [Étape 4 — Enregistrer les VMs](#8-étape-4--enregistrer-les-vms)
9. [Étape 5 — Démarrer le controller](#9-étape-5--démarrer-le-controller)
10. [Tester que tout fonctionne](#10-tester-que-tout-fonctionne)
11. [Visualiser l'activité en temps réel](#11-visualiser-lactivité-en-temps-réel)
12. [Cluster 2 nœuds, 4 nœuds ou plus](#12-cluster-2-nœuds-4-nœuds-ou-plus)
13. [Commandes utiles au quotidien](#13-commandes-utiles-au-quotidien)
14. [Dépannage](#14-dépannage)
15. [Référence des ports et fichiers](#15-référence-des-ports-et-fichiers)

---

## 1. Ce que fait Omega

Quand ta RAM est pleine sur un nœud Proxmox, au lieu de swapper sur disque local (lent), Omega déplace les pages mémoire froides vers la RAM des autres nœuds du cluster via le réseau. Pour la VM, rien ne change — elle continue à tourner normalement.

En plus de la RAM, Omega gère aussi :
- **CPU** : ajoute ou retire des vCPUs à chaud selon la charge
- **GPU** : partage un GPU physique entre plusieurs VMs
- **Disque** : donne plus d'I/O aux VMs qui en ont besoin, ralentit celles qui monopolisent le disque
- **Migrations** : déplace automatiquement les VMs vers le nœud le plus adapté

---

## 2. Architecture réelle

Dans un cluster Proxmox homogène, **toutes les machines sont équivalentes**. Chacune peut à la fois héberger des VMs ET stocker les pages évincées par les autres.

```
┌─────────────────────┐   ┌─────────────────────┐   ┌─────────────────────┐
│   Machine 1         │   │   Machine 2         │   │   Machine 3         │
│   10.10.0.11       │   │   10.10.0.12       │   │   10.10.0.13       │
│                     │   │                     │   │                     │
│  VMs (QEMU)         │   │  VMs (QEMU)         │   │  VMs (QEMU)         │
│  node-a-agent×N     │   │  node-a-agent×N     │   │  node-a-agent×N     │
│  omega-daemon ◄─────┼───►  omega-daemon ◄─────┼───►  omega-daemon       │
│  (store :9100)      │   │  (store :9100)      │   │  (store :9100)      │
│  (API    :9200)     │   │  (API    :9200)     │   │  (API    :9200)     │
│  (ctrl   :9300)     │   │  (ctrl   :9300)     │   │  (ctrl   :9300)     │
│                     │   │                     │   │                     │
│  omega-controller ──┼───►────────────────────►│   │                     │
│  (Python, 1 seul)   │   │                     │   │                     │
└─────────────────────┘   └─────────────────────┘   └─────────────────────┘
```

Quand une VM sur la machine 1 a besoin d'évincer de la RAM → ses pages partent vers les machines 2 et 3.
Quand une VM sur la machine 2 a besoin d'évincer → ses pages partent vers les machines 1 et 3.
Et ainsi de suite.

---

## 3. Ce qui tourne sur chaque machine

### Sur les 3 machines (identique)

| Composant | Comment | Description |
|---|---|---|
| `omega-daemon` | service systemd | Reçoit les pages des autres, surveille les VMs locales, gère CPU/GPU/disque |
| `kvm-omega` | remplace `/usr/bin/kvm` | Intercepte le démarrage QEMU, injecte le backend mémoire omega |
| `node-a-agent` | lancé automatiquement | Un agent par VM, gère les pages en RAM de cette VM |
| hookscript Proxmox | enregistré sur chaque VM | Démarre/arrête l'agent au démarrage/arrêt de chaque VM |

### Sur une seule machine (au choix)

| Composant | Comment | Description |
|---|---|---|
| `omega-controller` | service systemd (Python) | Surveille tout le cluster, décide des migrations automatiques |

---

## 4. Prérequis

### Sur chaque nœud Proxmox

```bash
# Vérifier la version du kernel (>= 5.7 requis)
uname -r

# Vérifier que cgroups v2 est actif
cat /proc/mounts | grep cgroup2
# Si rien ne s'affiche, activer cgroups v2 :
echo 'GRUB_CMDLINE_LINUX_DEFAULT="quiet systemd.unified_cgroup_hierarchy=1"' \
    >> /etc/default/grub
update-grub
reboot

# Vérifier userfaultfd (gestion des page faults mémoire)
ls /dev/userfaultfd
# Si absent :
echo 1 > /proc/sys/vm/unprivileged_userfaultfd
# Pour le rendre permanent :
echo 'vm.unprivileged_userfaultfd = 1' >> /etc/sysctl.conf
```

> **Cluster virtuel KVM (lab nested)** : ces prérequis s'appliquent également à l'intérieur
> des VMs Proxmox qui tournent sous KVM. `userfaultfd` est souvent absent par défaut dans
> les kernels nested — vérifier et activer sur chaque nœud. Les performances seront dégradées
> (double overhead de virtualisation) mais suffisantes pour tester. Voir `docs/cluster-kvm.md`
> pour la mise en place du lab.

### Sur la machine de compilation (peut être l'un des nœuds)

```bash
# Installer Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup default stable

# Dépendances système
apt install -y build-essential pkg-config libssl-dev libdrm-dev \
               python3 python3-pip git curl perl
```

---

## 5. Étape 1 — Compilation

```bash
git clone <url-du-repo> omega-remote-paging
cd omega-remote-paging

# Compiler tous les binaires (prend 2-5 minutes)
make build
# ou :
cargo build --release --workspace
```

Binaires produits dans `target/release/` :

| Fichier | Rôle |
|---|---|
| `omega-daemon` | daemon principal (un par nœud) |
| `node-a-agent` | agent mémoire par VM |
| `omega-qemu-launcher` | wrapper QEMU |
| `node-bc-store` | store standalone (uniquement pour machines dédiées — pas ton cas) |

Lancer les tests pour vérifier que tout compile correctement :

```bash
make test-rust
# Résultat attendu : 244 tests, 0 échecs
```

---

## 6. Étape 2 — Installation sur chaque nœud

Le script `omega-proxmox-install.sh` fait automatiquement :
1. Copie les binaires dans `/usr/local/bin/`
2. Sauvegarde `/usr/bin/kvm` → `/usr/bin/kvm.real`
3. Génère `/usr/local/bin/kvm-omega` (le wrapper QEMU)
4. Crée le symlink `/usr/bin/kvm` → `kvm-omega`
5. Copie le hookscript dans `/var/lib/vz/snippets/omega-hook.pl`
6. Crée le service systemd `omega-daemon`

Il y a deux façons de l'exécuter.

---

### Méthode A — Depuis la machine de dev (recommandée)

Tout se fait depuis ta machine de développement. Ne copie que les binaires nécessaires —
**ne pas faire `rsync` du dossier `target/`** (plusieurs Go, remplit le disque du nœud).

Tous les nœuds reçoivent exactement les mêmes binaires. Chaque nœud est configuré avec
`OMEGA_STORES` = les autres nœuds du cluster (les siens propres sont exclus).

**Déploiement en une boucle (adapter les IPs) :**

```bash
NODES=(10.10.0.11 10.10.0.12 10.10.0.13)
ALL_STORES=$(IFS=,; echo "${NODES[*]/%/:9100}")   # "10.10.0.11:9100,10.10.0.12:9100,10.10.0.13:9100"

for node in "${NODES[@]}"; do
    ssh root@${node} "mkdir -p /opt/omega-remote-paging/target/release /opt/omega-remote-paging/scripts /var/lib/vz/snippets"

    scp target/release/omega-daemon target/release/node-a-agent target/release/omega-qemu-launcher \
        root@${node}:/opt/omega-remote-paging/target/release/

    scp scripts/omega-proxmox-install.sh scripts/proxmox_hook.pl \
        root@${node}:/opt/omega-remote-paging/scripts/

    # Stores distants = tous les autres nœuds
    node_stores=$(printf '%s\n' "${NODES[@]}" | grep -v "^${node}$" | sed 's/$/:9100/' | paste -sd,)

    ssh root@${node} "cd /opt/omega-remote-paging && \
        OMEGA_STORES='${node_stores}' \
        INSTALL_DIR='/usr/local/bin' \
        OMEGA_RUN_DIR='/var/lib/omega-qemu' \
        bash scripts/omega-proxmox-install.sh"
done
```

Ou via le script de déploiement intégré (lit `scripts/cluster.conf`) :

```bash
bash scripts/deploy.sh
```

### Nettoyage complet (rollback ou réinstallation)

Utiliser `scripts/uninstall.sh`. Sur un cluster Proxmox tous les nœuds sont identiques —
chacun peut héberger des VMs et offrir sa RAM comme store. Le script nettoie donc **tout**
sur chaque nœud listé dans `OMEGA_NODES`.

Ce qu'il supprime sur chaque nœud :

- Services : `omega-daemon`, `node-bc-store`, `omega-agent@*`
- Binaires : `omega-daemon`, `node-a-agent`, `omega-qemu-launcher`, `kvm-omega`
- `/usr/bin/kvm` restauré depuis `kvm.real`
- Hookscript supprimé de toutes les VMs du nœud
- Bridge LD_PRELOAD (`omega-uffd-bridge.so`)
- Certs TLS (`/etc/omega-store/tls`)
- Répertoires : `/opt/omega-remote-paging`, `/var/lib/omega-qemu`, `/var/log/omega`

**Lancer avec DRY_RUN=1 d'abord pour vérifier :**

```bash
DRY_RUN=1 OMEGA_NODES=pve,pve2,pve3 bash scripts/uninstall.sh

# Supprimer pour de vrai
OMEGA_NODES=pve,pve2,pve3 bash scripts/uninstall.sh
```

---

Vérifier que le wrapper est en place après l'installation :

```bash
ls -la /usr/bin/kvm
# → /usr/bin/kvm -> /usr/local/bin/kvm-omega   ✓
```

### Configurer omega-daemon sur chaque nœud

Créer `/etc/default/omega-daemon` (adapter les IPs sur chaque machine) :

```bash
# Sur la machine 1 :
cat > /etc/default/omega-daemon <<'EOF'
OMEGA_NODE_ID=pve-node1
OMEGA_NODE_ADDR=10.10.0.11
OMEGA_PEERS=10.10.0.12:9200,10.10.0.13:9200
OMEGA_EVICT_THRESHOLD_PCT=75
OMEGA_GPU_ENABLED=true
RUST_LOG=info
EOF

# Sur la machine 2 :
cat > /etc/default/omega-daemon <<'EOF'
OMEGA_NODE_ID=pve-node2
OMEGA_NODE_ADDR=10.10.0.12
OMEGA_PEERS=10.10.0.11:9200,10.10.0.13:9200
OMEGA_EVICT_THRESHOLD_PCT=75
OMEGA_GPU_ENABLED=true
RUST_LOG=info
EOF

# Sur la machine 3 :
cat > /etc/default/omega-daemon <<'EOF'
OMEGA_NODE_ID=pve-node3
OMEGA_NODE_ADDR=10.10.0.13
OMEGA_PEERS=10.10.0.11:9200,10.10.0.12:9200
OMEGA_EVICT_THRESHOLD_PCT=75
OMEGA_GPU_ENABLED=true
RUST_LOG=info
EOF
```

Mettre à jour le service systemd pour charger ce fichier :

```bash
# Sur chaque nœud :
sed -i 's|ExecStart=.*|ExecStart=/usr/local/bin/omega-daemon|' \
    /etc/systemd/system/omega-daemon.service

# Ajouter EnvironmentFile juste avant ExecStart :
sed -i '/\[Service\]/a EnvironmentFile=/etc/default/omega-daemon' \
    /etc/systemd/system/omega-daemon.service

systemctl daemon-reload
```

### Variables d'environnement — node-a-agent (par VM)

Ces variables sont passées à `node-a-agent` par le hookscript ou dans le fichier d'environnement de l'agent.

#### vCPU élastique

| Variable | Défaut | Description |
|---|---|---|
| `AGENT_VM_VCPUS` | — | Nombre maximum de vCPUs allouables à la VM |
| `AGENT_VM_INITIAL_VCPUS` | — | Nombre de vCPUs au démarrage de la VM |
| `AGENT_VCPU_HIGH_THRESHOLD_PCT` | 80 | Charge CPU (%) au-dessus de laquelle ajouter des vCPUs |
| `AGENT_VCPU_LOW_THRESHOLD_PCT` | 20 | Charge CPU (%) en dessous de laquelle retirer des vCPUs |
| `AGENT_VCPU_SCALE_INTERVAL_SECS` | 30 | Intervalle en secondes entre deux évaluations de charge |
| `AGENT_VCPU_OVERCOMMIT_RATIO` | 3 | Overcommit maximum (ex. : 3 = 3 VMs par pCPU) |

#### GPU placement et partage

| Variable | Défaut | Description |
|---|---|---|
| `AGENT_GPU_REQUIRED` | false | La VM requiert un GPU — déclenche le GPU placement daemon |
| `AGENT_GPU_PLACEMENT_INTERVAL_SECS` | 60 | Intervalle de vérification du placement GPU |

### Variables d'environnement — node-bc-store

Ces variables configurent le comportement du store sur chaque nœud.

#### Nettoyage des pages orphelines

| Variable | Défaut | Description |
|---|---|---|
| `STORE_ORPHAN_CHECK_INTERVAL_SECS` | 300 | Intervalle (en secondes) entre deux passages du nettoyeur d'orphelins |
| `STORE_ORPHAN_GRACE_SECS` | 600 | Délai de grâce (en secondes) avant suppression d'une VM absente du cluster |

#### Backend Ceph

| Variable | Défaut | Description |
|---|---|---|
| `STORE_CEPH_CONF` | `/etc/ceph/ceph.conf` | Chemin vers la configuration Ceph |
| `STORE_CEPH_POOL` | `omega-pages` | Pool Ceph utilisé pour stocker les pages |

#### Réseau

| Variable | Défaut | Description |
|---|---|---|
| `STORE_STATUS_LISTEN` | `0.0.0.0:9200` | Adresse d'écoute du serveur HTTP status |

### Détection automatique Ceph

Le store détecte automatiquement si Ceph est disponible :

1. **À la compilation** : `build.rs` exécute `pkg-config librados`. Si la bibliothèque est trouvée, elle émet `cargo:rustc-cfg=ceph_detected` et le code Ceph est compilé. Si elle est absente, seul le store RAM est compilé — aucune erreur de compilation.

2. **Au démarrage** : même si `ceph_detected` est actif à la compilation, le store vérifie la présence de `/etc/ceph/ceph.conf` (ou du chemin configuré dans `STORE_CEPH_CONF`). Si le fichier est absent, le store démarre en mode RAM.

3. **Réplication** : quand tous les stores du cluster rapportent `ceph_enabled: true` dans leur endpoint `/status`, la réplication write-through entre stores est automatiquement désactivée (Ceph assure lui-même la redondance).

```bash
# Vérifier si Ceph est actif sur un store
curl -s http://127.0.0.1:9200/status | python3 -m json.tool | grep ceph_enabled
# → "ceph_enabled": true   si Ceph actif
# → "ceph_enabled": false  si mode RAM
```

---

## 7. Étape 3 — Démarrer les services

### Sur chaque nœud (dans cet ordre)

```bash
# 1. Démarrer omega-daemon
systemctl start omega-daemon
systemctl enable omega-daemon   # démarrage automatique au boot

# 2. Vérifier qu'il tourne
systemctl status omega-daemon
# Chercher : "Active: active (running)"

# 3. Vérifier les logs de démarrage
journalctl -u omega-daemon -n 30
# Chercher des lignes comme :
# omega-daemon V4 démarré node_id=pve-node1 store_port=9100 api_port=9200
# store TCP démarré sur 0.0.0.0:9100
# API HTTP démarrée sur 0.0.0.0:9200
# canal de contrôle HTTP démarré sur 0.0.0.0:9300
```

### Vérifier la connectivité entre nœuds

```bash
# Depuis la machine 1 — vérifier que les machines 2 et 3 répondent
curl -s http://10.10.0.12:9200/api/status
curl -s http://10.10.0.13:9200/api/status
# Doit afficher un JSON avec node_id, mem_usage_pct, pages_stored, etc.

# Vérifier les stores TCP
nc -zv 10.10.0.12 9100 && echo "store machine2 OK"
nc -zv 10.10.0.13 9100 && echo "store machine3 OK"
```

> **Note** : `python3 -m json.tool` peut être utilisé pour formater le JSON, mais nécessite
> python3 sur le nœud. Sur Proxmox (Debian), l'installer si absent : `apt install -y python3`.
> Le controller Python sera installé à l'étape 5 — inutile de l'attendre pour cette vérification.

---

## 8. Étape 4 — Enregistrer les VMs

Le hookscript démarre/arrête l'agent omega au démarrage/arrêt de chaque VM.
Il est inutile de le faire manuellement à chaque création de VM — un timer systemd
le fait automatiquement sur toute nouvelle VM détectée.

### Enregistrement automatique (recommandé)

Copier le script sur chaque nœud (depuis la machine de dev) :

```bash
for node in 10.10.0.11 10.10.0.12 10.10.0.13; do
    scp scripts/omega-auto-hook.sh root@${node}:/usr/local/bin/
    ssh root@${node} "chmod +x /usr/local/bin/omega-auto-hook.sh"
done
```

Créer le timer systemd sur chaque nœud :

```bash
# Sur chaque nœud
cat > /etc/systemd/system/omega-auto-hook.service <<'EOF'
[Unit]
Description=Omega — enregistrement automatique hookscript sur nouvelles VMs

[Service]
Type=oneshot
ExecStart=/usr/local/bin/omega-auto-hook.sh
EOF

cat > /etc/systemd/system/omega-auto-hook.timer <<'EOF'
[Unit]
Description=Omega — vérification hookscript toutes les 5 secondes

[Timer]
OnBootSec=30
OnUnitActiveSec=5

[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable --now omega-auto-hook.timer
```

Vérifier que le timer tourne :

```bash
systemctl status omega-auto-hook.timer
# Attendu : active (waiting)
```

À partir de là, toute nouvelle VM créée sur le nœud recevra automatiquement
le hookscript dans les 5 secondes qui suivent.

### Enregistrement manuel (ponctuel)

Pour forcer l'enregistrement immédiatement sans attendre le timer :

```bash
bash /usr/local/bin/omega-auto-hook.sh

# Ou sur une VM spécifique
qm set 100 --hookscript local:snippets/omega-hook.pl

# Vérifier
qm config 100 | grep hookscript
# → hookscript: local:snippets/omega-hook.pl
```

---

## 9. Étape 5 — Démarrer le controller

Le controller est le cerveau du cluster. Il tourne sur **une seule machine** (peu importe laquelle).

### Créer le token API Proxmox

Le controller a besoin d'un token Proxmox pour lire la configuration des VMs et déclencher les migrations.

**Dans l'interface web Proxmox** (`https://10.10.0.11:8006`) :

1. `Datacenter` → `Permissions` → `API Tokens` → **Add**
2. Remplir :

| Champ | Valeur |
|-------|--------|
| User | `root@pam` |
| Token ID | `omega` |
| Privilege Separation | **décocher** |

3. Cliquer **Add** — Proxmox affiche le token **une seule fois** :
```
root@pam!omega=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```
> **Copier immédiatement** — il ne sera plus affiché après fermeture de la fenêtre.

Vérifier via CLI que le token existe :
```bash
pveum user token list root@pam
# → doit afficher : omega
```

### Installer les dépendances Python

```bash
cd omega-remote-paging
pip install -r controller/requirements.txt
```

### Tester en mode dry-run d'abord

```bash
python3 -m controller.main daemon \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    --node http://10.10.0.13:9300 \
    --poll-interval 5 \
    --dry-run
```

Tu dois voir des logs comme :
```
[10:00:01] [info]  cycle daemon nodes=['node-a', 'node-b', 'node-c']
[10:00:01] [info]  aucune migration nécessaire
```

Si `dry-run` fonctionne, lancer pour de vrai :

```bash
python3 -m controller.main daemon \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    --node http://10.10.0.13:9300 \
    --poll-interval 5 \
    --proxmox-url https://10.10.0.11:8006 \
    --proxmox-token 'root@pam!omega=<ton-token-api>'
```

### En service systemd (pour le garder actif)

```bash
cat > /etc/systemd/system/omega-controller.service <<EOF
[Unit]
Description=Omega Controller (migration automatique)
After=network.target omega-daemon.service

[Service]
Type=simple
WorkingDirectory=/opt/omega-remote-paging
ExecStart=python3 -m controller.main daemon \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    --node http://10.10.0.13:9300 \
    --poll-interval 5 \
    --proxmox-url https://10.10.0.11:8006 \
    --log-format json
Restart=always
RestartSec=10
Environment=PYTHONPATH=/opt/omega-remote-paging

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now omega-controller
```

---

## 10. Tester que tout fonctionne

### Test 1 — Démarrer une VM et vérifier l'agent

```bash
qm start 100

# Sur le nœud qui héberge la VM 100, vérifier que l'agent est actif
omega-qemu-launcher status --vm-id 100
# Résultat attendu :
# {
#   "vm_id": 100,
#   "agent_running": true,
#   "agent_pid": 12345,
#   ...
# }

# Vérifier les logs de démarrage du hookscript
grep "vmid=100" /var/log/omega-hook.log
# Chercher : "agent memfd prêt pour vmid=100"
```

### Test 2 — Vérifier que les stores se joignent

```bash
# Depuis le nœud 1, voir l'état du cluster entier
for node in 10.10.0.11 10.10.0.12 10.10.0.13; do
    echo "=== $node ==="
    curl -s http://${node}:9200/api/status | python3 -m json.tool | grep -E "node_id|mem_usage_pct|pages_stored|vcpu_free"
done
```

### Test 3 — Simuler une pression mémoire et observer l'éviction

```bash
# Dans la VM 100, créer de la pression mémoire
# (dans la VM elle-même) :
stress-ng --vm 1 --vm-bytes 80% --timeout 60s &

# Sur le nœud hôte, observer les pages évincées
watch -n 1 'curl -s http://127.0.0.1:9300/control/pages/100'
# Le compteur "remote_pages" doit augmenter
```

### Test 4 — Vérifier le monitoring CPU

```bash
# Voir le statut des vCPUs de toutes les VMs
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool
```

### Test 5 — Vérifier le GPU

```bash
# Voir l'état GPU du nœud
curl -s http://127.0.0.1:9200/api/gpu | python3 -m json.tool
# Doit afficher : backend, total_vram_mib, free_vram_mib, vms_using_gpu
```

### Test 6 — Tous les tests unitaires

```bash
# Tests Rust (244 tests)
make test-rust

# Tests Python (controller)
make test-python
```

---

## 11. Visualiser l'activité en temps réel

### Vue d'ensemble du cluster

```bash
# Statut de tous les nœuds en une commande
python3 -m controller.main status \
    --stores 10.10.0.11:9100,10.10.0.12:9100,10.10.0.13:9100
```

### Monitoring continu (terminal dédié)

```bash
# Boucle de monitoring toutes les 10 secondes
python3 -m controller.main monitor \
    --stores 10.10.0.11:9100,10.10.0.12:9100,10.10.0.13:9100 \
    --interval 10

# Sortie typique :
# [10:00:01] [info]  cycle monitoring mem_usage_pct=62.3 swap_usage_pct=0.0
#                    psi_some_avg10=0.12 decision=local_only
# [10:00:11] [info]  cycle monitoring mem_usage_pct=78.1 ...
#                    decision=enable_remote   ← éviction activée
```

### Voir les pages évincées par VM

```bash
# Toutes les 2 secondes, voir les pages distantes de la VM 100
watch -n 2 'curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool'
```

### Logs en temps réel

```bash
# Daemon (RAM, CPU, GPU, disque) — terminal 1
journalctl -u omega-daemon -f

# Controller (décisions de migration) — terminal 2
journalctl -u omega-controller -f

# Hookscript (cycle de vie des VMs) — terminal 3
tail -f /var/log/omega-hook.log

# Agent d'une VM spécifique — terminal 4
tail -f /var/lib/omega-qemu/vm-100/agent.log
```

### API complète disponible

```bash
BASE_DAEMON="http://127.0.0.1:9200"
BASE_CTRL="http://127.0.0.1:9300"

# État détaillé du nœud local
curl -s $BASE_DAEMON/api/status | python3 -m json.tool

# État GPU
curl -s $BASE_DAEMON/api/gpu | python3 -m json.tool

# Statut général (santé du daemon)
curl -s $BASE_CTRL/control/status | python3 -m json.tool

# Pages distantes d'une VM
curl -s $BASE_CTRL/control/pages/100 | python3 -m json.tool

# Statut vCPUs de toutes les VMs
curl -s $BASE_CTRL/control/vcpu/status | python3 -m json.tool
```

---

## 12. Cluster 2 nœuds, 4 nœuds ou plus

Le controller supporte N nœuds via `--node` répétable. Le daemon Rust supporte aussi N peers dans `OMEGA_PEERS`.

### 2 nœuds — sans réplication

```bash
# Machine 1 : évince vers machine 2 (un seul store, pas de réplica)
OMEGA_STORES="10.10.0.12:9100" bash scripts/omega-proxmox-install.sh

# Machine 2 : évince vers machine 1
OMEGA_STORES="10.10.0.11:9100" bash scripts/omega-proxmox-install.sh

# omega-daemon — OMEGA_PEERS dans /etc/default/omega-daemon
OMEGA_PEERS=10.10.0.12:9200   # sur machine 1
OMEGA_PEERS=10.10.0.11:9200   # sur machine 2

# Controller
python3 -m controller.main daemon \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300
```

### 4 nœuds

```bash
# Installation sur chaque machine (adapter OMEGA_STORES = les autres nœuds)
OMEGA_STORES="10.10.0.12:9100,10.10.0.13:9100,10.10.0.14:9100" \
    bash scripts/omega-proxmox-install.sh

# omega-daemon — lister tous les autres nœuds en peers
OMEGA_PEERS=10.10.0.12:9200,10.10.0.13:9200,10.10.0.14:9200   # sur machine 1

# Controller avec 4 nœuds — ajouter autant de --node que nécessaire
python3 -m controller.main daemon \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    --node http://10.10.0.13:9300 \
    --node http://10.10.0.14:9300
```

### N nœuds (générique)

```bash
# Controller — répéter --node pour chaque nœud du cluster
python3 -m controller.main daemon \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    # ... autant de nœuds que nécessaire
    --poll-interval 5

# drain-node — même syntaxe, --source-node = node-a, node-b, node-c, node-d, ...
python3 -m controller.main drain-node \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    --node http://10.10.0.13:9300 \
    --source-node node-b \
    --dry-run
```

---

## 13. Commandes utiles au quotidien

```bash
# Voir l'état d'une VM
omega-qemu-launcher status --vm-id 100

# Forcer l'arrêt de l'agent d'une VM (si QEMU est déjà mort)
omega-qemu-launcher stop --vm-id 100

# Déclencher une migration manuelle
python3 -m controller.main migrate \
    --source http://10.10.0.11:9300 \
    --vm-id 100 \
    --target pve-node2 \
    --type live

# Vider un nœud avant maintenance (migre toutes ses VMs)
python3 -m controller.main drain-node \
    --node http://10.10.0.11:9300 \
    --node http://10.10.0.12:9300 \
    --node http://10.10.0.13:9300 \
    --source-node node-a \
    --dry-run          # enlever --dry-run pour exécuter réellement

# Nettoyer les pages distantes d'une VM arrêtée
curl -X DELETE http://127.0.0.1:9300/control/pages/100

# Recompiler après modifications
make build && systemctl restart omega-daemon
```

---

## 14. Dépannage

### La VM ne démarre pas

```bash
# Vérifier que le wrapper kvm est bien en place
ls -la /usr/bin/kvm
# Doit afficher : /usr/bin/kvm -> /usr/local/bin/kvm-omega

# Voir les logs du hookscript au démarrage
grep "vmid=100" /var/log/omega-hook.log | tail -20

# Voir les logs de l'agent de la VM
cat /var/lib/omega-qemu/vm-100/agent.log

# Tester manuellement le prepare (sans démarrer QEMU)
omega-qemu-launcher prepare \
    --vm-id 100 \
    --size-mib 2048 \
    --stores "10.10.0.12:9100,10.10.0.13:9100"
```

### omega-daemon ne démarre pas

```bash
journalctl -u omega-daemon -n 50 --no-pager
# Causes fréquentes :
# - port 9100 déjà utilisé → vérifier : ss -tlnp | grep 9100
# - /dev/dri/renderD128 absent → désactiver GPU : OMEGA_GPU_ENABLED=false
# - peers injoignables → normal au premier démarrage, ils se connectent ensuite
```

### Les stores ne se joignent pas

```bash
# Vérifier que le port 9100 est ouvert sur les autres nœuds
nc -zv 10.10.0.12 9100

# Vérifier le firewall Proxmox
iptables -L -n | grep 9100
# Si bloqué, ouvrir :
iptables -A INPUT -p tcp --dport 9100 -j ACCEPT
iptables -A INPUT -p tcp --dport 9200 -j ACCEPT
iptables -A INPUT -p tcp --dport 9300 -j ACCEPT
```

### Le controller ne voit pas tous les nœuds

```bash
# Tester manuellement chaque nœud
curl -s http://10.10.0.11:9300/control/status
curl -s http://10.10.0.12:9300/control/status
curl -s http://10.10.0.13:9300/control/status
# Chacun doit répondre un JSON avec "status": "ok"
```

### Restaurer le kvm original (rollback complet)

```bash
# Supprimer le wrapper et restaurer le kvm original
rm /usr/bin/kvm
mv /usr/bin/kvm.real /usr/bin/kvm

# Désactiver les services
systemctl stop omega-daemon omega-controller
systemctl disable omega-daemon omega-controller

# Désactiver le hookscript sur toutes les VMs
for vmid in $(qm list | awk 'NR>1 {print $1}'); do
    qm set "$vmid" --delete hookscript 2>/dev/null || true
done
```

---

## 15. Référence des ports et fichiers

### Ports réseau (ouvrir dans le firewall entre nœuds)

| Port | Protocole | Usage |
|---|---|---|
| **9100** | TCP | Store de pages (reçoit les pages évincées par les autres nœuds) |
| **9200** | HTTP | API cluster (état du nœud, métriques) |
| **9300** | HTTP | Canal de contrôle (migrations, quotas, statut) |

### Fichiers importants sur chaque nœud

| Chemin | Description |
|---|---|
| `/usr/bin/kvm` | → symlink vers `kvm-omega` |
| `/usr/bin/kvm.real` | kvm Proxmox original (sauvegardé) |
| `/usr/local/bin/kvm-omega` | wrapper shell généré par le launcher |
| `/usr/local/bin/omega-daemon` | daemon principal |
| `/usr/local/bin/omega-qemu-launcher` | gestionnaire agent par VM |
| `/usr/local/bin/node-a-agent` | agent mémoire (lancé par le launcher) |
| `/var/lib/vz/snippets/omega-hook.pl` | hookscript Proxmox |
| `/etc/default/omega-daemon` | configuration du daemon |
| `/var/lib/omega-qemu/vm-{id}/memory.json` | métadonnées memfd de la VM |
| `/var/lib/omega-qemu/vm-{id}/agent.log` | logs de l'agent de la VM |
| `/var/log/omega-hook.log` | logs du hookscript (démarrage/arrêt VMs) |

### Résumé services par machine

```
Chaque nœud Proxmox :
  systemctl status omega-daemon        ← toujours actif

Un seul nœud (au choix) :
  systemctl status omega-controller    ← toujours actif
```

---

## 16. Comportement en cas de crash — récupération automatique

### Que se passe-t-il si l'agent crashe en cours d'éviction ?

L'agent `node-a-agent` tient le handler uffd de la VM. Si le process crashe :

1. **La VM freeze** sur la prochaine page fault (le handler est mort — uffd ne répond plus).
2. **systemd redémarre l'agent** en 2 secondes (`RestartSec=2s`).
3. **L'agent reprend sa connexion** aux stores (les pages sont toujours là).
4. **Le nouveau handler uffd** re-mappe les pages en mémoire à la demande.
5. **La VM reprend** automatiquement — fenêtre de freeze < 3 secondes.

> **Note** : Les pages déjà évincées sur les stores ne sont pas perdues. Seules les pages
> en mémoire locale qui n'avaient pas encore été évincées peuvent être perdues si le crash
> survient pendant un batch PUT (cas extrêmement rare — window < 5 ms).

### Que se passe-t-il si le store crashe ?

Si un store (`node-bc-store`) crashe :

1. Les agents en mode réplication basculent sur l'autre store (si `--stores ip1:9100,ip2:9100`).
2. Les recalls en attente depuis le store crashé échouent → l'agent retente sur le store alternatif.
3. systemd redémarre le store en 3 secondes (`RestartSec=3s`).
4. Le store repart **vide** (les pages RAM sont perdues — pas de persistance disque par défaut).
   - Avec `STORE_DATA_PATH` et `persistent_store`, les pages sont récupérées depuis le disque.

> **Recommandation production** : activer la réplication sur 2 stores (`--stores ip1,ip2`)
> pour que le crash d'un store n'entraîne aucune perte de pages.

### Que se passe-t-il si un nœud redémarre ?

1. Les VMs hébergées sur ce nœud s'arrêtent (Proxmox shutdown).
2. L'orphan cleaner détecte les VMs disparues après 10 minutes de grâce.
3. Les pages orphelines sont supprimées automatiquement sur les autres nœuds.
4. Au redémarrage des VMs, l'agent repart proprement avec un nouveau handler uffd.

### Forcer un nettoyage manuel

```bash
# Supprimer les pages d'une VM spécifique sur tous les stores
for store_ip in 10.10.0.12 10.10.0.13; do
    curl -X DELETE "http://${store_ip}:9200/vm/100"
done

# Vérifier qu'il ne reste plus de pages
curl -s http://10.10.0.12:9200/status | python3 -c "import sys,json; print(json.load(sys.stdin))"
```

### Vérifier l'état de santé après un incident

```bash
# État de tous les stores
for n in 10.10.0.12 10.10.0.13; do
    echo "=== $n ==="
    curl -s "http://${n}:9200/status" | python3 -m json.tool
done

# Logs de l'agent (dernière minute)
journalctl -u omega-agent@100 --since "1 minute ago"

# Vérifier que les VMs tournent normalement
qm status 100
```

---

*omega-remote-paging — mis à jour le 2026-05-04*





