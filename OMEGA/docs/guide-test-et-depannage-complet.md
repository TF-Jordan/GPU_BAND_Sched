# Guide Complet De Test Et Dépannage

Ce document sert de guide unique pour :
- créer des VMs de test Proxmox compatibles avec le projet ;
- tester CPU, RAM, GPU, disque et migrations ;
- retrouver rapidement tous les logs utiles ;
- retrouver les erreurs réellement rencontrées sur le cluster réel et leurs corrections ;
- déployer et vérifier les binaires `omega-daemon` et `omega-controller`.

Il complète :
- [retour-experience-cluster-reel.md](./retour-experience-cluster-reel.md)
- [utilisation-physique.md](./utilisation-physique.md)
- [fonctionnement-complet.md](./fonctionnement-complet.md)

---

## 1. Topologie Réelle Retenue

Cluster réel validé :

| Nœud | IP | Rôle |
|------|----|------|
| `pve` | `10.10.0.11` | Proxmox + `omega-daemon` + `omega-controller` |
| `pve2` | `10.10.0.12` | Proxmox + `omega-daemon` |
| `pve3` | `10.10.0.13` | Proxmox + `omega-daemon` |

Réseau réel :
- réseau cluster privé : `10.10.0.0/24`
- sortie Internet : via NAT sortant
- stockage recommandé pour les disques VM : `ceph-vms`

Ports importants :

| Port | Service | Usage |
|------|---------|-------|
| `8006` | Proxmox Web UI | interface web |
| `9100` | `omega-daemon` store TCP | paging RAM / store |
| `9200` | `omega-daemon` cluster API | état nœud exposé au cluster |
| `9300` | `omega-daemon` control API | quotas, CPU, GPU, disque, migration, métriques |

Important :
- `omega-daemon` doit tourner sur chaque nœud ;
- `omega-controller` doit tourner sur une seule machine ;
- les migrations des VMs de test sont nettement plus simples si le disque est sur `ceph-vms`.

---

## 2. Vérifications Initiales

### 2.1 Vérifier les storages Proxmox

```bash
pvesm status
```

Sortie attendue typique :

```text
Name             Type     Status
ceph-vms          rbd     active
local             dir     active
local-lvm     lvmthin     active
```

Règles retenues :
- ISO : `local`
- disque VM : `ceph-vms`

### 2.2 Vérifier les services Omega

Sur chaque nœud :

```bash
systemctl status omega-daemon
ss -tlnp | grep -E '9100|9200|9300'
```

Sur le nœud controller :

```bash
systemctl status omega-controller
journalctl -u omega-controller -n 100 --no-pager
```

### 2.3 Vérifier les endpoints HTTP

Exemples :

```bash
curl -s http://10.10.0.11:9200/api/status | python3 -m json.tool
curl -s http://10.10.0.11:9300/control/status | python3 -m json.tool
curl -s http://10.10.0.11:9300/control/metrics
```

---

## 3. Création D’Une VM Test Qui Marche

### 3.0 Prérequis — ce qu’une VM doit avoir pour être gérée par omega

Pour qu’omega puisse gérer une VM correctement (balloon, vCPU élastique, migration, disk I/O), la VM doit être créée avec les bons paramètres dès le départ. Un oubli à la création oblige à recréer la VM.

**Résumé des contraintes obligatoires :**

| Fonctionnalité | Paramètre Proxmox requis | Pourquoi |
|---|---|---|
| RAM balloon | `--balloon <initial_mib>` inférieur ou égal à `--memory` | Active le driver virtio-balloon dans QEMU. Sans ça, `qm balloon` échoue. |
| RAM thin-provisioning | `--memory <max_mib>` = RAM max souhaitée | C’est le plafond absolu. La VM démarre plus petite via balloon/Omega. |
| vCPU élastique | `--hotplug cpu` + `--cores <max>` + `--vcpus 1` | Sans hotplug cpu, `qm set --vcpus N` échoue. Les cores définissent le plafond. |
| Migration live | `--net0 virtio,...` + stockage Ceph | La migration QEMU online nécessite un réseau virtio et des disques partagés. |
| Disk I/O scheduler | Aucun — géré via cgroups v2 automatiquement | Fonctionne dès que le kernel a cgroups v2 actif (PVE 8+). |

**Commande de création complète (VM de test `9001`) :**

```bash
qm create 9001 \
  --name omega-test \
  --memory 2048 \
  --balloon 512 \
  --cores 4 \
  --sockets 1 \
  --vcpus 1 \
  --hotplug cpu,disk,network \
  --net0 virtio,bridge=vmbr0 \
  --ostype l26 \
  --scsihw virtio-scsi-pci \
  --scsi0 ceph-vms:20 \
  --ide2 local:iso/debian-12-netinst.iso,media=cdrom \
  --boot order=ide2
```

Points clés :
- `--memory 2048` = plafond RAM absolu (omega ne dépassera jamais ça)
- `--balloon 512` = RAM initiale/minimale ; `--memory 2048` reste le plafond
- `--cores 4 --vcpus 1` = 4 cores disponibles (plafond hotplug), 1 actif au démarrage
- `--hotplug cpu,disk,network` = permet le hotplug CPU sans activer le hotplug mémoire Proxmox
- stockage sur Ceph = obligatoire pour la migration live (disques partagés entre nœuds)
- ne pas activer `memory` dans `--hotplug` pour les VMs Omega avec backend memfd ; la RAM progressive est gérée par balloon/Omega

**Vérifier qu’une VM existante est correctement configurée :**

```bash
qm config 9001 | grep -E "memory|balloon|cores|vcpus|hotplug"
# Doit afficher :
# balloon: 512
# cores: 4
# hotplug: cpu,disk,network
# memory: 2048
# vcpus: 1
```

**qemu-guest-agent (obligatoire pour les tests de charge) :**

