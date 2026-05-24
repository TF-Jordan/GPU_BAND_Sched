# Validation datacenter physique

Ce document sert a valider Omega sur un vrai cluster Proxmox, pas seulement sur une machine de developpement. Il couvre la creation de VMs conformes, les tests d'utilisation reels et les erreurs deja rencontrees pendant les essais.

## 1. Prerequis

- Cluster Proxmox VE avec au moins 2 noeuds, idealement 3 ou plus.
- Stockage partage pour les disques VM, idealement Ceph RBD.
- Reseau inter-noeuds stable entre les ports `9100`, `9200` et `9300`.
- Acces SSH root ou utilisateur admin vers tous les noeuds.
- `qemu-guest-agent` installe et actif dans les VMs de test.
- VMs de test sur bridge Proxmox routable ou avec NAT fonctionnel.
- Pour le paging distant reel: kernel avec `userfaultfd` autorise via capability ou sysctl.
- Pour le scheduler disque: cgroup v2 avec controleur `io` active sous `qemu.slice`.
- Pour GPU: render node `/dev/dri/renderD*` visible sur l'hote et, si test invite, passthrough ou paravirtualisation exposee a la VM.

Commandes de base sur chaque noeud:

```bash
systemctl status omega-daemon
curl -s http://127.0.0.1:9300/control/status | python3 -m json.tool
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool
curl -s http://127.0.0.1:9300/control/disk/status | python3 -m json.tool
curl -s http://127.0.0.1:9300/control/gpu/status | python3 -m json.tool
```

## 2. Creation de VMs conformes

Le point important est de separer le minimum au demarrage du maximum hotpluggable:

```bash
qm showcmd 9001 --pretty | grep -- '-smp'
# Attendu: -smp '1,sockets=1,cores=4,maxcpus=4'
```

Une VM Omega conforme doit verifier ces deux niveaux:

- Config Proxmox: `cores=4`, `sockets=1`, `vcpus=1`, `memory=4096`,
  `balloon=512` ou autre minimum strictement inferieur a `memory`,
  `hotplug=cpu,disk,network`, `agent=enabled=1`, disque `virtio-scsi-*`, reseau
  `virtio`.
- Runtime QEMU: `qm showcmd <vmid> --pretty | grep -- '-smp'` doit afficher
  `1,sockets=1,cores=4,maxcpus=4`. Si `qm config` dit `cores: 4` mais
  `showcmd` affiche `cores=1,maxcpus=1`, la VM n'est pas conforme pour les
  tests CPU: il faut l'arreter puis la redemarrer apres correction.
- L'option `I` reinstalle Omega et les hooks, mais ne recree pas une VM deja
  existante. Si une ancienne VM a ete creee avec `cores=1`, elle gardera
  `maxcpus=1` tant qu'elle n'est pas arretee puis corrigee. Le test 05 sait
  maintenant reparer ce profil automatiquement. Les tests mixtes `M1`, `M2`,
  `M3` et `M4` appliquent la meme normalisation avant de lire `vm_cores`: ils
  stoppent la VM si necessaire, appliquent `cores=<omega_max_vcpus ou 4>`,
  `sockets=1`, `vcpus=1`, `hotplug=cpu,disk,network`, puis redemarrent la VM.
  Sans cette etape, les logs affichent `vCPUs=1 (max=1)` meme si la description
  Omega declare un maximum superieur.

Verification stricte:

```bash
VMID=2370
qm config "$VMID" | grep -E '^(agent|memory|balloon|cores|sockets|vcpus|hotplug|scsihw|net0|description):'
qm showcmd "$VMID" --pretty | grep -- '-smp'
qm pending "$VMID"
```

Le champ `description` est volontairement utilise comme metadata lisible par le
controller:

```text
omega_min_vcpus=1 omega_max_vcpus=4 omega_memory_min_mib=512 omega_memory_max_mib=4096 omega_disk_max_gib=20 omega_gpu_vram_mib=0
```

Ces valeurs doivent rester coherentes avec la config Proxmox. Exemple:
`omega_max_vcpus` doit etre egal a `cores * sockets`, et
`omega_memory_min_mib` doit etre egal au `balloon`.

`--cores x --sockets` ne doit pas depasser le nombre de vCPU autorise par le
noeud Proxmox. Sur un noeud a 6 CPU logiques, utiliser par exemple `--cores 4`
ou `--cores 6`, mais pas `--cores 8`.

Verification rapide sur le noeud controleur:

```bash
nproc
```

Ne pas activer `memory` dans `--hotplug` pour les VMs Omega avec backend memfd.
La RAM progressive du projet passe par `--memory <plafond>` + `--balloon
<initial>` + Omega, pas par le hotplug memoire Proxmox. Le hotplug memoire force
NUMA et peut provoquer:

```text
kvm: Machine memory size does not match the size of the memory backend
```

Creation recommandee: passer par `scripts/omega-lab.sh`, option `[c]` pour
décrire le cluster, puis `[p]` pour créer les VMs avec le profil Omega. Cela
évite de figer les VMIDs, noms de nœuds, storage ou bridge dans les scripts.

Commande équivalente si le script est déjà présent sur le nœud Proxmox:

```bash
cd /opt/omega-remote-paging

./scripts/create-omega-vm.sh \
  --vmids 9001,9002,9003 \
  --name omega-test \
  --storage ceph-vms \
  --bridge vmbr0 \
  --image /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2 \
  --sshkey /root/.ssh/id_rsa.pub \
  --memory 2048 \
  --balloon 512 \
  --cores 4 \
  --vcpus 1 \
  --disk-max-gib 20 \
  --gpu-vram-mib 0 \
  --start
```

