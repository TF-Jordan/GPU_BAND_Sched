# Utiliser omega-remote-paging dans un cluster de machines physiques

> **Workflow de déploiement** : compiler sur ta machine de dev avec RustRover,
> puis déployer avec `./deploy.sh prod`. Ne pas compiler directement sur les nœuds.
> Voir `docs/developpement-et-deploiement.md` pour le workflow complet.

Ce guide suppose que vous avez un cluster Proxmox VE de 3 machines physiques fonctionnel, tel que décrit dans `cluster-physique.md`. Compatible PVE 8.x et 9.x. Les adresses utilisées correspondent à ce guide :

| Nœud | IP | Hostname | Rôle |
|------|----|----------|------|
| pve1 | 192.168.1.11 | pve1.monlab.local | Nœud (héberge VMs + store) |
| pve2 | 192.168.1.12 | pve2.monlab.local | Nœud (héberge VMs + store) |
| pve3 | 192.168.1.13 | pve3.monlab.local | Nœud (héberge VMs + store) |

Tous les nœuds sont identiques. pve1 héberge aussi le contrôleur (arbitraire — configurable via `OMEGA_CONTROLLER`).

---

## Différences importantes avec le lab KVM

Sur des machines physiques, plusieurs éléments changent par rapport au lab virtuel :

| Aspect | Lab KVM | Machines physiques |
|--------|---------|-------------------|
| RAM disponible | Limitée par la machine hôte | RAM réelle de chaque serveur |
| Réseau | Bridge virtuel (100 Mbps effectif) | Switch physique 1 Gbps ou plus |
| CPU | Partagé avec l'hôte | Entièrement dédié |
| GPU | Absent | Présent si équipé (NVIDIA/AMD) |
| Latence réseau | ~1 ms (loopback) | ~0.1–0.5 ms (switch local) |
| Stabilité | VMs KVM peuvent être suspendues | Serveurs physiques : uptime permanent |

Ces différences ont un impact sur la configuration : les seuils d'éviction peuvent être plus agressifs, le réseau est plus rapide donc les accès distants sont moins pénalisants.

---

## 1. Préparer les nœuds physiques

### 1.1 Installer les dépendances

Se connecter en SSH sur **chaque machine physique** :

```bash
ssh root@192.168.1.11   # pve1
```

Installer les prérequis :

```bash
apt update
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
    iptables \
    htop \
    numactl \
    linux-tools-common     # pour perf, cpupower

# Installer Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

### 1.2 Vérifier que userfaultfd est disponible

```bash
# Le kernel doit supporter userfaultfd (Proxmox VE 7+ = kernel ≥ 5.15 : OK)
ls -la /dev/userfaultfd 2>/dev/null || grep -r USERFAULTFD /boot/config-$(uname -r)
```

Si `/dev/userfaultfd` n'existe pas mais que la config kernel le supporte, l'accès se fait via syscall (comportement par défaut du projet).

### 1.3 Activer les hugepages (optionnel, améliore les performances)

```bash
# Hugepages transparentes — toujours actif sur les kernels récents
cat /sys/kernel/mm/transparent_hugepage/enabled

# Pour des performances optimales de l'agent uffd
echo madvise > /sys/kernel/mm/transparent_hugepage/enabled
echo "vm.nr_hugepages = 1024" >> /etc/sysctl.conf
sysctl -p
```

---

## 2. Compiler et déployer les binaires

La compilation se fait sur la machine de dev, le déploiement via `deploy.sh` ou manuellement.

```bash
# Sur la machine de dev — compiler
RUSTFLAGS="-C target-cpu=native" cargo build --release

# Déployer sur tous les nœuds en une commande (lit scripts/cluster.conf)
OMEGA_NODES="pve1,pve2,pve3" OMEGA_CONTROLLER="pve1" bash scripts/deploy.sh
```

Ou manuellement sur chaque nœud (même procédure pour tous) :

```bash
# Sur chaque nœud physique — installer les binaires
install -m 755 /tmp/omega-daemon         /usr/local/bin/
install -m 755 /tmp/node-a-agent         /usr/local/bin/
install -m 755 /tmp/omega-qemu-launcher  /usr/local/bin/

# Répertoires
mkdir -p /etc/omega /var/log/omega /var/lib/omega/store /run/omega
```

L'option `RUSTFLAGS="-C target-cpu=native"` active les instructions CPU spécifiques à la machine (AVX2, AVX-512 si disponibles) — gain de performance significatif sur les opérations de copie de pages.

---

## 3. Configurer le daemon sur chaque nœud physique

### 3.1 Détecter les ressources disponibles

```bash
# Nombre de cœurs physiques (pas de HT)
nproc --all
cat /proc/cpuinfo | grep "cpu cores" | head -1

