# Plan de packaging Cricket (.deb)

Ce document décrit le plan de création de deux paquets Debian autonomes
permettant d'installer et configurer Cricket (GPU Remoting) sans intervention
manuelle sur un hôte Proxmox VE 9.1.1 (serveur) et sur un client Debian/Ubuntu
(VM, LXC ou machine physique).

- **Version initiale** : `0.0.1`
- **Mainteneur** : `fivet.engineer@gmail.com`
- **Formats** : `.deb` uniquement
- **Wrapper `cricket-run`** : fourni dans le paquet client
- **Port RPC** : fixe (configurable)

---

## Structure générale

```
packaging/
├── cricket-server/              # Paquet serveur (Proxmox/PVE)
│   ├── DEBIAN/
│   │   ├── control              # Métadonnées + dépendances
│   │   ├── postinst             # Post-installation (systemd, user, rpcbind)
│   │   ├── prerm                # Pré-suppression (stop service)
│   │   ├── postrm               # Post-suppression (cleanup)
│   │   └── conffiles            # Fichiers de config protégés
│   ├── usr/local/bin/cricket-rpc-server
│   ├── usr/local/lib/cricket/libtirpc.so.3
│   ├── etc/cricket/server.conf
│   ├── etc/systemd/system/cricket-rpc.service
│   └── etc/profile.d/cricket-server.sh
│
├── cricket-client/              # Paquet client (VM / LXC / laptop)
│   ├── DEBIAN/
│   │   ├── control
│   │   ├── postinst
│   │   ├── prerm
│   │   └── conffiles
│   ├── usr/local/bin/cricket-run       # Wrapper d'exécution
│   ├── usr/local/lib/cricket/cricket-client.so
│   ├── usr/local/lib/cricket/libtirpc.so.3
│   ├── etc/cricket/client.conf
│   └── etc/profile.d/cricket-client.sh
│
└── build-packages.sh            # Script de build automatique
```

---

## Paquet 1 — `cricket-server_0.0.1_amd64.deb`

**Cible** : hôte avec GPU NVIDIA (Proxmox VE 9.1.1).

### Contenu

- Binaire `cricket-rpc-server` installé dans `/usr/local/bin/`
- Bibliothèque `libtirpc.so.3` dans `/usr/local/lib/cricket/`
- Fichier de configuration `/etc/cricket/server.conf` (port, transport)
- Service systemd `cricket-rpc.service` (auto-start, auto-restart)
- Script `/etc/profile.d/cricket-server.sh` exportant `LD_LIBRARY_PATH`

### Dépendances (`Depends:`)

```
libc6, libssl3, libelf1, rpcbind, libtirpc-common
```

Le driver NVIDIA est vérifié au `postinst` mais non requis comme dépendance
stricte (installé hors dépôts Debian officiels).

### Actions automatiques (`postinst`)

1. Création de l'utilisateur système `cricket` (groupes `video`, `render`)
2. Activation de `rpcbind.service`
3. `systemctl daemon-reload` puis `enable --now cricket-rpc`
4. Vérification de `nvidia-smi` (warning si absent)
5. Affichage du port d'écoute dans le log d'installation

### Désinstallation propre (`prerm` + `postrm`)

- Arrêt et désactivation du service
- Suppression de l'utilisateur `cricket` uniquement si `--purge`
- Conservation de `/etc/cricket/` sauf si `--purge`

---

## Paquet 2 — `cricket-client_0.0.1_amd64.deb`

**Cible** : VM, LXC ou machine cliente utilisant le GPU distant.

### Contenu

- Bibliothèque `cricket-client.so` dans `/usr/local/lib/cricket/`
- `libtirpc.so.3` dans le même dossier
- Wrapper `/usr/local/bin/cricket-run` (simplifie l'usage)
- Configuration `/etc/cricket/client.conf` (IP serveur, port)
- Script `/etc/profile.d/cricket-client.sh` (auto-preload optionnel)

### Wrapper `cricket-run`

Usage simplifié sans manipulation manuelle de `LD_PRELOAD` :

```bash
cricket-run python3 mon_script.py
cricket-run ./benchmark_cuda
cricket-run nvidia-smi
```

Le wrapper lit `/etc/cricket/client.conf`, exporte `REMOTE_GPU_ADDRESS` et
applique `LD_PRELOAD=/usr/local/lib/cricket/cricket-client.so` avant d'exécuter
la commande fournie.

### Dépendances (`Depends:`)

```
libc6, libssl3, libelf1
```

Aucun driver NVIDIA nécessaire côté client.

### Actions automatiques (`postinst`)

1. Écriture de `/etc/cricket/client.conf` avec valeurs par défaut
2. Test de connectivité TCP au serveur (si configuré)
3. Message d'aide post-installation (exemple d'utilisation)

---

## Fichiers de configuration auto-générés

### `/etc/cricket/server.conf`

```ini
# Cricket RPC Server configuration
TRANSPORT=tcp           # tcp | ib | shm
RPC_PORT=58648          # Port fixe
LOG_LEVEL=info          # debug | info | warn | error
CUDA_HOME=/usr/local/cuda
```

### `/etc/cricket/client.conf`

```ini
# Cricket Client configuration
REMOTE_GPU_ADDRESS=192.168.123.101
REMOTE_GPU_PORT=58648
TRANSPORT=tcp
AUTO_PRELOAD=false      # true = LD_PRELOAD exporté à chaque shell
```

---

## Script `build-packages.sh`

Automatise la création des deux `.deb` à partir du build Cricket :

1. Exécute `make` pour compiler `cricket-client.so`, `cricket-rpc-server`
   et `libtirpc.so.3`
2. Copie les binaires dans `packaging/cricket-{server,client}/...`
3. Génère les fichiers `control` avec la version passée en argument
4. Appelle `dpkg-deb --build` pour chaque paquet
5. Produit `cricket-server_<version>_amd64.deb` et
   `cricket-client_<version>_amd64.deb` à la racine

Usage :

```bash
./build-packages.sh 0.0.1
```

---

## Plan d'implémentation

| # | Étape                                              | Livrable                     |
|---|----------------------------------------------------|------------------------------|
| 1 | Créer l'arborescence `packaging/`                  | Squelette complet            |
| 2 | Écrire `DEBIAN/control` des deux paquets           | Métadonnées + dépendances    |
| 3 | Écrire `postinst`, `prerm`, `postrm`               | Scripts d'auto-configuration |
| 4 | Créer le service systemd serveur                   | `cricket-rpc.service`        |
| 5 | Écrire le wrapper `cricket-run`                    | Usage simplifié côté client  |
| 6 | Générer les configs par défaut                     | Templates `.conf`            |
| 7 | Écrire `build-packages.sh`                         | Automatisation du build      |
| 8 | Tester install/désinstall sur serveur et client    | Validation fonctionnelle     |

---

## Usage final

### Serveur Proxmox

```bash
dpkg -i cricket-server_0.0.1_amd64.deb
systemctl status cricket-rpc
```

### Client

```bash
dpkg -i cricket-client_0.0.1_amd64.deb
nano /etc/cricket/client.conf   # ajuster IP serveur
cricket-run python3 train.py
```

---

## Paramètres validés

- Noms des paquets : `cricket-server` / `cricket-client`
- Version initiale : `0.0.1`
- Mainteneur : `fivet.engineer@gmail.com`
- Wrapper `cricket-run` : fourni
- Port RPC : fixe (`58648` par défaut, modifiable dans `server.conf`)
- Format : `.deb` uniquement