Le script refuse maintenant une creation non conforme. Apres chaque VM, il relit
`qm config` et `qm showcmd`; si `maxcpus` ne correspond pas a `cores*sockets`,
la creation s'arrete au lieu de laisser une VM inutilisable pour les tests
Omega.

Par defaut, les VMs de test ne doivent pas avoir `hostpci0`. Le GPU Omega est
teste via les budgets `/control/vm/<vmid>/gpu` et le multiplexeur/placement
Omega; un passthrough PCI statique reste lie au materiel du noeud courant. Si
une VM migre vers un noeud qui n'a pas ce PCI, QEMU refuse de demarrer avec:

```text
no PCI device found for '0000:00:02.0'
```

Les tests non-GPU nettoient automatiquement un `hostpci*` invalide avant de
demarrer une VM. Pour desactiver ce garde-fou:

```bash
export OMEGA_TEST_REMOVE_INVALID_HOSTPCI=0
```

Attention au hookscript: l'installation Omega enregistre automatiquement
`local:snippets/omega-hook.pl` uniquement sur les VMs visibles par `qm list` du
noeud courant. C'est donc local au noeud Proxmox, pas global au cluster. Une VM
deja migree sur un autre noeud ne sera pas listee quand l'installation tourne
sur le noeud courant. Pour couvrir tout le cluster, executer l'installation sur
chaque noeud, ou passer explicitement les VMs locales au noeud:

```bash
OMEGA_VMIDS=9001,9002 ./scripts/omega-proxmox-install.sh
```

Avec l'action `[I]` de `omega-lab.sh`, le deploiement passe automatiquement
`OMEGA_TEST_VMIDS` a `OMEGA_INSTALL_VMIDS` si aucune liste dediee n'est definie.
Concretement, les binaires, le wrapper, le hookscript et le service
`omega-hookscript-watcher` sont installes sur tous les noeuds de `OMEGA_NODES`,
puis chaque noeud essaie d'enregistrer le hookscript sur les VMIDs cibles qu'il
voit localement. Les VMIDs non locaux peuvent afficher `qm set echoue ou VM non
locale`; c'est normal si la VM est actuellement hebergee par un autre noeud.

`[I]` ecrit aussi `/etc/omega/cluster.env` sur chaque noeud avec une identite
compatible Proxmox:

```text
OMEGA_NODE_ID=<hostname Proxmox>
OMEGA_NODE_ADDR=<IP du noeud>
OMEGA_PEERS=<autres noeuds:9200>
```

Cette partie est critique: si `/status` ou `/control/status` expose
`node_id=omega-node`, Omega peut choisir `omega-node` comme cible et Proxmox
refusera avec `no such cluster node 'omega-node'`. Apres `[I]`, verifier:

```bash
for n in NODE1 NODE2 NODE3; do
  ssh root@$n 'hostname; cat /etc/omega/cluster.env; curl -s http://127.0.0.1:9200/status'
done
```

Les VMs deja configurees sont maintenant affichees comme `hookscript deja
configure` au lieu d'etre seulement comptees silencieusement.

La section 6 du lab est volontairement adaptee aux clusters physiques
heterogenes:

- `30` verifie la conformite des VMs et tente maintenant la meme reparation
  automatique que les tests vCPU si une ancienne VM est restee en
  `maxcpus=1`.
- `24` ne suppose plus que les binaires Omega sont dans `/usr/local/bin`; il
  accepte aussi le deploiement dans `/opt/omega-remote-paging/bin` ou
  `/tmp/omega-tests-bins`.
- `26` ne force plus le pool `omega-pages` quand `OMEGA_CEPH_POOL` n'est pas
  defini. Il detecte les pools Ceph existants, par exemple `ceph-vm`,
  `ceph-vms` ou `VM-Storage`.
- `29` normalise aussi le profil vCPU avant le soak, pour eviter de tester une
  VM bloquee a `maxcpus=1`.

Depuis une machine de developpement, le chemin recommandé reste
`scripts/omega-lab.sh`: option `[c]` pour la configuration, `[I]` pour
installer, `[p]` pour provisionner, puis les sections de tests. Le wrapper
distant ci-dessous est l'équivalent non interactif: il copie
`create-omega-vm.sh` sur le noeud controleur, copie le `.qcow2` si besoin, puis
lance la creation Proxmox:

```bash
./scripts/provision-omega-vms-remote.sh \
  --controller NODE1 \
  --user root \
  --vmids 9001,9002,9003 \
  --storage ceph-vm \
  --bridge vmbr0 \
  --image-local /chemin/local/debian-12-generic-amd64.qcow2 \
  --image-remote /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2 \
  --sshkey-remote /root/.ssh/id_rsa.pub \
  --memory 2048 \
  --balloon 512 \
  --cores 4 \
  --vcpus 1
```

Si l'image existe deja sur le noeud controleur, `--image-local` peut etre omis:

```bash
./scripts/provision-omega-vms-remote.sh \
  --controller NODE1 \
  --vmids 9001,9002,9003 \
  --storage ceph-vm \
  --image-remote /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2
```

Commande equivalente sans script, pour une VM:

```bash
VMID=9001
STORAGE=ceph-vms
BRIDGE=vmbr0
IMAGE=/var/lib/vz/template/iso/debian-12-generic-amd64.qcow2

qm create "$VMID" \
  --name "omega-test-$VMID" \
  --ostype l26 \
  --machine q35 \
  --agent enabled=1 \
  --cpu kvm64 \
  --sockets 1 \
  --cores 4 \
  --vcpus 1 \
  --memory 2048 \
  --balloon 512 \
  --hotplug cpu,disk,network \
  --scsihw virtio-scsi-single \
  --net0 "virtio,bridge=${BRIDGE},firewall=0" \
  --serial0 socket \
  --vga serial0 \
  --tags omega,omega-test \
  --description "omega_min_vcpus=1 omega_max_vcpus=4 omega_memory_min_mib=512 omega_memory_max_mib=2048 omega_disk_max_gib=20 omega_gpu_vram_mib=0"

qm importdisk "$VMID" "$IMAGE" "$STORAGE"
qm set "$VMID" \
  --scsi0 "${STORAGE}:vm-${VMID}-disk-0,discard=on,iothread=1,ssd=1" \
  --boot order=scsi0 \
  --ide2 "${STORAGE}:cloudinit" \
  --ciuser root \
  --sshkeys /root/.ssh/id_rsa.pub \
  --ipconfig0 ip=dhcp \
  --nameserver 8.8.8.8
qm start "$VMID"
```

Correction d'une VM deja creee mais non conforme:

```bash
VMID=2370

qm stop "$VMID"
qm set "$VMID" \
  --agent enabled=1 \
  --cores 4 \
  --sockets 1 \
  --vcpus 1 \
  --memory 4096 \
  --balloon 512 \
  --hotplug cpu,disk,network \
  --scsihw virtio-scsi-single
qm set "$VMID" --description "omega_min_vcpus=1 omega_max_vcpus=4 omega_memory_min_mib=512 omega_memory_max_mib=4096 omega_disk_max_gib=20 omega_gpu_vram_mib=0"
qm start "$VMID"
qm showcmd "$VMID" --pretty | grep -- '-smp'
```

Si la ligne reste `maxcpus=1`, ne continuez pas les tests CPU: le probleme est
encore dans le profil Proxmox de la VM ou dans un etat pending non applique.
Verifier `qm pending <vmid>`, puis refaire un vrai `qm stop` / `qm start`.

Pour une VM GPU, garder la meme base et changer uniquement le budget declare:

```bash
qm set 9001 --description "omega_min_vcpus=1 omega_max_vcpus=4 omega_memory_min_mib=512 omega_memory_max_mib=2048 omega_disk_max_gib=20 omega_gpu_vram_mib=2048"
qm set 9001 --tags omega,omega-test,omega-gpu
```

Si vous faites du passthrough manuel pour valider le rendu invite:

```bash
qm set 9001 --hostpci0 0000:03:00,pcie=1
```

## 3. Reseau propre pour les VMs

Ne pas melanger obligatoirement les VMs de test avec le LAN du cluster. Une
configuration propre est:

```text
LAN cluster : 192.168.123.0/24 sur vmbr0
Reseau VMs  : 10.50.0.0/24 sur vmbr1
Gateway VM  : 10.50.0.1 sur chaque noeud Proxmox
DHCP VM     : dnsmasq sur vmbr1
Internet VM : NAT vmbr1 -> vmbr0
```

Sur chaque noeud Proxmox, creer le bridge interne:

```bash
cat >>/etc/network/interfaces <<'EOF'

auto vmbr1
iface vmbr1 inet static
    address 10.50.0.1/24
    bridge-ports none
    bridge-stp off
    bridge-fd 0
EOF

ifreload -a
ip -br addr show vmbr1
```

Activer le routage IPv4:

```bash
cat >/etc/sysctl.d/99-omega-vm-net.conf <<'EOF'
net.ipv4.ip_forward=1
EOF

sysctl --system
```

Identifier l'interface de sortie Internet:

```bash
ip route | grep default
# Exemple observe: default via 192.168.123.1 dev vmbr0 proto kernel onlink
```

Si la sortie est `vmbr0`, ajouter le NAT sur chaque noeud:

```bash
iptables -t nat -A POSTROUTING -s 10.50.0.0/24 -o vmbr0 -j MASQUERADE
iptables -A FORWARD -i vmbr1 -o vmbr0 -j ACCEPT
iptables -A FORWARD -i vmbr0 -o vmbr1 -m state --state ESTABLISHED,RELATED -j ACCEPT
```

Rendre le NAT persistant:

```bash
apt install -y iptables-persistent
netfilter-persistent save
```

Installer le DHCP VM sur `vmbr1`:

```bash
apt install -y dnsmasq

cat >/etc/dnsmasq.d/omega-vmbr1.conf <<'EOF'
interface=vmbr1
bind-interfaces
dhcp-range=10.50.0.100,10.50.0.250,255.255.255.0,12h
dhcp-option=3,10.50.0.1
dhcp-option=6,8.8.8.8,1.1.1.1
EOF

systemctl restart dnsmasq
systemctl enable dnsmasq
systemctl status dnsmasq --no-pager
```

Si `apt install` echoue a cause d'un paquet DKMS Nvidia casse, mais que
`dnsmasq` est deja installe, continuer la configuration `dnsmasq`. Le paquet
casse est une dette separee. Pour debloquer `apt` plus tard:

```bash
apt-mark hold nvidia-open-kernel-dkms
dpkg --configure -a
```

ou, si le module Nvidia n'est pas requis sur ce noeud:

```bash
apt remove --purge nvidia-open-kernel-dkms
dpkg --configure -a
```