# RAM totale
free -g

# Interfaces réseau
ip link show
```

### 3.2 Créer la configuration par nœud

**Sur pve1 (192.168.1.11) :**

```bash
cat > /etc/omega/daemon.env << 'EOF'
OMEGA_NODE_ID=pve1
OMEGA_NODE_ADDR=192.168.1.11
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=192.168.1.12:9200,192.168.1.13:9200
# Seuil plus agressif sur physique (RAM dédiée, pas de contention hôte)
OMEGA_EVICT_THRESHOLD_PCT=85
OMEGA_LOG_LEVEL=info
EOF
```

**Sur pve2 (192.168.1.12) :**

```bash
cat > /etc/omega/daemon.env << 'EOF'
OMEGA_NODE_ID=pve2
OMEGA_NODE_ADDR=192.168.1.12
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=192.168.1.11:9200,192.168.1.13:9200
OMEGA_EVICT_THRESHOLD_PCT=85
OMEGA_LOG_LEVEL=info
EOF
```

**Sur pve3 (192.168.1.13) :**

```bash
cat > /etc/omega/daemon.env << 'EOF'
OMEGA_NODE_ID=pve3
OMEGA_NODE_ADDR=192.168.1.13
OMEGA_STORE_PORT=9100
OMEGA_API_PORT=9200
OMEGA_CONTROL_PORT=9300
OMEGA_PEERS=192.168.1.11:9200,192.168.1.12:9200
OMEGA_EVICT_THRESHOLD_PCT=85
OMEGA_LOG_LEVEL=info
EOF
```

### 3.3 Service systemd avec affinité CPU (optionnel)

Sur des serveurs NUMA, exécuter le daemon sur les CPUs du bon NUMA node améliore la bande passante mémoire.

```bash
# Vérifier la topologie NUMA
numactl --hardware

# Créer le service — CPUAffinity réserve des cœurs pour le daemon
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
# Limites fichiers élevées (beaucoup de connexions TCP possibles)
LimitNOFILE=131072
LimitMEMLOCK=infinity
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable omega-daemon
systemctl start omega-daemon
```

### 3.4 Vérifier le démarrage

```bash
systemctl status omega-daemon
journalctl -u omega-daemon -n 50

# Test rapide
curl -s http://192.168.1.11:9200/health
```

---

## 4. Configurer la QoS réseau physique

Sur un réseau physique à 1 Gbps, plusieurs VMs qui pagent simultanément peuvent saturer le lien. Le script HTB (Hierarchical Token Bucket) limite le trafic de paging tout en préservant le trafic cluster Proxmox (corosync, migration).

```bash
# Identifier l'interface réseau principale
ip route get 192.168.1.12 | grep dev

# Configurer (exemple avec eno1)
bash /opt/omega-remote-paging/scripts/setup_qos.sh eno1

# Vérifier
tc qdisc show dev eno1
tc class show dev eno1
```

La configuration par défaut crée 3 classes HTB :
- **Classe 1** : corosync (trafic cluster Proxmox) → haute priorité, 200 Mbps
- **Classe 2** : trafic VMs normal → 600 Mbps
- **Classe 3** : trafic TCP paging omega (port 9100) → 200 Mbps maximum

Ajuster les bandes passantes selon votre switch et vos besoins :

```bash
# Modifier le script avant exécution
nano /opt/omega-remote-paging/scripts/setup_qos.sh
# Chercher les lignes "rate" et adapter
```

---

## 5. Configurer le contrôleur Python

Le contrôleur tourne sur **un seul nœud** (pve1 par défaut — arbitraire) et communique avec tous les nœuds.

### 5.1 Installer l'environnement

```bash
# Sur pve1 uniquement
cd /opt/omega-remote-paging/controller
python3 -m venv .venv
source .venv/bin/activate
pip install -e .
pip install -r requirements.txt
```

### 5.2 Configuration pour machines physiques

La configuration physique expose plus de détails : nombre de cœurs réels, VRAM si GPU présent, topologie rack si applicable.

```bash
cat > /etc/omega/controller.yaml << 'EOF'
proxmox:
  host: 192.168.1.11
  port: 8006
  user: root@pam
  password: "votre_mot_de_passe"
  verify_ssl: false