Les tests qui simulent une charge CPU/RAM dans la VM utilisent `qm guest exec` pour lancer `stress-ng` à l'intérieur du guest. Sans qemu-guest-agent, le test se rabat sur une injection dans le cgroup QEMU côté hôte (moins précis) et affiche des warnings.

```bash
# 1. Activer l'agent dans la config Proxmox (VM éteinte ou allumée)
qm set 9001 --agent enabled=1

# 2. Installer le paquet dans la VM
# Depuis la console de la VM (Debian/Ubuntu) :
apt install -y qemu-guest-agent
systemctl enable --now qemu-guest-agent

# 3. Vérifier depuis l'hôte
qm agent 9001 ping   # doit répondre sans erreur
```

**Reconfigurer une VM existante sans la recréer :**

```bash
# Activer balloon (à faire VM éteinte)
qm set 9001 --balloon 512

# Activer hotplug CPU (à faire VM éteinte)
qm set 9001 --hotplug cpu,disk,network
qm set 9001 --cores 4 --sockets 1 --vcpus 1
```

> **Note** : `--balloon` et `--hotplug cpu` ne peuvent pas être activés à chaud sur une VM déjà démarrée sans ces options. Il faut arrêter la VM, modifier la config, puis redémarrer.

---

Cette section rassemble les commandes Proxmox qui ont effectivement servi et qui sont compatibles avec le cluster réel.

### 3.1 Créer une VM vide

Exemple `9004` :

```bash
qm create 9004 \
  --name omega-test-cpu \
  --memory 2048 \
  --balloon 512 \
  --cores 4 \
  --sockets 1 \
  --vcpus 1 \
  --hotplug cpu,disk,network \
  --net0 virtio,bridge=vmbr0 \
  --ostype l26 \
  --scsihw virtio-scsi-pci
```

Arguments validés dans nos scénarios :
- `--vmid`
- `--name`
- `--memory`
- `--cores`
- `--sockets`
- `--net0`
- `--ostype`
- `--scsihw`

### 3.2 Ajouter un disque sur Ceph

```bash
qm set 9004 --scsihw virtio-scsi-pci
qm set 9004 --scsi0 ceph-vms:20
```

Vérifier :

```bash
qm config 9004
```

On doit voir :

```text
scsi0: ceph-vms:vm-9004-disk-0,...
```

### 3.3 Attacher un ISO Debian

```bash
qm set 9004 --ide2 local:iso/debian-13.4.0-amd64-netinst.iso,media=cdrom
qm set 9004 --boot order=ide2
```

Vérifier :

```bash
qm config 9004
```

### 3.4 Démarrer la VM

```bash
qm start 9004
```

Ensuite :
- ouvrir la console Proxmox ;
- installer Debian normalement ;
- une fois l’installation terminée, retirer l’ISO.

### 3.5 Retirer le CD-ROM et booter sur le disque

```bash
qm set 9004 --delete ide2
qm set 9004 --boot order=scsi0
```

### 3.5.1 Vérifier le réseau dans le guest Debian

Dans la VM, vérifier immédiatement après installation :

```bash
ip a
ip route
```

Si l’interface `ens18` est `DOWN`, la lever :

```bash
ip link set ens18 up
```

Si `dhclient` n’est pas installé dans le netinst minimal, poser temporairement une IP statique :

```bash
ip link set ens18 up
ip addr add 10.10.0.50/24 dev ens18
ip route add default via 10.10.0.1
echo 'nameserver 8.8.8.8' > /etc/resolv.conf
```

Tests immédiats :

```bash
ping 10.10.0.1
ping 8.8.8.8
ping deb.debian.org
```

Pour rendre la configuration persistante sur Debian :

```bash
cat > /etc/network/interfaces <<'EOF'
auto lo
iface lo inet loopback

auto ens18
iface ens18 inet static
    address 10.10.0.50/24
    gateway 10.10.0.1
    dns-nameservers 8.8.8.8 1.1.1.1
EOF
```

### 3.6 Préparer le hotplug CPU

Pour les tests d’élasticité CPU :

```bash
qm stop 9004
qm set 9004 --hotplug cpu
qm set 9004 --cores 4 --sockets 1
qm set 9004 --vcpus 1
qm start 9004
```

Vérifications indispensables :

```bash
qm config 9004
qm showcmd 9004 --pretty | grep -- '-smp'
```

Exemple attendu :

```text
-smp '1,sockets=1,cores=4,maxcpus=4'
```

### 3.7 Vérifier les CPU hotpluggables vus par QEMU

```bash
qm monitor 9004
```

Puis dans le monitor :

```text
info cpus
info hotpluggable-cpus
quit
```

Exemple sain :
- `info cpus` montre `CPU #0`
- `info hotpluggable-cpus` montre plusieurs entrées `kvm64-x86_64-cpu`
- une entrée a `qom_path`
- les autres n’ont pas encore de `qom_path`

### 3.8 Déclarer les métadonnées Omega dans Proxmox

Format description supporté :

```text
omega.gpu_vram_mib=2048
omega.min_vcpus=1
omega.max_vcpus=4
```

Ou via tags :

```text
omega-gpu-2048;omega-min-vcpus-1;omega-max-vcpus-4
```

Règles lues par le controller :
- `omega.max_vcpus` si présent, sinon `sockets × cores`
- `omega.min_vcpus` si présent, sinon `max_vcpus / 2`
- `omega.gpu_vram_mib` si présent pour le budget GPU

---

## 4. Déploiement Du Code Sur Les Nœuds Proxmox

### 4.1 Controller Python

Sur le nœud controller :

```bash
cd /opt/omega-remote-paging
git pull
source /opt/omega-controller-venv/bin/activate
pip install -e /opt/omega-remote-paging/controller/
systemctl restart omega-controller
```

### 4.2 Daemon Rust

