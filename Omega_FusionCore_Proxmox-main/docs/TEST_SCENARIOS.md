# Scénarios de tests — omega-remote-paging

## 0. Tests unitaires (toujours en premier)

```bash
cargo test --workspace -- --nocapture 2>&1 | tail -20
# Attendu : 0 failed
# Actuellement : 244 tests (agent-lib + agent-main + intégration + store + doc)
```

---

## 1. Smoke test local (1 machine)

### Objectif
Vérifier le cycle éviction → fault → recall sans Proxmox.

```bash
# Terminal 1 : store
cargo run -p node-bc-store -- \
  --listen 127.0.0.1:9100 \
  --status-listen 127.0.0.1:9200 \
  --node-id local-test

# Terminal 2 : agent en mode demo
cargo run -p node-a-agent -- \
  --stores 127.0.0.1:9100 \
  --vm-id 1 \
  --vm-requested-mib 64 \
  --region-mib 64 \
  --mode demo
```

**Résultat attendu :**
```
étape 1/5 : écriture ... ok
étape 2/5 : éviction des pages paires ... ok
étape 3/5 : lecture + vérification d'intégrité ... errors=0
étape 4/5 : recall LIFO ... ok
étape 5/5 : résultats ... integrity_ok=true
SUCCÈS : toutes les pages lues avec intégrité correcte
```

**Erreurs possibles :**
- `aucun store disponible` → store pas lancé ou mauvais port
- `userfaultfd: Operation not permitted` → `sysctl -w vm.unprivileged_userfaultfd=1`
- `fetch_page échoué: NotFound` → les pages n'ont pas été envoyées au store (voir logs éviction)

---

## 2. Test 2 stores (réplication)

```bash
# Store A et Store B en local
cargo run -p node-bc-store -- --listen 127.0.0.1:9100 --status-listen 127.0.0.1:9200 --node-id s0 &
cargo run -p node-bc-store -- --listen 127.0.0.1:9101 --status-listen 127.0.0.1:9201 --node-id s1 &

cargo run -p node-a-agent -- \
  --stores 127.0.0.1:9100,127.0.0.1:9101 \
  --status-addrs 127.0.0.1:9200,127.0.0.1:9201 \
  --vm-id 1 \
  --vm-requested-mib 64 \
  --region-mib 64 \
  --replication-enabled \
  --mode demo
```

**Vérifier :** les deux stores ont des pages pour vm_id=1
```bash
curl -s http://127.0.0.1:9200/status | jq .
curl -s http://127.0.0.1:9201/status | jq .
```

---

## 3. Test résilience store (failover)

```bash
# Lancer agent avec réplication
# Évincer des pages (mode demo)
# Tuer le store primaire
kill <pid-store-0>
# Tenter un recall — doit basculer sur le store secondaire automatiquement
```

**Résultat attendu :** recall réussi depuis le store 1 malgré la perte du store 0.

---

## 4. Test éviction / recall daemon (pression mémoire simulée)

```bash
cargo run -p node-bc-store -- --listen 127.0.0.1:9100 --status-listen 127.0.0.1:9200 &

cargo run -p node-a-agent -- \
  --stores 127.0.0.1:9100 \
  --vm-id 2 \
  --vm-requested-mib 256 \
  --region-mib 256 \
  --eviction-threshold-mib 99999 \  # toujours évincer (simuler pression)
  --eviction-batch-size 16 \
  --eviction-interval-secs 2 \
  --recall-threshold-mib 0 \        # jamais rappeler automatiquement
  --mode daemon &

# Observer les logs : éviction toutes les 2s
# Métriques
watch -n2 'curl -s http://127.0.0.1:9300/metrics'
```

**Métriques à surveiller :**
- `pages_evicted` — doit augmenter
- `pages_recalled` — 0 si recall désactivé
- `fault_count` / `fault_served` — quand on lit des pages évincées
- `eviction_alerts` — si store plein ou inaccessible

---

## 5. Test vCPU élastique

**Prérequis :** Proxmox avec VM 9001 et `qm` accessible.

La VM doit être créée avec un plafond supérieur à 1 vCPU. Le champ `vcpus`
est le nombre actif au démarrage, mais `cores * sockets` définit le maximum
que le test peut atteindre.

```bash
qm stop 9001
qm set 9001 --cores 4 --sockets 1 --vcpus 1 --hotplug cpu,disk,network
qm start 9001
qm showcmd 9001 --pretty | grep -- '-smp'
```

La ligne QEMU doit contenir un équivalent de `-smp '1,sockets=1,cores=4,maxcpus=4'`.

```bash
# Démarrer l'agent pour VM 9001 avec vCPU initial = 1, max = 8
AGENT_VM_ID=9001 \
AGENT_VM_VCPUS=8 \
AGENT_VM_INITIAL_VCPUS=1 \
AGENT_VCPU_HIGH_THRESHOLD_PCT=75 \
AGENT_VCPU_LOW_THRESHOLD_PCT=25 \
AGENT_VCPU_SCALE_INTERVAL_SECS=30 \
AGENT_VCPU_OVERCOMMIT_RATIO=3 \
node-a-agent --mode daemon
```

**Vérification :**
```bash
# Voir les vCPUs actifs de la VM
qm config 9001 | grep vcpus

# Pool partagé
cat /run/omega-vcpu-pool.json

# Simuler charge CPU dans la VM (depuis l'intérieur)
stress-ng --cpu 0 --timeout 60s  # dans la VM

# Observer dans les logs agent :
# "vCPUs ajustés" from=1 to=2, puis to=3, etc.
```

