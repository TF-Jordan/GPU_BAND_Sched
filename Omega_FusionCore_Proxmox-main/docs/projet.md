# omega-remote-paging — Description technique du projet

## Problème résolu

Un cluster Proxmox VE standard répartit les VMs sur ses nœuds en fonction de la RAM disponible sur chaque nœud. Lorsque les nœuds sont remplis, il n'est plus possible de créer de nouvelles VMs même si l'ensemble du cluster a collectivement de la mémoire disponible. La RAM reste cloisonnée par nœud physique.

Par ailleurs, une VM qui demande 8 Go de RAM ne les utilise pas nécessairement tous en permanence. Dans la pratique, une partie de sa mémoire est rarement accédée (données inactives, cache, pages dormantes). Cette mémoire physique reste bloquée sur le nœud alors qu'elle pourrait être libérée.

Ce projet apporte deux réponses à ce problème :

1. **Mutualisation de la RAM entre nœuds** — les pages mémoire rarement utilisées d'une VM sont déplacées vers la RAM d'un autre nœud, libérant de l'espace local pour d'autres VMs.

2. **Contrôle fin des ressources** — chaque VM dispose d'un budget précis (local + distant) qui ne peut pas être dépassé, garantissant qu'aucune VM ne consomme plus que ce qui lui a été accordé.

---

## Architecture

Le projet est composé de deux couches logicielles qui s'exécutent en parallèle sur chaque nœud du cluster.

```
┌─────────────────────────────────────────────────────────────┐
│  omega-controller (Python)  ←→  API HTTP Proxmox           │
│  Tourne sur un nœud du cluster                             │
│  Décide : quelles pages déplacer, vers quel nœud           │
└───────────────┬─────────────────────────────────────────────┘
                │ HTTP (port 9200 / 9300)
┌───────────────▼─────────────────────────────────────────────┐
│  omega-daemon (Rust) — un daemon par nœud physique         │
│                                                             │
│  ┌──────────────┐  ┌─────────────┐  ┌───────────────────┐  │
│  │ Store TCP    │  │ Eviction    │  │ vCPU Scheduler    │  │
│  │ (port 9100)  │  │ Engine      │  │                   │  │
│  └──────────────┘  └─────────────┘  └───────────────────┘  │
│  ┌──────────────┐  ┌─────────────┐  ┌───────────────────┐  │
│  │ API HTTP     │  │ VM Tracker  │  │ GPU Multiplexer   │  │
│  │ (port 9200)  │  │             │  │                   │  │
│  └──────────────┘  └─────────────┘  └───────────────────┘  │
│  ┌──────────────┐  ┌─────────────┐                         │
│  │ Control HTTP │  │ Quota       │                         │
│  │ (port 9300)  │  │ Registry    │                         │
│  └──────────────┘  └─────────────┘                         │
└─────────────────────────────────────────────────────────────┘
                │
                │  Accès direct à la mémoire des VMs
                │  via userfaultfd (Linux kernel)
┌───────────────▼─────────────────────────────────────────────┐
│  VMs QEMU/KVM sur ce nœud                                  │
└─────────────────────────────────────────────────────────────┘
```

---

## Mécanisme central : le paging distant

Quand une VM accède à une page mémoire qui a été déplacée vers un autre nœud, le processeur génère un défaut de page. Linux expose ces défauts via l'interface kernel `userfaultfd`, qui permet à un programme en espace utilisateur de les intercepter.

Le daemon capture ce défaut, envoie une requête TCP au nœud qui possède la page, reçoit les 4 Ko de données, et les place en mémoire locale avant de laisser l'accès se terminer. Du point de vue de la VM, l'accès a simplement pris quelques millisecondes de plus — elle ne sait pas que la page était à distance.

```
VM accède à une page absente en RAM locale
          │
          ▼
  Défaut de page intercepté par userfaultfd
          │
          ▼
  omega-daemon envoie GET_PAGE(vm_id, page_id) → nœud distant
          │
          ▼
  Nœud distant retourne les 4 Ko → déplacés en RAM locale
          │
          ▼
  VM reprend son exécution normalement
```

La décision de déplacer une page vers un autre nœud (éviction) est prise par le moteur d'éviction, qui surveille en continu la pression mémoire du nœud local et déplace les pages les moins récemment utilisées (algorithme CLOCK).

---

## Composants

### omega-daemon (Rust)

Daemon unifié qui remplit plusieurs rôles simultanément sur chaque nœud.

**Store TCP (port 9100)**
Accepte les pages envoyées par d'autres nœuds. Chaque page est indexée par `(vm_id, page_id)` et stockée en RAM avec une mise sur disque optionnelle via sled (base B-tree embarquée). Un nœud peut héberger les pages distantes de plusieurs VMs simultanément.