Si `cargo` n’est pas installé sur les nœuds Proxmox, builder localement puis copier le binaire.

Depuis la machine de développement :

```bash
cd /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging
cargo build -p omega-daemon --release
```

Copie vers `pve3` :

```bash
scp /home/blhack/Projets/Omega/Proxmox/RAM/omega-remote-paging/target/release/omega-daemon root@10.10.0.13:/tmp/omega-daemon
```

Puis sur le nœud :

```bash
systemctl stop omega-daemon
install -m 755 /tmp/omega-daemon /usr/local/bin/omega-daemon
systemctl start omega-daemon
systemctl status omega-daemon
```

Même procédure pour `10.10.0.11` et `10.10.0.12`.

Important :
- ne pas lancer un vieux binaire à la main en parallèle ;
- si besoin :

```bash
pkill -f omega-daemon
systemctl restart omega-daemon
```

---

## 5. Endpoints Et Commandes De Logs

### 5.1 Interface web Proxmox

```text
https://10.10.0.11:8006
https://10.10.0.12:8006
https://10.10.0.13:8006
```

### 5.2 Endpoints Omega les plus utiles

#### API cluster du daemon

```bash
curl -s http://10.10.0.11:9200/api/status | python3 -m json.tool
```

#### État complet local du daemon

```bash
curl -s http://10.10.0.11:9300/control/status | python3 -m json.tool
```

#### RAM

```bash
curl -s http://10.10.0.11:9300/control/quotas | python3 -m json.tool
curl -s http://10.10.0.11:9300/control/vm/9004/quota | python3 -m json.tool
```

#### CPU

```bash
curl -s http://10.10.0.11:9300/control/vcpu/status | python3 -m json.tool
```

#### GPU

```bash
curl -s http://10.10.0.11:9300/control/gpu/status | python3 -m json.tool
```

#### Disque

```bash
curl -s http://10.10.0.11:9300/control/disk/status | python3 -m json.tool
```

#### Migrations

```bash
curl -s http://10.10.0.11:9300/control/migrate/recommend | python3 -m json.tool
curl -s http://10.10.0.11:9300/control/migrations | python3 -m json.tool
```

#### Prometheus

```bash
curl -s http://10.10.0.11:9300/control/metrics
```

### 5.3 Logs systemd

Daemon :

```bash
journalctl -u omega-daemon -f
journalctl -u omega-daemon -n 200 --no-pager
```

Controller :

```bash
journalctl -u omega-controller -f
journalctl -u omega-controller -n 200 --no-pager
```

Services Proxmox utiles :

```bash
journalctl -u pvedaemon -n 100 --no-pager
journalctl -u pveproxy -n 100 --no-pager
journalctl -u pve-cluster -n 100 --no-pager
```

### 5.4 Commandes Proxmox utiles

```bash
qm list
qm status 9004
qm config 9004
qm showcmd 9004 --pretty
qm monitor 9004
pvesm status
pvecm status
```

### 5.5 Réseau et ports

```bash
ss -tlnp | grep -E '9100|9200|9300|8006'
ip route
iptables -L -n
nc -zv 10.10.0.12 9200
nc -zv 10.10.0.12 9300
```

---

## 6. Scénarios De Test

Les scénarios ci-dessous sont faits pour être exécutés directement.

Convention utilisée :
- VM de test principale : `9004`
- nœud où tourne `9004` : `pve3` (`10.10.0.13`)
- controller : `pve` (`10.10.0.11`)

Adapter l’IP si la VM migre entre-temps.

## 6.1 Test CPU simple

### Préparation

Sur `pve3` :

```bash
qm status 9004
qm config 9004
qm showcmd 9004 --pretty | grep -- '-smp'
curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool
```

Dans la VM Debian :

```bash
apt update
apt install -y stress-ng procps
```

### Charge

Dans la VM :

```bash
stress-ng --cpu 4 --timeout 180s
```

Ou en arrière-plan :

```bash
stress-ng --cpu 4 --timeout 300s &
```

Alternative minimale :

```bash
yes > /dev/null &
yes > /dev/null &
yes > /dev/null &
yes > /dev/null &
```

### Observation

Sur `pve3` :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool'
```

Et dans un autre terminal :

```bash
journalctl -u omega-daemon -f
```

Dans la VM :

```bash
top
uptime
```

### Résultat attendu

- `cpu_usage_pct` monte ;
- `current_vcpus` peut monter progressivement ;
- `max_vcpus` reste le plafond ;
- si le nœud souffre trop, la VM peut devenir candidate à migration CPU.

### Échec typique / diagnostic immédiat

- `cpu_usage_pct` reste à `0.0`
  - vérifier le bon nœud avec `qm status 9004`
  - vérifier le cgroup réel avec `cat /proc/$(cat /var/run/qemu-server/9004.pid)/cgroup`
  - relire `curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool`
- `current_vcpus` reste à `1`
  - vérifier `qm showcmd 9004 --pretty | grep -- '-smp'`
  - vérifier `qm monitor 9004`, puis `info hotpluggable-cpus`
  - relire `journalctl -u omega-daemon -n 200 --no-pager`
- erreur `Invalid CPU type`
  - le daemon du nœud n’est probablement pas à jour
  - vérifier le binaire actif avec `systemctl status omega-daemon`

### Redescente

Dans la VM :

```bash
pkill stress-ng
pkill yes
```

Observation :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool'
```

Résultat attendu :
- la charge retombe ;
- les vCPU redescendent progressivement ;
- la VM ne descend jamais sous son minimum.

### Échec typique / diagnostic immédiat

- les vCPU ne redescendent jamais
  - vérifier qu’aucun `stress-ng` ou `yes` ne tourne encore
  - vérifier `top` dans le guest
  - vérifier `journalctl -u omega-daemon -f`

## 6.2 Test RAM simple

