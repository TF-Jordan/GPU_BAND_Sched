# Architecture — omega-remote-paging V4

## Vue d'ensemble

```
┌─────────────────────────────────────────────────────────────────────┐
│  Cluster Proxmox (N nœuds identiques)                               │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  Chaque nœud (ex: pve, pve2, pve3)                           │   │
│  │                                                              │   │
│  │  VMs QEMU/KVM                                               │   │
│  │    VM 100 : 8 Gio RAM max                                   │   │
│  │    VM 101 : 4 Gio RAM max                                   │   │
│  │         │ page fault (userfaultfd)                          │   │
│  │  ┌──────▼──────────────────────────────────────────────┐   │   │
│  │  │  omega-daemon (Rust + Tokio)                         │   │   │
│  │  │                                                      │   │   │
│  │  │  ┌─────────────┐  ┌─────────────┐  ┌────────────┐  │   │   │
│  │  │  │ Store TCP   │  │ Eviction    │  │ vCPU       │  │   │   │
│  │  │  │ :9100 (TLS) │  │ Engine      │  │ Scheduler  │  │   │   │
│  │  │  └─────────────┘  └─────────────┘  └────────────┘  │   │   │
│  │  │  ┌─────────────┐  ┌─────────────┐  ┌────────────┐  │   │   │
│  │  │  │ Cluster API │  │ VM Tracker  │  │ Migration  │  │   │   │
│  │  │  │ :9200       │  │             │  │ Executor   │  │   │   │
│  │  │  └─────────────┘  └─────────────┘  └────────────┘  │   │   │
│  │  │  ┌─────────────┐  ┌─────────────┐  ┌────────────┐  │   │   │
│  │  │  │ Control API │  │ Quota       │  │ Fault Bus  │  │   │   │
│  │  │  │ :9300       │  │ Registry    │  │ (IPC)      │  │   │   │
│  │  │  └─────────────┘  └─────────────┘  └────────────┘  │   │   │
│  │  └──────────────────────────────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  Le même daemon tourne sur chaque nœud (rôle symétrique :           │
│  héberge des VMs ET stocke les pages des autres nœuds)              │
│                                                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  omega-controller (Python — tourne sur un seul nœud)        │   │
│  │  Nœud contrôleur défini par OMEGA_CONTROLLER (arbitraire)   │   │
│  │                                                              │   │
│  │  MigrationPolicy   → GET /control/status (chaque nœud)      │   │
│  │  MigrationExecutor → POST /control/migrate (nœud source)    │   │
│  │  AdmissionEngine   → contrôle RAM/vCPU/GPU à l'admission    │   │
│  │  CgroupCpuMonitor  → lecture cgroups v2 en temps réel       │   │
│  └──────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

## Flux de données — éviction d'une page

```
1. Pression mémoire détectée (mem_available < seuil)
2. EvictionEngine sélectionne les pages froides (algorithme CLOCK)
3. Page envoyée via PUT_PAGE TCP (TLS) → store d'un autre nœud du cluster
4. madvise(MADV_DONTNEED) → page libérée du page table local
5. Page marquée "distante" dans le VM Tracker
```

## Flux de données — récupération d'une page (page fault)

```
1. VM accède à une adresse dont la page est distante
2. Kernel intercepte le page fault (userfaultfd)
3. omega-daemon reçoit l'événement via FaultBus
4. GET_PAGE TCP (TLS) → nœud qui stocke la page
5. UFFDIO_COPY → page copiée en RAM locale
6. VM reprend son exécution (<2ms sur GbE)
```

## Flux de données — migration live

```
1. MigrationPolicy détecte un nœud surchargé (RAM, CPU ou GPU réservé)
2. Le controller choisit automatiquement un nœud Y qui peut accueillir la VM
3. POST /control/migrate sur nœud source
4. MigrationExecutor lance : qm migrate {vmid} {target} --online
5. KVM pre-copy : RAM transférée page par page (VM reste active)
6. Bascule finale : downtime < 1s
7. cleanup_after_migration() : supprime pages store, libère slots vCPU, quotas RAM et budgets GPU
```

## Flux de données — migration cold

```
Identique sauf :
  - VM stoppée avant transfert (qm migrate sans --online)
  - Pas de dirty pages à gérer
  - Forcée si RAM > 95% (live ne converge pas) ou VM idle > 60s
