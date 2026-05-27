# Omega Remote Paging — Plan de Tests Complet

> Couvre tous les niveaux : unitaires (automatisés), intégration (sur cluster), scénarios fonctionnels, cas limites, et résilience.
> Chaque test inclut la commande exacte et le résultat attendu.

---

## Table des matières

1. [Tests automatisés (CI)](#1-tests-automatisés-ci)
2. [Tests de déploiement](#2-tests-de-déploiement)
3. [Tests de connectivité cluster](#3-tests-de-connectivité-cluster)
4. [Tests RAM — éviction et paging distant](#4-tests-ram--éviction-et-paging-distant)
5. [Tests balloon manager](#5-tests-balloon-manager)
6. [Tests vCPU — hotplug et migration](#6-tests-vcpu--hotplug-et-migration)
7. [Tests GPU](#7-tests-gpu)
8. [Tests migration live et cold](#8-tests-migration-live-et-cold)
9. [Tests admission controller](#9-tests-admission-controller)
10. [Tests bin-packing vCPU](#10-tests-bin-packing-vcpu)
11. [Tests disque I/O](#11-tests-disque-io)
12. [Tests résilience et pannes](#12-tests-résilience-et-pannes)
13. [Tests de charge](#13-tests-de-charge)
14. [Tests scénarios mixtes](#14-tests-scénarios-mixtes)
15. [Checklist finale avant mise en production](#15-checklist-finale-avant-mise-en-production)

---

## Validation physique rapide

Le plan complet pour un cluster Proxmox/datacenter est dans [`docs/VALIDATION_DATACENTER.md`](VALIDATION_DATACENTER.md). Il contient les commandes de creation de VMs conformes, les scenarios physiques, les criteres d'acceptation et le depannage des erreurs deja rencontrees.

Commandes minimales:

```bash
# Creer des VMs compatibles Omega
./scripts/create-omega-vm.sh \
  --vmids 9001,9002,9003 \
  --storage ceph-vms \
  --bridge vmbr0 \
  --image /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2 \
  --sshkey /root/.ssh/id_rsa.pub \
  --start

# Verifier la conformite avant les tests lourds
./scripts/tests/30-vm-conformity.sh 9001,9002,9003

# Lancer la validation physique non destructive
./scripts/omega-lab.sh --gpu --ceph --auto

# Lancer le soak long
OMEGA_SOAK_SECS=7200 ./scripts/omega-lab.sh --gpu --ceph --long --auto

# Lancer la validation scalabilite 500 VMs
OMEGA_SCALE_VMIDS="$(seq -s, 9001 9500)" \
OMEGA_SCALE_TARGET=500 \
OMEGA_SCALE_BATCH_SIZE=20 \
OMEGA_SCALE_SOAK_SECS=3600 \
./scripts/omega-lab.sh --gpu --ceph --long --scale --auto
```

Le test `30` est volontairement strict: il bloque les VMs qui n'ont pas `vcpus`, `maxcpus`, `hotplug cpu`, agent invite, disque virtio-scsi, reseau virtio et metadata Omega.
Le test `31` valide le passage a grande echelle: inventaire, conformite rapide, demarrage par lots, surveillance daemons/API et stabilite Proxmox.

---

## 1. Tests automatisés (CI)

Ces tests se lancent sans cluster réel.

### 1.1 Tests Rust

```bash
cd omega-remote-paging

# Tous les tests unitaires et d'intégration
cargo test --workspace
# Attendu : 244 passed, 0 failed

# Tests d'un module spécifique
cargo test --package omega-daemon --lib quota
cargo test --package omega-daemon --lib policy_engine
cargo test --package omega-daemon --lib vm_migration
cargo test --package omega-daemon --lib balloon

# Tests avec logs visibles (debug)
RUST_LOG=debug cargo test --package omega-daemon --lib balloon -- --nocapture
```

### 1.2 Tests Python

```bash
cd omega-remote-paging/controller

# Tous les tests
python3 -m pytest tests/ -v
# Attendu : 289 passed, 1 skipped (benchmark)

# Tests par module
python3 -m pytest tests/test_policy.py -v
python3 -m pytest tests/test_admission.py -v
python3 -m pytest tests/test_migration_policy.py -v
python3 -m pytest tests/test_vcpu_pool.py -v
python3 -m pytest tests/test_consolidation.py -v

# Tests avec couverture
python3 -m pytest tests/ --cov=controller --cov-report=term-missing
# Attendu : couverture > 80%

# Benchmarks (nécessite pytest-benchmark)
pip install pytest-benchmark
python3 -m pytest tests/test_benchmarks.py --benchmark-only -v
```

### 1.3 Build release complet

```bash
make build
# ou :
cargo build --release --workspace
# Attendu : 4 binaires dans target/release/ sans erreur ni warning critique
ls -lh target/release/{omega-daemon,node-a-agent,omega-qemu-launcher,node-bc-store}
```

---

## 2. Tests de déploiement

Sur le cluster réel. Remplacer les IPs par les vôtres.

### 2.1 Installation sur un nœud

```bash
# Sur la machine 1 (192.168.1.1)
OMEGA_STORES="192.168.1.2:9100,192.168.1.3:9100" \
INSTALL_DIR="/usr/local/bin" \
OMEGA_RUN_DIR="/var/lib/omega-qemu" \
    bash scripts/omega-proxmox-install.sh

# Vérifier que le wrapper est en place
ls -la /usr/bin/kvm
# Attendu : /usr/bin/kvm -> /usr/local/bin/kvm-omega

# Vérifier que kvm.real existe toujours
ls -la /usr/bin/kvm.real
# Attendu : fichier exécutable existant

# Vérifier le hookscript
ls /var/lib/vz/snippets/omega-hook.pl
# Attendu : fichier présent

# Vérifier le service systemd
systemctl status omega-daemon
# Attendu : "Active: active (running)"
```

### 2.2 Déploiement automatisé multi-nœuds

```bash
# Depuis la machine de build
export OMEGA_NODES=192.168.1.1,192.168.1.2,192.168.1.3
export OMEGA_CONTROLLER=192.168.1.1
bash scripts/deploy.sh
# Attendu : "=== Déploiement terminé ===" sans erreur
```

---

## 3. Tests de connectivité cluster

### 3.1 Store TCP

```bash
# Depuis n'importe quel nœud, tester les stores des autres
nc -zv 192.168.1.2 9100 && echo "store node2 OK" || echo "FAIL"
nc -zv 192.168.1.3 9100 && echo "store node3 OK" || echo "FAIL"

# Test plus complet (ping protocol)
python3 -c "
import socket, struct
s = socket.socket()
s.connect(('192.168.1.2', 9100))
# Envoi PING (magic=0x524D, type=0x01)
s.sendall(struct.pack('>HBBIQI', 0x524D, 0x01, 0, 0, 0, 0))
data = s.recv(15)
magic, mtype = struct.unpack('>HB', data[:3])
print('PONG reçu' if magic == 0x524D and mtype == 0x02 else 'FAIL')
s.close()
"
# Attendu : "PONG reçu" sur chaque nœud
# Si FAIL : vérifier que omega-daemon est actif (systemctl status omega-daemon)
#           et que le port 9100 n'est pas bloqué (iptables -L INPUT)
```

### 3.2 API HTTP daemon (port 9200)

```bash
# État du nœud local
curl -s http://127.0.0.1:9200/api/node | python3 -m json.tool
# Attendu : JSON avec node_id, mem_usage_pct, local_vms, etc.

# État du cluster (vue des autres nœuds depuis le nœud 1)
curl -s http://127.0.0.1:9200/api/cluster | python3 -m json.tool
# Attendu : JSON avec 2+ nœuds peers

# GPU
curl -s http://127.0.0.1:9200/api/gpu | python3 -m json.tool
# Attendu : backend, total_vram_mib, free_vram_mib
```

### 3.3 API de contrôle (port 9300)

```bash
# Santé globale
curl -s http://127.0.0.1:9300/control/status | python3 -m json.tool
# Attendu : {"status": "ok", "node_id": "...", "uptime_secs": ...}

# Métriques format Prometheus
curl -s http://127.0.0.1:9300/control/metrics
# Attendu : lignes "omega_mem_usage_pct ...", "omega_pages_stored ..."

# Recommandations de migration (sans cluster chargé = liste vide)
curl -s http://127.0.0.1:9300/control/migrate/recommend | python3 -m json.tool
# Attendu : {"recommendations": [], "count": 0}

# vCPU status
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool
```

### 3.4 Controller Python (dry-run)

```bash
python3 -m controller.main daemon \
    --node-a http://192.168.1.1:9300 \
    --node-b http://192.168.1.2:9300 \
    --node-c http://192.168.1.3:9300 \
    --poll-interval 5 \
    --dry-run
# Attendu : logs "[info] cycle daemon nodes=['node-a', 'node-b', 'node-c']"
# puis     : "[info] aucune migration nécessaire" (si cluster idle)
# PAS de migration réelle avec --dry-run
```

---

## 4. Tests RAM — éviction et paging distant

### 4.1 Démarrage d'une VM avec hookscript

```bash
# Enregistrer la VM
qm set 100 --hookscript local:snippets/omega-hook.pl

# Démarrer la VM
qm start 100

# Vérifier que l'agent est actif
omega-qemu-launcher status --vm-id 100
# Attendu : {"vm_id": 100, "agent_running": true, ...}

# Vérifier dans les logs
journalctl -u omega-daemon -n 20 | grep "vm.*100\|vmid=100"
# Chercher : "VM locale découverte vmid=100"
```

### 4.2 Pages distantes après pression mémoire

```bash
# Dans la VM 100 (connectez-vous en SSH ou console) :
# Installer stress-ng si pas présent
apt install -y stress-ng   # ou yum install stress-ng

# Créer une pression mémoire de 80% de la RAM de la VM
stress-ng --vm 1 --vm-bytes 80% --timeout 120s &

# Sur le nœud hôte, observer les pages évincées en temps réel
watch -n 2 'curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool'
# Attendu : "remote_pages" augmente progressivement

# Vérifier que le store du nœud distant stocke bien les pages
curl -s http://192.168.1.2:9200/api/node | python3 -m json.tool | grep pages_stored
# Attendu : pages_stored > 0
```

### 4.3 Récupération des pages après arrêt de la pression

```bash
# Arrêter stress-ng dans la VM
killall stress-ng

# Observer la diminution des pages distantes (eviction inverse = prefetch)
watch -n 2 'curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool'
# Attendu : "remote_pages" diminue ou reste stable (pas de montée)

# Vérifier les logs de page fault
journalctl -u omega-daemon -f | grep "fault\|prefetch"
```

### 4.4 Quota mémoire

```bash
# Enregistrer un quota pour la VM 100 (16 Go max, 8 Go local)
curl -s -X POST http://127.0.0.1:9300/control/vm/100/quota \
    -H "Content-Type: application/json" \
    -d '{"max_mem_mib": 16384, "local_budget_mib": 8192}' | python3 -m json.tool
# Attendu : {"status": "ok", "vm_id": 100, "max_mem_mib": 16384, "remote_budget_mib": 8192}

# Lire le quota
curl -s http://127.0.0.1:9300/control/vm/100/quota | python3 -m json.tool
# Attendu : remote_budget_mib = 8192

# Supprimer le quota (post-arrêt VM)
curl -s -X DELETE http://127.0.0.1:9300/control/vm/100/quota
```

---

## 5. Tests balloon manager

### 5.1 Vérifier que le balloon driver est actif dans la VM

```bash
# Dans la VM (nécessite virtio-balloon chargé)
lsmod | grep balloon
# Attendu : virtio_balloon  xxxxx  0

# Si absent, vérifier la config QEMU
qm config 100 | grep balloon
# Attendu : balloon: 1  (doit être activé dans la config Proxmox)
```

### 5.2 Observer les ajustements balloon

```bash
# Surveiller les logs du balloon manager
journalctl -u omega-daemon -f | grep "BALLOON\|balloon"
# Attendu après ~30s :
# "BALLOON inflate — récupération RAM inutilisée" si VM sur-provisionnée
# "BALLOON deflate — restitution RAM au guest sous pression" si VM sous pression

# Voir la taille actuelle du balloon dans QEMU (via QMP manuel)
echo '{"execute":"qmp_capabilities"}{"execute":"query-balloon"}' | \
    socat - UNIX-CONNECT:/var/run/qemu-server/100.qmp
# Attendu : {"return":{"actual":<octets>}}
```

### 5.3 Vérifier l'hysteresis (pas de ping-pong)

```bash
# Lancer une VM avec RAM oscillante
# Dans la VM : alterner entre pression et repos toutes les 20s
(stress-ng --vm 1 --vm-bytes 75% --timeout 20s; sleep 20) &

# Observer que le balloon ne change pas plus d'une fois par 60s par VM
journalctl -u omega-daemon | grep "BALLOON" | awk '{print $1, $2}' | head -20
# Attendu : au moins 60s entre deux ajustements pour la même VM
```

### 5.4 Récupération RAM après balloon inflate

```bash
# Vérifier que adjust_for_balloon met à jour le quota
# Avant inflate :
curl -s http://127.0.0.1:9300/control/vm/100/quota | python3 -m json.tool
# Attendu (avant) :
#   {"vm_id": 100, "max_mem_mib": 16384, "local_budget_mib": 16384, "remote_budget_mib": 0}

# Laisser la VM idle (peu de RAM utilisée) et attendre un cycle balloon manager (~30s) :
sleep 35
curl -s http://127.0.0.1:9300/control/vm/100/quota | python3 -m json.tool
# Attendu (après inflate) :
#   {"vm_id": 100, "max_mem_mib": 16384, "local_budget_mib": 8192, "remote_budget_mib": 8192}
#   → le balloon a récupéré ~8 Go, remote_budget augmente en conséquence
#
# Si remote_budget_mib reste 0 après 60s :
#   → vérifier que le balloon driver est actif dans la VM (lsmod | grep balloon)
#   → vérifier que monitor_vms=true dans la config du daemon (journalctl | grep BalloonManager)
#   → vérifier que la VM a bien de la RAM inutilisée (available_pct > 20%)

# Simuler une pression mémoire pour vérifier la déflation automatique :
ssh vm-100 'stress-ng --vm 1 --vm-bytes 88% --timeout 120s &'
sleep 35
curl -s http://127.0.0.1:9300/control/vm/100/quota | python3 -m json.tool
# Attendu (après déflation) :
#   remote_budget_mib diminue (la RAM est rendue au guest)
#   local_budget_mib augmente
```

---

## 6. Tests vCPU — hotplug et migration

### 6.1 vCPU actuel d'une VM

```bash
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool
# Attendu : liste des VMs avec vcpu_current, throttle_ratio, avg_cpu_pct

# Pour une VM spécifique
curl -s http://127.0.0.1:9300/control/vcpu/100 | python3 -m json.tool
```

### 6.2 Hotplug vCPU

```bash
# Ajouter 2 vCPUs à la VM 100
curl -s -X POST http://127.0.0.1:9300/control/vm/100/vcpu \
    -H "Content-Type: application/json" \
    -d '{"vcpus": 4}' | python3 -m json.tool
# Attendu : {"status": "ok", "vm_id": 100, "vcpus_allocated": 4}

# Dans la VM, vérifier que les nouveaux CPUs sont visibles
nproc   # ou : lscpu | grep "CPU(s)"
# Attendu : nombre augmenté
```

### 6.3 Throttle détecté → migration vCPU

```bash
# Saturer les vCPUs sur le nœud 1 (dans plusieurs VMs) :
# Dans VM 100 : stress --cpu 8 --timeout 300 &
# Dans VM 101 : stress --cpu 8 --timeout 300 &

# Observer le throttle ratio
watch -n 3 'curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool'
# Attendu : throttle_ratio > 0.30 pour les VMs sous charge

# Observer la recommandation de migration
curl -s http://127.0.0.1:9300/control/migrate/recommend | python3 -m json.tool
# Attendu : une recommandation avec reason.type = "cpu_saturation"

# Le controller doit déclencher la migration automatiquement
journalctl -u omega-controller -f | grep "migration\|vcpu"
```

### 6.4 Libération des vCPUs

```bash
# Libérer les vCPUs d'une VM arrêtée
curl -s -X DELETE http://127.0.0.1:9300/control/vm/100/vcpu
# Attendu : {"status": "ok", "released": true}
```

---

## 7. Tests GPU

### 7.1 État GPU du nœud

```bash
curl -s http://127.0.0.1:9200/api/gpu | python3 -m json.tool
# Attendu (avec GPU physique) :
# {"backend": "drm_amdgpu", "total_vram_mib": 8192, "free_vram_mib": 8192, "vms_using_gpu": []}

# Attendu (sans GPU) :
# {"backend": "mock", "total_vram_mib": xxxx, ...}
```

### 7.2 Réservation VRAM pour une VM

```bash
# Réserver 2 Go de VRAM pour la VM 100
curl -s -X POST http://127.0.0.1:9300/control/vm/100/gpu \
    -H "Content-Type: application/json" \
    -d '{"vram_mib": 2048}' | python3 -m json.tool
# Attendu : {"status": "ok", "vram_allocated_mib": 2048}

# Vérifier que la VRAM libre diminue
curl -s http://127.0.0.1:9200/api/gpu | python3 -m json.tool
# Attendu : free_vram_mib = total - 2048

# Libérer
curl -s -X DELETE http://127.0.0.1:9300/control/vm/100/gpu
```

### 7.3 VM nécessitant plus de VRAM que disponible

```bash
# Si le nœud a 4 Go VRAM libre et qu'on demande 8 Go
curl -s -X POST http://127.0.0.1:9300/control/vm/102/gpu \
    -H "Content-Type: application/json" \
    -d '{"vram_mib": 8192}' | python3 -m json.tool
# Attendu : code 422 ou {"error": "insufficient_vram", ...}

# Le controller doit proposer une migration vers un nœud avec assez de VRAM
curl -s http://127.0.0.1:9300/control/migrate/recommend | python3 -m json.tool
# Attendu : recommandation avec reason liée au GPU
```

---

## 8. Tests migration live et cold

### 8.1 Migration manuelle live (Ceph RBD)

```bash
# Déclencher une migration live de la VM 100 vers le nœud 2
curl -s -X POST http://127.0.0.1:9300/control/migrate \
    -H "Content-Type: application/json" \
    -d '{"vm_id": 100, "target": "pve-node2", "type": "live"}' | python3 -m json.tool
# Attendu : {"status": "migration_started", "task_id": 1, ...}

# Suivre l'avancement
TASK_ID=1
watch -n 2 "curl -s http://127.0.0.1:9300/control/migrations | python3 -m json.tool"
# Attendu : state passe de "running" à "success"

# Vérifier que les pages source ont été nettoyées
curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool
# Attendu : remote_pages = 0 sur l'ancien nœud
```

### 8.2 Migration cold (VM arrêtée)

```bash
qm stop 100
curl -s -X POST http://127.0.0.1:9300/control/migrate \
    -H "Content-Type: application/json" \
    -d '{"vm_id": 100, "target": "pve-node2", "type": "cold"}' | python3 -m json.tool
# Attendu : {"status": "migration_started", "task_id": 2, "vm_id": 100, "type": "cold"}

# Attendre la fin
sleep 30
curl -s http://127.0.0.1:9300/control/migrations | python3 -m json.tool | grep -E "state|vm_id"
# Attendu : "state": "success"

# Vérifier sur le nœud destination que la VM est présente
ssh 192.168.1.2 'qm list | grep 100'
# Attendu : ligne avec "100  ... stopped" (VM présente sur node2, arrêtée)
# Si absent : la migration a échoué → consulter journalctl -u omega-daemon | grep "task_id=2"
```

### 8.3 Migration avec stockage local (LVM/ZFS)

```bash
# Démarrer le controller avec flag --with-local-disks
python3 -m controller.main daemon \
    --node-a http://192.168.1.1:9300 \
    --node-b http://192.168.1.2:9300 \
    --node-c http://192.168.1.3:9300 \
    --proxmox-url https://192.168.1.1:8006 \
    --proxmox-token "root@pam!omega=<token>" \
    --with-local-disks \
    --poll-interval 5

# Vérifier dans les logs que qm migrate utilise --with-local-disks
journalctl -u omega-controller | grep "with-local-disks"
# Attendu : "qm migrate 100 pve-node2 --online --with-local-disks"
# Si absent : vérifier que le flag --with-local-disks est bien passé au daemon

# Vérifier que la migration a copié les disques (visible dans les logs Proxmox)
journalctl -u pvestatd | grep "100.*migrat\|copy.*disk"
# Attendu : entrées indiquant la copie des disques locaux vers le nœud cible
# Note : migration avec disques locaux est plus lente (proportionnel à la taille disque)
```

### 8.4 Nettoyage post-migration

```bash
# Après migration réussie, vérifier que les ressources source sont libérées
# 1. Pages store
curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool
# Attendu : {"remote_pages": 0} ou 404

# 2. Quota supprimé
curl -s http://127.0.0.1:9300/control/vm/100/quota
# Attendu : 404

# 3. vCPUs libérés
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool | grep "100"
# Attendu : VM 100 absente de la liste
```

---

## 9. Tests admission controller

### 9.1 Admission d'une VM dans la capacité disponible

```bash
# Vérifier la RAM disponible du cluster
python3 -m controller.main status \
    --stores 192.168.1.1:9100,192.168.1.2:9100,192.168.1.3:9100

# Créer une VM dont la RAM < capacité libre du cluster
qm create 200 --memory 4096 --cores 2 --name test-admission
qm set 200 --hookscript local:snippets/omega-hook.pl
qm start 200
# Attendu : démarrage normal, quota enregistré automatiquement
journalctl -u omega-controller | grep "vmid=200\|vm_id.*200"
# Chercher : "quota enregistré" ou "admission acceptée"
```

### 9.2 Refus d'admission quand cluster plein

```bash
# Remplir volontairement la RAM du cluster avec des VMs
# Puis tenter de démarrer une VM dont la RAM dépasse la capacité libre
qm create 201 --memory 131072 --name test-refus  # 128 Go

# Surveiller les logs du controller
journalctl -u omega-controller -f | grep "201\|admission\|refus"
# Attendu : "capacité cluster insuffisante" ou migration automatique proposée
```

### 9.3 Placement optimal (bin-packing RAM)

```bash
# Vérifier que le placement choisit le nœud avec le moins de RAM libre
# (pour concentrer la charge et garder des nœuds libres)
python3 -m controller.main status \
    --stores 192.168.1.1:9100,192.168.1.2:9100,192.168.1.3:9100
# Attendu (exemple avec 3 nœuds) :
#   node-a : mem=62%  vcpu_free=12  pages_stored=0
#   node-b : mem=40%  vcpu_free=20  pages_stored=1024
#   node-c : mem=55%  vcpu_free=16  pages_stored=512
#
# Quand une VM de 8 Go démarre, elle doit être placée sur node-b (40% = plus libre)
# Le bin-packing concentre la charge pour garder un nœud le plus libre possible.
#
# Si le placement est mauvais (VM sur node-a le plus chargé) :
#   → vérifier que le controller tourne (systemctl status omega-controller)
#   → vérifier que --auto-admit est actif (flag par défaut)
```

---

## 10. Tests bin-packing vCPU

### 10.1 Scénario de base — consolidation

```bash
# Préparer : remplir les vCPUs sur tous les nœuds
# Node 1 : VMs consommant 28/32 vCPUs
# Node 2 : VMs consommant 28/32 vCPUs
# Node 3 : VMs consommant 30/32 vCPUs

# Demander 6 vCPUs pour une nouvelle VM (aucun nœud n'en a autant)
# Le controller doit détecter qu'en déplaçant 2 VMs de Node 1 vers Node 3,
# Node 1 aura 10 vCPUs libres

# Observer les logs du controller
journalctl -u omega-controller -f | grep "consolidation\|bin.pack\|vcpu_consolid"
# Attendu :
#   [info] bin-packing vCPU : aucun nœud direct disponible pour vm_id=XXX desired=6
#   [info] consolidation plan : target=node-1 evictions=[(vm_a, node1→node3), ...]
#   [info] migration démarrée vm_id=vm_a source=node-1 target=node-3
#   [info] migration démarrée vm_id=XXX source=needy target=node-1
#
# Si aucune consolidation n'apparaît :
#   → Vérifier que les VMs ont bien throttle_ratio > 0 (charge CPU réelle)
#   → Vérifier que le cooldown de 120s est écoulé depuis la dernière migration vCPU
#   → curl http://127.0.0.1:9300/control/vcpu/status pour voir l'état réel
```

### 10.2 Test unitaire de la consolidation

```bash
cd omega-remote-paging/controller
python3 -c "
from controller.main import _find_vcpu_consolidation_plan
from controller.migration_policy import VmState
from controller.main import MigNodeState
import time

# Construire un état de cluster artificiel
# Node A : cible (12/32 vCPUs libres)
# Node B : source (4/32 vCPUs libres, a des VMs déplaçables)
# Objectif : libérer 10 vCPUs sur Node B pour accueillir une VM de 10 vCPUs

needy_vm = VmState(vm_id=999, status='stopped', max_mem_mib=4096)
result = _find_vcpu_consolidation_plan(
    needy_vm=needy_vm,
    needy_source='node-b',
    desired_vcpus=10,
    gpu_budget_mib=0,
    node_states={},  # simplification : avec node_states vide, aucun candidat
    last_vcpu_migrations={},
    now=time.time(),
)
print('Plan trouvé :', result is not None)
"
# Attendu : "Plan trouvé : False"  (node_states vide = pas de candidat, correct)
#
# Pour un test avec état réel :
python3 -m pytest tests/test_consolidation.py -v
# Attendu : tous les tests passent (plan trouvé / non trouvé selon les cas)
# Exemple de sortie :
#   test_consolidation_plan_found PASSED
#   test_consolidation_no_candidate PASSED
#   test_consolidation_respects_cooldown PASSED
```

---

## 11. Tests disque I/O

### 11.1 Scheduler I/O actif

```bash
# Vérifier que le scheduler I/O est actif sur les VMs
curl -s http://127.0.0.1:9300/control/vm/100/io | python3 -m json.tool
# Attendu : {"vm_id": 100, "io_weight": 100, "read_bps": ..., "write_bps": ...}

# Changer la priorité I/O d'une VM
curl -s -X POST http://127.0.0.1:9300/control/vm/100/io \
    -H "Content-Type: application/json" \
    -d '{"io_weight": 500}' | python3 -m json.tool
# Attendu : {"status": "ok", "io_weight": 500}
```

### 11.2 Détection de VM monopolisant le disque

```bash
# Dans la VM 100, créer un I/O intensif
dd if=/dev/zero of=/tmp/test bs=1M count=10000 &

# Observer que le scheduler réduit son poids I/O
watch -n 2 'curl -s http://127.0.0.1:9300/control/vm/100/io | python3 -m json.tool'
# Attendu après 10-20s :
#   io_weight passe de 100 → 50 (VM identifiée comme consommatrice excessive)
#   Les autres VMs (101, 102) passent de 100 → 200 (boostées en compensation)
#
# Vérifier dans les cgroups directement :
cat /sys/fs/cgroup/system.slice/qemu-100.scope/io.weight 2>/dev/null || \
cat /sys/fs/cgroup/machine.slice/qemu-100.scope/io.weight 2>/dev/null
# Attendu : 50 (ou la valeur configurée pour les donors)
#
# Après arrêt du dd (attendre ~30s) :
# Attendu : io_weight revient à 100 pour toutes les VMs (scheduler remet l'équilibre)
```

---

## 12. Tests résilience et pannes

### 12.1 Panne d'un store (nœud B indisponible)

```bash
# Simuler la panne du store sur le nœud 2
ssh 192.168.1.2 'systemctl stop omega-daemon'

# Vérifier que le nœud 1 continue à fonctionner
curl -s http://127.0.0.1:9300/control/status | python3 -m json.tool
# Attendu : status = "ok" (le daemon local fonctionne toujours)

# Les pages évincées doivent aller uniquement vers le nœud 3
stress-ng --vm 1 --vm-bytes 80% --timeout 60s &  # dans la VM
watch -n 2 'curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool'
# Attendu : les pages continuent à être évincées (vers nœud 3 uniquement)

# Logs : warning sur le nœud 2 indisponible
journalctl -u omega-daemon | grep "192.168.1.2\|node.*2\|store.*error"
# Chercher : timeout/erreur de connexion au nœud 2

# Rétablir
ssh 192.168.1.2 'systemctl start omega-daemon'
```

### 12.2 Redémarrage du daemon

```bash
# Arrêter puis redémarrer le daemon en cours d'utilisation
systemctl restart omega-daemon

# Vérifier que les VMs continuent à fonctionner (pas de page fault fatal)
# Dans la VM en cours d'utilisation — la VM doit rester responsive :
ping -c 5 8.8.8.8   # dans la VM

# Vérifier que les quotas sont restaurés
journalctl -u omega-daemon -n 30 | grep "quota\|vm.*découvert"
# Attendu : "VM locale découverte vmid=100 pid=xxx max_mem_mib=8192"
#            "quota enregistré vmid=100"
# Si absent : le daemon ne redécouvre pas les VMs en cours → vérifier vm_tracker
#
# Vérifier que la VM répond toujours
ping -c 3 <ip-vm-100>
# Attendu : 3 paquets transmis, 0 perdus (la VM n'a pas redémarré)
# Note : un bref délai de quelques secondes sur les page faults est normal pendant le restart
```

### 12.3 Arrêt brutal d'une VM (crash)

```bash
# Simuler un crash VM
qm stop 100 --skiplock 1  # arrêt forcé

# Vérifier que les pages de la VM sont nettoyées
curl -s -X DELETE http://127.0.0.1:9300/control/pages/100
# Attendu : {"deleted": N} si des pages restaient

# Vérifier que les ressources sont libérées
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool | grep "100"
# Attendu : VM 100 absente (quota supprimé par hookscript post-stop)
```

### 12.4 Crash du controller Python

```bash
# Arrêter le controller
systemctl stop omega-controller

# Vérifier que les daemons continuent à fonctionner indépendamment
curl -s http://127.0.0.1:9300/control/status
# Attendu : status "ok" (le daemon n'a pas besoin du controller pour fonctionner)

# Les VMs continuent à paginer sans le controller
# (pas de nouvelles décisions de migration, mais pas de crash)

# Redémarrer le controller
systemctl start omega-controller
# Il reprend depuis l'état actuel sans avoir besoin d'état persistant
```

### 12.5 Perte du réseau entre nœuds (split-brain)

```bash
# Couper temporairement le réseau vers le nœud 2 depuis le nœud 1
# (nécessite accès root sur les routeurs ou utiliser iptables)
iptables -A OUTPUT -d 192.168.1.2 -j DROP
iptables -A INPUT  -s 192.168.1.2 -j DROP

# Observer : le nœud 1 ne peut plus évincer vers le nœud 2
# Il doit utiliser uniquement le nœud 3
watch -n 2 'curl -s http://127.0.0.1:9200/api/cluster | python3 -m json.tool'
# Attendu : nœud 2 apparaît comme unreachable ou absent

# Rétablir
iptables -D OUTPUT -d 192.168.1.2 -j DROP
iptables -D INPUT  -s 192.168.1.2 -j DROP
```

---

## 13. Tests de charge

### 13.1 20 VMs démarrant simultanément (scénario M6)

```bash
# Créer 20 VMs légères
for i in $(seq 201 220); do
    qm create $i --memory 2048 --cores 2 --name "stress-$i" \
        --hookscript local:snippets/omega-hook.pl
done

# Démarrer toutes en parallèle
for i in $(seq 201 220); do
    qm start $i &
done
wait

# Vérifier que toutes sont démarrées
qm list | grep stress | grep running | wc -l
# Attendu : 20

# Observer la consommation cluster
python3 -m controller.main status \
    --stores 192.168.1.1:9100,192.168.1.2:9100,192.168.1.3:9100
# Attendu : RAM distribuée sur les 3 nœuds via bin-packing

# Nettoyage
for i in $(seq 201 220); do qm stop $i && qm destroy $i; done
```

### 13.2 Saturation progressive RAM (scénario M4)

```bash
# Augmenter progressivement la charge RAM dans plusieurs VMs
for vmid in 100 101 102; do
    ssh vm-$vmid 'stress-ng --vm 1 --vm-bytes 70% --timeout 300s &'
done

# Observer l'évolution sur 5 minutes
for i in $(seq 1 30); do
    echo "=== $(date) ==="
    curl -s http://127.0.0.1:9200/api/cluster | python3 -m json.tool | \
        grep -E "mem_usage_pct|pages_stored|remote_pages"
    sleep 10
done
# Attendu — progression sur 5 minutes :
#   t=0min  : mem_usage_pct ≈ 60%, pages_stored=0, remote_pages=0
#   t=1min  : mem_usage_pct ≈ 72%, pages_stored=0 (pression montante)
#   t=2min  : mem_usage_pct ≈ 78%, éviction CLOCK démarre, pages_stored > 0
#   t=3min  : mem_usage_pct ≈ 80% (plafonne), remote_pages augmente sur 100,101,102
#   t=5min  : mem_usage_pct stable ~80%, pages_stored important (milliers de pages)
#             controller propose des migrations si mem > 85%
#
# Si mem_usage_pct dépasse 85% → vérifier les logs du controller :
journalctl -u omega-controller | grep "migration\|pression"
# Attendu : recommandation de migration live vers le nœud le moins chargé
```

### 13.3 Débit de paging (benchmark réseau)

```bash
# Mesurer la latence d'une page fault distante
# (nécessite une VM configurée avec beaucoup de pages distantes)
# Dans la VM :
time dd if=/dev/zero of=/tmp/test bs=4k count=1000
# Attendu (référence locale, pas de pages distantes) :
#   real 0m0.12s  (environ 120ms pour 4 Mo en local)
#
# Attendu (avec pages distantes, réseau 1 Gbit) :
#   real 0m0.8s à 2s  (latence réseau 100µs-300µs par page fault × 1000 pages)
#
# Si la latence est > 5s : problème réseau ou store saturé
#   → nc -zv 192.168.1.2 9100  (vérifier connectivité store)
#   → ping 192.168.1.2  (vérifier RTT réseau, doit être < 1ms en LAN)
#   → curl http://192.168.1.2:9200/api/node | grep pages_stored  (store pas saturé ?)
#
# Pour mesurer le débit net du store :
python3 -c "
import socket, struct, time, os
s = socket.socket(); s.connect(('192.168.1.2', 9100))
N = 100; t0 = time.time()
for _ in range(N):
    s.sendall(struct.pack('>HBBIQI', 0x524D, 0x01, 0, 0, 0, 0))
    s.recv(15)
print(f'{N/(time.time()-t0):.0f} pings/s RTT={1000*(time.time()-t0)/N:.2f}ms')
s.close()
"
# Attendu : > 1000 pings/s, RTT < 1ms (réseau LAN sain)
```

---

## 14. Tests scénarios mixtes

### 14.1 RAM + CPU simultanés (scénario M1)

```bash
# VM avec charge RAM + CPU croissante
ssh vm-100 'stress-ng --vm 1 --vm-bytes 70% --cpu 4 --timeout 300s &'

# Observer les deux axes en parallèle
while true; do
    echo "--- $(date) ---"
    curl -s http://127.0.0.1:9300/control/pages/100 | python3 -m json.tool | grep remote_pages
    curl -s http://127.0.0.1:9300/control/vcpu/100  | python3 -m json.tool | grep throttle_ratio
    sleep 5
done
# Attendu — évolution progressive :
#   t=0min  remote_pages=0          throttle_ratio=0.00  (idle)
#   t=1min  remote_pages=0          throttle_ratio=0.15  (CPU monte)
#   t=2min  remote_pages=0          throttle_ratio=0.32  → hotplug vCPU déclenché
#           remote_pages=0          throttle_ratio=0.08  (après hotplug, soulagement)
#   t=3min  remote_pages=512        throttle_ratio=0.10  (RAM commence à déborder)
#   t=5min  remote_pages=2048+      throttle_ratio=0.12  (éviction active, CPU stable)
#
# Si throttle_ratio reste > 0.30 et n'est pas soulagé :
#   → vérifier que le nœud a des vCPUs libres (curl .../control/vcpu/status)
#   → si vcpu_free=0 → le controller doit proposer migration (check journalctl -u omega-controller)
```

### 14.2 Migration pendant pression (scénario M5)

```bash
# Lancer une pression mémoire sur la VM 100
ssh vm-100 'stress-ng --vm 1 --vm-bytes 75% &'

# Déclencher une migration live pendant la pression
curl -s -X POST http://127.0.0.1:9300/control/migrate \
    -H "Content-Type: application/json" \
    -d '{"vm_id": 100, "target": "pve-node2", "type": "live"}' | python3 -m json.tool

# La VM doit rester accessible pendant la migration
ssh vm-100 'ping -c 30 8.8.8.8'
# Attendu : 30 paquets transmis, 0 perdus, aucune interruption > 1s
# La migration live QEMU maintient la VM active (pre-copy dirty pages)
# Un seul "downtime" de < 300ms est normal lors du cut-over final
# Si perte > 1s : réseau saturé ou dirty page rate trop élevé → essayer cold migration

# Observer le statut de migration
watch -n 2 'curl -s http://127.0.0.1:9300/control/migrations | python3 -m json.tool'
# Attendu :
#   t=0s   state: "running"
#   t=5s   state: "running" (transfert des dirty pages en cours)
#   t=15s  state: "success"  elapsed_ms=12000
#
# Si state reste "running" > 5 minutes :
#   → migration bloquée (dirty page rate > transfert rate)
#   → solution : limiter la charge dans la VM, ou forcer cold migration
# Si state passe directement à "failed" :
#   → voir error dans le JSON et journalctl -u omega-daemon | grep "task_id"
```

### 14.3 CPU saturé + RAM saturée : double migration (scénario M2)

```bash
# État initial : Nœud 1 à 90% RAM et 95% CPU
# Saturer le nœud 1 avec deux VMs lourdes (101=batch, 102=dev)
ssh vm-101 'stress-ng --vm 1 --vm-bytes 90% --cpu 8 --timeout 600s &'
ssh vm-102 'stress-ng --vm 1 --vm-bytes 75% --cpu 4 --timeout 600s &'

# Observer l'état du nœud
watch -n 3 'curl -s http://127.0.0.1:9200/api/node | python3 -m json.tool | grep -E "mem_usage|vcpu"'

# Le controller doit détecter et migrer VM 101 en premier (plus grosse, non critique)
journalctl -u omega-controller -f | grep "migration\|vm_id.*101"
# Attendu :
# [info] migrations recommandées count=1 vm_id=101 target=node-b type=live
# [info] migration démarrée UPID=...

# Vérifier qu'après migration de VM 101, le nœud 1 respire
# et que VM 102 n'est PAS migrée (seuil retombé en dessous du trigger)
sleep 30
curl -s http://127.0.0.1:9200/api/node | python3 -m json.tool | grep mem_usage_pct
# Attendu : mem_usage_pct < 75% (plus de migration nécessaire)

qm list   # VM 101 doit être sur node2, VM 102 reste sur node1
```

### 14.4 GPU + vCPU : placement multi-contraintes (scénario M3)

```bash
# Préparer l'état initial
# Nœud 1 : pas de GPU
# Nœud 2 : GPU 8 Go, 4 vCPUs libres
# Nœud 3 : GPU 8 Go, 12 vCPUs libres

# Vérifier l'état GPU des nœuds
for NODE in 192.168.1.1 192.168.1.2 192.168.1.3; do
    echo "=== $NODE ==="
    curl -s http://$NODE:9200/api/gpu | python3 -m json.tool | grep -E "backend|free_vram|vcpu"
done

# Créer une VM nécessitant GPU (4 Go) ET vCPUs (6 minimum)
# → Seul Nœud 3 satisfait les deux contraintes
qm create 200 --memory 16384 --cores 6 --name test-gpu-vcpu
qm set 200 --hookscript local:snippets/omega-hook.pl

# Injecter les métadonnées GPU dans la config Proxmox (pour le controller)
qm set 200 --description "omega_gpu_vram_mib=4096 omega_min_vcpus=6"

qm start 200
sleep 5

# Vérifier que la VM a démarré sur le nœud 3 (seul avec GPU + vCPUs suffisants)
qm status 200
# Attendu : "status: running" sur pve-node3
# Si la VM est sur node1 (sans GPU) : le placement n'a pas pris en compte la contrainte GPU
#   → vérifier que omega-controller est actif et que les métadonnées omega_gpu_vram_mib sont lues
# Si la VM est sur node2 (GPU ok mais 4 vCPUs < 6 requis) : contrainte vCPU ignorée
#   → consulter journalctl -u omega-controller | grep "vm_id.*200"

journalctl -u omega-controller | grep "vm_id.*200\|placement.*200"
# Attendu :
#   [info] placement VM 200 : node1 éliminé (pas de GPU)
#   [info] placement VM 200 : node2 éliminé (4 vCPUs libres < 6 requis)
#   [info] placement VM 200 : node3 sélectionné (GPU 7 Go ok, 12 vCPUs ok)
#
# Vérifier que le GPU est alloué sur node3
curl -s http://192.168.1.3:9200/api/gpu | python3 -m json.tool
# Attendu : free_vram_mib réduit de 4096 (7168 → 3072)

# Nettoyage
qm stop 200 && qm destroy 200
```

### 14.5 Cluster complet sur tous les axes — refus GPU (scénario M4)

```bash
# Saturer la VRAM sur tous les nœuds (remplir avec des VMs GPU)
# Puis tenter de démarrer une VM GPU supplémentaire
qm create 999 --memory 8192 --cores 4 --name test-gpu-full
qm set 999 --description "omega_gpu_vram_mib=2048"
qm start 999

# Observer que l'admission est refusée si VRAM cluster insuffisante
journalctl -u omega-controller | grep "999\|vram\|GPU\|insuffisant"
# Attendu : "admission VM 999 impossible : VRAM cluster insuffisante"

# Libérer manuellement une VM GPU pour débloquer
qm stop <vmid_gpu_idle>
sleep 10  # attendre le prochain cycle du controller (5s)

# La VM 999 doit être admise automatiquement
journalctl -u omega-controller | grep "999\|admis\|accept"

# Nettoyage
qm stop 999 && qm destroy 999
```

### 14.6 Rafale de démarrages simultanés (scénario M6)

```bash
# Créer 20 VMs légères avec hookscript
for i in $(seq 201 220); do
    qm create $i --memory 2048 --cores 2 --name "stress-$i" \
        --hookscript local:snippets/omega-hook.pl
    echo "VM $i créée"
done

# Démarrer toutes en simultané (vraie rafale)
time for i in $(seq 201 220); do qm start $i & done
wait
echo "Toutes lancées"

# Vérifier que toutes sont démarrées (pas de race condition dans le hookscript)
sleep 15
VMS_UP=$(qm list | grep -E "^(20[0-9]|21[0-9]|220)" | grep running | wc -l)
echo "VMs actives : $VMS_UP / 20"
# Attendu : 20

# Vérifier la répartition sur les nœuds (bin-packing)
python3 -m controller.main status \
    --stores 192.168.1.1:9100,192.168.1.2:9100,192.168.1.3:9100
# Attendu : VMs réparties sur les 3 nœuds selon la RAM disponible

# Vérifier qu'aucune collision de quotas n'a eu lieu
for i in $(seq 201 220); do
    RES=$(curl -s http://127.0.0.1:9300/control/vm/$i/quota 2>/dev/null | python3 -c \
        "import sys,json; d=json.load(sys.stdin); print('OK' if d.get('max_mem_mib') else 'MISSING')" 2>/dev/null)
    echo "VM $i quota: $RES"
done
# Attendu : toutes affichent "OK"

# Nettoyage
for i in $(seq 201 220); do qm stop $i && qm destroy $i & done
wait
echo "Nettoyage terminé"
```

### 14.7 Maintenance planifiée — drain complet (scénario M7)

```bash
# Migrer toutes les VMs du nœud 1 vers les nœuds 2 et 3
# Utiliser la raison "maintenance"
for vmid in $(qm list | awk 'NR>1 {print $1}'); do
    echo "Migration VM $vmid..."
    curl -s -X POST http://127.0.0.1:9300/control/migrate \
        -H "Content-Type: application/json" \
        -d "{\"vm_id\": $vmid, \"target\": \"auto\", \"type\": \"live\", \"reason\": \"maintenance\"}" \
        | python3 -m json.tool
    # Attendu par VM :
    # {"status": "migration_started", "task_id": N, "vm_id": XXX, "type": "live"}
    sleep 5  # attendre que la migration démarre avant la suivante
done

# Suivre toutes les migrations en cours
watch -n 3 'curl -s http://127.0.0.1:9300/control/migrations | python3 -m json.tool | grep -E "vm_id|state|elapsed"'
# Attendu : chaque migration passe par running → success (ou failed si problème réseau)
# Durée typique : 10-30s par VM selon sa taille et la charge mémoire

# Vérifier que le nœud est vide
sleep 60
qm list | awk 'NR>1'
# Attendu : vide (aucune VM locale)
# Si des VMs restent : vérifier les migrations failed
curl -s http://127.0.0.1:9300/control/migrations | python3 -m json.tool | grep '"state": "failed"'
# Pour chaque failed : consulter journalctl -u omega-daemon | grep "task_id=N"

# Vérifier la répartition sur node2 et node3
for NODE in 192.168.1.2 192.168.1.3; do
    echo "=== $NODE ==="
    ssh $NODE 'qm list'
    # Attendu : les VMs sont réparties entre les deux nœuds
done

# Arrêter le daemon en sécurité (nœud vide)
systemctl stop omega-daemon
systemctl disable omega-daemon
# Attendu : "Removed /etc/systemd/system/multi-user.target.wants/omega-daemon.service"
echo "Nœud 1 drainé — maintenance possible"
```

---

## 15. Checklist finale avant mise en production

```bash
# ─── Infrastructure ───────────────────────────────────────────────────────────

# 1. Kernel >= 5.7
uname -r | awk -F. '$1>5 || ($1==5 && $2>=7) {print "OK kernel " $0}' || echo "FAIL: kernel trop ancien"

# 2. cgroups v2
mount | grep cgroup2 && echo "OK cgroups v2" || echo "FAIL: cgroups v2 absent"

# 3. userfaultfd disponible
[ -c /dev/userfaultfd ] && echo "OK userfaultfd" || echo "FAIL: /dev/userfaultfd absent"

# 4. Connectivité entre nœuds
for NODE in 192.168.1.2 192.168.1.3; do
    nc -zv $NODE 9100 2>&1 | grep -q "succeeded" && echo "OK store $NODE:9100" || echo "FAIL store $NODE:9100"
    nc -zv $NODE 9200 2>&1 | grep -q "succeeded" && echo "OK api   $NODE:9200" || echo "FAIL api   $NODE:9200"
    nc -zv $NODE 9300 2>&1 | grep -q "succeeded" && echo "OK ctrl  $NODE:9300" || echo "FAIL ctrl  $NODE:9300"
done

# ─── Services ─────────────────────────────────────────────────────────────────

# 5. Daemons actifs sur les 3 nœuds
for NODE in 192.168.1.1 192.168.1.2 192.168.1.3; do
    STATUS=$(ssh $NODE 'systemctl is-active omega-daemon')
    echo "$NODE omega-daemon: $STATUS"
done

# 6. Controller actif
systemctl is-active omega-controller && echo "OK controller" || echo "FAIL: controller inactif"

# 7. Wrapper QEMU en place sur les 3 nœuds
for NODE in 192.168.1.1 192.168.1.2 192.168.1.3; do
    ssh $NODE 'ls -la /usr/bin/kvm | grep kvm-omega' && echo "$NODE: OK kvm wrapper" || echo "$NODE: FAIL kvm wrapper"
done

# ─── Fonctionnel ──────────────────────────────────────────────────────────────

# 8. Hookscript enregistré sur les VMs
qm config 100 | grep hookscript | grep -q omega-hook && echo "OK hookscript VM 100" || echo "FAIL hookscript"

# 9. Une VM démarre avec l'agent
qm start 100
sleep 5
omega-qemu-launcher status --vm-id 100 | python3 -m json.tool | grep -q '"agent_running": true' \
    && echo "OK agent VM 100" || echo "FAIL agent VM 100"

# 10. Mode stub désactivé (migrations réelles actives)
journalctl -u omega-controller | grep -q "STUB" && echo "ATTENTION: controller en mode STUB" \
    || echo "OK: controller avec token API réel"
```

---

## Résumé des commandes de monitoring quotidien

```bash
# Vue d'ensemble cluster
python3 -m controller.main status \
    --stores 192.168.1.1:9100,192.168.1.2:9100,192.168.1.3:9100

# Pages distantes par VM (toutes les VMs)
for vmid in $(qm list | awk 'NR>1 {print $1}'); do
    PAGES=$(curl -s http://127.0.0.1:9300/control/pages/$vmid | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('remote_pages',0))")
    [ "$PAGES" -gt 0 ] && echo "VM $vmid : $PAGES pages distantes"
done

# Dernières migrations
curl -s http://127.0.0.1:9300/control/migrations | python3 -m json.tool

# Alerte si RAM > 85% sur un nœud
for NODE in 192.168.1.1 192.168.1.2 192.168.1.3; do
    PCT=$(curl -s http://$NODE:9200/api/node | python3 -c "import sys,json; print(json.load(sys.stdin).get('mem_usage_pct',0))")
    echo "$NODE: RAM ${PCT}%" $(python3 -c "print('⚠️ ALERTE' if float('$PCT') > 85 else 'OK')")
done
```