### Préparation

Sur le controller :

```bash
curl -X POST http://10.10.0.13:9300/control/vm/9004/quota \
  -H "Content-Type: application/json" \
  -d '{"max_mem_mib": 2048, "local_budget_mib": 1536}'
```

Vérification :

```bash
curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool
curl -s http://10.10.0.13:9200/api/status | python3 -m json.tool
```

Dans la VM Debian :

```bash
apt update
apt install -y stress-ng procps
```

### Charge

Dans la VM :

```bash
stress-ng --vm 1 --vm-bytes 1536M --timeout 180s
```

Alternative :

```bash
python3 - <<'EOF'
a = bytearray(1400 * 1024 * 1024)
input("RAM allouee, appuie sur Entree pour liberer...")
EOF
```

Si la VM a seulement `512` Mio de RAM, ne pas utiliser `1400M` ou `1536M`. Utiliser plutôt :

```bash
python3 - <<'EOF'
a = bytearray(200 * 1024 * 1024)
input("200 Mio alloues, appuie sur Entree pour liberer...")
EOF
```

Ou :

```bash
stress-ng --vm 1 --vm-bytes 200M --timeout 120s
```

### Observation

Sur le nœud :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool'
```

Et :

```bash
watch -n 2 'curl -s http://10.10.0.13:9200/api/status | python3 -m json.tool'
```

Logs :

```bash
journalctl -u omega-daemon -f
```

### Résultat attendu

- budget local + distant cohérent ;
- mises à jour via balloon ;
- pression mémoire visible ;
- éventuellement recommandation de migration si le nœud devient mauvais.

### Échec typique / diagnostic immédiat

- le quota ne change pas
  - vérifier `curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool`
  - vérifier les logs daemon autour du balloon
- la VM devient instable trop vite
  - réduire `--vm-bytes`
  - vérifier `qm config 9004`
- `python3` lève `MemoryError`
  - la VM n’a simplement pas assez de RAM pour la taille demandée
  - réduire fortement la taille du test
  - ou augmenter `qm set <vmid> --memory ...`

## 6.3 Test GPU simple

### Préparation

Déclarer le budget dans la VM via description ou tags Proxmox, ou via API :

```bash
curl -s -X POST http://10.10.0.13:9300/control/vm/9004/gpu \
  -H "Content-Type: application/json" \
  -d '{"vram_budget_mib": 2048}'
```

Vérification :

```bash
curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool
curl -s http://10.10.0.13:9200/api/status | python3 -m json.tool
```

### Charge

Selon ce que tu as dans le guest, par exemple :

```bash
glxinfo | head
```

Si un workload GPU est disponible dans la VM, le lancer ici. Sinon ce test valide surtout :
- la détection du budget ;
- la présence du backend GPU ;
- la cohérence controller/daemon.

### Observation

```bash
curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool
journalctl -u omega-controller -f
```

### Résultat attendu

- budget VRAM déclaré ;
- charge GPU nœud lue depuis le backend réel ;
- placement/migration GPU-aware si le cluster y gagne.

### Échec typique / diagnostic immédiat

- `control/gpu/status` reste vide ou incohérent
  - vérifier la déclaration `omega.gpu_vram_mib` ou l’appel API
  - vérifier `journalctl -u omega-daemon -n 200 --no-pager`
  - vérifier `journalctl -u omega-controller -n 200 --no-pager`
- le guest n’a pas de vrai test GPU
  - ce n’est pas bloquant pour valider le budget et le placement
  - valider au moins `control/gpu/status` et `api/status`

## 6.4 Test disque simple

### Préparation

Dans la VM :

```bash
apt update
apt install -y fio
```

État initial :

```bash
curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool
curl -s http://10.10.0.13:9300/control/status | python3 -m json.tool
```

### Charge

Dans la VM :

```bash
fio --name=randwrite --filename=/tmp/fio.bin --size=512M --bs=4k --rw=randwrite --iodepth=32 --runtime=120 --time_based
```

Puis lecture :

```bash
fio --name=randread --filename=/tmp/fio.bin --size=512M --bs=4k --rw=randread --iodepth=32 --runtime=120 --time_based
```

### Observation

Sur le nœud :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool'
```

Et :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/status | python3 -m json.tool'
```

Logs :

```bash
journalctl -u omega-daemon -f
```

### Résultat attendu

- `disk_pressure_pct` évolue ;
- la VM chargée peut recevoir un `io.weight` plus élevé ;
- les VMs idle locales peuvent voir leur `io.weight` baisser temporairement ;
- si le nœud reste mauvais, le cluster peut proposer une migration.

Sur certains backends, en particulier des VMs sur `ceph-vms` / Ceph RBD, le cgroup
de la VM peut ne pas exposer `io.weight`. Dans ce cas, le daemon bascule
automatiquement en mode :

- observation disque uniquement ;
- plus de tentative répétée d’écriture `io.weight` ;
- migration/placement disk-aware conservés.

### Échec typique / diagnostic immédiat

- `disk_pressure_pct` ne bouge pas
  - vérifier que `fio` écrit réellement dans la VM
  - augmenter `--runtime`, `--size` ou `--iodepth`
  - vérifier `curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool`
- `io.weight` ne change jamais
  - relire `journalctl -u omega-daemon -n 200 --no-pager`
  - vérifier si `control/disk/status` expose `io_control_supported: false`
  - si oui, le backend ne supporte pas ce levier local et le fallback est normal

## 6.5 Test migration simple

### Recommandation automatique

```bash
curl -s http://10.10.0.13:9300/control/migrate/recommend | python3 -m json.tool
```

### Migration manuelle

```bash
curl -s -X POST http://10.10.0.13:9300/control/migrate \
  -H "Content-Type: application/json" \
  -d '{"vm_id": 9004, "target": "pve2", "mtype": "live", "reason": "maintenance"}'