Bascule des VMs existantes vers `vmbr1`:

```bash
for vmid in 2370 2371 2372 2373 2374 2375; do
  qm set "$vmid" --net0 "virtio,bridge=vmbr1,firewall=0"
  qm set "$vmid" --ipconfig0 ip=dhcp
  qm cloudinit update "$vmid"
done

for vmid in 2370 2371 2372 2373 2374 2375; do
  qm reset "$vmid"
done
```

`qm reboot` peut echouer si `qemu-guest-agent` n'est pas encore installe dans la
VM. Pour des VMs de test, `qm reset` est acceptable.

Trouver les IPs DHCP des VMs:

```bash
for vmid in 2370 2371 2372 2373 2374 2375; do
  mac=$(qm config "$vmid" | sed -n 's/^net0: virtio=\([^,]*\).*/\1/p')
  echo "=== VM $vmid mac=$mac ==="
  ip neigh | grep -i "$mac" || true
done
```

Verifier l'acces Internet depuis une VM:

```bash
ssh root@10.50.0.X 'ip route; ping -c2 8.8.8.8; ping -c2 deb.debian.org'
```

Installer `qemu-guest-agent` dans les VMs:

```bash
apt update
apt install -y qemu-guest-agent
systemctl enable --now qemu-guest-agent
```

Depuis l'hote Proxmox:

```bash
qm agent 2370 ping
qm agent 2370 network-get-interfaces
```

Pour installer l'agent sur plusieurs VMs apres recuperation des IPs:

```bash
for ip in 10.50.0.100 10.50.0.101 10.50.0.102 10.50.0.103 10.50.0.104 10.50.0.105; do
  ssh root@"$ip" 'apt update && apt install -y qemu-guest-agent && systemctl enable --now qemu-guest-agent'
done
```

Les tests physiques qui ont besoin de charge invitee (`05`, `08`, `11`, `12`,
`13`, `22`) tentent maintenant d'installer uniquement les paquets manquants dans
la VM. Si `stress-ng` est deja installe mais `qemu-guest-agent` manque, le test
ne relance pas inutilement l'installation de `stress-ng`. Si l'agent invite ne
repond pas, les tests utilisent les fallbacks possibles ou affichent un warning
explicite.

Les tests qui chargent directement les hotes Proxmox (`M4`, `08`, `12`, `15`,
`19`) tentent aussi d'installer `stress-ng` cote hote via `apt-get` quand il
manque. Si `apt` est bloque par un paquet DKMS casse, corriger d'abord `dpkg
--configure -a` ou installer `stress-ng` manuellement sur chaque noeud. Les
tests mixtes recents ne doivent plus echouer uniquement parce que `stress-ng`
est absent: `M2` utilise un fallback Python pour la pression CPU hote et `M4`
ignore seulement les noeuds ou la charge hote ne peut pas etre lancee.

## 4. Configuration du lab physique

Creer ou modifier `scripts/cluster.conf`:

```bash
OMEGA_NODES="10.10.0.11,10.10.0.12,10.10.0.13"
OMEGA_CONTROLLER="10.10.0.11"
OMEGA_TEST_VMIDS="9001,9002,9003"
OMEGA_TEST_VMID="9001"
OMEGA_PROVISION_VMIDS="9001,9002,9003"
DEPLOY_USER="root"
SSH_KEY="/root/.ssh/omega_ed25519"
STORE_PORT=9100
STATUS_PORT=9200
```

`OMEGA_TEST_VMIDS` sert aux tests courants. Garder une petite liste lisible,
par exemple `9001,9002,9003`.

`OMEGA_PROVISION_VMIDS` sert a l'action `[p]` qui cree les VMs. Il peut etre
plus large, par exemple `9001-9150` dans le menu ou `9001,9002,9003` dans le
fichier apres expansion.

Validation SSH:

```bash
for n in 10.10.0.11 10.10.0.12 10.10.0.13; do
  ssh -i /root/.ssh/omega_ed25519 root@$n 'hostname && systemctl is-active omega-daemon'