nodes:
  - id: pve1
    api_url: http://192.168.1.11:9200
    control_url: http://192.168.1.11:9300
    # Adapter selon votre serveur réel
    num_pcpus: 8
    # VRAM en Mio si le serveur a un GPU (0 si pas de GPU)
    vram_mib: 0
    # Rack physique (pour le placement topologique)
    rack: rack-a
    zone: zone-1

  - id: pve2
    api_url: http://192.168.1.12:9200
    control_url: http://192.168.1.12:9300
    num_pcpus: 8
    vram_mib: 0
    rack: rack-a
    zone: zone-1

  - id: pve3
    api_url: http://192.168.1.13:9200
    control_url: http://192.168.1.13:9300
    num_pcpus: 8
    vram_mib: 0
    rack: rack-a
    zone: zone-1

policy:
  # Seuil plus élevé sur physique : éviction plus tardive
  evict_threshold_pct: 85
  # Nombre de pages distantes avant migration suggérée
  migration_threshold_pages: 2000
  # Intervalle de vérification
  check_interval_seconds: 15
  circuit_failure_threshold: 3
  collector_retries: 3

topology:
  enabled: true
  weights:
    ram: 0.50
    topology: 0.25
    cpu: 0.15
    migrations: 0.10
EOF
```

### 5.3 Service systemd du contrôleur

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
```

---

## 6. Configurer le GPU (si les serveurs ont un GPU)

Si vos machines physiques sont équipées de GPU (NVIDIA ou AMD), le multiplexeur GPU permet de partager le GPU entre toutes les VMs du nœud.

### 6.1 Vérifier le GPU

```bash
# NVIDIA
nvidia-smi
# Sortie : GPU name, VRAM total, pilote

# AMD
rocm-smi
# ou
lspci | grep -i vga
```

### 6.2 Configurer le budget VRAM par VM

Le budget peut être poussé directement via l'API de contrôle, ou déclaré dans
la configuration Proxmox de la VM pour que le controller le réconcilie
automatiquement :

```text
description:
  omega.gpu_vram_mib=2048
  omega.min_vcpus=2
  omega.max_vcpus=8
```

Ou via les tags Proxmox :

```text
omega-gpu-2048;omega-min-vcpus-2;omega-max-vcpus-8
```

```bash
# Exemple : pve1 a un GPU avec 8 Go de VRAM
# VM 101 → 2 Go VRAM, VM 102 → 4 Go VRAM, VM 103 → 2 Go VRAM

# Configurer via l'API de contrôle (après démarrage des VMs)
curl -s -X POST http://192.168.1.11:9300/control/vm/101/gpu \
    -H "Content-Type: application/json" \
    -d '{"vram_budget_mib": 2048}'

curl -s -X POST http://192.168.1.11:9300/control/vm/102/gpu \
    -H "Content-Type: application/json" \
    -d '{"vram_budget_mib": 4096}'
```

### 6.3 Surveiller la VRAM

```bash
# État du multiplexeur GPU
curl -s http://192.168.1.11:9300/control/gpu/status | python3 -m json.tool
```

---

## 7. Admettre une VM dans le système

Lorsque vous créez une nouvelle VM dans Proxmox, le contrôleur doit l'admettre pour lui allouer un budget RAM et éventuellement CPU et GPU.

### 7.1 Via le contrôleur automatique (mode daemon)

Le contrôleur surveille l'API Proxmox et détecte automatiquement les nouvelles VMs. À la création d'une VM, il :
1. Calcule le budget local et distant (`local + distant = max_mem`)
2. Choisit le nœud cible le plus adapté ou migre automatiquement la VM si elle a démarré sur un mauvais nœud
3. Configure le quota via `/control/vm/{vmid}/quota`
4. Réconcilie automatiquement le profil vCPU :
   `omega.max_vcpus` si présent, sinon `sockets × cores`,
   et `omega.min_vcpus` si présent, sinon `max_vcpus / 2`
5. Réconcilie automatiquement le budget GPU si `omega.gpu_vram_mib` est déclaré dans la config Proxmox

Pour une VM créée manuellement dans Proxmox (RAM : 8192 Mo, nœud cible : pve1) :

```bash
# Le contrôleur configurera automatiquement quelque chose comme :
# local_budget = 6144 Mo (75%)
# remote_budget = 2048 Mo (25%)
# total = 8192 Mo ✓
```

### 7.2 Configuration manuelle d'un quota

Si le contrôleur n'est pas encore actif ou si vous voulez forcer une configuration :