```

### Observation

Sur source et cible :

```bash
journalctl -u omega-daemon -f
```

Et :

```bash
qm list
qm status 9004
```

### Drain manuel d’un nœud

```bash
omega-controller drain-node \
  --node-a http://10.10.0.11:9300 \
  --node-b http://10.10.0.12:9300 \
  --node-c http://10.10.0.13:9300 \
  --source-node node-b
```

Dry-run :

```bash
omega-controller drain-node \
  --node-a http://10.10.0.11:9300 \
  --node-b http://10.10.0.12:9300 \
  --node-c http://10.10.0.13:9300 \
  --source-node node-b \
  --dry-run
```

### Résultat attendu

- la migration live aboutit ;
- les ressources source sont nettoyées ;
- la VM apparaît localement sur la cible ;
- pas de `target=auto` exécuté.

### Échec typique / diagnostic immédiat

- erreur `can't migrate running VM without --online`
  - le type de migration est mauvais
  - relire `journalctl -u omega-daemon -n 200 --no-pager`
- erreur `target=auto`
  - le daemon ne doit plus lancer `qm migrate ... auto`
  - vérifier que le bon binaire est installé sur le nœud
- la VM ne bouge pas dans l’UI
  - vérifier `qm list` sur source et cible
  - vérifier que le disque est sur `ceph-vms`

Important :
- si on éteint un nœud sans drain préalable, les VMs qui tournent dessus s’arrêtent avec lui ;
- c’est normal sans HA/redrain préalable.

## 6.6 Scénario mixte CPU + RAM

Objectif :
- vérifier qu’une VM monte en CPU tout en consommant fortement la RAM ;
- observer si la pression RAM ou CPU devient dominante.

### Préparation

```bash
curl -X POST http://10.10.0.13:9300/control/vm/9004/quota \
  -H "Content-Type: application/json" \
  -d '{"max_mem_mib": 2048, "local_budget_mib": 1536}'
```

Dans la VM :

```bash
apt update
apt install -y stress-ng
```

### Charge

Dans la VM :

```bash
stress-ng --cpu 4 --vm 1 --vm-bytes 1400M --timeout 180s
```

### Observation

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool'
journalctl -u omega-daemon -f
```

### Résultat attendu

- `cpu_usage_pct` monte ;
- les quotas RAM évoluent ;
- la VM peut être candidate à migration si le nœud devient mauvais globalement.

### Échec typique / diagnostic immédiat

- seul le CPU réagit
  - augmenter `--vm-bytes`
  - vérifier `control/quotas`
- seule la RAM réagit
  - augmenter la charge CPU
  - vérifier `top` dans la VM

## 6.7 Scénario mixte CPU + disque

Objectif :
- vérifier qu’une VM fortement CPU + I/O reçoit du partage local puis peut devenir candidate à migration.

Dans la VM, lancer dans deux terminaux :

```bash
stress-ng --cpu 4 --timeout 180s
```

et :

```bash
fio --name=randwrite --filename=/tmp/fio.bin --size=512M --bs=4k --rw=randwrite --iodepth=32 --runtime=180 --time_based
```

Observation :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool'
journalctl -u omega-daemon -f
```

Résultat attendu :
- `cpu_usage_pct` et `disk_pressure_pct` montent ;
- `current_vcpus` peut monter si des slots existent ;
- `io.weight` peut être ajusté ;
- la VM peut être recommandée pour migration si le cluster y gagne.

### Échec typique / diagnostic immédiat

- le CPU réagit mais pas le disque
  - augmenter `fio`
  - vérifier `control/disk/status`
- le disque réagit mais pas le CPU
  - vérifier `control/vcpu/status`
  - vérifier la ligne `-smp`

## 6.8 Scénario mixte RAM + disque

Objectif :
- vérifier que la pression mémoire et la pression I/O peuvent coexister sans casser la VM.

Dans la VM :

```bash
stress-ng --vm 1 --vm-bytes 1400M --timeout 180s &
fio --name=randwrite --filename=/tmp/fio.bin --size=512M --bs=4k --rw=randwrite --iodepth=32 --runtime=180 --time_based
```

Observation :

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool'
journalctl -u omega-daemon -f
```

Résultat attendu :
- le daemon continue à suivre la VM correctement ;
- la RAM distante et la priorité I/O peuvent évoluer ;
- une migration peut être proposée si le cluster global s’améliore.

### Échec typique / diagnostic immédiat

- la VM devient instable trop vite
  - réduire `--vm-bytes`
  - réduire le test `fio`
- l’I/O prend le dessus trop tôt
  - observer séparément `control/quotas` et `control/disk/status`

## 6.9 Scénario mixte CPU + GPU

Objectif :
- vérifier qu’une VM avec budget GPU déclaré reste correctement suivie pendant une forte charge CPU ;
- vérifier que la décision cluster tient compte à la fois du CPU et du budget GPU.

### Préparation

Déclarer le budget GPU si ce n’est pas déjà fait :

```bash
curl -s -X POST http://10.10.0.13:9300/control/vm/9004/gpu \
  -H "Content-Type: application/json" \
  -d '{"vram_budget_mib": 2048}'
```

Vérifier :

```bash
curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool
curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool
```

Dans la VM :

```bash
apt update
apt install -y stress-ng mesa-utils
```

### Charge

Dans la VM :

```bash
stress-ng --cpu 4 --timeout 180s
```

Si un rendu GPU est disponible dans le guest, lancer aussi :

```bash
glxinfo | head
glxgears
```

Si `glxgears` n’est pas disponible ou non pertinent, le test reste valable pour :
- le budget GPU déclaré ;
- le placement/migration GPU-aware ;
- la coexistence charge CPU + budget GPU.

### Observation

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool'
journalctl -u omega-daemon -f
journalctl -u omega-controller -f
```

