# Utiliser omega-remote-paging dans un cluster KVM (lab virtuel)

> **Workflow de déploiement** : compiler sur ta machine de dev avec RustRover,
> puis déployer avec `./deploy.sh lab`. Ne pas compiler directement sur les nœuds.
> Voir `docs/developpement-et-deploiement.md` pour le workflow complet.

Ce guide suppose que vous avez un cluster Proxmox VE de 3 nœuds fonctionnel sur KVM, tel que décrit dans `cluster-kvm.md`. Compatible PVE 8.x et 9.x. Les adresses utilisées correspondent à ce guide :

| Nœud | IP | Hostname |
|------|----|----------|
| pve1 | 10.10.0.11 | pve1.lab.local |
| pve2 | 10.10.0.12 | pve2.lab.local |
| pve3 | 10.10.0.13 | pve3.lab.local |

---

## 1. Préparer les nœuds Proxmox

### 1.1 Installer les dépendances système

Se connecter en SSH sur **chaque nœud Proxmox** (les 3 VMs KVM) et exécuter :

```bash
# Dépendances Rust et build
apt install -y \
    curl \
    git \
    build-essential \
    pkg-config \
    libssl-dev \
    python3 \
    python3-pip \
    python3-venv \
    iproute2 \
    iptables

# Installer Rust (si pas présent)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Vérifier
rustc --version
cargo --version
```

### 1.2 Cloner le dépôt

```bash
# Sur chaque nœud
cd /opt
git clone https://github.com/votre-org/omega-remote-paging.git
cd omega-remote-paging
```

Si vous n'avez pas de dépôt distant, copier les fichiers depuis votre machine de développement :

```bash
# Depuis votre machine de dev, copier vers chaque nœud
rsync -avz /chemin/vers/omega-remote-paging/ root@10.10.0.11:/opt/omega-remote-paging/
rsync -avz /chemin/vers/omega-remote-paging/ root@10.10.0.12:/opt/omega-remote-paging/
rsync -avz /chemin/vers/omega-remote-paging/ root@10.10.0.13:/opt/omega-remote-paging/
```

---

## 2. Compiler le daemon Rust

```bash
# Sur chaque nœud — dans /opt/omega-remote-paging
cd /opt/omega-remote-paging

# Build en mode release (optimisé)
cargo build --release 2>&1 | tail -20

# Les binaires sont dans target/release/
ls -lh target/release/omega-daemon
ls -lh target/release/node-bc-store
ls -lh target/release/node-a-agent
```

Le build prend environ 2 à 5 minutes la première fois (téléchargement des dépendances + compilation). Les builds suivants sont beaucoup plus rapides.

```bash
# Copier les binaires dans /usr/local/bin
cp target/release/omega-daemon /usr/local/bin/
cp target/release/node-bc-store /usr/local/bin/
cp target/release/node-a-agent  /usr/local/bin/
```

---

## 3. Configurer le daemon sur chaque nœud

### 3.1 Créer les répertoires de travail

```bash
# Sur chaque nœud
mkdir -p /etc/omega
mkdir -p /var/log/omega
mkdir -p /var/lib/omega/store
mkdir -p /run/omega
```

### 3.2 Créer le fichier de configuration

Le daemon lit sa configuration depuis les arguments CLI. Créer un script de démarrage par nœud.

**Sur pve1 (10.10.0.11) :**

```bash
cat > /etc/omega/daemon.env << 'EOF'
OMEGA_NODE_ID=pve1
OMEGA_NODE_ADDR=10.10.0.11
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=10.10.0.12:9200,10.10.0.13:9200
OMEGA_EVICT_THRESHOLD_PCT=80
OMEGA_LOG_LEVEL=info
EOF
```

**Sur pve2 (10.10.0.12) :**

```bash
cat > /etc/omega/daemon.env << 'EOF'
OMEGA_NODE_ID=pve2
OMEGA_NODE_ADDR=10.10.0.12
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=10.10.0.11:9200,10.10.0.13:9200
OMEGA_EVICT_THRESHOLD_PCT=80
OMEGA_LOG_LEVEL=info
EOF
```

**Sur pve3 (10.10.0.13) :**

```bash
cat > /etc/omega/daemon.env << 'EOF'
OMEGA_NODE_ID=pve3
OMEGA_NODE_ADDR=10.10.0.13
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=10.10.0.11:9200,10.10.0.12:9200
OMEGA_EVICT_THRESHOLD_PCT=80
OMEGA_LOG_LEVEL=info
EOF
```

