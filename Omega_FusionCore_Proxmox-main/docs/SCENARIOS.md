# Omega — Scénarios complets

> Chaque scénario décrit l'état initial, ce qui se passe pas à pas dans le système,
> et l'état final. Les scénarios mixtes combinent plusieurs axes simultanément.

---

## Table des matières

**Scénarios simples**
- [S1 — VM démarre avec plus de RAM que disponible localement](#s1--vm-démarre-avec-plus-de-ram-que-disponible-localement)
- [S2 — VM monte en charge : CPU hotplug automatique](#s2--vm-monte-en-charge--cpu-hotplug-automatique)
- [S3 — Nœud plein en vCPUs : migration directe](#s3--nœud-plein-en-vcpus--migration-directe)
- [S4 — Aucun nœud n'a assez de vCPUs : bin-packing](#s4--aucun-nœud-na-assez-de-vcpus--bin-packing)
- [S5 — RAM cluster saturée : solution et limites](#s5--ram-cluster-saturée--solution-et-limites)
- [S6 — GPU : VM demande de la VRAM indisponible localement](#s6--gpu--vm-demande-de-la-vram-indisponible-localement)
- [S7 — Disque : un backup étouffe les VMs de prod](#s7--disque--un-backup-étouffe-les-vms-de-prod)
- [S8 — Nœud qui tombe](#s8--nœud-qui-tombe)

**Scénarios mixtes**
- [M1 — RAM + CPU : VM qui grossit sur tous les axes](#m1--ram--cpu--vm-qui-grossit-sur-tous-les-axes)
- [M2 — CPU saturé + RAM saturée : double migration](#m2--cpu-saturé--ram-saturée--double-migration)
- [M3 — GPU + CPU : placement multi-contraintes](#m3--gpu--cpu--placement-multi-contraintes)
- [M4 — Cluster complet sur tous les axes](#m4--cluster-complet-sur-tous-les-axes)
- [M5 — Migration live pendant une pression mémoire](#m5--migration-live-pendant-une-pression-mémoire)
- [M6 — Rafale de démarrages simultanés](#m6--rafale-de-démarrages-simultanés)
- [M7 — Réorganisation totale pour maintenance](#m7--réorganisation-totale-pour-maintenance)

---

## S1 — VM démarre avec plus de RAM que disponible localement

**État initial**
```
Nœud 1 : 64 Go total, 8 Go libres
Nœud 2 : 64 Go total, 40 Go libres
Nœud 3 : 64 Go total, 42 Go libres
```

**Action** : `qm start 100` — VM 100 configurée avec 32 Go de RAM

**Ce qui se passe**

```
t=0s   hookscript pre-start :
         omega-qemu-launcher prepare --vm-id 100 --size-mib 32768
         → memfd de 32 Go créé (virtuel, 0 page physique allouée)
         → node-a-agent démarre, enregistre la région userfaultfd

t=1s   QEMU démarre via kvm-omega :
         -object memory-backend-file,id=ram0,size=32768M,mem-path=/proc/<agent>/fd/<fd>,share=on
         -machine ...,memory-backend=ram0
         QEMU mappe le memfd de l'agent → voit 32 Go disponibles

t=5s   Guest boot (kernel + services) :
         ~800 Mo accédés → 800 Mo physiques alloués sur Nœud 1
         RAM locale utilisée : 8 Go → 8.8 Go

t=2min  Guest en fonctionnement normal :
         6 Go actifs → RAM locale : ~14 Go
         Seuil 75% = 48 Go → pas encore de pression, aucune éviction

t=30min Peak de charge :
         20 Go actifs → RAM locale dépasse 48 Go (75%)
         Moteur CLOCK démarre l'éviction :
           pages froides VM 100 → Nœud 2 (primaire)
           répliques → Nœud 3
         RAM locale redescend à 46 Go
         12 Go de VM 100 sont sur Nœud 2 et Nœud 3
```

**État final**
```
Nœud 1 : 20 Go de VM 100 en local (pages chaudes)
Nœud 2 : 12 Go de pages de VM 100 (store)
Nœud 3 : 12 Go de pages de VM 100 (réplica)
VM 100  : voit 32 Go disponibles, aucune interruption
```

**Point clé** : l'`AdmissionController` vérifie avant le démarrage que
`cluster_free (8+40+42 = 90 Go) ≥ 32 Go`. Si ce n'était pas le cas,
`qm start` serait bloqué avec un message d'erreur explicite.

---

## S2 — VM monte en charge : CPU hotplug automatique

**État initial**
```
VM 100 sur Nœud 1 : 2 vCPUs actifs, maxcpus=8
Nœud 1 : 4 vCPUs physiques libres
```

**Action** : la VM lance un traitement intensif (compilation, ML training...)

**Ce qui se passe**

```
t=0ms   CPU usage VM 100 : 20%
t=500ms CgroupCpuMonitor (polling 1ms) : usage monte à 82%
         → CpuPressureEvent émis (seuil HOTPLUG_TRIGGER_PCT = 80%)
t=501ms VcpuScheduler reçoit l'événement
         safe_vcpu_floor(55%) = ceil(55/35) = 2 → minimum 2
         desired = 4 vCPUs (charge ÷ seuil = 82÷80 × 2 ≈ 2 → arrondi au-dessus)
t=502ms QMP : device_add virtio-cpu-pci,id=cpu2
         QMP : device_add virtio-cpu-pci,id=cpu3
         → VM passe à 4 vCPUs
t=600ms CPU usage : 48% (charge distribuée sur 4 vCPUs)

[1h plus tard]
t=3600s CPU usage redescend à 18% (traitement terminé)
         DOWNSCALE_TRIGGER_PCT = 35% → safe_vcpu_floor(18%) = 1
         desired = 2 vCPUs
t=3601s QMP : device_del virtio-cpu-pci,id=cpu3
         QMP : device_del virtio-cpu-pci,id=cpu2
         → VM repasse à 2 vCPUs
         → Nœud 1 récupère 2 vCPUs physiques pour les autres VMs
```

**État final**
```
VM 100 : 2 vCPUs (retour à la normale)
Nœud 1 : 4 vCPUs physiques libres (comme au départ)
```

---

## S3 — Nœud plein en vCPUs : migration directe

**État initial**
```
Nœud 1 : 32 threads physiques, 0 libres (toutes les VMs en chargent 100%)
Nœud 2 : 32 threads physiques, 8 libres
Nœud 3 : 32 threads physiques, 14 libres
VM 100 sur Nœud 1 : 2 vCPUs, maxcpus=8, usage CPU = 90%
```

**Action** : VcpuScheduler veut ajouter 4 vCPUs à VM 100

**Ce qui se passe**

```
t=0s   VcpuScheduler : desired=6, Nœud 1 vcpu_free=0 → impossible localement

t=1s   _ensure_vm_vcpu_profile :
         _best_reconciliation_target(desired_min=6) :
           Nœud 2 : vcpu_free=8  ≥ 6 ✓, mem_ok ✓  → candidat
           Nœud 3 : vcpu_free=14 ≥ 6 ✓, mem_ok ✓  → meilleur candidat
         → Nœud 3 sélectionné (plus de ressources)

t=2s   POST /control/migrate {vm_id: 100, target: "pve-node3", type: "live"}
         QEMU live migration démarre :
           - pre-copy : pages mémoire copiées en arrière-plan (VM continue)
           - stop-and-copy : dernière synchro (VM figée ~100ms)
           - VM reprend sur Nœud 3

t=8s   VM 100 tourne sur Nœud 3
         VcpuScheduler : vcpu_free=14, desired=6
         QMP : device_add ×4 → VM passe à 6 vCPUs
         CPU usage : 38%
```

**État final**
```
Nœud 1 : VM 100 absente (ses vCPUs sont libérés)
Nœud 3 : VM 100 avec 6 vCPUs, CPU usage 38%
```

---

## S4 — Aucun nœud n'a assez de vCPUs : bin-packing

**État initial**
```
Nœud 1 : 32 threads, 1 libre   — VM 100 ici, veut 6 vCPUs
Nœud 2 : 32 threads, 3 libres  — VMs 201, 202, 203 ici
Nœud 3 : 32 threads, 4 libres  — VMs 301, 302 ici
```
VM 100 veut 6 vCPUs, aucun nœud n'en a 5 de libres d'un coup.

**Ce qui se passe**

```
t=0s   _best_reconciliation_target(desired=6) → None (aucun nœud ≥ 6)
       _best_partial_reconciliation_target    → None (aucun nœud > 1 libre)

t=1s   _find_vcpu_consolidation_plan() démarre :

       Candidats triés par vcpu_free :
         Nœud 3 (4 libres) → needed = 6-4 = 2 vCPUs supplémentaires
           VMs sur Nœud 3 triées par charge :
             VM 302 (avg=60%) → estimé 3 vCPUs libérés
               Destination ? Nœud 2 (3 libres) → peut accueillir VM 302 ✓
               → ajouter au plan : (VM 302, Nœud3 → Nœud2)
             freed=3 ≥ 2 ✓
           Nœud 3 projeté : 4+3=7 vCPUs libres ≥ 6 ✓
           → Plan trouvé !

t=2s   Exécution du plan :
         POST migrate {vm_id:302, from:node3, to:node2}
         [VM 302 live-migrée vers Nœud 2 — 6 secondes]

t=8s   Nœud 3 : 7 vCPUs libres
         POST migrate {vm_id:100, from:node1, to:node3}
         [VM 100 live-migrée vers Nœud 3 — 6 secondes]

t=14s  VM 100 sur Nœud 3 : QMP device_add ×4 → 6 vCPUs
```

**État final**
```
Nœud 1 : VM 100 absente
Nœud 2 : VM 302 ajoutée (3 libres → 0 libre)
Nœud 3 : VM 100 avec 6 vCPUs (7-6=1 libre)
```

**Ce qui a changé dans le code** : `_find_vcpu_consolidation_plan()` implémentée
dans `controller/main.py`. Elle fait un bin-packing greedy : pour chaque nœud
candidat, elle cherche les VMs à déplacer (en respectant les cooldowns de migration)
pour atteindre le seuil nécessaire.

---

## S5 — RAM cluster saturée : solution et limites

**État initial**
```
Nœud 1 : 64 Go total, 2 Go libres
Nœud 2 : 64 Go total, 1 Go libre
Nœud 3 : 64 Go total, 3 Go libres
Total cluster libre : 6 Go
```

**Cas 1 — Nouvelle VM demande 4 Go** (< 6 Go libres)

```
AdmissionController :
  cluster_free = 6 Go ≥ 4 Go → admission acceptée
  Nœud 3 (3 Go libres) sélectionné pour le placement
  local_budget = 3 Go, remote_budget = 1 Go

VM démarre :
  3 Go en local sur Nœud 3
  1 Go peut déborder vers Nœud 1 ou 2 si nécessaire
```

**Cas 2 — Nouvelle VM demande 10 Go** (> 6 Go libres)

```
AdmissionController :
  cluster_free = 6 Go < 10 Go → REFUS
  Message : "capacité cluster insuffisante : 6 Go dispo < 10 Go demandé"

Options pour débloquer la situation :
```

**Option A — Balloon : compresser les VMs existantes** *(implémentée)*
```
BalloonManager (tâche proactive toutes les 30s dans omega-daemon) détecte
que certaines VMs utilisent moins que leur max_mem.
  → seuil inflation : RAM disponible guest > 20% de sa RAM actuelle
  → seuil déflation : RAM disponible guest < 10% (priorité guest)
  → plancher garanti : jamais en-dessous de 50% de max_mem

Ex : VM 200 configurée à 16 Go mais n'utilise que 9 Go
     (available = 7 Go = 43% > seuil 20%)
     → BalloonManager gonfle : new_target = 9 Go - (7 Go / 2) = 5.5 Go
     → QmpClient::set_balloon_target(5_905_580_032)  [QMP balloon commande]
     → QuotaRegistry::apply_balloon_update(vm_id=200, actual_mib=5632)
        remote_budget de VM 200 passe de 0 à ~10 Go
     → Nœud 1 a maintenant ~10 Go libres
     → Admission de la VM 10 Go acceptée
```

**Option B — Éteindre une VM non critique**
```
qm stop 201   # VM de dev/test
→ ses pages distantes sont supprimées (DELETE /control/pages/201)
→ cluster_free augmente
```

**Option C — Ajouter un nœud au cluster**
```
# Nouveau nœud 4 (192.168.1.4) avec 64 Go
OMEGA_STORES="192.168.1.1:9100,192.168.1.2:9100" bash scripts/omega-proxmox-install.sh
omega-daemon --node-id pve-node4 --node-addr 192.168.1.4 \
             --peers 192.168.1.1:9200,192.168.1.2:9200,192.168.1.3:9200
# Le controller le détecte automatiquement au prochain cycle
# cluster_free : 6 + 64 = 70 Go → admission possible
```

**Limite réelle** : Omega ne crée pas de RAM ex nihilo. Il redistribue ce qui existe.
Si la somme de toute la RAM physique du cluster est inférieure à la somme de toutes
les VMs actives, il n'y a pas de solution sans éteindre des VMs ou ajouter du matériel.

---

## S6 — GPU : VM demande de la VRAM indisponible localement

**État initial**
```
Nœud 1 : GPU 8 Go VRAM, 1 Go libre  — VM 100 demande 4 Go VRAM
Nœud 2 : GPU 8 Go VRAM, 6 Go libres
Nœud 3 : pas de GPU
```

**Ce qui se passe**

```
t=0s   controller cycle :
         VM 100 sur Nœud 1, gpu_budget=4096 Mio
         gpu_free_vram_mib=1024 < 4096 → placement GPU impossible en local

t=1s   _ensure_vm_gpu_placement :
         _gpu_candidate_target(source=nœud1, gpu_budget=4096) :
           Nœud 2 : gpu_total=8192, gpu_free=6144 ≥ 4096 ✓
           amélioration : nœud1.gpu_used_pct - nœud2.gpu_used_pct = 87%-25% = 62% ≥ 10%
           → Nœud 2 sélectionné

t=2s   POST migrate {vm_id:100, target:"pve-node2", type:"live"}
t=8s   VM 100 sur Nœud 2 :
         GpuMultiplexer alloue 4096 Mio de VRAM sur le GPU de Nœud 2
         VM 100 peut maintenant utiliser son GPU
```

**Cas alternatif : tous les GPUs pleins**

```
Nœud 1 : GPU 8 Go, 0 Go libre
Nœud 2 : GPU 8 Go, 3 Go libre  — mais VM 200 (idle, 4 Go VRAM) est là
Nœud 3 : GPU 8 Go, 1 Go libre  — VM 201 (idle, 6 Go VRAM) est là

VM 100 demande 4 Go VRAM → aucun nœud n'a 4 Go libres directement.

_find_gpu_space_creation_plan :
  Nœud 2 : needs_free = 4-3 = 1 Go
    VM 200 (4 Go, idle) → peut aller sur Nœud 3 (GPU 1 Go libre + 4 Go de VM 201)
    → non : Nœud 3 n'a que 1 Go GPU libre, VM 200 a besoin de 4 Go → impossible

  Nœud 3 : needs_free = 4-1 = 3 Go
    VM 201 (6 Go, idle) → peut aller sur Nœud 2 (3 Go libres) ? non (6>3)
    VM 201 → peut aller sur Nœud 1 (0 Go libres) ? non
    → impossible

→ aucun plan GPU trouvé
→ VM 100 démarre sans GPU (mode CPU software rendering, dégradé)
→ warning dans les logs, recheck toutes les 5 secondes
```

---

## S7 — Disque : un backup étouffe les VMs de prod

**État initial**
```
VM 100 (backup rsync)  : io.pressure = 92%, io.weight = 100
VM 101 (serveur web)   : io.pressure = 45%, io.weight = 100
VM 102 (base de données): io.pressure = 68%, io.weight = 100
```

**Ce qui se passe**

```
t=0s   reconcile_local_disk_sharing() sur Nœud 1 :
         VM 100 : pressure=92% > STRESS_THRESHOLD (80%)  → BOOSTED ou DONOR ?
                  → VM 100 est le consommateur excessif : DONOR (io.weight=50)
         VM 101 : pressure=45% < STRESS_THRESHOLD → DEFAULT
                  → mais d'autres VMs sont stressées : BOOSTED (io.weight=200)
         VM 102 : pressure=68% < STRESS_THRESHOLD → DEFAULT
                  → BOOSTED (io.weight=200)

         echo 50  > /sys/fs/cgroup/.../qemu-100.scope/io.weight
         echo 200 > /sys/fs/cgroup/.../qemu-101.scope/io.weight
         echo 200 > /sys/fs/cgroup/.../qemu-102.scope/io.weight

Distribution résultante :
  Total weights = 50 + 200 + 200 = 450
  VM 100 (backup) : 50/450 = 11% des I/O
  VM 101 (web)    : 200/450 = 44% des I/O
  VM 102 (BDD)    : 200/450 = 44% des I/O

t=5min  Le backup se termine.
         VM 100 : io.pressure = 5%
         reconcile : plus de VM stressée → tous remis à DEFAULT (100)
```

**État final**
```
Toutes les VMs : io.weight = 100 (retour équitable)
VM 101 et 102 ont pu traiter leurs requêtes sans interruption
```

---

## S8 — Nœud qui tombe

**État initial**
```
Nœud 1 : VMs 100, 101, 102 — tombe brutalement (kernel panic)
Nœud 2 : VMs 200, 201
Nœud 3 : VMs 300, 301
Stores : pages de VM 100/101/102 partiellement sur Nœuds 2 et 3
```

**Ce qui se passe**

```
t=0s   Nœud 1 tombe

t=5s   controller : GET /control/status sur Nœud 1 → timeout
         → "nœud injoignable, ignoré"

t=5s   Proxmox HA (côté Proxmox natif) détecte la perte du nœud.
         Proxmox redémarre les VMs 100/101/102 sur Nœuds 2 et 3.
         [Omega n'intervient pas dans la décision HA — c'est Proxmox]

t=30s  VM 100 redémarre sur Nœud 2 :
         hookscript pre-start :
           omega-qemu-launcher prepare --vm-id 100 --size-mib 16384
           → nouveau node-a-agent pour VM 100 sur Nœud 2
           → se connecte aux stores (Nœud 2 local + Nœud 3 distant)

         Pages de VM 100 encore présentes sur les stores :
           Nœud 2 : pages primaires de VM 100 (déjà là !)
           Nœud 3 : répliques de VM 100
         → VM 100 retrouve sa mémoire sans tout retransférer

t=35s  VM 100 en fonctionnement sur Nœud 2
         Les pages chaudes sont rechargées à la demande depuis le store local
         (Nœud 2 les a déjà — latence quasi nulle)
```

**Point clé** : la réplication (tokio::join! → Nœud primaire + réplica) permet à VM 100
de retrouver ses pages même si le Nœud 1 était le store primaire de certaines d'entre elles.

---

## M1 — RAM + CPU : VM qui grossit sur tous les axes

**État initial**
```
Nœud 1 : 64 Go RAM (6 libres), 32 threads (8 libres)
VM 100  : 32 Go RAM configurée, 2 vCPUs actifs, maxcpus=8
Charge  : légère (idle)
```

**Action** : un data scientist lance une pipeline ML sur la VM

**Ce qui se passe**

```
t=0min  VM 100 : CPU=15%, RAM utilisée=4 Go, tout en local

t=2min  Pipeline démarre :
          CPU monte à 88% → CpuPressureEvent
          RAM active monte à 12 Go

t=2min5s Hotplug CPU :
           QMP device_add ×2 → VM 100 passe à 4 vCPUs
           CPU usage redescend à 48%

t=5min  Pipeline phase intensive :
          CPU=95% → +2 vCPUs → 6 vCPUs (CPU usage 38%)
          RAM active = 22 Go
          Nœud 1 : 6+22=28 Go utilisés → dépasse 75% du total
          → Éviction CLOCK démarre
          → 8 Go de VM 100 (pages froides du dataset initial) → Nœud 2 et 3

t=20min  Calcul matriciel :
          RAM active monte à 30 Go
          CLOCK continue → 20 Go de VM 100 maintenant sur Nœud 2 et 3
          CPU : 6 vCPUs, usage 42% (stable)

t=45min  Résultats calculés, pipeline termine :
          CPU redescend : usage 12% → 5min plus tard, scale-down → 2 vCPUs
          RAM active redescend : 6 Go
          CLOCK reprend les pages depuis Nœud 2 et 3 au fil des accès
          → prefetch préventif : pages probablement réaccédées préchargées
```

**État final**
```
VM 100 : 2 vCPUs (retour initial), 6 Go actifs en local
         Pages de résultats : encore sur Nœud 2 et 3 (rapatriées à la demande)
         Aucune interruption de la pipeline
```

---

## M2 — CPU saturé + RAM saturée : double migration

**État initial**
```
Nœud 1 : RAM 90% utilisée, CPU 95% utilisé
          VM 100 (web, 4 Go, 2 vCPUs, critique)
          VM 101 (batch, 20 Go, 8 vCPUs, non critique)
          VM 102 (dev, 8 Go, 4 vCPUs, non critique)

Nœud 2 : RAM 40% utilisée, CPU 30% utilisé
Nœud 3 : RAM 55% utilisée, CPU 50% utilisé
```

**Ce qui se passe**

```
t=0s   controller cycle :
         Nœud 1 : ram_used_pct=90% > threshold=75%, cpu_used=95%
         → doit soulager Nœud 1

         MigrationPolicy évalue les VMs de Nœud 1 par ordre de priorité :
           VM 101 (batch, non critique, grosse) → meilleur candidat à migrer
           VM 102 (dev, non critique)           → second candidat

t=1s   Candidat 1 : VM 101 → Nœud 2 (40% RAM, 30% CPU → peut accueillir 20 Go + 8 vCPUs)
         POST migrate {vm_id:101, target:pve-node2, type:live}

t=8s   VM 101 sur Nœud 2 :
         Nœud 1 : RAM 90%→62%, CPU 95%→58% (libéré 20 Go et 8 vCPUs)
         → Nœud 1 respire, plus sous seuil critique

t=9s   controller recheck Nœud 1 : RAM 62% < 75%, CPU 58% < 80%
         → plus de migration nécessaire, VM 102 reste en place
```

**État final**
```
Nœud 1 : VM 100 + VM 102, RAM 62%, CPU 58%
Nœud 2 : VM 101 ajoutée, RAM 68%, CPU 54%
Nœud 3 : inchangé
```

**Nota** : le controller migre **le moins possible** — il s'arrête dès que le nœud
respire, sans chercher l'équilibre parfait inutilement.

---

## M3 — GPU + CPU : placement multi-contraintes

**État initial**
```
Nœud 1 : pas de GPU, 20 vCPUs libres
Nœud 2 : GPU 8 Go (6 Go libres), 4 vCPUs libres
Nœud 3 : GPU 8 Go (7 Go libres), 12 vCPUs libres

VM 200 à démarrer :
  RAM = 16 Go
  GPU = 4 Go VRAM
  min_vcpus = 6, max_vcpus = 16
```

**Ce qui se passe**

```
t=0s   AdmissionController évalue VM 200 :
         Contraintes simultanées :
           GPU : 4 Go VRAM nécessaires
           CPU : 6 vCPUs minimum
           RAM : 16 Go

         Nœud 1 : pas de GPU → éliminé immédiatement
         Nœud 2 : GPU ok (6 Go), CPU: 4 libres < 6 → éliminé
         Nœud 3 : GPU ok (7 Go), CPU: 12 libres ≥ 6 ✓, RAM ok ✓ → sélectionné

         Placement : Nœud 3
         local_budget = 16 Go, remote_budget = 0

t=1s   VM 200 démarre sur Nœud 3
         GpuMultiplexer alloue 4 Go VRAM (GPU de Nœud 3)
         VcpuScheduler : démarre à 6 vCPUs

t=10min  VM 200 charge monte :
          CPU = 85% → hotplug → 8 vCPUs (Nœud 3 a 12-6=6 libres, peut en donner 2 de plus)
          GPU : dans les limites des 4 Go alloués
          RAM : 12 Go actifs sur 16 → CLOCK commence à évincer les 4 Go froids
```

**État final**
```
VM 200 sur Nœud 3 : 8 vCPUs, 4 Go VRAM, 12 Go RAM locale + 4 Go sur stores
```

---

## M4 — Cluster complet sur tous les axes

**État initial**
```
Nœud 1 : RAM 88%, CPU 92%, GPU 95% VRAM, disque sous pression
Nœud 2 : RAM 85%, CPU 87%, GPU 90% VRAM, disque sous pression
Nœud 3 : RAM 82%, CPU 80%, GPU 85% VRAM, disque sous pression
```

Nouvelle VM 999 veut démarrer : 8 Go RAM, 4 vCPUs, 2 Go VRAM.

**Ce qui se passe**

```
t=0s   AdmissionController :
         cluster_free_ram = (12% × N1 + 15% × N2 + 18% × N3) × 64 Go
                          ≈ 7.7 + 9.6 + 11.5 = 28.8 Go
         28.8 Go > 8 Go → RAM OK en théorie

         GPU :
           Nœud 1 : 5% libre = 400 Mo < 2 Go → non
           Nœud 2 : 10% libre = 800 Mo < 2 Go → non
           Nœud 3 : 15% libre = 1.2 Go < 2 Go → non

         → REFUS : aucun nœud n'a 2 Go de VRAM libres
         Message : "GPU : 2 Go VRAM requis, max disponible = 1.2 Go sur Nœud 3"

t=1s   controller cherche si une VM GPU peut être libérée :
         VM 150 (Nœud 3, 2 Go VRAM, idle depuis 2h) → peut aller sur Nœud 1 ou 2
           Nœud 1 : 400 Mo GPU libre < 2 Go → non
           Nœud 2 : 800 Mo GPU libre < 2 Go → non
         → impossible de déplacer VM 150 pour libérer de la place GPU

         → Situation bloquée : cluster GPU saturé

t=2s   controller log :
         [WARNING] admission VM 999 impossible : VRAM cluster insuffisante
                   max_available=1.2 Go Nœud3, requis=2 Go
                   action : éteindre une VM GPU ou ajouter un GPU/nœud

t=?    Opérateur éteint VM 150 (idle) :
         qm stop 150
         → Nœud 3 : +2 Go VRAM libres
         → Admission VM 999 acceptée au prochain cycle (5s)
```

**Leçon** : RAM et CPU sont élastiques (on peut redistribuer). GPU et disque physique
ne le sont pas — si tous les GPUs sont pleins, il faut libérer ou acheter.

---

## M5 — Migration live pendant une pression mémoire

**État initial**
```
VM 100 sur Nœud 1 :
  RAM configurée = 32 Go
  14 Go en local (Nœud 1)
  18 Go sur stores (Nœud 2 = 10 Go primaire, Nœud 3 = 18 Go réplica)
  VM active, 200 req/s
```

**Action** : Nœud 1 dépasse 90% RAM → migration live déclenchée vers Nœud 2

**Ce qui se passe**

```
t=0s   POST migrate {vm_id:100, target:pve-node2, type:live}
         QEMU live migration démarre en arrière-plan

t=0-5s Pre-copy phase :
         QEMU copie les pages chaudes de Nœud 1 vers Nœud 2
         Pour chaque page : est-elle dans le memfd (locale) ?
           - Oui → copie directe vers Nœud 2
           - Non (évincée sur store) → page fault interceptée par node-a-agent Nœud 1
             → agent fetch depuis Nœud 2 store (déjà sur Nœud 2 !)
             → page remise dans le memfd → copiée vers Nœud 2

         Optimisation : les pages sur le store Nœud 2 sont rapatriées "gratuitement"
         (même machine que la destination → latence ~0)

t=5s   Stop-and-copy : VM figée ~80ms
         Dernières pages modifiées copiées

t=5.1s VM 100 reprend sur Nœud 2 :
         Tout son memfd est maintenant sur Nœud 2
         node-a-agent relancé sur Nœud 2

t=5.2s  Nœud 1 libéré : 14 Go récupérés

t=6s   VM 100 : 200 req/s continues (zéro downtime visible)
         CLOCK reprend : pages chaudes en local, froides évincées vers Nœud 1 et 3
```

**État final**
```
VM 100 sur Nœud 2 : 14 Go en local, pages froides sur Nœud 1 et Nœud 3
Nœud 1 : 14 Go de RAM récupérés, pression mémoire retombée
```

---

## M6 — Rafale de démarrages simultanés

**État initial**
```
Cluster vide : Nœud 1/2/3 à 10% RAM chacun
```

**Action** : démarrage simultané de 20 VMs de 8 Go (160 Go total, cluster = 192 Go)

**Ce qui se passe**

```
t=0s   20 × qm start → 20 hookscripts pre-start simultanés

       AdmissionController.admit_batch() :
         Évalue les 20 VMs en séquence avec réservations mutuelles :

         VM 1  : cluster_free=172 Go → Nœud 1 (58 Go libres) → local=8, remote=0
         VM 2  : cluster_free=164 Go → Nœud 2 (58 Go libres) → local=8, remote=0
         VM 3  : cluster_free=156 Go → Nœud 3 (58 Go libres) → local=8, remote=0
         VM 4  : cluster_free=148 Go → Nœud 1 (50 Go libres) → local=8, remote=0
         ...
         VM 16 : cluster_free=48 Go  → réparti équitablement
         VM 17 : cluster_free=40 Go  → Nœud X (16 Go libres) → local=16, remote=0 → ATTENDS
                  → local=8, remote_budget=0 (juste sur le fil)
         VM 18-20 : cluster_free tombe à 16 Go < 3×8=24 Go
                  → REFUS pour VM 19 et 20 si pas de remote possible
                  → OU : remote_budget activé pour les dernières

         VMs 18-20 admises avec remote_budget :
           local=5 Go (ce qui reste sur nœud de placement)
           remote=3 Go (sur les 2 autres nœuds)

t=1-30s  Les 20 VMs démarrent de façon asynchrone
          Lazy allocation : en pratique seules les pages réellement
          accédées sont physiquement présentes
          → les 20 VMs tiennent dans 192 Go si leur "working set" est < 9.6 Go chacune
```

**Répartition finale** (exemple si toutes utilisent 6 Go actifs)
```
Nœud 1 : ~40 Go utilisés (6-7 VMs × 6 Go)
Nœud 2 : ~40 Go utilisés
Nœud 3 : ~40 Go utilisés
Stores  : pages froides des VMs à fort remote_budget
```

---

## M7 — Réorganisation totale pour maintenance

**Objectif** : vider le Nœud 1 pour une maintenance hardware sans éteindre les VMs

**État initial**
```
Nœud 1 : VM 100 (8 Go, 4 vCPU), VM 101 (16 Go, 8 vCPU), VM 102 (4 Go, 2 vCPU)
Nœud 2 : VM 200 (12 Go, 6 vCPU), 20 Go libres, 10 vCPUs libres
Nœud 3 : VM 300 (8 Go, 4 vCPU),  30 Go libres, 14 vCPUs libres
```

**Action** : `python3 -m controller.main drain-node --source-node node-a`

**Ce qui se passe**

```
t=0s   drain-node démarre :
         _plan_node_drain("node-a", node_states) :

         VMs triées par "coût de déplacement" (GPU puis RAM puis CPU) :
           VM 101 (16 Go, 8 vCPU) — grosse, prioritaire
           VM 100 (8 Go, 4 vCPU)
           VM 102 (4 Go, 2 vCPU)  — petite, la dernière

         Simulation de placement (on réserve au fur et à mesure) :
           VM 101 → Nœud 3 (30 Go ≥ 16 ✓, 14 vCPU ≥ 8 ✓) → réservé
                    Nœud 3 simulé : 14 Go libres, 6 vCPUs libres
           VM 100 → Nœud 2 (20 Go ≥ 8 ✓, 10 vCPU ≥ 4 ✓) → réservé
                    Nœud 2 simulé : 12 Go libres, 6 vCPUs libres
           VM 102 → Nœud 2 (12 Go ≥ 4 ✓, 6 vCPU ≥ 2 ✓) → réservé
           → Plan complet, toutes les VMs ont un atterrissage

t=1s   Exécution des migrations (max 1 simultanée par défaut) :
         POST migrate {vm_id:101, target:pve-node3, type:live}

t=8s   VM 101 sur Nœud 3 ✓

t=9s   POST migrate {vm_id:100, target:pve-node2, type:live}
t=15s  VM 100 sur Nœud 2 ✓

t=16s  POST migrate {vm_id:102, target:pve-node2, type:live}
t=22s  VM 102 sur Nœud 2 ✓

t=23s  drain-node : source_state.local_vms = [] → drain terminé
         log : "drain node terminé, source_node=node-a"
```

**État final**
```
Nœud 1 : vide (prêt pour maintenance)
Nœud 2 : VM 100 + VM 102 (12+4 = 16 Go, 4+2 = 6 vCPUs)
Nœud 3 : VM 101 (16 Go, 8 vCPUs)
Zéro VM éteinte, zéro interruption de service
```

Après la maintenance, relancer le daemon sur Nœud 1 :
```bash
systemctl start omega-daemon
# Le controller le redécouvre automatiquement
# Les VMs peuvent être rebalancées manuellement ou par le controller
```

---

*omega-remote-paging — scénarios V4 — 2026-04-29*