### Résultat attendu

- le budget GPU reste cohérent ;
- la charge CPU monte normalement ;
- la VM peut devenir candidate à migration si un autre nœud GPU-compatible améliore l’état global du cluster.

### Échec typique / diagnostic immédiat

- la partie GPU n’apparaît pas
  - vérifier `control/gpu/status`
  - vérifier les métadonnées Proxmox de la VM
- seul le CPU bouge
  - c’est acceptable si le guest n’a pas de vrai workload GPU
  - valider au moins le budget GPU et la recommandation de placement

## 6.10 Scénario mixte RAM + GPU

Objectif :
- vérifier qu’une VM avec budget GPU déclaré continue à être correctement gérée quand elle consomme beaucoup de RAM.

### Préparation

```bash
curl -X POST http://10.10.0.13:9300/control/vm/9004/quota \
  -H "Content-Type: application/json" \
  -d '{"max_mem_mib": 2048, "local_budget_mib": 1536}'

curl -s -X POST http://10.10.0.13:9300/control/vm/9004/gpu \
  -H "Content-Type: application/json" \
  -d '{"vram_budget_mib": 2048}'
```

### Charge

Dans la VM :

```bash
stress-ng --vm 1 --vm-bytes 1400M --timeout 180s
```

Si un test GPU est disponible :

```bash
glxgears
```

### Observation

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool'
journalctl -u omega-daemon -f
journalctl -u omega-controller -f
```

### Résultat attendu

- les quotas RAM évoluent correctement ;
- le budget GPU est conservé ;
- si le nœud devient mauvais globalement, la recommandation de migration prend en compte RAM + GPU.

### Échec typique / diagnostic immédiat

- le budget GPU disparaît
  - vérifier `control/gpu/status`
  - vérifier `api/status`
- seule la RAM réagit
  - ce n’est pas bloquant si le guest n’a pas de vrai test GPU

## 6.11 Scénario mixte disque + GPU

Objectif :
- vérifier qu’une VM avec budget GPU peut aussi être priorisée côté I/O disque ;
- vérifier la cohérence des décisions si un nœud GPU devient mauvais en même temps sur l’I/O.

### Préparation

```bash
curl -s -X POST http://10.10.0.13:9300/control/vm/9004/gpu \
  -H "Content-Type: application/json" \
  -d '{"vram_budget_mib": 2048}'
```

Dans la VM :

```bash
apt update
apt install -y fio mesa-utils
```

### Charge

Dans la VM :

```bash
fio --name=randrw --filename=/tmp/fio-gpu.bin --size=512M --bs=4k --rw=randrw --rwmixread=70 --iodepth=32 --runtime=180 --time_based
```

Si un test GPU existe dans le guest :

```bash
glxgears
```

### Observation

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool'
journalctl -u omega-daemon -f
journalctl -u omega-controller -f
```

### Résultat attendu

- `disk_pressure_pct` peut monter ;
- `io.weight` peut être ajusté ;
- le budget GPU reste visible ;
- le controller peut préférer une cible qui améliore à la fois I/O et GPU.

### Échec typique / diagnostic immédiat

- le disque réagit mais pas le GPU
  - vérifier le budget GPU côté control API
  - le test reste valable si aucun rendu GPU réel n’est disponible dans le guest
- la migration ne tient pas compte du GPU
  - vérifier `omega.gpu_vram_mib` et `control/gpu/status`

## 6.12 Scénario mixte CPU + RAM + GPU + disque + migration

Objectif :
- reproduire le cas le plus réaliste : une VM devient lourde sur plusieurs ressources et le système doit décider entre rééquilibrage local et migration.

### Charge

Dans la VM :

```bash
stress-ng --cpu 4 --vm 1 --vm-bytes 1400M --timeout 240s &
fio --name=randrw --filename=/tmp/fio-mix.bin --size=1G --bs=4k --rw=randrw --rwmixread=70 --iodepth=32 --runtime=240 --time_based
```

Si le guest permet un test GPU :

```bash
glxgears
```

### Observation locale

```bash
watch -n 2 'curl -s http://10.10.0.13:9300/control/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/quotas | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/gpu/status | python3 -m json.tool'
journalctl -u omega-daemon -f
```

### Observation cluster

Sur le controller :

```bash
watch -n 2 'curl -s http://10.10.0.11:9200/api/status | python3 -m json.tool'
watch -n 2 'curl -s http://10.10.0.13:9300/control/migrate/recommend | python3 -m json.tool'
journalctl -u omega-controller -f
```

### Résultat attendu

- la VM reste vivante ;
- le daemon essaie d’abord les ajustements locaux sûrs ;
- le budget GPU reste cohérent ;
- si le cluster global est meilleur ailleurs, une migration peut être recommandée ou déclenchée ;
- après migration, les ressources locales source sont nettoyées.

### Échec typique / diagnostic immédiat

- trop de signaux à la fois, diagnostic flou
  - commencer par `control/status`
  - puis `control/vcpu/status`, `control/quotas`, `control/disk/status`, `control/gpu/status`
  - finir par `journalctl -u omega-daemon -n 200 --no-pager`
- la VM migre trop tôt ou pas du tout
  - comparer source et cibles via `api/status`
  - vérifier `control/migrate/recommend`

---

## 7. Catalogue Des Erreurs Réellement Rencontrées

Cette section résume les erreurs réellement rencontrées sur le cluster réel jusqu’au 22 avril 2026.

### 7.1 Réseau/NAT des VMs et nœuds

#### Symptôme
- les VMs ou les nœuds du cluster privé n’avaient pas d’accès Internet ;
- `apt`, `git`, `pip` ou les téléchargements ne fonctionnaient pas.

#### Cause
- réseau privé `10.10.0.0/24` sans sortie ;
- tentative de penser que le bridge privé devait aussi porter la sortie directe.