### 3.3 Créer le service systemd

```bash
# Sur chaque nœud — adapter le fichier ExecStart selon le nœud
cat > /etc/systemd/system/omega-daemon.service << 'EOF'
[Unit]
Description=omega-remote-paging daemon
After=network.target

[Service]
Type=simple
EnvironmentFile=/etc/omega/daemon.env
ExecStart=/usr/local/bin/omega-daemon \
    --node-id ${OMEGA_NODE_ID} \
    --node-addr ${OMEGA_NODE_ADDR} \
    --store-port ${OMEGA_STORE_PORT} \
    --api-port ${OMEGA_API_PORT} \
    --peers ${OMEGA_PEERS} \
    --evict-threshold-pct ${OMEGA_EVICT_THRESHOLD_PCT} \
    --log-level ${OMEGA_LOG_LEVEL}
Restart=on-failure
RestartSec=5
StandardOutput=journal
StandardError=journal
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

# Activer et démarrer
systemctl daemon-reload
systemctl enable omega-daemon
systemctl start omega-daemon
```

### 3.4 Vérifier que le daemon démarre

```bash
# Vérifier le statut
systemctl status omega-daemon

# Suivre les logs
journalctl -u omega-daemon -f

# Tester l'API HTTP
curl -s http://10.10.0.11:9200/api/status | python3 -m json.tool
curl -s http://10.10.0.11:9200/health
```

Résultat attendu de `/health` :
```json
{"status":"ok"}
```

---

## 4. Configurer la QoS réseau

Le script `setup_qos.sh` priorise le trafic cluster Proxmox (corosync) et limite le trafic de paging pour ne pas saturer le réseau.

```bash
# Sur chaque nœud — adapter l'interface réseau (eth0, ens3, ...)
# Vérifier le nom de l'interface
ip link show

# Configurer (remplacer eth0 par le nom réel)
bash /opt/omega-remote-paging/scripts/setup_qos.sh eth0

# Vérifier les règles tc
tc qdisc show dev eth0
tc class show dev eth0
```

---

## 5. Configurer le contrôleur Python

Le contrôleur tourne sur **un seul nœud** (pve1 par convention) et gère le cluster entier.

### 5.1 Installer l'environnement Python

```bash
# Sur pve1 uniquement
cd /opt/omega-remote-paging/controller

python3 -m venv .venv
source .venv/bin/activate

pip install -e .
pip install -r requirements.txt
```

### 5.2 Configurer le contrôleur

```bash
cat > /etc/omega/controller.yaml << 'EOF'
proxmox:
  host: 10.10.0.11
  port: 8006
  user: root@pam
  password: "votre_mot_de_passe_proxmox"
  verify_ssl: false

nodes:
  - id: pve1
    api_url: http://10.10.0.11:9200
    control_url: http://10.10.0.11:9300
    num_pcpus: 4
    vram_mib: 0       # pas de GPU dans ce lab

  - id: pve2
    api_url: http://10.10.0.12:9200
    control_url: http://10.10.0.12:9300
    num_pcpus: 4
    vram_mib: 0

  - id: pve3
    api_url: http://10.10.0.13:9200
    control_url: http://10.10.0.13:9300
    num_pcpus: 4
    vram_mib: 0

policy:
  evict_threshold_pct: 80
  migration_threshold_pages: 1000
  check_interval_seconds: 30
  circuit_failure_threshold: 3
  collector_retries: 3
EOF
```

### 5.3 Créer le service systemd du contrôleur

```bash
cat > /etc/systemd/system/omega-controller.service << 'EOF'
[Unit]
Description=omega-remote-paging controller
After=network.target omega-daemon.service

[Service]
Type=simple
WorkingDirectory=/opt/omega-remote-paging/controller
ExecStart=/opt/omega-remote-paging/controller/.venv/bin/python \
    -m controller.main \
    --config /etc/omega/controller.yaml
Restart=on-failure
RestartSec=10
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable omega-controller
systemctl start omega-controller

# Suivre les logs
journalctl -u omega-controller -f
```

---

## 6. Tester le système

### 6.1 Vérifier que tous les daemons se voient