**API HTTP cluster (port 9200)**
Expose l'état du nœud aux autres composants : RAM totale, RAM disponible, nombre de pages stockées, liste des VMs locales et leurs compteurs de pages distantes. Utilisé par le contrôleur Python pour collecter l'état du cluster.

**Control HTTP (port 9300)**
Canal de contrôle interne : déclencher une éviction immédiate, configurer les quotas par VM, consulter les métriques, gérer les vCPU.

**Moteur d'éviction**
Surveille la pression mémoire locale via `/proc/meminfo`. Quand la RAM disponible passe sous le seuil configuré, il sélectionne les pages les moins récemment utilisées et les envoie vers un nœud moins chargé. Un canal IPC interne (FaultBus) l'informe en temps réel du trafic de pages : si les défauts de page sont nombreux, il réduit son agressivité pour éviter de créer un ping-pong de pages.

**Quota Registry**
Maintient pour chaque VM un budget strict : `budget_local_mib + budget_distant_mib = max_mem_mib`. Aucune page ne peut être stockée si elle dépasserait le budget distant de la VM. L'invariant est garanti à la fois au niveau du contrôleur Python (à l'admission) et au niveau du daemon Rust (à chaque écriture de page).

**VM Tracker**
Détecte les VMs QEMU en cours d'exécution sur le nœud local en lisant les fichiers de configuration Proxmox (`/etc/pve/qemu-server/`) et les PID QEMU. Maintient un compteur de pages distantes par VM.

**Balloon Monitor**
Communique avec QEMU via le protocole QMP (socket Unix) pour lire les statistiques internes de mémoire de chaque VM (RAM libre côté guest, nombre de défauts de page majeurs). Permet d'ajuster le budget distant d'une VM si son OS guest libère de la mémoire (le balloon virtio réduit la RAM physique consommée).

**vCPU Scheduler**
Gère l'allocation élastique des vCPU : chaque machine physique expose 3 vCPU par cœur physique. Une VM démarre avec un minimum de vCPU et reçoit des vCPU supplémentaires (hotplug) selon sa charge CPU, jusqu'à son plafond déclaré à la création. Un slot vCPU peut être partagé entre au maximum 3 VMs simultanément (le scheduler CFS du noyau Linux gère le découpage temporel). Si un nœud est saturé (steal time > 10%), la migration de la VM vers un nœud moins chargé est recommandée.

**GPU Multiplexer**
Un seul GPU physique par nœud est partagé entre toutes les VMs du nœud. Le multiplexeur écoute sur un socket Unix, reçoit les commandes GPU de chaque VM, les soumet au GPU physique (via le driver kernel), et renvoie les résultats. Chaque VM dispose d'un budget VRAM configurable — les allocations qui dépasseraient ce budget sont rejetées.

**TLS**
Le canal TCP entre nœuds (port 9100) est chiffré. Le daemon génère automatiquement un certificat auto-signé au premier démarrage et vérifie l'empreinte des pairs (modèle TOFU — Trust On First Use).

---

### omega-controller (Python)

Processus unique qui s'exécute en arrière-plan et prend les décisions de gestion du cluster. Il interroge l'API de chaque nœud, analyse l'état global, et envoie des commandes aux daemons.

**Collecteur résilient**
Collecte périodiquement l'état de chaque nœud via son API HTTP. Si un nœud ne répond pas, un disjoncteur (circuit-breaker) l'isole temporairement et le contrôleur continue de fonctionner avec les données en cache (valides pendant 120 secondes par défaut).

**Moteur d'admission**
Décide si une nouvelle VM peut être créée dans le cluster. Il vérifie que la capacité mémoire totale (locale + distante) est suffisante, sélectionne le nœud le plus adapté, calcule la répartition `local_budget / remote_budget` et envoie la configuration au daemon du nœud cible. L'invariant `local + remote = max_mem` est vérifié mathématiquement à cette étape.

**Moteur de placement topologique**
Prend en compte la topologie physique du cluster (rack, zone réseau, latence) pour éviter de placer les pages distantes d'une VM trop loin de son nœud hôte. Score multi-critères : disponibilité RAM (50%), topologie réseau (25%), charge CPU (15%), migrations actives (10%).