#### Correction
- garder le réseau cluster privé ;
- ajouter un NAT sortant via l’interface qui porte réellement la route par défaut ;
- ne pas faire de bridge direct sur le Wi-Fi.

Commandes réellement utilisées sur l’hôte Proxmox :

```bash
ip route | grep default
sysctl -w net.ipv4.ip_forward=1
iptables -t nat -A POSTROUTING -s 10.10.0.0/24 -o eno2 -j MASQUERADE
iptables -A FORWARD -s 10.10.0.0/24 -o eno2 -j ACCEPT
iptables -A FORWARD -d 10.10.0.0/24 -m state --state ESTABLISHED,RELATED -i eno2 -j ACCEPT
```

Diagnostics utiles :

```bash
sysctl net.ipv4.ip_forward
iptables -t nat -L -n -v
iptables -L -n -v
```

### 7.1.1 Dépôt `cdrom:` Debian netinst encore actif

#### Erreur

```text
The repository 'cdrom://[...] trixie Release' does not have a Release file
```

#### Cause
- l’install Debian netinst a laissé `deb cdrom:` actif dans les sources APT ;
- `apt update` continue donc d’interroger l’ISO.

#### Correction

```bash
rm -f /etc/apt/sources.list.d/cdrom.list
cat > /etc/apt/sources.list <<'EOF'
deb http://deb.debian.org/debian trixie main contrib non-free-firmware
deb http://deb.debian.org/debian trixie-updates main contrib non-free-firmware
deb http://security.debian.org/debian-security trixie-security main contrib non-free-firmware
EOF
grep -R "^deb cdrom" /etc/apt/sources.list /etc/apt/sources.list.d 2>/dev/null
apt update
```

Point d’attention :
- la commande `cat <<'EOF'` seule n’écrit rien ;
- il faut bien `cat > /etc/apt/sources.list <<'EOF'`.

### 7.1.2 `ping 8.8.8.8` : `Network is unreachable`

#### Symptôme
- `ping 8.8.8.8` échoue avec `Network is unreachable` ;
- `apt update` échoue ensuite sur la résolution DNS ;
- `ip route` est vide dans la VM.

#### Cause
- l’interface invité `ens18` est `DOWN` ;
- aucune IP et aucune route par défaut n’existent encore dans le guest.

#### Vérification

```bash
ip a
ip route
```

Sortie réellement observée :
- `ens18 ... state DOWN`
- aucune adresse IPv4 ;
- aucune route.

#### Correction rapide

```bash
ip link set ens18 up
ip addr add 10.10.0.50/24 dev ens18
ip route add default via 10.10.0.1
echo 'nameserver 8.8.8.8' > /etc/resolv.conf
```

Puis :

```bash
ping 10.10.0.1
ping 8.8.8.8
ping deb.debian.org
```

### 7.1.3 `dhclient: command not found`

#### Cause
- image Debian netinst minimale sans client DHCP installé.

#### Correction
- soit installer ensuite un client DHCP une fois le réseau revenu ;
- soit utiliser directement une configuration statique temporaire comme dans la section précédente.

### 7.2 Token API Proxmox

#### Symptôme
- confusion sur la création du token par nœud.

#### Correction
- le token est créé une seule fois au niveau cluster :

```bash
pveum user token add root@pam omega-controller --privsep 0
```

#### Format correct

```bash
OMEGA_PROXMOX_TOKEN=root@pam!omega-controller=SECRET
```

### 7.3 Compilation/installation controller Python

#### Erreur

```text
BackendUnavailable: Cannot import 'setuptools.backends.legacy'
```

#### Cause
- backend Python incorrect dans `controller/pyproject.toml`.

#### Correction

```toml
build-backend = "setuptools.build_meta"
```

### 7.4 Mauvais nom de nœud Proxmox

#### Erreur

```text
hostname lookup 'node-a' failed
```

#### Cause
- le controller utilisait `node-a/node-b/node-c` au lieu de `pve/pve2/pve3`.

#### Correction
- utiliser le vrai nom de nœud Proxmox remonté par le daemon.

### 7.5 Boucle de repositionnement automatique

#### Symptôme
- même repositionnement relancé toutes les 5 secondes.

#### Correction
- ajout d’un cooldown de repositionnement automatique.

### 7.6 Migration cold d’une VM running

#### Erreur

```text
can't migrate running VM without --online
```

#### Cause
- la VM tournait, mais la migration était partie en `cold`.

#### Correction
- `cold` seulement si l’état est explicitement `stopped` ;
- sinon `live`.

### 7.7 Port de contrôle confondu

#### Symptôme
- interrogation de `9004` ou d’une mauvaise IP ;
- JSON vide ou erreur de parsing.

#### Correction
- l’API contrôle du daemon est sur `9300`, pas `9004`.

Exemple correct :

```bash
curl -s http://10.10.0.11:9300/control/vcpu/status | python3 -m json.tool
```

### 7.8 Option Proxmox ISO incorrecte

#### Erreur

```text
Unknown option: cdrom
```

#### Cause
- tentative d’utiliser `--cdrom` avec `qm set`.

#### Correction

```bash
qm set 9004 --ide2 local:iso/debian-13.4.0-amd64-netinst.iso,media=cdrom
qm set 9004 --boot order=ide2
```

### 7.9 Limite de vCPU sur le nœud

#### Erreur

```text
MAX 4 vcpus allowed per VM on this node
```

#### Signification
- limitation environnementale du nœud ou de la config ;
- le test doit alors être fait avec un plafond `4`.

### 7.10 CPU hotplug : type CPU QMP invalide

#### Erreur

```text
Invalid CPU type, expected cpu type: 'kvm64-x86_64-cpu'
```

#### Cause
- type CPU hardcodé côté hotplug.