```bash
# VM 200, 8 Go RAM, nœud pve1
# Budget : 6 Go local, 2 Go distant
curl -s -X POST http://192.168.1.11:9300/control/vm/200/quota \
    -H "Content-Type: application/json" \
    -d '{
        "max_mem_mib": 8192,
        "local_budget_mib": 6144,
        "remote_budget_mib": 2048
    }' | python3 -m json.tool

# Vérifier
curl -s http://192.168.1.11:9300/control/vm/200/quota | python3 -m json.tool
```

---

## 8. Surveillance en production

### 8.1 Métriques Prometheus sur tous les nœuds

```bash
for node in 192.168.1.11 192.168.1.12 192.168.1.13; do
    echo "=== $node ==="
    curl -s http://${node}:9300/control/metrics
    echo ""
done
```

### 8.2 Dashboard de surveillance en temps réel

```bash
# Script de surveillance simple
watch -n5 '
echo "=== CLUSTER OMEGA — $(date) ===\n"
for node in 192.168.1.11 192.168.1.12 192.168.1.13; do
    data=$(curl -s http://${node}:9200/api/status 2>/dev/null)
    if [ $? -eq 0 ]; then
        echo "$data" | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f\"  {d['"'"'node_id'"'"']}: RAM {d['"'"'mem_usage_pct'"'"']:.1f}% | pages {d['"'"'pages_stored'"'"']} | VMs {len(d['"'"'local_vms'"'"'])}\")
"
    else
        echo "  ${node}: HORS LIGNE"
    fi
done
'
```

### 8.3 Intégrer avec Prometheus et Grafana

```yaml
# prometheus.yml — ajouter le job omega
scrape_configs:
  - job_name: 'omega-cluster'
    static_configs:
      - targets:
          - '192.168.1.11:9300'
          - '192.168.1.12:9300'
          - '192.168.1.13:9300'
    metrics_path: '/control/metrics'
    scrape_interval: 15s
```

Importer le dashboard Grafana depuis `docs/grafana-dashboard.json` (si disponible).

---

## 9. Maintenance

### 9.1 Mise à jour d'un nœud sans interruption

```bash
# 1. Drainer le nœud depuis le controller
python3 -m controller.main drain-node \
    --node http://192.168.1.11:9300 \
    --node http://192.168.1.12:9300 \
    --node http://192.168.1.13:9300 \
    --source-node pve2

# 2. Vérifier que le nœud n'héberge plus de VM
curl -s http://192.168.1.12:9200/api/status | python3 -m json.tool | grep -A5 local_vms

# 3. Arrêter le daemon
systemctl stop omega-daemon

# 4. Mettre à jour le code et recompiler
cd /opt/omega-remote-paging
git pull
RUSTFLAGS="-C target-cpu=native" cargo build --release
install -m 755 target/release/omega-daemon /usr/local/bin/

# 5. Redémarrer
systemctl start omega-daemon

# 6. Vérifier
systemctl status omega-daemon
curl -s http://192.168.1.11:9200/health
```

Si on veut seulement voir le plan sans déclencher les migrations :

```bash
python3 -m controller.main drain-node \
    --node http://192.168.1.11:9300 \
    --node http://192.168.1.12:9300 \
    --node http://192.168.1.13:9300 \
    --source-node pve2 \
    --dry-run
```

Historique détaillé de tous les problèmes rencontrés sur cluster réel :
[retour-experience-cluster-reel.md](retour-experience-cluster-reel.md).

### 9.2 Vider les pages d'une VM après migration

Quand une VM est migrée vers un autre nœud, ses pages distantes stockées sur l'ancien nœud doivent être libérées :

```bash
# Supprimer les pages de la VM 200 sur pve1 (après migration vers pve2)
curl -s -X DELETE http://192.168.1.11:9300/control/pages/200 | python3 -m json.tool

# Supprimer aussi le quota (optionnel — il sera reconfiguré par le contrôleur)
curl -s -X DELETE http://192.168.1.11:9300/control/vm/200/quota
```

### 9.3 Sauvegarde de la configuration

```bash
# Sur chaque nœud
tar -czf /backup/omega-config-$(date +%Y%m%d).tar.gz \
    /etc/omega/ \
    /var/lib/omega/

# Sauvegarder aussi les certificats TLS (générés automatiquement au premier démarrage)
ls -la /var/lib/omega/tls/
```

---

## 10. Dimensionnement recommandé

### RAM