**Moteur de migration**
Surveille les VMs candidates à la migration : VMs dont les pages distantes dépassent un seuil (trop d'accès distants → latence élevée), VMs sur des nœuds surchargés. Déclenche les migrations Proxmox via l'API REST Proxmox. Tente d'abord une migration live (VM continue de fonctionner) et bascule sur une migration offline si la migration live échoue.

**Moniteur LXC**
Surveille les conteneurs LXC en plus des VMs QEMU. Lit les fichiers PSI (Pressure Stall Information) des cgroups v2 pour détecter une pression mémoire sur les conteneurs, sans avoir besoin d'un agent dans le conteneur.

**Admission CPU**
Gère l'allocation de vCPU à l'échelle du cluster. Sélectionne le nœud avec le plus de slots vCPU libres et non surchargé pour héberger une nouvelle VM.

**Admission GPU**
Gère les budgets VRAM à l'échelle du cluster. Garantit qu'aucun nœud ne se voit attribuer plus de VRAM que son GPU physique n'en possède.

---

## Protocoles

### Protocole TCP store (nœud → nœud)

Protocole binaire sur TCP, header fixe de 20 octets :

```
┌──────────┬──────────┬──────────┬──────────┬──────────────────┐
│ magic 2B │ opcode 1B│ flags 1B │ vm_id 4B │ page_id 8B       │
├──────────┴──────────┴──────────┴──────────┴──────────────────┤
│ payload_len 4B  │  payload (0 à 8192 octets)                 │
└─────────────────┴───────────────────────────────────────────┘
```

Opcodes : `PUT_PAGE`, `GET_PAGE`, `DELETE_PAGE`, `PING`, `PONG`, `OK`, `NOT_FOUND`, `ERROR`, `STATS_REQUEST`, `STATS_RESPONSE`.

Les pages peuvent être compressées (flag `FLAG_COMPRESSED = 0x01`).

### Protocole GPU (VM → daemon)

Protocole binaire sur socket Unix, header fixe de 22 octets :

```
┌────────┬─────────┬────────┬────────┬──────────────┬──────────┐
│magic 4B│version 1│type  1B│vm_id 4B│  seq 4B      │p_len 4B  │
├────────┴─────────┴────────┴────────┴──────────────┴──────────┤
│ priority 1B │ reserved 3B │ payload (variable)               │
└─────────────┴─────────────┴──────────────────────────────────┘
```

Types : `GPU_CMD`, `GPU_RESULT`, `GPU_ALLOC`, `GPU_ALLOC_RESP`, `GPU_FREE`, `GPU_SYNC`, `GPU_ERROR`.

---

## État des fonctionnalités

| Fonctionnalité | Composant | État |
|---------------|-----------|------|
| Store TCP de pages | node-bc-store | Stable (V1) |
| Agent userfaultfd | node-a-agent | Stable (V1) |
| Daemon unifié | omega-daemon | Opérationnel (V4) |
| Canal IPC FaultBus | fault_bus.rs | Intégré (V4) |
| Quotas RAM par VM | quota.rs | Intégré (V4) |
| Persistance pages (sled) | persistent_store.rs | Intégré (V4) |
| TLS inter-nœuds | tls.rs | Intégré (V4) |
| Circuit-breaker collecteur | resilient_collector.py | Intégré (V4) |
| Migration offline fallback | proxmox.py | Intégré (V4) |
| QoS réseau (tc/HTB) | setup_qos.sh | Intégré (V4) |
| Moniteur LXC cgroup | lxc_monitor.py | Intégré (V4) |
| Placement topologique | topology_placement.py | Intégré (V4) |
| Contrôle d'admission RAM | admission.py | Intégré (V4) |
| Scheduler vCPU élastique | vcpu_scheduler.rs | Intégré (V4) |
| Admission CPU cluster | cpu_admission.py | Intégré (V4) |
| Multiplexeur GPU | gpu_multiplexer.rs | Intégré (V4) |
| Protocole GPU binaire | gpu_protocol.rs | Intégré (V4) |
| Admission GPU cluster | gpu_admission.py | Intégré (V4) |
| Migration live/cold executor | vm_migration.rs | Intégré (V4) |
| Politique migration cluster | migration_policy.py | Intégré (V4) |
| vCPU + CPU par VM dans /control/status | node_state.rs | Intégré (V4) |
| État cluster cross-nœuds | cluster.rs | Intégré |
| Compaction mémoire cross-nœuds | compaction.rs | Intégré |
| GPU placement auto (PCI sysfs + migration) | gpu_placement.rs | Intégré |
| GPU partage round-robin QMP + flock | gpu_scheduler.rs | Intégré |
| Serveur métriques Prometheus | metrics_server.rs | Intégré |
| Agent migration RAM/CPU | migration.rs | Intégré |
| Pool vCPU flock + cgroup v2 + hotplug | vcpu_scheduler.rs (node-a-agent) | Intégré |
| Backend Ceph RADOS | ceph_store.rs | Intégré |
| Surveillance disque statvfs | hardware.rs | Intégré |
| Nettoyage pages orphelines | orphan_cleaner.rs | Intégré |
| Serveur HTTP status store | status_server.rs | Intégré |

---

## Structure du dépôt

```
omega-remote-paging/
├── Cargo.toml                  Workspace Rust
├── Makefile                    Cibles de build et test
│
├── node-a-agent/               Agent uffd V1 (nœud hébergeant les VMs)
│   └── src/
│       ├── uffd.rs             Interface userfaultfd kernel
│       ├── memory.rs           Région mémoire + éviction/fetch
│       ├── remote.rs           Client TCP pool vers les stores
│       ├── clock_eviction.rs   Algorithme CLOCK
│       ├── prefetch.rs         Préfetch séquentiel
│       ├── cluster.rs          ClusterState, NodeStatus, local_available_mib()
│       ├── compaction.rs       ClusterCompactor (compaction mémoire cross-nœuds)
│       ├── gpu_placement.rs    GpuPlacementDaemon : détection GPU PCI + migration offline
│       ├── gpu_scheduler.rs    GpuScheduler : partage QMP round-robin, leader flock
│       ├── metrics_server.rs   Serveur HTTP métriques Prometheus (port 9300)
│       ├── migration.rs        MigrationAgent : détection pression, recall, qm migrate
│       └── vcpu_scheduler.rs   VCpuScheduler : pool flock, cgroup v2, hotplug, overcommit 3×
│
├── node-bc-store/              Store TCP V1 (nœuds stockant les pages)
│   └── src/
│       ├── store.rs            Index DashMap + list_vm_ids(), delete_vm(), evict_lru()
│       ├── protocol.rs         Protocole binaire TCP
│       ├── server.rs           Serveur TCP async — AnyStore (Ram | Ceph)
│       ├── persistent_store.rs Journalisation sled (durabilité)
│       ├── ceph_store.rs       CephStore : backend Ceph RADOS, OID "{vm_id:08x}_{page_id:016x}"
│       ├── hardware.rs         Surveillance disque via statvfs, alerte < 5%
│       ├── orphan_cleaner.rs   OrphanCleaner : cross-ref pvesh, grâce 10 min
│       └── status_server.rs    Serveur HTTP status :9200 (RAM, pages, vcpu, ceph_enabled)
│
├── omega-daemon/               Daemon unifié V4 (tous rôles)
│   └── src/
│       ├── main.rs             Point d'entrée — lance toutes les tâches
│       ├── node_state.rs       État partagé entre composants
│       ├── store_server.rs     Store TCP avec intégration VM Tracker
│       ├── cluster_api.rs      API HTTP cluster (/api/*)
│       ├── control_api.rs      API HTTP contrôle (/control/*)
│       ├── eviction_engine.rs  Moteur d'éviction CLOCK + déclencheur migration
│       ├── fault_bus.rs        IPC uffd ↔ moteur éviction + AdaptiveInterval
│       ├── balloon.rs          Monitor virtio-balloon QMP
│       ├── quota.rs            Quotas RAM par VM
│       ├── tls.rs              TLS sur canal store TCP (TOFU)
│       ├── vm_tracker.rs       Détection VMs QEMU locales
│       ├── vcpu_scheduler.rs   Scheduler vCPU élastique (hotplug, steal)
│       ├── vm_migration.rs     Executor migration live/cold + MigrationPolicy
│       ├── gpu_multiplexer.rs  Multiplexeur GPU (budget VRAM)
│       └── gpu_protocol.rs     Protocole binaire GPU
│
├── controller/                 Contrôleur Python (décisions cluster)
│   └── controller/
│       ├── admission.py        Contrôle d'admission RAM
│       ├── cpu_admission.py    Contrôle d'admission vCPU
│       ├── gpu_admission.py    Contrôle d'admission GPU
│       ├── placement.py        Placement de base
│       ├── topology_placement.py Placement topologique
│       ├── resilient_collector.py Collecteur avec circuit-breaker
│       ├── migration_daemon.py Daemon de migration (Proxmox API — legacy)
│       ├── migration_policy.py Politique migration live/cold (omega API)
│       ├── lxc_monitor.py      Moniteur cgroup LXC
│       ├── proxmox.py          Client API Proxmox REST
│       └── main.py             Point d'entrée CLI (daemon, migrate, monitor…)
│
├── scripts/
│   ├── setup_qos.sh            Configuration QoS réseau (tc/HTB)
│   ├── omega-hook.pl           Hookscript Proxmox pre/post-start/stop
│   ├── cluster.conf            Configuration cluster exemple
│   └── systemd/               Units systemd (omega-daemon, omega-controller, …)
│
└── docs/                       Documentation technique
```

---

## Ce que ce projet ne fait pas

- Il ne remplace pas la couche de virtualisation de Proxmox (QEMU/KVM reste inchangé).
- Il ne modifie pas le noyau Linux (tout fonctionne en espace utilisateur via userfaultfd).
- Il ne gère pas la persistance des données des VMs (les disques virtuels restent inchangés).
- Il n'assure pas une haute disponibilité au sens Proxmox HA (pas de redémarrage automatique en cas de panne nœud — c'est le rôle de Proxmox HA natif).
- Il ne compresse pas les pages mémoire (compression optionnelle mais non activée par défaut).
