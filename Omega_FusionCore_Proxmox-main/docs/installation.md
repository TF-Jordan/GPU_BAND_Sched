**Guide d'installation — omega-remote-paging**  
Ce guide couvre l'installation complète sur un cluster de 3 nœuds Proxmox.

> **Installation automatisée (recommandée)**
>
> Tout peut se faire depuis le script interactif sans lire ce guide :
>
> ```bash
> bash scripts/omega-lab.sh
> # [c] → configurer les nœuds
> # [I] → installation complète (uninstall + build + deploy)
> # [A] → lancer tous les tests
> ```
>
> Ce guide détaille les étapes manuelles pour ceux qui veulent comprendre
> chaque opération ou déboguer un problème spécifique.  
   
 Toutes les commandes sont à exécuter en **root** sauf mention contraire.  
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANklEQVR4nO3OQQmAABRAsSeYxZy/lHd7GMACBrCCNxG2BFtmZquOAAD4i3Ot7mr/egIAwGvXA7GTBde8bLBeAAAAAElFTkSuQmCC)  
**Sommaire**  
1. [Prérequis](#anchor-1 "#anchor-1")  
2. [Compilation du daemon Rust](#anchor-2 "#anchor-2")  
3. [Installation du daemon sur chaque nœud](#anchor-3 "#anchor-3")  
4. [Configuration réseau et firewall](#anchor-4 "#anchor-4")  
5. [Démarrage et vérification du daemon](#anchor-5 "#anchor-5")  
6. [Installation du controller Python](#anchor-6 "#anchor-6")  
7. [Configuration du controller](#anchor-7 "#anchor-7")  
8. [TLS — distribution des empreintes](#anchor-8 "#anchor-8")  
9. [Vérification end-to-end](#anchor-9 "#anchor-9")  
10. [Mise à jour](#anchor-10 "#anchor-10")  
11. [Désinstallation](#anchor-11 "#anchor-11")  
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANElEQVR4nO3OMQ0AIAwAwZIgBKnVgjN8dGDBABMhuZt+/JaZIyJmAADwi9VP1NMNAABu1AaU3AUhiyfJeAAAAABJRU5ErkJggg==)  
**1. Prérequis**  
**1.1 Système**  
Chaque nœud doit être un Proxmox VE 7.x, 8.x ou 9.x.  
| | | | |  
|-|-|-|-|  
| **Version PVE** | **Base Debian** | **Kernel PVE** | **Statut** |   
| 7.x | Bullseye (11) | 5.15 | compatible, non recommandé |   
| 8.x | Bookworm (12) | 6.2 – 6.8 | recommandé |   
| 9.x | Trixie (13) | 6.11+ | compatible, recommandé |   
   
# Vérifier la version Proxmox  
 pveversion  
 # PVE 8 → pve-manager/8.x.x (running kernel: 6.x.x-x-pve)  
 # PVE 9 → pve-manager/9.x.x (running kernel: 6.x.x-x-pve)  
   
 # Vérifier le kernel (doit supporter cgroups v2 + userfaultfd)  
 uname -r  
 # → 6.x.x-x-pve  
   
 # Vérifier que cgroups v2 est actif (identique PVE 8 et 9)  
 mount | grep cgroup2  
 # → cgroup2 on /sys/fs/cgroup type cgroup2 ...  
   
**1.2 SSH sans mot de passe (obligatoire pour le déploiement)**

Le script `deploy.sh` et `omega-lab.sh` se connectent en SSH root sur chaque nœud.
La connexion doit fonctionner sans saisie de mot de passe (clé publique).

```bash
# Sur la machine de développement (là où vous compilez)

# Générer une clé ED25519 si vous n'en avez pas encore
ssh-keygen -t ed25519 -C "omega-deploy" -N ""
# Répond à la question "Enter file" avec Entrée → enregistré dans ~/.ssh/id_ed25519

# Copier la clé sur chaque nœud du cluster
ssh-copy-id root@192.168.10.1   # node-a / pve1
ssh-copy-id root@192.168.10.2   # node-b / pve2
ssh-copy-id root@192.168.10.3   # node-c / pve3

# Vérification — aucun mot de passe ne doit être demandé
ssh root@192.168.10.1 hostname
ssh root@192.168.10.2 hostname
ssh root@192.168.10.3 hostname
```

Si `ssh-copy-id` n'est pas disponible sur votre distrib :

```bash
cat ~/.ssh/id_ed25519.pub | ssh root@192.168.10.1 \
    "mkdir -p ~/.ssh && chmod 700 ~/.ssh && cat >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys"
```

> Sur Proxmox, `PermitRootLogin` est activé par défaut dans `/etc/ssh/sshd_config`.
> Si vous l'avez désactivé, réactivez-le ou créez un utilisateur sudoers et ajustez `DEPLOY_USER`.

**1.3 Réseau**  
Les 3 nœuds doivent se joindre sur un réseau dédié (VLAN ou interface de stockage).  
Exemples d'adresses utilisées dans ce guide :  
   node-a : 192.168.10.1  (hostname: pve-node1)  
   node-b : 192.168.10.2  (hostname: pve-node2)  
   node-c : 192.168.10.3  (hostname: pve-node3)  
   
Vérifier la connectivité avant de commencer :  
# Depuis node-a  
 ping -c 3 192.168.10.2  
 ping -c 3 192.168.10.3  
   
**1.3 Ports requis**  
| | | | |  
|-|-|-|-|  
| **Port** | **Protocole** | **Sens** | **Usage** |   
| 9100 | TCP | tous ↔ tous | Store de pages (paging RAM) |   
| 9200 | TCP | tous ↔ tous | API cluster HTTP |   
| 9300 | TCP | localhost | Canal de contrôle (controller → daemon) |   
   
Le port 9300 n'a pas besoin d'être ouvert entre nœuds (local uniquement).  
**1.4 Dépendances système**  
Les noms de paquets sont identiques sur Debian 12 (PVE 8) et Debian 13 (PVE 9).  
# Sur chaque nœud (PVE 8 et PVE 9)  
 apt-get update  
 apt-get install -y \  
     build-essential \  
     curl \  
     git \  
     pkg-config \  
     libssl-dev \  
     python3 \  
     python3-pip \  
     python3-venv  
   
 # Vérifier la version Python disponible  
 python3 --version  
 # PVE 8 (Debian 12) → Python 3.11.x  
 # PVE 9 (Debian 13) → Python 3.12.x ou 3.13.x  
 # Les deux versions sont compatibles avec le controller.  
   
**1.5 Spécificités Proxmox VE 9.x**  
Si tu utilises PVE 9.x (Debian 13 Trixie), noter les points suivants :  
**Ce qui est identique à PVE 8 :**  
- Chemins cgroups v2 : /sys/fs/cgroup/machine.slice/ — inchangé  
- Fichiers PID QEMU : /var/run/qemu-server/{vmid}.pid — inchangé  
- Commandes qm migrate, pvecm, pveversion — inchangées  
- nftables comme firewall par défaut — inchangé  
- systemd — inchangé  
- Ports 9100/9200/9300 — inchangés  
**Ce qui change avec PVE 9 :**  
| | | |  
|-|-|-|  
| **Élément** | **PVE 8 (Debian 12)** | **PVE 9 (Debian 13)** |   
| Python | 3.11.x | 3.12.x ou 3.13.x |   
| OpenSSL | 3.0.x | 3.3.x |   
| Kernel PVE | 6.2 – 6.8 | 6.11+ |   
| os-variant virt-install | debian12 | debian13 |   
   
***Compatibilité du code*** * : le daemon Rust et le controller Python sont*  
 *  
 totalement compatibles avec PVE 9. Aucune modification du code n'est nécessaire.*  
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAAM0lEQVR4nO3OMQ0AIAwAwZKQ6kBqjSAOJywYYCIkd9OP36pqRMQMAAB+sfqJfLoBAMCN3NYoAzBA+QG0AAAAAElFTkSuQmCC)  
**2. Compilation du daemon Rust**  
La compilation se fait sur **une seule machine** (ou en CI), le binaire est  
   
 ensuite copié sur les 3 nœuds.  
**2.1 Installer Rust**  
# Installer rustup (gestionnaire de toolchain Rust)  
 curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y  
   
 # Charger l'environnement dans le shell courant  
 source "$HOME/.cargo/env"  
   
 # Vérifier l'installation  
 rustc --version  
 # → rustc 1.77.x (ou plus récent)  
 cargo --version  
 # → cargo 1.77.x  
   
**2.2 Cloner le dépôt**  
git clone <url-du-depot> /opt/omega-remote-paging  
 cd /opt/omega-remote-paging  
   
**2.3 Compiler en mode release**  
cargo build --release -p omega-daemon  
   
La compilation prend 2–5 minutes la première fois (téléchargement des dépendances).  
   
 Le binaire est produit dans :  
target/release/omega-daemon  
   
**2.4 Vérifier le binaire**  
./target/release/omega-daemon --version  
 # → omega-daemon 0.4.0  
   
 ./target/release/omega-daemon --help  
 # → affiche toutes les options disponibles  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANElEQVR4nO3OQQmAABRAsad4EEtY9QcxnUms4E2ELcGWmTmrKwAA/uLeqrU6vp4AAPDa/gDzXgM37EF77AAAAABJRU5ErkJggg==)  
**3. Installation du daemon sur chaque nœud**  
Les étapes 3.1 à 3.6 sont à répéter sur **node-a, node-b et node-c**.  
**3.1 Copier le binaire**  
Depuis la machine de compilation :  
# Copier vers node-a  
 scp target/release/omega-daemon root@192.168.10.1:/usr/local/bin/omega-daemon  
   
 # Copier vers node-b  
 scp target/release/omega-daemon root@192.168.10.2:/usr/local/bin/omega-daemon  
   
 # Copier vers node-c  
 scp target/release/omega-daemon root@192.168.10.3:/usr/local/bin/omega-daemon  
   
Sur chaque nœud, rendre le binaire exécutable :  
chmod 755 /usr/local/bin/omega-daemon  
   
**3.2 Créer les répertoires**  
# Répertoire de configuration TLS (généré automatiquement au premier démarrage)  
 mkdir -p /etc/omega-store/tls  
 chmod 700 /etc/omega-store/tls  
   
 # Répertoire des logs (optionnel — journald suffit)  
 mkdir -p /var/log/omega  
   
**3.3 Créer l'utilisateur système**  
useradd --system --no-create-home --shell /usr/sbin/nologin omega  
   
 # Donner accès aux ressources nécessaires  
 # (cgroups, /proc, /sys, QMP sockets — nécessite root ou CAP_SYS_PTRACE)  
 # Pour simplifier en V4 : omega-daemon tourne en root  
   
***Note*** * : omega-daemon lit les cgroups v2 (* */sys/fs/cgroup/machine.slice/* *),*  
 *  
 les PIDs QEMU (* */var/run/qemu-server/* *), et écrit les sockets QMP.*  
 *  
 Ces opérations nécessitent root sur Proxmox. Une version future utilisera*  
 *  
 des capabilities Linux pour limiter les privilèges.*  
**3.4 Créer le fichier d'environnement**  
Créer /etc/omega-store/daemon.env avec les paramètres du nœud.  
**Sur node-a** (/etc/omega-store/daemon.env) :  
cat > /etc/omega-store/daemon.env << 'EOF'  
 # Identité du nœud  
 OMEGA_NODE_ID=pve-node1  
 OMEGA_NODE_ADDR=192.168.10.1  
   
 # Ports d'écoute  
 OMEGA_STORE_PORT=9100  
 OMEGA_API_PORT=9200  
   
 # Pairs du cluster (les 2 autres nœuds)  
 OMEGA_PEERS=192.168.10.2:9200,192.168.10.3:9200  
   
 # Seuil d'éviction RAM (% au-delà duquel on déclenche la migration)  
 OMEGA_EVICT_THRESHOLD_PCT=80.0  
   
 # Monitoring des VMs QEMU locales  
 OMEGA_MONITOR_VMS=true  
 OMEGA_QEMU_PID_DIR=/var/run/qemu-server  
 OMEGA_QEMU_CONF_DIR=/etc/pve/qemu-server  
   
 # Logging  
 RUST_LOG=info  
 OMEGA_LOG_FORMAT=text  
   
 # Stats périodiques (secondes)  
 OMEGA_STATS_INTERVAL=30  
   
 # Timeout vers les stores distants (ms)  
 OMEGA_STORE_TIMEOUT_MS=2000  
 EOF  
   
 chmod 600 /etc/omega-store/daemon.env  
   
**Sur node-b** (changer NODE_ID et NODE_ADDR, adapter PEERS) :  
cat > /etc/omega-store/daemon.env << 'EOF'  
 OMEGA_NODE_ID=pve-node2  
 OMEGA_NODE_ADDR=192.168.10.2  
 OMEGA_STORE_PORT=9100  
 OMEGA_API_PORT=9200  
 OMEGA_PEERS=192.168.10.1:9200,192.168.10.3:9200  
 OMEGA_EVICT_THRESHOLD_PCT=80.0  
 OMEGA_MONITOR_VMS=true  
 OMEGA_QEMU_PID_DIR=/var/run/qemu-server  
 OMEGA_QEMU_CONF_DIR=/etc/pve/qemu-server  
 RUST_LOG=info  
 OMEGA_LOG_FORMAT=text  
 OMEGA_STATS_INTERVAL=30  
 OMEGA_STORE_TIMEOUT_MS=2000  
 EOF  
   
 chmod 600 /etc/omega-store/daemon.env  
   
**Sur node-c** :  
cat > /etc/omega-store/daemon.env << 'EOF'  
 OMEGA_NODE_ID=pve-node3  
 OMEGA_NODE_ADDR=192.168.10.3  
 OMEGA_STORE_PORT=9100  
 OMEGA_API_PORT=9200  
 OMEGA_PEERS=192.168.10.1:9200,192.168.10.2:9200  
 OMEGA_EVICT_THRESHOLD_PCT=80.0  
 OMEGA_MONITOR_VMS=true  
 OMEGA_QEMU_PID_DIR=/var/run/qemu-server  
 OMEGA_QEMU_CONF_DIR=/etc/pve/qemu-server  
 RUST_LOG=info  
 OMEGA_LOG_FORMAT=text  
 OMEGA_STATS_INTERVAL=30  
 OMEGA_STORE_TIMEOUT_MS=2000  
 EOF  
   
 chmod 600 /etc/omega-store/daemon.env  
   
**3.5 Créer le service systemd**  
Sur **chaque nœud**, créer /etc/systemd/system/omega-daemon.service :  
cat > /etc/systemd/system/omega-daemon.service << 'EOF'  
 [Unit]  
 Description=omega-daemon — paging RAM distant + scheduler vCPU/GPU  
 Documentation=https://github.com/votre-org/omega-remote-paging  
 After=network-online.target pve-cluster.service  
 Wants=network-online.target  
 # Démarrer après le cluster Proxmox pour que /etc/pve soit monté  
 Requires=pve-cluster.service  
   
 [Service]  
 Type=simple  
 EnvironmentFile=/etc/omega-store/daemon.env  
 ExecStart=/usr/local/bin/omega-daemon  
 Restart=on-failure  
 RestartSec=5  
 StandardOutput=journal  
 StandardError=journal  
 SyslogIdentifier=omega-daemon  
   
 # Limites de ressources  
 LimitNOFILE=65536  
 LimitNPROC=4096  
   
 # Sécurité minimale (root requis pour cgroups + QMP)  
 # En V5 : passer à un utilisateur dédié avec capabilities  
 User=root  
 Group=root  
   
 [Install]  
 WantedBy=multi-user.target  
 EOF  
   
**3.6 Activer et démarrer le service**  
systemctl daemon-reload  
 systemctl enable omega-daemon  
 systemctl start omega-daemon  
   
 # Vérifier le statut  
 systemctl status omega-daemon  
 # → Active: active (running)  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANUlEQVR4nO3OMQ2AABAAsSPBCUbfEm6YmFDBhAU2QtIq6DIzW7UHAMBfnGt1V8fXEwAAXrse/w8F7pbTa1oAAAAASUVORK5CYII=)  
**4. Configuration réseau et firewall**  
**4.1 Avec nftables (Proxmox 8.x et 9.x — défaut)**  
# Créer un fichier de règles omega  
 cat > /etc/nftables.d/omega.conf << 'EOF'  
 # omega-remote-paging — ports inter-nœuds  
 table inet omega {  
     chain input {  
         type filter hook input priority 0;  
   
         # Store TCP : paging RAM distant  
         tcp dport 9100 ip saddr {  
             192.168.10.1,  
             192.168.10.2,  
             192.168.10.3  
         } accept comment "omega store TCP"  
   
         # API HTTP cluster  
         tcp dport 9200 ip saddr {  
             192.168.10.1,  
             192.168.10.2,  
             192.168.10.3  
         } accept comment "omega API HTTP"  
     }  
 }  
 EOF  
   
 # Appliquer  
 nft -f /etc/nftables.d/omega.conf  
   
 # Rendre persistant  
 systemctl reload nftables  
   
**4.2 Avec iptables (Proxmox 7.x uniquement)**  
# Node-a : autoriser les connexions depuis node-b et node-c  
 iptables -A INPUT -s 192.168.10.2 -p tcp --dport 9100 -j ACCEPT  
 iptables -A INPUT -s 192.168.10.3 -p tcp --dport 9100 -j ACCEPT  
 iptables -A INPUT -s 192.168.10.2 -p tcp --dport 9200 -j ACCEPT  
 iptables -A INPUT -s 192.168.10.3 -p tcp --dport 9200 -j ACCEPT  
   
 # Persister  
 iptables-save > /etc/iptables/rules.v4  
   
**4.3 Vérifier la connectivité inter-nœuds**  
# Depuis node-a, tester le store de node-b  
 nc -zv 192.168.10.2 9100  
 # → Connection to 192.168.10.2 9100 port [tcp/*] succeeded!  
   
 # Tester l'API de node-c  
 curl -s http://192.168.10.3:9200/api/status | python3 -m json.tool | head -10  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAAM0lEQVR4nO3OMQ0AIAwAwdIgBKl1gjacsGCAiZDcTT9+q6oRETMAAPjF6ify6QYAADdyA9Y0AypN+bdfAAAAAElFTkSuQmCC)  
**5. Démarrage et vérification du daemon**  
**5.1 Vérifier les logs au démarrage**  
journalctl -u omega-daemon -f  
   
Vous devez voir (dans les 30 premières secondes) :  
omega-daemon V4 démarré  node_id=pve-node1  addr=192.168.10.1  
 canal de contrôle HTTP démarré  addr=0.0.0.0:9300  
 monitoring VMs locales actif (intervalle 15s)  
 moteur d'éviction démarré  threshold_pct=80.0  
 TLS initialisé — empreinte à distribuer aux pairs pour TOFU  
 daemon opérationnel — CTRL+C pour arrêter  
   
**5.2 Vérifier l'API HTTP**  
# État du nœud local  
 curl -s http://localhost:9200/api/status | python3 -m json.tool  
   
Réponse attendue :  
{  
     "node_id": "pve-node1",  
     "mem_total_kb": 33554432,  
     "mem_available_kb": 28000000,  
     "mem_usage_pct": 16.5,  
     "pages_stored": 0,  
     "local_vms": [],  
     "timestamp_secs": 1713200000  
 }  
   
**5.3 Vérifier le canal de contrôle**  
# Métriques du scheduler vCPU  
 curl -s http://localhost:9300/control/vcpu/status | python3 -m json.tool  
   
 # Quotas mémoire (vide au départ)  
 curl -s http://localhost:9300/control/quotas | python3 -m json.tool  
   
 # Recommandations de migration (vide si cluster sain)  
 curl -s http://localhost:9300/control/migrate/recommend | python3 -m json.tool  
   
**5.4 Vérifier le TLS**  
Au premier démarrage, le daemon génère automatiquement les certificats :  
ls -la /etc/omega-store/tls/  
 # → cert.pem  key.pem  
   
 # Voir l'empreinte du certificat local  
 openssl x509 -in /etc/omega-store/tls/cert.pem -fingerprint -sha256 -noout  
 # → SHA256 Fingerprint=AB:CD:EF:...  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANUlEQVR4nO3OMQ2AABAAsSNhwgJuUPYDMpnRgQU2QtIq6DIze3UGAMBf3Gu1VcfXEwAAXrseaHEEM+cJoFcAAAAASUVORK5CYII=)  
**6. Installation du controller Python**  
Le controller Python tourne sur **un seul nœud** (ou sur une machine dédiée).  
   
 Il a une vue globale du cluster et pilote les décisions.  
Recommandation : l'installer sur **node-a** (ou un nœud de management dédié).  
**6.1 Installer le controller**  
git clone https://github.com/Savhel/Omega_FusionCore_Proxmox /opt/omega-remote-paging  
cd /opt/omega-remote-paging  
   
 # Créer un environnement virtuel Python  
 python3 -m venv /opt/omega-controller-venv  
   
 # Activer l'environnement  
 source /opt/omega-controller-venv/bin/activate  
   
 # Installer le controller et ses dépendances  
 pip install -e ./controller/  
   
 # Vérifier l'installation  
 omega-controller --help  
 # → affiche les commandes disponibles  
   
**6.2 Lancer les tests du controller**  
cd /opt/omega-remote-paging/controller  
 python3 -m pytest -q  
 # → 261 passed in 7.xx s  
   
**6.3 Créer le service systemd du controller**  
cat > /etc/systemd/system/omega-controller.service << 'EOF'  
 [Unit]  
 Description=omega-controller — politique de paging et migration  
 After=omega-daemon.service  
 Requires=omega-daemon.service  
   
 [Service]  
 Type=simple  
 WorkingDirectory=/opt/omega-remote-paging/controller  
 ExecStart=/opt/omega-controller-venv/bin/python3 -m controller.main \  
     daemon \  
     --node-a http://192.168.10.1:9300 \  
     --node-b http://192.168.10.2:9300 \  
     --node-c http://192.168.10.3:9300 \  
     --poll-interval 5  
 Restart=on-failure  
 RestartSec=10  
 StandardOutput=journal  
 StandardError=journal  
 SyslogIdentifier=omega-controller  
 User=root  
 Group=root  
   
 [Install]  
 WantedBy=multi-user.target  
 EOF  
   
 systemctl daemon-reload  
 systemctl enable omega-controller  
 systemctl start omega-controller  
   
***Note*** * : Si * *controller.main* * n'est pas encore implémenté, lancer à la place*  
 *  
 le moniteur CPU directement :*  
*python3 -c "  
 from controller.cpu_cgroup_monitor import CgroupCpuController, CgroupCpuMonitor  
 import time  
   
 def on_pressure(vm_id, usage_pct, throttle)* *:  
     print(f'VM {vm_id}: usage={usage_pct:.1f}% throttle={throttle:.2%}')  
     # Ici : appeler POST /control/migrate si nécessaire  
   
 ctrl = CgroupCpuController()  
 mon  = CgroupCpuMonitor(ctrl, on_pressure=on_pressure)  
 mon.start()  
 while True: time.sleep(1)  
 "  
 *  
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANUlEQVR4nO3OsQ1AABRAwSdRaPXGMOCv7WkPK+hEcjfBLTNzVFcAAPzFvVZbdX49AQDgtf0BSpoDXv5TGXgAAAAASUVORK5CYII=)  
**7. Configuration du controller**  
**7.1 Variables d'environnement du controller**  
Créer /etc/omega-store/controller.env :  
cat > /etc/omega-store/controller.env << 'EOF'  
 # URLs des canaux de contrôle des 3 nœuds  
 OMEGA_NODES_CONTROL=http://192.168.10.1:9300,http://192.168.10.2:9300,http://192.168.10.3:9300  
   
 # Seuils de déclenchement des migrations  
 OMEGA_RAM_HIGH_PCT=85.0  
 OMEGA_RAM_CRITICAL_PCT=95.0  
 OMEGA_VCPU_THROTTLE_TRIGGER=0.30  
 OMEGA_REMOTE_PAGING_PCT=60.0  
 OMEGA_IDLE_CPU_PCT=5.0  
 OMEGA_IDLE_DURATION_SECS=60.0  
   
 # Polling de l'état du cluster (secondes)  
 OMEGA_POLL_INTERVAL=5  
 EOF  
   
 chmod 600 /etc/omega-store/controller.env  
   
**7.2 Configurer le quota mémoire d'une VM**  
Après création d'une VM (ex: vmid=101, 8 Go RAM) :  
# Enregistrer le quota sur le nœud hôte (node-a dans cet exemple)  
 curl -X POST http://192.168.10.1:9300/control/vm/101/quota \  
     -H "Content-Type: application/json" \  
     -d '{  
         "max_mem_mib": 8192,  
         "local_budget_mib": 6144,  
         "remote_budget_mib": 2048  
     }'  
 # → {"status": "quota_set", "vm_id": 101, ...}  
   
**7.3 Enregistrer une VM dans le scheduler vCPU**  
# VM 101 : min 2 vCPU, max 8 vCPU (élasticité)  
 curl -X POST http://192.168.10.1:9300/control/vm/101/vcpu \  
     -H "Content-Type: application/json" \  
     -d '{"min_vcpus": 2, "max_vcpus": 8}'  
 # → {"status": "allocated", "vm_id": 101, ...}  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANUlEQVR4nO3OQQmAABRAsSd4NIGBzPXBmAawhhW8ibAl2DIze3UGAMBf3Gu1VcfXEwAAXrsehaQEN+8fLHEAAAAASUVORK5CYII=)  
**8. TLS — distribution des empreintes**  
Chaque nœud génère son propre certificat auto-signé au premier démarrage.  
   
 Pour que les nœuds se fassent confiance (TOFU), il faut échanger les empreintes.  
**8.1 Récupérer les empreintes de chaque nœud**  
# Depuis node-a (ou en SSH)  
 FINGERPRINT_A=$(ssh root@192.168.10.1 \  
     "openssl x509 -in /etc/omega-store/tls/cert.pem -fingerprint -sha256 -noout" \  
     | cut -d= -f2)  
   
 FINGERPRINT_B=$(ssh root@192.168.10.2 \  
     "openssl x509 -in /etc/omega-store/tls/cert.pem -fingerprint -sha256 -noout" \  
     | cut -d= -f2)  
   
 FINGERPRINT_C=$(ssh root@192.168.10.3 \  
     "openssl x509 -in /etc/omega-store/tls/cert.pem -fingerprint -sha256 -noout" \  
     | cut -d= -f2)  
   
 echo "node-a: $FINGERPRINT_A"  
 echo "node-b: $FINGERPRINT_B"  
 echo "node-c: $FINGERPRINT_C"  
   
**8.2 Distribuer les empreintes**  
# Créer le fichier de confiance sur chaque nœud  
 # (en V4, les empreintes sont vérifiées lors de la connexion TLS)  
   
 # Sur node-a : faire confiance à B et C  
 cat > /etc/omega-store/tls/trusted_peers.conf << EOF  
 pve-node2 $FINGERPRINT_B  
 pve-node3 $FINGERPRINT_C  
 EOF  
   
 # Sur node-b : faire confiance à A et C  
 ssh root@192.168.10.2 "cat > /etc/omega-store/tls/trusted_peers.conf << EOF  
 pve-node1 $FINGERPRINT_A  
 pve-node3 $FINGERPRINT_C  
 EOF"  
   
 # Sur node-c : faire confiance à A et B  
 ssh root@192.168.10.3 "cat > /etc/omega-store/tls/trusted_peers.conf << EOF  
 pve-node1 $FINGERPRINT_A  
 pve-node2 $FINGERPRINT_B  
 EOF"  
   
**8.3 Redémarrer les daemons après échange d'empreintes**  
for node in 192.168.10.1 192.168.10.2 192.168.10.3; do  
     ssh root@$node "systemctl restart omega-daemon"  
 done  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANklEQVR4nO3OQQmAABRAsScYxpg/h5VMYARvRrCCNxG2BFtmZquOAAD4i3Ot7mr/egIAwGvXA224BcUMk6pDAAAAAElFTkSuQmCC)  
**9. Vérification end-to-end**  
**9.1 Vérifier que les 3 nœuds se voient**  
# Depuis node-a  
 curl -s http://192.168.10.1:9200/api/status | python3 -m json.tool | grep node_id  
 curl -s http://192.168.10.2:9200/api/status | python3 -m json.tool | grep node_id  
 curl -s http://192.168.10.3:9200/api/status | python3 -m json.tool | grep node_id  
 # → "node_id": "pve-node1"  
 # → "node_id": "pve-node2"  
 # → "node_id": "pve-node3"  
   
**9.2 Créer une VM de test et l'enregistrer**  
# Créer une VM minimale (sur node-a)  
 pvesh create /nodes/pve-node1/qemu \  
     --vmid 9001 \  
     --name omega-test \  
     --memory 2048 \  
     --cores 2 \  
     --net0 virtio,bridge=vmbr0 \  
     --ostype l26  
   
 # Démarrer la VM  
 qm start 9001  
   
 # Attendre 10s que le daemon détecte la VM  
 sleep 10  
   
 # Vérifier que la VM apparaît dans le daemon  
 curl -s http://localhost:9200/api/status | python3 -m json.tool | grep -A5 local_vms  
   
**9.3 Enregistrer la VM dans omega**  
# Quota mémoire  
 curl -X POST http://localhost:9300/control/vm/9001/quota \  
     -H "Content-Type: application/json" \  
     -d '{"max_mem_mib": 2048, "local_budget_mib": 1536}'  
   
 # Scheduler vCPU  
 curl -X POST http://localhost:9300/control/vm/9001/vcpu \  
     -H "Content-Type: application/json" \  
     -d '{"min_vcpus": 1, "max_vcpus": 4}'  
   
 # Vérifier les deux  
 curl -s http://localhost:9300/control/vm/9001/quota | python3 -m json.tool  
 curl -s http://localhost:9300/control/vcpu/status | python3 -m json.tool | grep -A5 9001  
   
**9.4 Tester le monitoring CPU (1 ms)**  
cd /opt/omega-remote-paging/controller  
 source /opt/omega-controller-venv/bin/activate  
   
 python3 << 'EOF'  
 import time  
 from controller.cpu_cgroup_monitor import CgroupCpuController, CgroupCpuMonitor  
   
 ctrl = CgroupCpuController()  
 vms  = ctrl.list_active_vms()  
 print(f"VMs actives détectées : {vms}")  
   
 if vms:  
     stat = ctrl.read_stat(vms[0])  
     print(f"VM {vms[0]} : usage_usec={stat.usage_usec}, throttle_ratio={stat.throttle_ratio:.2%}")  
   
 mon = CgroupCpuMonitor(ctrl, poll_interval=0.001)  
 mon.start()  
 time.sleep(2)  
 snap = mon.snapshot()  
 print(f"Snapshot après 2s : {snap}")  
 mon.stop()  
 EOF  
   
**9.5 Tester une migration manuelle**  
# Déclencher une migration cold de la VM de test vers node-b  
 curl -X POST http://192.168.10.1:9300/control/migrate \  
     -H "Content-Type: application/json" \  
     -d '{  
         "vm_id": 9001,  
         "target": "pve-node2",  
         "type": "cold",  
         "reason": "admin_request"  
     }'  
 # → {"status": "migration_started", "task_id": 1, ...}  
   
 # Vérifier que la VM tourne sur node-b  
 sleep 30  
 qm status 9001  
 # → status: running (sur node-b maintenant)  
   
**9.6 Vérifier les recommandations de migration**  
# Consulter les recommandations automatiques sur chaque nœud  
 for node in 192.168.10.1 192.168.10.2 192.168.10.3; do  
     echo "=== $node ==="  
     curl -s http://$node:9300/control/migrate/recommend | python3 -m json.tool  
 done  
 # → {"count": 0} si le cluster est sain  
   
**9.7 Vérifier les métriques Prometheus**  
# Métriques format Prometheus sur chaque nœud  
 curl -s http://192.168.10.1:9300/control/metrics  
 # → omega_pages_stored{node="pve-node1"} 0  
 # → omega_mem_usage_pct{node="pve-node1"} 16.50  
 # → ...  
   
**9.8 Nettoyage de la VM de test**  
qm stop 9001  
 qm destroy 9001  
   
 # Supprimer le quota et les vCPU  
 curl -X DELETE http://localhost:9300/control/vm/9001/quota  
 curl -X DELETE http://localhost:9300/control/vm/9001/vcpu  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANElEQVR4nO3OQQmAABRAsad4EEtY9QcxnUms4E2ELcGWmTmrKwAA/uLeqrU6vp4AAPDa/gDzXgM37EF77AAAAABJRU5ErkJggg==)  
**10. Mise à jour**  
**10.1 Recompiler le daemon**  
cd /opt/omega-remote-paging  
 git pull  
 cargo build --release -p omega-daemon  
   
**10.2 Déployer le nouveau binaire**  
for node in 192.168.10.1 192.168.10.2 192.168.10.3; do  
     echo "=== Mise à jour $node ==="  
     scp target/release/omega-daemon root@$node:/usr/local/bin/omega-daemon  
     ssh root@$node "systemctl restart omega-daemon"  
     sleep 5  
     ssh root@$node "systemctl is-active omega-daemon"  
 done  
   
**10.3 Mettre à jour le controller Python**  
source /opt/omega-controller-venv/bin/activate  
 pip install -e /opt/omega-remote-paging/controller/  
 systemctl restart omega-controller  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANklEQVR4nO3OQQmAABRAsSeYxZw/lieLGMACBrCCNxG2BFtmZquOAAD4i3Ot7mr/egIAwGvXA6fGBdgoVMwYAAAAAElFTkSuQmCC)  
**11. Désinstallation**  
# Arrêter et désactiver les services  
 systemctl stop omega-daemon omega-controller  
 systemctl disable omega-daemon omega-controller  
   
 # Supprimer les fichiers  
 rm -f /usr/local/bin/omega-daemon  
 rm -f /etc/systemd/system/omega-daemon.service  
 rm -f /etc/systemd/system/omega-controller.service  
 rm -rf /etc/omega-store/  
 rm -rf /var/log/omega/  
   
 # Supprimer le controller Python  
 rm -rf /opt/omega-controller-venv/  
   
 systemctl daemon-reload  
   
![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAnEAAAACCAYAAAA3pIp+AAAABmJLR0QA/wD/AP+gvaeTAAAACXBIWXMAAA7EAAAOxAGVKw4bAAAANElEQVR4nO3OQQmAABRAsad4FCtY9ecwnkms4E2ELcGWmTmrKwAA/uLeqrU6vp4AAPDa/gDzUgM9+S8z3AAAAABJRU5ErkJggg==)  
**Référence rapide — commandes utiles**  
# Statut global  
 systemctl status omega-daemon omega-controller  
 journalctl -u omega-daemon --since "10 min ago"  
 journalctl -u omega-controller --since "10 min ago"  
   
 # État du nœud  
 curl -s http://localhost:9200/api/status | python3 -m json.tool  
 curl -s http://localhost:9300/control/vcpu/status | python3 -m json.tool  
 curl -s http://localhost:9300/control/quotas | python3 -m json.tool  
   
 # Migrations  
 curl -s http://localhost:9300/control/migrate/recommend | python3 -m json.tool  
 curl -s http://localhost:9300/control/migrations | python3 -m json.tool  
   
 # Logs en temps réel  
 journalctl -u omega-daemon -f  
   
 # Redémarrage d'urgence  
 systemctl restart omega-daemon  
   
 # Modifier le seuil d'éviction à chaud (sans redémarrage)  
 curl -X POST http://localhost:9300/control/config \  
     -H "Content-Type: application/json" \  
     -d '{"evict_threshold_pct": 90.0}'  
   
**Référence — Ports et URLs**  
| | | | | |  
|-|-|-|-|-|  
| **Nœud** | **IP** | **Store TCP** | **API cluster** | **Contrôle** |   
| node-a | 192.168.10.1 | :9100 | :9200 | :9300 (local) |   
| node-b | 192.168.10.2 | :9100 | :9200 | :9300 (local) |   
| node-c | 192.168.10.3 | :9100 | :9200 | :9300 (local) |   
   
**Référence — Variables d'environnement daemon**  
| | | |  
|-|-|-|  
| **Variable** | **Défaut** | **Description** |   
| OMEGA_NODE_ID | omega-node | Identifiant unique du nœud |   
| OMEGA_NODE_ADDR | 127.0.0.1 | IP annoncée aux pairs |   
| OMEGA_STORE_PORT | 9100 | Port TCP du store de pages |   
| OMEGA_API_PORT | 9200 | Port HTTP API cluster |   
| OMEGA_PEERS | `` | IPs des pairs host:port,... |   
| OMEGA_EVICT_THRESHOLD_PCT | 75.0 | Seuil RAM pour déclencher l'éviction |   
| OMEGA_MONITOR_VMS | true | Surveiller les VMs QEMU locales |   
| OMEGA_QEMU_PID_DIR | /var/run/qemu-server | Répertoire PIDs QEMU |   
| OMEGA_QEMU_CONF_DIR | /etc/pve/qemu-server | Répertoire configs VM |   
| OMEGA_STORE_TIMEOUT_MS | 2000 | Timeout TCP vers stores distants |   
| OMEGA_STATS_INTERVAL | 30 | Intervalle stats périodiques (s) |   
| RUST_LOG | info | Niveau de log (debug, info, warn) |   
| OMEGA_LOG_FORMAT | text | Format log (text ou json) |   

**Référence — Variables d'environnement node-a-agent (balloon thin-provisioning)**  
Le balloon permet à la VM de démarrer avec moins de RAM visible que sa configuration Proxmox
et de grandir jusqu'au plafond selon la demande. Désactivé par défaut.

| | | |  
|-|-|-|  
| **Variable** | **Défaut** | **Description** |   
| AGENT_BALLOON_ENABLED | false | Activer le thin-provisioning RAM guest |   
| AGENT_BALLOON_INITIAL_MIB | 512 | RAM visible au démarrage (MiB) |   
| AGENT_BALLOON_STEP_MIB | 256 | Incrément de croissance/shrink par étape (MiB) |   
| AGENT_BALLOON_INTERVAL_SECS | 10 | Intervalle entre deux ajustements (secondes) |   
| AGENT_BALLOON_GROW_FAULTS_PER_SEC | 10 | Taux de page-faults/s déclenchant la croissance |   
| AGENT_BALLOON_SHRINK_FAULTS_PER_SEC | 1 | Taux de page-faults/s déclenchant le shrink |   

Exemple — VM créée avec 2048 MiB, démarre à 512 MiB, croît par paliers de 256 MiB :  
```bash
node-a-agent \
  --stores 10.10.0.12:9100,10.10.0.13:9100 \
  --vm-id 9001 \
  --vm-requested-mib 2048 \
  --region-mib 2048 \
  --balloon-enabled \
  --balloon-initial-mib 512 \
  --balloon-step-mib 256 \
  --mode daemon
```

**Référence — Variables d'environnement node-a-agent (politique de migration)**  
La migration est déclenchée quand le nœud cible est **meilleur** que le nœud courant
sur au moins une dimension. GPU = veto absolu si la VM en requiert un.

| | | |  
|-|-|-|  
| **Variable** | **Défaut** | **Description** |   
| AGENT_MIGRATION_ENABLED | false | Activer la recherche automatique de migration |   
| AGENT_MIGRATION_INTERVAL_SECS | 30 | Intervalle entre deux recherches (secondes) |   
| AGENT_GPU_REQUIRED | false | La VM requiert un GPU (veto si absent sur le cible) |   
| AGENT_VM_VCPUS | 1 | vCPUs max de la VM (plafond, fixé à la création) |   
| AGENT_VM_INITIAL_VCPUS | 1 | vCPUs actifs au démarrage |   
| AGENT_VCPU_SCALE_INTERVAL_SECS | 30 | Intervalle mesure vCPU (secondes) |   
   