done
```

## 5. Tests d'utilisation reels

Validation complete non destructive:

```bash
./scripts/omega-lab.sh --gpu --ceph --auto
```

Workflow interactif recommande sur vrai cluster:

```bash
./scripts/omega-lab.sh --gpu --ceph
```

Dans le menu:

```text
[c] configurer les noeuds, le controleur, les VMIDs de test, les VMIDs a creer, SSH et le profil VM
[p] creer les VMs physiques Omega sur Proxmox
[I] installer/deployer Omega sur les noeuds
[6] lancer les tests physiques reels
[A] lancer toutes les sections
```

Pour les tests normaux, mettre par exemple:

```text
VMIDs de test       : 9001,9002,9003
VMIDs a provisionner: 9001,9002,9003
```

Pour preparer un scale 150 sans utiliser 150 VMs dans tous les tests:

```text
VMIDs de test       : 9001,9002,9003
VMIDs a provisionner: 9001-9150
```

Le choix `[p]` ne lance pas de mock: il appelle Proxmox via SSH, copie le
`.qcow2` si necessaire, puis execute `qm create`, `qm importdisk`, `qm set` et
`qm start` sur le noeud controleur.

Le profil cree par `[p]` vient des champs `OMEGA_VM_*` sauvegardes par `[c]`.
Pour un test CPU/RAM standard, utiliser:

```text
OMEGA_VM_MEMORY=4096
OMEGA_VM_BALLOON=512
OMEGA_VM_CORES=4
OMEGA_VM_SOCKETS=1
OMEGA_VM_VCPUS=1
```

La VM creee doit ensuite afficher:

```bash
qm showcmd <vmid> --pretty | grep -- '-smp'
# -smp '1,sockets=1,cores=4,maxcpus=4'
```

Si une VM existe deja, `[p]` ne la transforme pas: il refuse la creation pour
eviter d'ecraser une VM existante. Corriger ou detruire manuellement l'ancienne
VM avant de reprovisionner.

Avant chaque test, `omega-lab.sh` affiche maintenant un court texte qui explique:

- ce que le test va verifier;
- si le test agit sur des composants isoles ou sur le cluster physique;
- les causes probables d'echec a regarder en premier.

### Timeouts sur cluster lent ou degrade

L'action `[c]` demande aussi les timeouts des tests reels. Ces valeurs servent a
eviter les faux echecs quand le cluster est lent, par exemple pendant une
recovery Ceph ou avec un lien reseau limite a 100 Mbps. Elles ne rendent pas les
mesures de performance valides: elles donnent seulement plus de temps aux tests.

Profil normal:

```text
Multiplicateur global timeouts : 1
Timeout migration secondes     : 120
Timeout Ceph secondes          : 60
Timeout disque/I/O secondes    : 60
Timeout/soak scale secondes    : 1800
```

Profil lent recommande:

```text
Multiplicateur global timeouts : 3
Timeout migration secondes     : 900
Timeout Ceph secondes          : 900
Timeout disque/I/O secondes    : 600
Timeout/soak scale secondes    : 1200
```

Ces champs alimentent `OMEGA_TEST_TIMEOUT_MULTIPLIER`,
`OMEGA_MIGRATION_TIMEOUT_SECS`, `OMEGA_CEPH_TIMEOUT_SECS`,
`OMEGA_DISK_TIMEOUT_SECS` et `OMEGA_SCALE_TIMEOUT_SECS` dans
`scripts/cluster.conf`. Les timeouts dedies sont utilises directement; le
multiplicateur global sert aux tests qui n'ont pas encore de timeout dedie.

Mode automatique avec creation des VMs avant tests:

```bash
./scripts/omega-lab.sh --gpu --ceph --provision --auto
```

Validation longue duree:

```bash
OMEGA_SOAK_SECS=7200 ./scripts/omega-lab.sh --gpu --ceph --long --auto
```

Validation avec panne reseau controlee, uniquement en fenetre de maintenance:

```bash
./scripts/omega-lab.sh --gpu --ceph --long --destructive --auto
```

Validation scalabilite 500 VMs:

```bash
# Exemple: VMs 9001 a 9500 deja creees avec scripts/create-omega-vm.sh
export OMEGA_SCALE_VMIDS="$(seq -s, 9001 9500)"
export OMEGA_SCALE_TARGET=500
export OMEGA_SCALE_BATCH_SIZE=20
export OMEGA_SCALE_STEPS=50,100,150,200,250,300,350,400,450,500
export OMEGA_SCALE_SOAK_SECS=3600

./scripts/omega-lab.sh --gpu --ceph --long --scale --auto
```

Nettoyage automatique apres test scale:

```bash
# Arreter les VMs demarrees par le test, meme si le test echoue.
export OMEGA_SCALE_STOP_AFTER=1
export OMEGA_SCALE_CLEANUP_SCOPE=started