```bash
# Depuis pve1 — l'API de chaque nœud doit répondre
for node in 10.10.0.11 10.10.0.12 10.10.0.13; do
    echo "=== $node ==="
    curl -s http://${node}:9200/api/status | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f\"  node_id: {d['node_id']}\")
print(f\"  mem_available_kb: {d['mem_available_kb']}\")
print(f\"  pages_stored: {d['pages_stored']}\")
"
done
```

### 6.2 Créer une VM de test et surveiller les pages

```bash
# Créer une VM depuis l'interface Proxmox web ou CLI
# Exemple : VM 101 avec 512 Mo RAM sur pve1

# Après démarrage de la VM, surveiller ses pages distantes
curl -s http://10.10.0.11:9300/control/status | python3 -m json.tool

# Consulter les quotas RAM
curl -s http://10.10.0.11:9300/control/quotas | python3 -m json.tool
```

### 6.3 Configurer manuellement un quota pour une VM de test

```bash
# Configurer la VM 101 avec 512 Mo total (400 Mo local + 112 Mo distant)
curl -s -X POST http://10.10.0.11:9300/control/vm/101/quota \
    -H "Content-Type: application/json" \
    -d '{
        "max_mem_mib": 512,
        "local_budget_mib": 400,
        "remote_budget_mib": 112
    }' | python3 -m json.tool

# Vérifier le quota
curl -s http://10.10.0.11:9300/control/vm/101/quota | python3 -m json.tool
```

### 6.4 Simuler une pression mémoire (lab)

```bash
# Dans une VM Linux (par exemple VM 101)
# Remplir la RAM pour déclencher l'éviction
stress-ng --vm 1 --vm-bytes 80% --timeout 60s &

# Sur pve1 — observer les métriques en temps réel
watch -n2 'curl -s http://10.10.0.11:9300/control/metrics'
```

### 6.5 Déclencher manuellement une éviction

```bash
# Demander l'éviction de 100 pages depuis pve1
curl -s -X POST http://10.10.0.11:9300/control/evict \
    -H "Content-Type: application/json" \
    -d '{"count": 100}' | python3 -m json.tool
```

---

## 7. Surveiller le système

### 7.1 Métriques Prometheus

```bash
# Format Prometheus sur chaque nœud
curl -s http://10.10.0.11:9300/control/metrics
```

Sortie exemple :
```
omega_pages_stored{node="pve1"} 847
omega_mem_available_kb{node="pve1"} 3145728
omega_mem_usage_pct{node="pve1"} 61.23
omega_store_get_total{node="pve1"} 1284
omega_store_put_total{node="pve1"} 2156
omega_store_hit_rate_pct{node="pve1"} 94.2
```

### 7.2 État des vCPU

```bash
curl -s http://10.10.0.11:9300/control/vcpu/status | python3 -m json.tool
```

### 7.3 Pages par VM

```bash
curl -s http://10.10.0.11:9200/api/pages | python3 -m json.tool
```

---

## 8. Dépannage

### Le daemon ne démarre pas

```bash
# Voir l'erreur complète
journalctl -u omega-daemon --no-pager | tail -30

# Problème fréquent : port déjà utilisé
ss -tlnp | grep -E '9100|9200|9300'
```

### Les nœuds ne se voient pas

```bash
# Tester la connectivité TCP entre nœuds
# Depuis pve1 vers pve2
nc -zv 10.10.0.12 9100   # store
nc -zv 10.10.0.12 9200   # API
nc -zv 10.10.0.12 9300   # control

# Vérifier le firewall
iptables -L -n | grep -E '9100|9200|9300'
```

### Ouvrir les ports si nécessaire

```bash
# Sur chaque nœud — autoriser les ports omega
iptables -A INPUT -p tcp --dport 9100 -s 10.10.0.0/24 -j ACCEPT
iptables -A INPUT -p tcp --dport 9200 -s 10.10.0.0/24 -j ACCEPT
iptables -A INPUT -p tcp --dport 9300 -s 10.10.0.0/24 -j ACCEPT

# Rendre persistant
apt install iptables-persistent -y
netfilter-persistent save
```

### Les pages ne sont pas évincées

Vérifier le seuil d'éviction : si la RAM disponible est au-dessus du seuil (80% par défaut), aucune éviction n'a lieu.

```bash
# Voir la RAM disponible
free -m

# Voir le seuil configuré
curl -s http://10.10.0.11:9300/control/status | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f\"usage: {d['node']['mem_usage_pct']:.1f}%\")
"
```

---

## 9. Arrêter proprement

