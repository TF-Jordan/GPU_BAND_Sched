# Développement et déploiement — omega-remote-paging

Ce guide couvre les trois environnements du projet :
- **Machine de dev** : ton PC avec RustRover, sans cluster
- **Lab** : cluster Proxmox en VMs sur ta machine physique (pour valider)
- **Production** : serveurs physiques Proxmox en cluster réel

---

## Sommaire

1. [Architecture des environnements](#1-architecture-des-environnements)
2. [Machine de développement](#2-machine-de-développement)
3. [Lab — cluster Proxmox en VMs locales](#3-lab--cluster-proxmox-en-vms-locales)
4. [Production — Proxmox bare metal](#4-production--proxmox-bare-metal)
5. [Script de déploiement](#5-script-de-déploiement)
6. [Workflow quotidien](#6-workflow-quotidien)
7. [Utilisation du système](#7-utilisation-du-système)

---

## 1. Architecture des environnements

```
┌──────────────────────────────────────────────────────────────┐
│  Ta machine physique                                         │
│                                                              │
│  RustRover · cargo · pytest · RustRover                     │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │  VM pve-a    │  │  VM pve-b    │  │  VM pve-c    │      │
│  │  Proxmox 8   │  │  Proxmox 8   │  │  Proxmox 8   │      │
│  │  4 Go RAM    │  │  4 Go RAM    │  │  4 Go RAM    │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│           LAB — réseau virtuel 192.168.100.0/24             │
└──────────────────────────────────────────────────────────────┘

             ↓ même binaire, même procédure de déploiement

┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Serveur A   │     │  Serveur B   │     │  Serveur C   │
│  Proxmox 8   │     │  Proxmox 8   │     │  Proxmox 8   │
│  bare metal  │     │  bare metal  │     │  bare metal  │
└──────────────┘     └──────────────┘     └──────────────┘
        PRODUCTION — réseau physique dédié
```

La distinction lab / prod se résume à une différence d'IPs et de ressources.
Le binaire `omega-daemon` et le controller Python sont identiques dans les deux cas.

---

## 2. Machine de développement

### 2.1 Outillage

```bash
# Rust (rustup recommandé — jamais le paquet apt)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup update stable

# Dépendances système pour compiler
sudo apt install pkg-config libssl-dev build-essential

# Python
sudo apt install python3-venv python3-pip

# Venv controller
cd controller
python3 -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"
```

### 2.2 RustRover

Ouvrir le dossier racine du repo (`omega-remote-paging/`).
RustRover détecte automatiquement le workspace `Cargo.toml` et indexe tous les crates.

Configurations utiles à créer dans RustRover :

| Nom | Commande | Répertoire |
|-----|----------|------------|
| `test all` | `cargo test` | racine |
| `build daemon` | `cargo build --release -p omega-daemon` | racine |
| `pytest` | `python3 -m pytest tests/ -v` | `controller/` |

### 2.3 Tests sans cluster

Tous les tests unitaires tournent sans Proxmox, sans VMs, sans réseau.

```bash
# Rust — 73 tests
cargo test

# Rust — doctests uniquement
cargo test --doc -p omega-daemon

# Python — 261 tests
cd controller && python3 -m pytest tests/ -q

# Tout d'un coup
cargo test && (cd controller && python3 -m pytest tests/ -q)
```

Résultat attendu : `73 passed` Rust · `261 passed` Python · `0 failed`.

### 2.4 Ce qui ne peut pas tourner en local

| Fonctionnalité | Raison |
|----------------|--------|
| `qm migrate` | Commande Proxmox uniquement |
| Lecture des PIDs QEMU | `/var/run/qemu-server/*.pid` n'existe qu'avec Proxmox |
| cgroups des VMs | `/sys/fs/cgroup/machine.slice/` créé par libvirt/Proxmox |
| TLS multi-nœuds | Requiert 3 adresses joignables |
| Remote paging réel | Requiert un agent uffd actif |

Pour tout le reste (politique de migration, scheduler vCPU, quotas, métriques) : les tests unitaires suffisent.

---

## 3. Lab — cluster Proxmox en VMs locales

Le lab te permet de valider le déploiement et les interactions réseau sans toucher à la production.

### 3.1 Prérequis : KVM nested

```bash
# Vérifier (Intel)
cat /sys/module/kvm_intel/parameters/nested   # doit afficher Y ou 1

# Vérifier (AMD)
cat /sys/module/kvm_amd/parameters/nested     # doit afficher 1

# Activer si nécessaire (Intel)
echo "options kvm-intel nested=1" | sudo tee /etc/modprobe.d/kvm-intel.conf
sudo modprobe -r kvm_intel && sudo modprobe kvm_intel

# Activer si nécessaire (AMD)
echo "options kvm-amd nested=1" | sudo tee /etc/modprobe.d/kvm-amd.conf
sudo modprobe -r kvm_amd && sudo modprobe kvm_amd
```

### 3.2 Créer les 3 VMs Proxmox

Télécharger l'ISO depuis proxmox.com/downloads, puis adapter selon ta version :

| Version PVE | Fichier ISO | `--os-variant` |
|-------------|-------------|----------------|
| 8.x | `proxmox-ve_8.x.iso` | `debian12` |
| 9.x | `proxmox-ve_9.x.iso` | `debian13` |

```bash
# Définir selon ta version
PVE_ISO=~/iso/proxmox-ve_9.1.1.iso   # adapter le nom du fichier
OS_VARIANT=debian13                    # debian12 pour PVE 8, debian13 pour PVE 9

# Créer les 3 VMs avec virt-install (libvirt)
for node in a b c; do
  virt-install \
    --name        pve-node-$node \
    --ram         4096 \
    --vcpus       2 \
    --disk        path=/var/lib/libvirt/images/pve-$node.qcow2,size=40,format=qcow2 \
    --cdrom       "$PVE_ISO" \
    --network     bridge=virbr0,model=virtio \
    --cpu         host-passthrough \
    --os-variant  "$OS_VARIANT" \
    --graphics    vnc \
    --noautoconsole
done
```

> `--cpu host-passthrough` est indispensable : sans ça, les VMs Proxmox
> ne peuvent pas créer leurs propres VMs QEMU (pas de KVM nested).
>
> Si `debian13` n'est pas reconnu par ta version de `osinfo-db`,
> utilise `--os-variant debian12` ou mets à jour la base :
> `sudo apt install osinfo-db osinfo-db-tools && osinfo-db-import --local`

### 3.3 Réseau lab recommandé

```
Réseau virtuel : 192.168.100.0/24 (virbr0 ou bridge dédié)

  pve-node-a  →  192.168.100.10
  pve-node-b  →  192.168.100.11
  pve-node-c  →  192.168.100.12
```

Configurer l'IP statique dans chaque VM pendant l'installation Proxmox,
ou via `/etc/network/interfaces` après :

```bash
# Sur pve-node-a (/etc/network/interfaces)
auto lo
iface lo inet loopback

auto ens3
iface ens3 inet static
    address 192.168.100.10/24
    gateway 192.168.100.1
```

### 3.4 Former le cluster Proxmox lab

```bash
# Sur pve-node-a (première fois uniquement)
pvecm create omega-lab

# Sur pve-node-b
pvecm add 192.168.100.10

# Sur pve-node-c
pvecm add 192.168.100.10

# Vérifier
pvecm status
```

### 3.5 Installation initiale omega sur le lab

Suivre `docs/installation.md` avec les IPs lab.
En résumé pour le lab :

```bash
# Depuis ta machine de dev, après compilation
./deploy.sh lab --first-install
```

Le script `deploy.sh` (section 5) gère la création des répertoires,
le fichier `.env` par nœud, et le démarrage des services.

---

## 4. Production — Proxmox bare metal

### 4.1 Différences avec le lab

| Aspect | Lab | Production |
|--------|-----|------------|
| Version PVE | 8.x ou 9.x | 8.x ou **9.x** |
| IPs nœuds | 192.168.100.10-12 | selon ton plan d'adressage |
| RAM par nœud | 4 Go (test) | 64-256 Go (réel) |
| Réseau omega | virbr0 virtuel | interface dédiée (bond, VLAN) |
| Disques store | image qcow2 | NVMe ou SSD local |
| `OMEGA_EVICT_THRESHOLD_PCT` | 70 (on déclenche vite) | 85 (plus conservateur) |

### 4.2 Réseau production recommandé

```
Interface publique (VM traffic) : eth0  — 10.0.1.0/24
Interface omega (store + contrôle) : eth1 — 192.168.10.0/24

  pve-node-a  →  192.168.10.10
  pve-node-b  →  192.168.10.11
  pve-node-c  →  192.168.10.12
```

Isoler le trafic omega sur une interface dédiée évite toute interférence
avec le trafic VM et simplifie les règles firewall.

### 4.3 Variables d'environnement production

```bash
# /etc/omega-store/daemon.env sur node-a
OMEGA_NODE_ID=pve-node-a
OMEGA_NODE_ADDR=192.168.10.10
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=192.168.10.11:9100,192.168.10.12:9100
OMEGA_EVICT_THRESHOLD_PCT=85
OMEGA_REPLICATION_FACTOR=2
OMEGA_TLS_DIR=/etc/omega-store/tls
OMEGA_NUM_PCPUS=32
```

---

## 5. Script de déploiement

Créer `deploy.sh` à la racine du repo :

```bash
#!/usr/bin/env bash
# deploy.sh — compile et déploie omega-remote-paging
#
# Usage :
#   ./deploy.sh lab              # déploie sur le lab (VMs locales)
#   ./deploy.sh prod             # déploie sur la production
#   ./deploy.sh lab --first-install   # installation initiale complète

set -euo pipefail

# ─── Configuration ────────────────────────────────────────────────────────────

LAB_NODES=(192.168.100.10 192.168.100.11 192.168.100.12)
LAB_NODE_IDS=(pve-node-a pve-node-b pve-node-c)
LAB_PEERS="192.168.100.11:9100,192.168.100.12:9100"   # depuis node-a

PROD_NODES=(192.168.10.10 192.168.10.11 192.168.10.12)
PROD_NODE_IDS=(pve-node-a pve-node-b pve-node-c)
PROD_PEERS="192.168.10.11:9100,192.168.10.12:9100"

FIRST_INSTALL=false
TARGET="${1:-}"
shift || true
[[ "${1:-}" == "--first-install" ]] && FIRST_INSTALL=true

# ─── Sélection de l'environnement ────────────────────────────────────────────

case "$TARGET" in
  lab)
    NODES=("${LAB_NODES[@]}")
    NODE_IDS=("${LAB_NODE_IDS[@]}")
    PEERS="$LAB_PEERS"
    EVICT_THRESHOLD=70
    NUM_PCPUS=2
    ;;
  prod)
    NODES=("${PROD_NODES[@]}")
    NODE_IDS=("${PROD_NODE_IDS[@]}")
    PEERS="$PROD_PEERS"
    EVICT_THRESHOLD=85
    NUM_PCPUS=32
    ;;
  *)
    echo "Usage: $0 lab|prod [--first-install]"
    exit 1
    ;;
esac

CONTROLLER="${NODES[0]}"   # nœud arbitraire qui tourne le controller

# ─── Compilation ─────────────────────────────────────────────────────────────

echo "==> Compilation omega-daemon..."
cargo build --release -p omega-daemon
echo "    OK — $(du -sh target/release/omega-daemon | cut -f1)"

# ─── Installation initiale (première fois) ───────────────────────────────────

first_install_node() {
  local ip="$1"
  local node_id="$2"
  local idx="$3"

  # Adresses des peers (tout sauf ce nœud)
  local node_peers=""
  for i in "${!NODES[@]}"; do
    [[ $i -eq $idx ]] && continue
    node_peers+="${NODES[$i]}:9100,"
  done
  node_peers="${node_peers%,}"

  echo "    [first-install] $node_id ($ip)"

  ssh root@"$ip" bash <<EOF
set -e
mkdir -p /etc/omega-store/tls /var/log/omega

# Binaire
cp /tmp/omega-daemon /usr/local/bin/omega-daemon
chmod 755 /usr/local/bin/omega-daemon

# Fichier d'environnement
cat > /etc/omega-store/daemon.env << 'ENVEOF'
OMEGA_NODE_ID=$node_id
OMEGA_NODE_ADDR=$ip
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=$node_peers
OMEGA_EVICT_THRESHOLD_PCT=$EVICT_THRESHOLD
OMEGA_REPLICATION_FACTOR=2
OMEGA_TLS_DIR=/etc/omega-store/tls
OMEGA_NUM_PCPUS=$NUM_PCPUS
ENVEOF
chmod 600 /etc/omega-store/daemon.env

# Unité systemd
cat > /etc/systemd/system/omega-daemon.service << 'SVCEOF'
[Unit]
Description=omega-daemon — paging RAM distant + scheduler vCPU
After=network.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=/etc/omega-store/daemon.env
ExecStart=/usr/local/bin/omega-daemon
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
SyslogIdentifier=omega-daemon

[Install]
WantedBy=multi-user.target
SVCEOF

systemctl daemon-reload
systemctl enable omega-daemon
systemctl start omega-daemon
EOF
}

# ─── Déploiement du daemon ────────────────────────────────────────────────────

echo "==> Déploiement omega-daemon sur ${#NODES[@]} nœuds ($TARGET)..."
for i in "${!NODES[@]}"; do
  ip="${NODES[$i]}"
  node_id="${NODE_IDS[$i]}"
  echo "  → $node_id ($ip)"

  scp -q target/release/omega-daemon root@"$ip":/tmp/omega-daemon

  if $FIRST_INSTALL; then
    first_install_node "$ip" "$node_id" "$i"
  else
    ssh root@"$ip" "
      mv /tmp/omega-daemon /usr/local/bin/omega-daemon
      chmod 755 /usr/local/bin/omega-daemon
      systemctl restart omega-daemon
      systemctl is-active omega-daemon
    "
  fi
done

# ─── Déploiement du controller Python (un seul nœud au choix) ───────────────

echo "==> Déploiement controller Python sur $CONTROLLER..."
rsync -aq --exclude='__pycache__' --exclude='*.pyc' --exclude='.venv' \
  controller/ root@"$CONTROLLER":/opt/omega-remote-paging/controller/

# Construire la liste --node http://IP:9300 pour tous les nœuds
NODE_ARGS=""
for ip in "${NODES[@]}"; do
  NODE_ARGS+=" --node http://${ip}:9300"
done

if $FIRST_INSTALL; then
  ssh root@"$CONTROLLER" bash <<EOF
set -e
python3 -m venv /opt/omega-controller-venv
/opt/omega-controller-venv/bin/pip install -q -e /opt/omega-remote-paging/controller/

cat > /etc/systemd/system/omega-controller.service << 'SVCEOF'
[Unit]
Description=omega-controller — politique de migration
After=omega-daemon.service

[Service]
Type=simple
WorkingDirectory=/opt/omega-remote-paging/controller
ExecStart=/opt/omega-controller-venv/bin/python3 -m controller.main daemon \
    $NODE_ARGS \
    --poll-interval 5
Restart=on-failure
RestartSec=10
SyslogIdentifier=omega-controller

[Install]
WantedBy=multi-user.target
SVCEOF

systemctl daemon-reload
systemctl enable omega-controller
systemctl start omega-controller
EOF
else
  ssh root@"$CONTROLLER" "
    /opt/omega-controller-venv/bin/pip install -q -e /opt/omega-remote-paging/controller/ 2>/dev/null || true
    systemctl restart omega-controller
    systemctl is-active omega-controller
  "
fi

echo ""
echo "==> Déploiement $TARGET terminé."
echo ""
echo "    Vérification rapide :"
for ip in "${NODES[@]}"; do
  status=$(ssh root@"$ip" "systemctl is-active omega-daemon" 2>/dev/null || echo "ERREUR")
  echo "      $ip  omega-daemon : $status"
done
echo "      $CONTROLLER  omega-controller : $(ssh root@$CONTROLLER 'systemctl is-active omega-controller' 2>/dev/null || echo 'ERREUR')"
```

```bash
chmod +x deploy.sh
```

---

## 6. Workflow quotidien

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│  1. Coder dans RustRover                                    │
│                                                             │
│  2. Tester en local (sans cluster)                         │
│       cargo test && cd controller && pytest tests/ -q      │
│                                                             │
│  3. Déployer sur le lab                                     │
│       ./deploy.sh lab                                       │
│                                                             │
│  4. Valider sur le lab                                      │
│       (voir section 7 — utilisation)                        │
│                                                             │
│  5. Déployer en production                                  │
│       ./deploy.sh prod                                      │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Première installation

```bash
# Lab
./deploy.sh lab --first-install

# Production
./deploy.sh prod --first-install
```

### Mises à jour courantes (redéploiement)

```bash
# Après modification du daemon Rust
./deploy.sh lab

# Après modification du controller Python seulement (pas de recompilation)
rsync -av controller/ root@192.168.100.10:/opt/omega-remote-paging/controller/
ssh root@192.168.100.10 "systemctl restart omega-controller"
```

---

## 7. Utilisation du système

### 7.1 Vérifier l'état du cluster

```bash
# État de chaque nœud (RAM, vCPU, VMs)
for ip in 192.168.100.10 192.168.100.11 192.168.100.12; do
  echo "=== $ip ==="
  curl -s http://$ip:9300/control/status | jq '{
    ram_pct:      .node.mem_usage_pct,
    vcpu_free:    .node.vcpu_free,
    vcpu_total:   .node.vcpu_total,
    vms:          (.node.local_vms | length),
    pages_stored: .node.pages_stored
  }'
done
```

### 7.2 Enregistrer une VM pour le paging

Après avoir créé la VM 101 dans Proxmox :

```bash
NODE=http://192.168.100.10:9300

# Quota mémoire (limite le paging de cette VM)
curl -s -X POST $NODE/control/vm/101/quota \
  -H "Content-Type: application/json" \
  -d '{"max_mem_mib": 2048}'

# Enregistrement vCPU (min=1, max=4)
curl -s -X POST $NODE/control/vm/101/vcpu \
  -H "Content-Type: application/json" \
  -d '{"min_vcpus": 1, "max_vcpus": 4}'
```

### 7.3 Voir les quotas et le scheduler vCPU

```bash
# Liste des quotas mémoire
curl -s http://192.168.100.10:9300/control/quotas | jq .

# État du scheduler vCPU
curl -s http://192.168.100.10:9300/control/vcpu/status | jq .

# Métriques Prometheus brutes
curl -s http://192.168.100.10:9300/control/metrics
```

### 7.4 Migration manuelle

```bash
# Live (VM reste allumée, KVM pre-copy)
python3 -m controller.main migrate \
  --source http://192.168.100.10:9300 \
  --vm-id 101 \
  --target pve-node-b \
  --type live

# Cold (VM stoppée, transférée, redémarrée)
python3 -m controller.main migrate \
  --source http://192.168.100.10:9300 \
  --vm-id 101 \
  --target pve-node-b \
  --type cold

# Voir le statut de la migration
curl -s http://192.168.100.10:9300/control/migrations | jq .
curl -s http://192.168.100.10:9300/control/migrations/1 | jq .
```

### 7.5 Voir ce que le système recommande (sans migrer)

```bash
# Recommandations de migration sur node-a
curl -s http://192.168.100.10:9300/control/migrate/recommend | jq .

# Ou via le controller Python en mode dry-run
python3 -m controller.main daemon \
  --node-a http://192.168.100.10:9300 \
  --node-b http://192.168.100.11:9300 \
  --node-c http://192.168.100.12:9300 \
  --poll-interval 10 \
  --dry-run
```

### 7.6 Suivre les logs en temps réel

```bash
# Daemon (sur le nœud)
ssh root@192.168.100.10 "journalctl -u omega-daemon -f"

# Controller (politique de migration)
ssh root@192.168.100.10 "journalctl -u omega-controller -f"

# Les deux en parallèle depuis ta machine
ssh root@192.168.100.10 "journalctl -u omega-daemon -u omega-controller -f"
```

### 7.7 Éviction manuelle

```bash
# Évincer des pages d'une VM spécifique (libère de la RAM distante)
curl -s -X POST http://192.168.100.10:9300/control/evict/101 | jq .

# Déclencher un cycle d'éviction général
curl -s -X POST http://192.168.100.10:9300/control/evict \
  -H "Content-Type: application/json" \
  -d '{"count": 64}' | jq .
```

### 7.8 Supprimer les pages d'une VM après migration

```bash
# Après avoir migré la VM 101 de node-a vers node-b,
# supprimer ses pages du store de node-a (libère la RAM du store)
curl -s -X DELETE http://192.168.100.10:9300/control/pages/101 | jq .
```

---

## Référence rapide

| Objectif | Commande |
|----------|----------|
| Tests locaux complets | `cargo test && cd controller && pytest tests/ -q` |
| Première installation lab | `./deploy.sh lab --first-install` |
| Mise à jour lab | `./deploy.sh lab` |
| Mise à jour prod | `./deploy.sh prod` |
| État nœud | `curl http://IP:9300/control/status \| jq .node` |
| Migration manuelle live | `python3 -m controller.main migrate --source ... --type live` |
| Migration manuelle cold | `python3 -m controller.main migrate --source ... --type cold` |
| Daemon migration auto | `python3 -m controller.main daemon --node-a ... --node-b ... --node-c ...` |
| Logs daemon | `journalctl -u omega-daemon -f` |
| Logs controller | `journalctl -u omega-controller -f` |
| Métriques Prometheus | `curl http://IP:9300/control/metrics` |