# Detruire les VMs de test apres le run. A utiliser uniquement pour des VMs jetables.
export OMEGA_SCALE_DESTROY_AFTER=1
export OMEGA_SCALE_CLEANUP_SCOPE=all
```

Exemple prudent pour monter progressivement 50 puis 100 puis 150 VMs et les
arreter a la fin:

```bash
OMEGA_SCALE_VMIDS="$(seq -s, 9001 9150)" \
OMEGA_SCALE_TARGET=150 \
OMEGA_SCALE_STEPS=50,100,150 \
OMEGA_SCALE_BATCH_SIZE=25 \
OMEGA_SCALE_SOAK_SECS=600 \
OMEGA_SCALE_STOP_AFTER=1 \
OMEGA_SCALE_CLEANUP_SCOPE=started \
./scripts/omega-lab.sh --ceph --scale
```

Runner direct sans menu:

```bash
OMEGA_NODES=10.10.0.11,10.10.0.12,10.10.0.13 \
OMEGA_CONTROLLER=10.10.0.11 \
OMEGA_TEST_VMIDS=9001,9002,9003 \
DEPLOY_USER=root \
./scripts/tests/run-cluster.sh 9001 --gpu --ceph --long
```

Runner direct pour 500 VMs:

```bash
OMEGA_NODES=10.10.0.11,10.10.0.12,10.10.0.13 \
OMEGA_CONTROLLER=10.10.0.11 \
OMEGA_TEST_VMIDS=9001,9002,9003 \
OMEGA_SCALE_VMIDS="$(seq -s, 9001 9500)" \
OMEGA_SCALE_TARGET=500 \
OMEGA_SCALE_BATCH_SIZE=20 \
OMEGA_SCALE_SOAK_SECS=3600 \
DEPLOY_USER=root \
./scripts/tests/run-cluster.sh 9001 --gpu --ceph --long --scale
```

Test de conformite VM seul:

```bash
./scripts/tests/30-vm-conformity.sh 9001,9002,9003
```

## 6. Catalogue des tests physiques

| Test | Objectif |
|------|----------|
| `24-install-doctor` | Valide installation, wrapper QEMU, hookscript, services et diagnostic launcher. |
| `25-vm-network` | Valide agent invite, interface, route par defaut, ping IP, DNS et `apt update`. |
| `26-ceph-real` | Valide Ceph reel, librados, pool et exposition `ceph_enabled`. |
| `27-gpu-real-render` | Valide inventaire GPU hote, API GPU Omega et rendu minimal invite si possible. |
| `32-gpu-proxy` | Valide le proxy GPU applicatif: budget VM, soumission de job, file d'attente et résultat. |
| `28-network-partition` | Simule une partition reseau controlee et verifie la resilience. Destructif. |
| `29-long-run-soak` | Charge longue CPU/RAM avec surveillance daemon et APIs. |
| `30-vm-conformity` | Verifie que les VMs sont compatibles Omega avant de lancer les tests lourds. |
| `31-scale-vms` | Demarre et surveille un grand nombre de VMs physiques, par defaut cible 500. |
| `M1` a `M7` | Scenarios mixtes RAM, CPU, GPU, migrations, rafales de demarrage et drain de noeud. |

Le test `22-balloon-thinprov` ne se limite plus a verifier que le balloon
grandit. Il valide maintenant la chaine complete:

- remettre la VM au minimum `balloon`;
- lancer une forte charge memoire dans l'invite;
- observer la croissance de `info balloon`;
- activer eviction + migration dans l'agent de test;
- echouer si la RAM visible QEMU (`info balloon actual`) grandit mais qu'aucune
  recherche ou migration Omega n'apparait.

Cela permet de detecter le cas dangereux ou la VM grossit localement mais ne
cherche jamais un noeud plus sain alors que la pression RAM continue.

## 6.1 Validation 500 VMs

Le test `31-scale-vms` ne prouve la scalabilite que si les 500 VMs existent deja et sont conformes. Il ne remplace pas le sizing materiel.

Preparation:

```bash
# Creer progressivement les VMs, par exemple par tranches de 50
for start in 9001 9051 9101 9151 9201 9251 9301 9351 9401 9451; do
  end=$((start + 49))
  vmids="$(seq -s, "$start" "$end")"
  ./scripts/create-omega-vm.sh \
    --vmids "$vmids" \
    --storage ceph-vms \
    --bridge vmbr0 \
    --image /var/lib/vz/template/iso/debian-12-generic-amd64.qcow2 \
    --sshkey /root/.ssh/id_rsa.pub \
    --memory 2048 \
    --balloon 512 \
    --cores 4 \
    --vcpus 1