**Erreurs possibles :**
- `qm set --vcpus échoué` → VM sans `--hotplug cpu` → l'agent essaie de la configurer automatiquement au 1er démarrage
- `aucun scale-up observé` avec `cores=1` → VM créée avec un seul vCPU maximum ; corriger `--cores`, `--sockets`, `--vcpus 1` puis redémarrer la VM
- `flock pool vCPU échoué` → problème de permissions sur `/run/`
- cgroup v2 absent → fallback sur `/proc/<pid>/stat` automatique

---

## 6. Test GPU placement

**Prérequis :** Proxmox, VM 9001 avec tag `omega-gpu`, au moins un nœud avec GPU PCI.

```bash
AGENT_VM_ID=9001 \
AGENT_CURRENT_NODE=$(hostname) \
AGENT_GPU_REQUIRED=true \
AGENT_GPU_PLACEMENT_INTERVAL_SECS=30 \
node-a-agent --mode daemon
```

**Vérification :**
```bash
# Le démon doit :
# 1. Détecter qu'on n'est pas sur un nœud GPU
# 2. Chercher un nœud avec GPU dans le cluster
# 3. Configurer hostpci0 sur la VM
# 4. Lancer qm migrate <vmid> <target> (offline)

# Observer :
journalctl -f | grep -E "GPU|migration|hostpci"
qm config 9001 | grep hostpci
```

---

## 7. Test GPU partage (scheduler round-robin)

**Prérequis :** 2 VMs GPU sur le même nœud, socket QMP accessible.

```bash
# Les deux agents démarrent leur GpuScheduler
# Un seul devient leader (flock)
# Observer la rotation dans les logs
grep "GPU assigné\|scheduler GPU" /var/log/omega/*.log
```

**Vérification :**
```bash
# Dans la VM GPU courante, le GPU doit être visible
lspci | grep -i vga   # dans la VM active

# Vérifier le lock
ls -la /run/omega-gpu-scheduler-*.lock
```

---

## 8. Test migration RAM

**Prérequis :** Cluster 3 nœuds identiques (OMEGA_NODES), VM 9001 active.

```bash
# Configurer l'agent avec migration activée
AGENT_MIGRATION_ENABLED=true \
AGENT_MIGRATION_INTERVAL_SECS=30 \
AGENT_EVICTION_THRESHOLD_MIB=512 \
AGENT_VM_MIN_RAM_MIB=1024 \
node-a-agent --mode daemon

# Simuler pression mémoire sur le nœud courant :
# Lancer d'autres process qui consomment de la RAM
stress-ng --vm 1 --vm-bytes 80% --timeout 120s

# Observer :
# 1. Pages évincées vers stores
# 2. Si tous les stores sont pleins ou inaccessibles → "migration déclenchée"
# 3. recall complet avant qm migrate
# 4. qm migrate 9001 pve2 --online
```

**Résultat attendu dans les logs :**
```
recall complet avant migration recalled=N
lancement qm migrate --online
migration réussie
```

---

## 9. Test nettoyage orphelins

```bash
# Démarrer un store avec orphan cleaner
STORE_ORPHAN_CHECK_INTERVAL_SECS=60 \  # 1 min pour tester
STORE_ORPHAN_GRACE_SECS=120 \           # 2 min de grâce
node-bc-store --listen 0.0.0.0:9100 --status-listen 0.0.0.0:9200

# Créer des pages pour une VM inexistante (simuler crash)
# Utiliser le client de test ou un agent demo puis tuer l'agent sans arrêt propre
cargo run -p node-a-agent -- --vm-id 9999 --stores 127.0.0.1:9100 --mode demo
kill -9 <pid-agent>  # crash brutal, pas de purge

# Attendre 3 min (check_interval + grace)
# Observer dans les logs du store :
# "pages orphelines supprimées" vmid=9999 pages_deleted=N
```

**Vérifier :**
```bash
# pvesh doit retourner que la VM 9999 n'existe pas
pvesh get /cluster/resources --type vm --output-format json | jq '.[].vmid'
# → 9999 absent → détecté comme orphelin après grâce
```

---

## 10. Test charge multi-VM

```bash
# 3 stores
for port in 9100 9101 9102; do
  cargo run -p node-bc-store -- --listen 127.0.0.1:$port --status-listen 127.0.0.1:$((port+100)) &
done

# 3 agents simultanés pour 3 VMs différentes
for vmid in 1 2 3; do
  cargo run -p node-a-agent -- \
    --stores 127.0.0.1:9100,127.0.0.1:9101,127.0.0.1:9102 \
    --vm-id $vmid \
    --vm-requested-mib 64 \
    --region-mib 64 \
    --vm-count-hint 3 \
    --recall-priority $vmid \
    --mode demo &
done
wait
```

**Vérifier :**
- Chaque VM utilise sa part du pool de stores (proportionnelle à la capacité)
- La priorité de recall (1 < 2 < 3) est respectée (délais différents dans les logs)
- Pas de collision de pages entre VMs (vm_id différents)

---

## Checklist avant tests Proxmox réels

```bash
# 1. userfaultfd autorisé
sysctl vm.unprivileged_userfaultfd    # → 1

# 2. Stores accessibles depuis chaque nœud (port 9100)
for node in $OMEGA_NODES; do nc -zv $node 9100 && echo "store $node OK"; done

# 3. pvesh disponible (pour orphan cleaner)
pvesh get /cluster/resources --type vm --output-format json | head -5

# 4. qm accessible (pour vCPU hotplug + GPU + migration)
qm list

# 5. Ceph (si utilisé)
ceph status
rados lspools | grep omega-pages

# 6. GPU (si utilisé)
lspci | grep -i vga
ls /sys/bus/pci/devices/*/class | xargs grep -l "^0x03" 2>/dev/null

# 7. Vérifier que le pool vCPU est initialisé proprement
cat /run/omega-vcpu-pool.json  # ou vide si premier démarrage
```