| RAM par nœud | Budget local conseillé | Budget distant max |
|-------------|----------------------|-------------------|
| 32 Go | 85% (27 Go) | 15% (5 Go) |
| 64 Go | 80% (51 Go) | 20% (13 Go) |
| 128 Go | 75% (96 Go) | 25% (32 Go) |
| 256 Go | 70% (179 Go) | 30% (77 Go) |

Plus la RAM locale est grande, plus on peut tolérer un ratio distant élevé sans impacter les performances (la pression mémoire est moins fréquente).

### Réseau

| Trafic de paging | Réseau requis |
|-----------------|--------------|
| < 100 VMs actives | 1 Gbps suffit |
| 100–500 VMs | 10 Gbps recommandé |
| > 500 VMs | 25 Gbps ou RDMA |

### CPU

| Nombre de VMs | Overhead omega-daemon |
|--------------|----------------------|
| < 50 VMs | < 1% d'un cœur |
| 50–200 VMs | 2–5% d'un cœur |
| > 200 VMs | 5–10% d'un cœur |

Le daemon est async (tokio) et scale bien. L'agent uffd utilise un thread par défaut de page.

---

## 11. Mode standalone — node-a-agent + node-bc-store

Sur cluster physique, le déploiement standalone permet d'activer toutes les fonctionnalités (vCPU élastique, GPU passthrough, Ceph store, orphan cleaner) sans passer par omega-daemon.

Tous les nœuds font tourner un store. Chaque agent cible les stores des autres nœuds.

### Stores (sur chaque nœud)

```bash
# Prérequis Ceph (si Ceph utilisé)
apt install librados-dev    # fournit librados.so (symlink dev requis)

# Démarrer le store (backend Ceph auto si /etc/ceph/ceph.conf présent)
node-bc-store \
  --listen 0.0.0.0:9100 \
  --status-listen 0.0.0.0:9200 \
  --node-id $(hostname) \
  --store-data-path /var/lib/omega-store

# Variables complètes
STORE_ORPHAN_CHECK_INTERVAL_SECS=300  # nettoyage pages orphelines toutes les 5 min
STORE_ORPHAN_GRACE_SECS=600           # délai de grâce avant suppression
STORE_CEPH_CONF=/etc/ceph/ceph.conf   # auto-détection Ceph
STORE_CEPH_POOL=omega-pages
STORE_STATUS_LISTEN=0.0.0.0:9200      # expose vcpu_total/free, ceph_enabled
```

### Agent (par VM, sur le nœud qui l'héberge)

```bash
# Adapter STORES = les autres nœuds (pas le nœud local)
node-a-agent \
  --stores 192.168.1.12:9100,192.168.1.13:9100 \
  --status-addrs 192.168.1.12:9200,192.168.1.13:9200 \
  --vm-id 9001 \
  --vm-requested-mib 2048 \
  --region-mib 2048 \
  --current-node pve1 \
  --mode daemon

# Variables vCPU élastique
AGENT_VM_VCPUS=8                  # max à la création
AGENT_VM_INITIAL_VCPUS=1          # vCPUs au démarrage
AGENT_VCPU_HIGH_THRESHOLD_PCT=75
AGENT_VCPU_OVERCOMMIT_RATIO=3     # 1 pCPU = 3 vCPUs max, au-delà → migration

# Variables GPU
AGENT_GPU_REQUIRED=false          # auto-détection via qm config + sysfs PCI 0x03xx
AGENT_GPU_QUANTUM_SECS=30         # rotation round-robin entre VMs GPU

# Hookscript (démarrage/arrêt automatique avec la VM)
cp scripts/omega-hook.pl /var/lib/vz/snippets/
qm set 9001 --hookscript local:snippets/omega-hook.pl
```

---

## 12. Dépannage spécifique au physique

| Symptôme | Cause | Solution |
|----------|-------|----------|
| Latence élevée lors des accès distants | Switch surchargé ou câble défectueux | `iperf3` entre nœuds, remplacer câble |
| Pages non évincées malgré forte pression | Seuil trop élevé | Réduire `OMEGA_EVICT_THRESHOLD_PCT` à 75 |
| Daemon crash avec "Cannot allocate memory" | ulimits trop faibles | `LimitMEMLOCK=infinity` dans le service |
| TLS handshake fail entre nœuds | Certificats expirés ou mal copiés | Supprimer `/var/lib/omega/tls/` et redémarrer |
| Store plein (disque `/var/lib/omega`) | Pages persistantes s'accumulent | Nettoyage : `curl -X DELETE .../control/pages/{vmid}` |
| Steal time > 10% sans VMs actives | Autre processus sur le nœud | `htop`, investiguer les processus hôte |