#### Correction
- lecture de `query-hotpluggable-cpus` ;
- utilisation du vrai `type`, `socket-id`, `core-id`, `thread-id`.

### 7.11 CPU hotplug : aucun slot hors-ligne

#### Erreur

```text
aucun slot CPU hotpluggable hors-ligne — démarrer la VM avec -smp maxcpus=4
```

#### Cause
- VM démarrée sans slots hotplug réels.

#### Vérification

```bash
qm showcmd 9004 --pretty | grep -- '-smp'
```

Attendu :

```text
-smp '1,sockets=1,cores=4,maxcpus=4'
```

### 7.12 CPU usage à `0.0` malgré une vraie charge

#### Cause
- le daemon ne lisait pas le bon chemin cgroup.

#### Chemin réel observé

```text
/sys/fs/cgroup/qemu.slice/<vmid>.scope
```

#### Correction
- support explicite de `qemu.slice/<vmid>.scope`.

### 7.13 VM enregistrée à `max_vcpus=1` au lieu de `4`

#### Symptôme
- `qm showcmd` indiquait `maxcpus=4`, mais le daemon voyait `max_vcpus=1`.

#### Correction
- récupération robuste du profil CPU depuis la config Proxmox ;
- meilleure lecture des slots hotpluggables.

### 7.14 VMs arrêtées encore visibles dans le scheduler

#### Symptôme
- une VM stoppée restait visible dans `/control/vcpu/status`.

#### Correction
- purge des VMs non running localement du scheduler et des états CPU/I/O.

### 7.15 Migration locale `target=auto`

#### Erreur

```text
qm migrate 9004 auto --online
no such cluster node 'auto'
```

#### Cause
- recommandation locale non résolue exécutée directement.

#### Correction
- le daemon ne doit plus exécuter `qm migrate ... auto` ;
- il bloque toute migration sans cible résolue.

### 7.16 `cargo` absent sur les nœuds Proxmox

#### Erreur

```text
bash: cargo: command not found
```

#### Correction
- builder localement ;
- copier le binaire ;
- l’installer avec `install -m 755`.

### 7.17 `scp` multi-ligne ou écriture directe vers `/usr/local/bin`

#### Symptômes
- commande `scp` coupée sur deux lignes ;
- échec d’écriture directe vers `/usr/local/bin/omega-daemon`.

#### Correction
- faire le `scp` sur une seule ligne ;
- copier d’abord vers `/tmp/omega-daemon` ;
- puis installer localement :

```bash
install -m 755 /tmp/omega-daemon /usr/local/bin/omega-daemon
```

### 7.18 Adresse déjà utilisée

#### Erreur

```text
Address already in use
```

#### Cause
- ancien daemon encore lancé à la main ou service déjà en écoute.

#### Correction

```bash
pkill -f omega-daemon
systemctl restart omega-daemon
```

### 7.19 Extinction d’un nœud avec ses VMs

#### Observation
- un nœud éteint “garde” ses VMs avec lui.

#### Explication
- sans drain préalable ni HA, c’est normal ;
- il faut d’abord vider le nœud.

#### Solution
- utiliser `omega-controller drain-node ...` ;
- attendre que le nœud soit vide ;
- seulement ensuite l’éteindre.

### 7.20 Test RAM trop gros dans une petite VM

#### Erreur

```text
MemoryError
```

#### Cause
- tentative d’allouer `1400 * 1024 * 1024` octets dans une VM configurée avec seulement `512` Mio ;
- ce n’est pas un bug Omega.

#### Correction
- augmenter la RAM de la VM :

```bash
qm stop 9004
qm set 9004 --memory 2048
qm start 9004
```

- ou réduire la charge mémoire :

```bash
stress-ng --vm 1 --vm-bytes 200M --timeout 120s
```

---

## 8. Ce Que Le Système Fait Automatiquement

### CPU
- montée progressive de `min_vcpus` vers `max_vcpus` ;
- descente progressive si la charge retombe ;
- réclamation locale prudente sur des VMs réellement donneuses ;
- partage local temporaire via `cpu.weight` ;
- migration si le cluster global devient meilleur ailleurs ;
- nettoyage CPU si la VM n’est plus running localement.

### RAM
- quotas locaux/distants ;
- paging distant ;
- mise à jour via `virtio-balloon` ;
- nettoyage RAM local si la VM n’est plus running localement.

### GPU
- budget VRAM par VM ;
- capacité nœud lue depuis le backend réel ;
- placement/migration GPU-aware ;
- nettoyage du budget GPU local si la VM n’est plus running localement.

### Disque
- arbitrage I/O local via `io.weight` quand le backend/cgroup le supporte ;
- fallback automatique vers télémétrie + migration quand `io.weight` n’est pas disponible ;
- prise en compte de la pression disque dans le score de migration ;
- aucune suppression des données disque des VMs.

Important :
- le système ne supprime jamais le disque d’une VM ;
- le nettoyage automatique concerne CPU/RAM/GPU/I/O locaux, pas les données disque.

---

## 9. Checklist Rapide Avant Un Test

```bash
pvesm status
qm config 9004
qm showcmd 9004 --pretty | grep -- '-smp'
systemctl status omega-daemon
curl -s http://10.10.0.13:9300/control/status | python3 -m json.tool
curl -s http://10.10.0.13:9300/control/vcpu/status | python3 -m json.tool
curl -s http://10.10.0.13:9300/control/disk/status | python3 -m json.tool
journalctl -u omega-daemon -n 100 --no-pager
```

Si un de ces points est faux, corriger avant de conclure qu’un test “ne marche pas”.

---

## 10. Références Internes

- [retour-experience-cluster-reel.md](./retour-experience-cluster-reel.md)
- [utilisation-physique.md](./utilisation-physique.md)
- [deploiement.md](./deploiement.md)
- [fonctionnement-complet.md](./fonctionnement-complet.md)