```bash
# Sur chaque nœud
systemctl stop omega-daemon
systemctl stop omega-controller   # sur pve1 uniquement

# Vider les pages stockées (optionnel — elles seront perdues au redémarrage sans persistance)
curl -s -X DELETE http://10.10.0.11:9300/control/pages/101
```

---

## 10. Mode standalone — node-a-agent + node-bc-store

Plutôt que d'utiliser `omega-daemon`, on peut déployer les composants individuellement. C'est le mode recommandé pour tester les nouvelles fonctionnalités (vCPU élastique, GPU, Ceph, orphan cleaner).

Tous les nœuds font tourner un store. Chaque agent cible les stores des autres nœuds.

### Stores (sur chaque nœud)

```bash
# Variables d'environnement complètes
cat > /etc/omega/store.env << 'EOF'
STORE_LISTEN=0.0.0.0:9100
STORE_STATUS_LISTEN=0.0.0.0:9200        # HTTP status (RAM dispo, vcpu_total/free, ceph_enabled)
STORE_NODE_ID=$(hostname)
STORE_DATA_PATH=/var/lib/omega-store
STORE_MAX_PAGES=0                        # 0 = illimité
STORE_ORPHAN_CHECK_INTERVAL_SECS=300     # nettoyage orphelins toutes les 5 min
STORE_ORPHAN_GRACE_SECS=600              # 10 min de grâce avant suppression
# Ceph — auto-détecté si /etc/ceph/ceph.conf présent (apt install librados-dev)
STORE_CEPH_CONF=/etc/ceph/ceph.conf
STORE_CEPH_POOL=omega-pages
STORE_CEPH_USER=client.admin
RUST_LOG=info
EOF

node-bc-store \
  --listen 0.0.0.0:9100 \
  --status-listen 0.0.0.0:9200 \
  --node-id $(hostname) \
  --store-data-path /var/lib/omega-store
```

### Agent (une instance par VM, sur le nœud qui l'héberge)

```bash
# Adapter STORES = les autres nœuds (pas le nœud local)
cat > /etc/omega/agent-9001.env << 'EOF'
AGENT_STORES=10.10.0.12:9100,10.10.0.13:9100
AGENT_STATUS_ADDRS=10.10.0.12:9200,10.10.0.13:9200
AGENT_VM_ID=9001
AGENT_VM_REQUESTED_MIB=2048
AGENT_REGION_MIB=2048
AGENT_CURRENT_NODE=pve1
AGENT_MODE=daemon

# vCPU élastique
AGENT_VM_VCPUS=8                    # max vCPUs (valeur demandée à la création)
AGENT_VM_INITIAL_VCPUS=1            # vCPUs au démarrage
AGENT_VCPU_HIGH_THRESHOLD_PCT=75    # scale-up si util > 75%
AGENT_VCPU_LOW_THRESHOLD_PCT=25     # scale-down si util < 25%
AGENT_VCPU_SCALE_INTERVAL_SECS=30
AGENT_VCPU_OVERCOMMIT_RATIO=3       # 1 cœur physique = 3 vCPUs max

# GPU (auto-détecté via qm config)
AGENT_GPU_REQUIRED=false
AGENT_GPU_PLACEMENT_INTERVAL_SECS=60
AGENT_GPU_QUANTUM_SECS=30

# Réplication (auto-désactivée si tous les stores = Ceph)
AGENT_REPLICATION_ENABLED=true
AGENT_METRICS_LISTEN=0.0.0.0:9300
RUST_LOG=info
EOF

node-a-agent \
  --stores 10.10.0.12:9100,10.10.0.13:9100 \
  --vm-id 9001 \
  --vm-requested-mib 2048 \
  --region-mib 2048 \
  --mode daemon
```

### Hookscript Proxmox (démarrage/arrêt automatique)

```bash
# Copier le hook
cp /opt/omega-remote-paging/scripts/omega-hook.pl /var/lib/vz/snippets/

# Associer à la VM 9001
qm set 9001 --hookscript local:snippets/omega-hook.pl
```

### Vérification

```bash
# Status store
curl -s http://10.10.0.12:9200/status | python3 -m json.tool
# → {"node_id":"pve2","available_mib":...,"vcpu_total":24,"vcpu_free":18,"ceph_enabled":false,...}

# Métriques agent (Prometheus)
curl http://10.10.0.11:9300/metrics

# Pool vCPU partagé
cat /run/omega-vcpu-pool.json
```