done
```

Validation avant demarrage massif:

```bash
./scripts/tests/30-vm-conformity.sh "$(seq -s, 9001 9500)"
```

Demarrage et surveillance par lots:

```bash
OMEGA_SCALE_VMIDS="$(seq -s, 9001 9500)" \
OMEGA_SCALE_TARGET=500 \
OMEGA_SCALE_BATCH_SIZE=20 \
OMEGA_SCALE_SOAK_SECS=3600 \
./scripts/tests/31-scale-vms.sh
```

Variables utiles:

| Variable | Defaut | Role |
|----------|--------|------|
| `OMEGA_PROVISION_VMIDS` | `OMEGA_TEST_VMIDS` | Liste des VMs creees par `[p]`. Accepte les plages dans le menu. |
| `OMEGA_SCALE_VMIDS` | `OMEGA_PROVISION_VMIDS` | Liste exacte des VMs a tester en scale. |
| `OMEGA_SCALE_TARGET` | `500` | Nombre de VMs attendues. |
| `OMEGA_SCALE_BATCH_SIZE` | `20` | Nombre de VMs demarrees par vague. |
| `OMEGA_SCALE_STEPS` | `50,100,...target` | Paliers progressifs a valider. |
| `OMEGA_SCALE_SOAK_SECS` | `1800` | Duree de surveillance apres demarrage. |
| `OMEGA_SCALE_START` | `1` | Autorise le test a demarrer les VMs non running. |
| `OMEGA_SCALE_STOP_AFTER` | `0` | Arrete les VMs a la fin si mis a `1`. |
| `OMEGA_SCALE_DESTROY_AFTER` | `0` | Detruit les VMs a la fin si mis a `1`. |
| `OMEGA_SCALE_CLEANUP_SCOPE` | `started` | Nettoie seulement les VMs demarrees, ou `all`. |
| `OMEGA_SCALE_REQUIRE_TARGET` | `1` | Echoue si moins de VMs que la cible existent. |

Le test echoue si:

- moins de VMs que `OMEGA_SCALE_TARGET` existent;
- une VM n'est pas conforme a `vcpus/maxcpus/hotplug`;
- une VM ne demarre pas;
- un `omega-daemon` tombe;
- `/control/status` ou `/control/vcpu/status` devient inaccessible;
- une partie des VMs n'est plus `running` pendant la fenetre de soak.

## 7. Criteres d'acceptation production

Le projet est acceptable pour un essai physique encadre seulement si:

- `cargo test --workspace` passe.
- `bash -n scripts/**/*.sh` ne remonte pas d'erreur de syntaxe.
- `./scripts/tests/30-vm-conformity.sh` passe sur toutes les VMs de test.
- `./scripts/omega-lab.sh --gpu --ceph --auto` passe, avec `26`/`27` skip uniquement si Ceph/GPU ne sont pas dans le perimetre.
- Le test `29` passe sur au moins 2 heures pour un essai pre-production.
- Le test `31` passe si l'objectif est de supporter 500 VMs.
- Aucun log ne boucle sur `qm migrate ... auto`, `Device 'cpu-X' not found`, `aucun slot CPU hotpluggable`, `userfaultfd Operation not permitted` ou `io.weight non disponible` si ces fonctions sont exigees.

## 8. Depannage des erreurs deja rencontrees

### CPU hotplug: type CPU invalide

Symptome:

```text
Invalid CPU type, expected cpu type: 'kvm64-x86_64-cpu'
```

Cause: le daemon ajoutait un type CPU different du modele QEMU reel.

Verification:

```bash
qm monitor 9001
info hotpluggable-cpus
```

Correction: utiliser le type publie par QMP, pas une constante codee en dur.

### Aucun slot CPU hotpluggable

Symptome:

```text
aucun slot CPU hotpluggable hors-ligne — demarrer la VM avec -smp maxcpus=4
```

Cause: la VM a ete demarree avec tous les cores en ligne ou sans `maxcpus`.

Correction:

```bash
qm stop 9001
qm set 9001 --hotplug cpu,disk,network
qm set 9001 --cores 4 --sockets 1 --vcpus 1
qm start 9001
qm showcmd 9001 --pretty | grep -- '-smp'
```

### `current_vcpus` bloque a 1

Cause la plus courante: le controller lit une config Proxmox incomplete ou la VM n'a pas de metadata `omega_max_vcpus`.

Correction:

```bash
qm set 9001 --description "omega_min_vcpus=1 omega_max_vcpus=4 omega_memory_min_mib=512 omega_memory_max_mib=2048 omega_disk_max_gib=20 omega_gpu_vram_mib=0"
curl -s http://127.0.0.1:9300/control/vcpu/status | python3 -m json.tool
```

### Hot-unplug CPU: device introuvable

Symptome:

```text
Device 'cpu-3' not found
```

Cause: l'identifiant QMP du CPU retire ne correspond pas au device reel.

Correction attendue cote code: retirer via les chemins QMP reels publies par `query-hotpluggable-cpus`/`query-cpus-fast`, puis rollback scheduler si QMP refuse.

### `io.weight` absent

Symptome:

```text
io.weight non disponible dans le cgroup VM
```

Verification:

```bash
cat /sys/fs/cgroup/qemu.slice/cgroup.controllers
cat /sys/fs/cgroup/qemu.slice/cgroup.subtree_control
ls -l /sys/fs/cgroup/qemu.slice/9001.scope/io.weight
```

Correction temporaire:

```bash
echo +io > /sys/fs/cgroup/qemu.slice/cgroup.subtree_control
systemctl restart omega-daemon
```

Si systemd retire `+io`, configurer le slice via drop-in systemd et redemarrer proprement le noeud en fenetre de maintenance.

### Ceph `OSD down`, PG degraded ou undersized

Symptomes:

```text
HEALTH_WARN
1 osds down
Degraded data redundancy
active+undersized+degraded
```

Diagnostic:

```bash
ceph -s
ceph health detail
ceph osd tree
ceph osd stat
```

Exemple rencontre:

```text
osd.0 down reweight 0
3 osds: 2 up, 2 in
```

Sur le noeud qui porte l'OSD:

```bash
systemctl status ceph-osd@0 --no-pager
journalctl -u ceph-osd@0 -n 120 --no-pager
ceph-volume lvm list
lsblk -f
df -h
```

Si le disque et le keyring existent mais que le service est arrete:

```bash
systemctl reset-failed ceph-osd@0
systemctl restart ceph-osd@0
sleep 10
systemctl status ceph-osd@0 -l --no-pager
ceph osd tree
ceph -s
```

Si l'OSD repasse `up` mais reste `reweight 0`, le remettre en service:

```bash
ceph osd reweight 0 1
watch -n5 'ceph -s; ceph osd tree'
```

Attendre la fin de la recovery avant les tests disque/migration:

```text
pgs: active+clean
degraded: 0
misplaced: 0
remapped: 0
```

Pendant la recovery, eviter les tests `8`, `12`, `15`, `17`, `23`, `26`, `29`
et `31`. Les tests `30`, `25`, `5`, `22` peuvent continuer.

### Ceph `BlueStore slow operations`

Symptome:

```text
HEALTH_WARN
1 OSD(s) experiencing slow operations in BlueStore
```

Identifier l'OSD:

```bash
ceph health detail
```

Sur le noeud concerne:

```bash
systemctl status ceph-osd@2 --no-pager
journalctl -u ceph-osd@2 -n 120 --no-pager
dmesg -T | tail -80
```

Dans le cas rencontre, le disque n'avait pas d'erreur evidente, mais le kernel
montrait:

```text
r8169 ... eth0: Link is Down
r8169 ... eth0: Link is Up - 100Mbps/Full
```

Verification:

```bash
ethtool eth0 | grep -E 'Speed|Duplex|Link detected'
cat /sys/class/net/eth0/speed
cat /sys/class/net/eth0/duplex
```

Si le lien est a `100Mb/s`, les tests Ceph/disque/migration peuvent passer
fonctionnellement mais les performances et les timeouts ne sont pas fiables. La
correction reelle est physique: cable, port switch, negociation, carte reseau.

Si le materiel ne peut pas etre corrige tout de suite, utiliser les timeouts
lents dans `[c]`:

```text
Multiplicateur global timeouts : 3
Timeout migration secondes     : 900
Timeout Ceph secondes          : 900
Timeout disque/I/O secondes    : 600
Timeout/soak scale secondes    : 1200
```

### `userfaultfd Operation not permitted`

Symptome:

```text
SYS_userfaultfd echoue : Operation not permitted
```

Correction de test:

```bash
sysctl vm.unprivileged_userfaultfd=1
```

Correction production preferee: lancer le composant qui ouvre `userfaultfd` avec les capabilities necessaires, au minimum `CAP_SYS_PTRACE`, sans ouvrir globalement le sysctl si la politique securite l'interdit.

### VM sans reseau ou `ping 8.8.8.8` impossible

Dans la VM:

```bash
ip a
ip route
```

Sur l'hote Proxmox/NAT:

```bash
iptables -t nat -A POSTROUTING -s 10.10.0.0/24 -o wlo1 -j MASQUERADE
iptables -A FORWARD -s 10.10.0.0/24 -o wlo1 -j ACCEPT
iptables -A FORWARD -d 10.10.0.0/24 -m state --state ESTABLISHED,RELATED -i wlo1 -j ACCEPT
```

Adapter `wlo1` a l'interface de sortie reelle. La faute `MASQUE RADE` vient d'un espace dans `MASQUERADE`.

Si `ip neigh` ne montre que des adresses `fe80::` pour les MAC des VMs, les VMs
n'ont pas recu d'IPv4 DHCP:

```bash
for vmid in 2370 2371 2372 2373 2374 2375; do
  mac=$(qm config "$vmid" | sed -n 's/^net0: virtio=\([^,]*\).*/\1/p')
  echo "=== VM $vmid mac=$mac ==="
  ip neigh | grep -i "$mac" || true