```

## Composants du daemon (omega-daemon, Rust)

### Store TCP (port 9100, TLS)
Accepte les pages envoyées par d'autres nœuds. Index `DashMap<(vm_id, page_id) → [u8; 4096]>`, sharded, sans verrou global. Chiffrement TLS avec certificats auto-signés, vérification par empreinte (TOFU).

### Cluster API (port 9200, HTTP)
Expose l'état du nœud : RAM, pages stockées, VMs locales avec leur `avg_cpu_pct`, `throttle_ratio`, `remote_pages`, ainsi que l'état GPU local (backend, VRAM totale, VRAM libre, budgets réservés). Interrogé par le controller Python.

### Control API (port 9300, HTTP)
Canal de contrôle local : éviction, quotas par VM, vCPU, budgets GPU, migrations live/cold, métriques Prometheus (`/control/metrics`) et état GPU (`/control/gpu/status`).

### Eviction Engine
Surveille `/proc/meminfo`. Quand `mem_available < seuil`, sélectionne les pages froides (CLOCK) et les envoie au nœud avec le meilleur `placement_score`. Taux d'éviction adapté via `AdaptiveInterval` selon la pression du FaultBus.

### Fault Bus
Canal IPC interne (tokio mpsc) entre le handler uffd et l'EvictionEngine. Chaque page fault notifie le bus → l'EvictionEngine accélère ou ralentit en conséquence.

### Quota Registry
Budget par VM : `max_mem_mib` (local + distant). Aucune page ne peut être stockée si le quota est dépassé. Invariant garanti à chaque PUT_PAGE.

### VM Tracker
Détecte les VMs QEMU via `/var/run/qemu-server/{vmid}.pid` et `/etc/pve/qemu-server/{vmid}.conf`. Compteurs de pages distantes par VM.

### vCPU Scheduler
Allocation élastique : 3 vCPU par pCPU, jusqu'à 3 VMs par slot. Hotplug si usage > 80%. Si le hotplug réel via QMP n'est plus possible ou si le nœud reste trop throttlé, l'état remonté par le scheduler pousse le controller à migrer automatiquement la VM vers un autre nœud.

### Migration Executor
Lance `qm migrate {vmid} {target} [--online]` via `tokio::process::Command`. Ceph RBD : disques partagés, seule la RAM est transférée (live) ou la VM redémarre sur le même disque (cold). Suivi par task_id. Nettoyage post-migration : supprime les pages store, libère les slots vCPU, retire le quota.

### GPU Multiplexer
Partage d'un GPU physique entre plusieurs VMs. Budget VRAM configurable par VM via `/control/vm/{vmid}/gpu`, état exposé via `/control/gpu/status`, contraintes VRAM remontées au controller pour le placement et la migration.

### GPU Placement Daemon (node-a-agent)
Détecte les VMs nécessitant un GPU via `qm config <vmid>` et la classe PCI sysfs `0x03xx`. Si la VM courante n'est pas sur un nœud GPU, migre la VM offline (`qm migrate <vmid> <target>`) vers le premier nœud GPU disponible dans le cluster, puis configure `hostpci0` pour le passthrough.

### GPU Scheduler (node-a-agent)
Partage round-robin d'un GPU physique entre plusieurs VMs via QMP (`device_del` / `device_add`). Leader election par `flock` sur `/run/omega-gpu-scheduler-<pci>.lock` — un seul scheduler actif par GPU sur le nœud. Reset GPU implicite via l'ioctl VFIO `VFIO_DEVICE_RESET` (sans module externe).

### Balloon Manager (node-a-agent)
Thin-provisioning RAM côté guest : la VM démarre avec `balloon_initial_mib` visibles (défaut 512 MiB) et grandit progressivement jusqu'à `region_mib` (plafond fixé à la création). Signal de croissance : taux de page-faults par seconde (`metrics.fault_count`). Si `fault_rate ≥ grow_faults_per_sec` → `qm balloon <vmid> <mib+step>` ; si `fault_rate ≤ shrink_faults_per_sec` → récupération. Aucune RAM n'est réservée sur les stores tant que les pages ne sont pas effectivement accédées par le guest.

### Migration Agent (node-a-agent)
Détecte la pression RAM et CPU du nœud local. Identifie un nœud cible via l'API status avec un critère **relatif** : le nœud cible doit être meilleur que le nœud courant (plus de RAM libre **ou** plus de vCPU libres). Veto absolu si la VM requiert un GPU et que le nœud cible n'en a pas. Effectue un recall complet des pages distantes avant de déclencher `qm migrate <vmid> <target> --online`.

### vCPU Scheduler (node-a-agent)
Pool vCPU partagé persisté dans `/run/omega-vcpu-pool.json` avec accès coordonné par `flock`. Mesure la charge CPU via les compteurs cgroup v2 `usage_usec` avec fallback sur `/proc/<pid>/stat`. Hotplug via `qm set --vcpus N`. Overcommit 3× au maximum. Indicateur `cpu_pressure` (AtomicBool) exposé aux décideurs de migration.

### Metrics Server (node-a-agent)
Serveur HTTP exposant les métriques Prometheus de l'agent sur le port 9300.

### Orphan Cleaner (node-bc-store)
Toutes les 5 minutes, compare les vm_ids présents dans le store avec la liste des VMs actives obtenue via `pvesh get /cluster/resources --type vm`. Après un délai de grâce de 10 minutes, supprime toutes les pages des VMs absentes du cluster.

### Status Server (node-bc-store)
Serveur HTTP sur le port 9200 exposant : RAM disponible, pages stockées, `vcpu_total`, `vcpu_free`, `ceph_enabled`. Consulté par les agents et le controller pour le placement.

### Ceph Store (node-bc-store)
Backend de stockage Ceph RADOS utilisé à la place du store RAM quand librados est disponible à la compilation et que `/etc/ceph/ceph.conf` est présent. Format OID : `"{vm_id:08x}_{page_id:016x}"`. La réplication write-through inter-stores est automatiquement désactivée si tous les stores rapportent `ceph_enabled: true`.

## Composants du controller (Python)

### MigrationPolicy
Vue cluster-wide. Collecte `GET /control/status` sur les 3 nœuds. Décide : quoi migrer, vers où, live ou cold. Règles : RAM > 95% → cold forcée, RAM 85–95% → live, VM idle > 60s → cold, VM throttlée → live, GPU réservé trop haut → migration vers un nœud avec assez de VRAM libre.

### AdmissionEngine
À la création d'une VM : sélectionne le nœud cible, calcule le budget `local + remote = max_mem`, envoie la config au daemon.

### CgroupCpuMonitor
Lecture des cgroups v2 en temps réel (fenêtre glissante 100ms). Callbacks CPU_HIGH / CPU_IDLE / THROTTLE. Alimente le `VcpuScheduler` du daemon via `update_from_cgroup()`.

### TopologyPlacement
Score multi-critères pour le placement : RAM (50%), topologie réseau (25%), charge CPU (15%), migrations actives (10%).

### ResilientCollector
Collecte avec circuit-breaker. Nœud injoignable → isolation temporaire, données en cache (120s).

## Distribution des pages entre nœuds

Sélection déterministe : `hash(vm_id, page_id) % num_stores`.
Réplication factor configurable (défaut : 2 — chaque page sur 2 nœuds).

## Ports réseau

| Port | Protocole | Usage |
|------|-----------|-------|
| 9100 | TCP + TLS | Store de pages (inter-nœuds) |
| 9200 | HTTP | API cluster / status store (état du nœud, vcpu, ceph_enabled) |
| 9300 | HTTP | API contrôle daemon (local) + métriques Prometheus agent |