done
```

Verifier cloud-init:

```bash
qm config 2370 | grep -E 'ipconfig0|ide2|ciuser|sshkeys|net0'
qm cloudinit dump 2370 network
qm cloudinit dump 2370 user
```

Si cloud-init indique bien `dhcp4` mais que les VMs n'ont pas d'IPv4, le bridge
n'a pas de DHCP accessible. Utiliser le reseau `vmbr1` + `dnsmasq` decrit plus
haut, ou attribuer des IPs statiques:

```bash
for i in 0 1 2 3 4 5; do
  vmid=$((2370+i))
  ip="192.168.123.$((170+i))"
  qm set "$vmid" --ipconfig0 "ip=${ip}/24,gw=192.168.123.1"
  qm cloudinit update "$vmid"
  qm reset "$vmid"
done
```

Si `qm reboot` echoue avec:

```text
QEMU Guest Agent is not running
```

utiliser `qm reset` pour des VMs de test, puis installer l'agent invite une fois
l'IP connue:

```bash
apt update
apt install -y qemu-guest-agent
systemctl enable --now qemu-guest-agent
```

### `apt update` bloque sur cdrom

Dans la VM Debian:

```bash
rm -f /etc/apt/sources.list.d/cdrom.list
cat >/etc/apt/sources.list <<'EOF'
deb http://deb.debian.org/debian trixie main contrib non-free-firmware
deb http://security.debian.org/debian-security trixie-security main contrib non-free-firmware
deb http://deb.debian.org/debian trixie-updates main contrib non-free-firmware
EOF
apt update
```

Si DNS echoue, regler d'abord le reseau VM.

### Migration vers `auto`

Symptome:

```text
qm migrate 9001 auto --online
no such cluster node 'auto'
```

Cause: cible de migration non resolue.

Correction attendue: le controller doit convertir `auto` en nom de noeud Proxmox reel avant d'appeler `qm migrate`.

### M4 bloque sur installation `stress-ng`

Symptome:

```text
E: Unable to correct problems, you have held broken packages.
libegl-mesa0 : Depends: libgbm1 (= 22.3.6-1+deb12u1) but 25.0.7-2 is to be installed
```

Cause: le noeud a un melange de paquets Debian/Proxmox ou des paquets retenus,
donc `apt install stress-ng` ne peut pas resoudre les dependances.

Comportement attendu du lab: M4 ne doit plus echouer avant de tester Omega. Le
script tente l'installation de `stress-ng`, puis ignore uniquement la charge
hote sur le noeud ou `apt` est casse. Le test reste utile pour les autres noeuds,
les stores et les agents. Pour une validation complete, reparer ensuite `apt` sur
le noeud concerne.

### M7 lance `qm migrate` depuis le mauvais noeud

Symptome:

```text
Configuration file 'nodes/emilia/qemu-server/2370.conf' does not exist
```

Cause: la VM est en realite sur `ram` ou `rem`, mais le test lançait `qm migrate`
depuis `emilia`. Avec Proxmox, `qm migrate` doit etre execute depuis le noeud
source reel de la VM.

Correction attendue: le test M7 detecte maintenant le noeud source de chaque VM
avant la migration et execute la commande via SSH sur ce noeud source.

## 9. Ce qui valide vraiment le projet

Une validation serieuse doit combiner:

- VMs conformes avec `30`.
- Reseau invite valide avec `25`.
- CPU elastique et rollback avec `05`.
- RAM/paging/migration avec `08`, `22`, `M1`, `M2`, `M5`.
- Disque local et cgroup I/O avec `23C`.
- GPU avec `06`, `07`, `27`, `32`.
- Ceph avec `26`.
- Resilience avec `M7` et `28`.
- Stabilite avec `29`.
- Scalabilite grand nombre avec `31`.

Les tests locaux prouvent que les modules compilent et que les algorithmes de base fonctionnent. Les tests physiques prouvent que Proxmox, QEMU, cgroups, Ceph, GPU, reseau et systemd se comportent comme attendu dans le datacenter.
