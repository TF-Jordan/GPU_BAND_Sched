# PVE GPU Scheduler — Synthèse du projet

## Le problème

Proxmox VE permet de passer un GPU physique à une VM via VFIO (PCIe passthrough). Mais ce mécanisme est exclusif : **une seule VM peut utiliser le GPU à la fois**. Les autres VMs n'y ont pas accès du tout.

Dans un contexte cloud où plusieurs VMs sont actives simultanément avec des utilisateurs réels dessus, c'est un frein majeur. Le GPU reste monopolisé par une VM même quand elle ne l'utilise pas activement, pendant que les autres attendent.

Ceph résout ce problème pour le stockage — il virtualise les disques physiques et les partage nativement entre toutes les VMs. Il n'existe pas d'équivalent natif pour le GPU dans Proxmox.

---

## La solution proposée

Un **hyperviseur GPU natif pour Proxmox/KVM**, inspiré du projet de recherche [gxen](https://github.com/CPFL/gxen) (GPU hypervisor pour Xen, 2012) et de l'algorithme [BAND](https://www.usenix.org/system/files/conference/atc12/atc12-final319.pdf) (Kato et al., USENIX ATC 2012).

### Principe

Au lieu de passer le GPU directement à une VM, on interpose un driver kernel qui :

1. S'enregistre comme **mediated device** (mdev) sur le GPU physique via l'API VFIO du kernel Linux
2. Expose **N GPU virtuels** dans sysfs — un par VM, configurés depuis `PCI.pm` de Proxmox
3. **Intercepte tous les accès MMIO** des VMs vers le GPU (lectures/écritures dans BAR0, BAR1, BAR3)
4. **Ordonnance ces accès** selon l'algorithme BAND avec Weighted Fair Queuing
5. **Translate les adresses** (VRAM, canaux PFIFO) pour que chaque VM croie avoir un GPU dédié

```
VM 1 ──→ accès BAR ──→ pve_gpu_sched.ko ──→ GPU physique
VM 2 ──→ accès BAR ──→ pve_gpu_sched.ko ──┘   (ordonnancé)
VM N ──→ accès BAR ──→ pve_gpu_sched.ko ──┘
```

### Algorithme BAND

Chaque VM a un **budget temporel** rechargé à chaque période (30ms par défaut). Trois threads kernel tournent en permanence :

- `run_thread` : attend une commande → choisit la VM prioritaire → soumet au GPU
- `replenish_thread` : recharge les budgets proportionnellement au temps GPU réel
- `sampler_thread` : mesure l'utilisation effective (prévu)

Priorité de sélection : VM sous son quota > VM dans sa bande > VM à budget épuisé.

### Translation d'adresse

Chaque VM se voit allouer une **tranche de VRAM** et une **plage de canaux PFIFO** :
- VRAM : `phys = virt + id × vram_size`
- Canal : `phys_channel = virt_channel + id × channels_per_vm`

---

## Références étudiées

| Source | Rôle |
|--------|------|
| `gxen/tools/a3/` (CPFL) | Architecture de référence — hyperviseur GPU pour Xen |
| `gdev/common/gdev_sched.c` (CPFL) | Algorithme BAND original |
| `linux/samples/vfio-mdev/mtty.c` | Modèle de driver mdev kernel |
| `linux/drivers/gpu/drm/scheduler/` | DRM GPU scheduler du kernel |
| `linux/Documentation/driver-api/vfio-mediated-device.rst` | API mdev |
| `proxmox/qemu-server/src/PVE/QemuServer/PCI.pm` | Point d'insertion Proxmox |
| Kato et al., USENIX ATC 2012 | Article fondateur — Gdev/BAND |

---

## Ce qui a été produit

### Fichiers kernel C — le module `pve_gpu_sched.ko`

| Fichier | Contenu | Lignes |
|---------|---------|--------|
| `pve_gpu_sched.h` | Structures, constantes, API publique | 409 |
| `pve_gpu_sched.c` | Device init/fini, gestion contextes VM, accesseurs BAR | 440 |
| `pve_gpu_sched_band.c` | Algorithme BAND complet (3 threads, scheduler WFQ) | 644 |
| `pve_gpu_sched_mdev.c` | Interface VFIO/mdev — pont kernel ↔ hyperviseur | 728 |
| `pve_gpu_sched_bar.c` | Handlers BAR0/BAR1/BAR3 avec translation d'adresse | 523 |
| `Makefile` | Build du module kernel | 14 |
| **Total** | | **2758 lignes** |

### Correspondance avec gxen/a3

| gxen (C++ Xen userspace) | Ce module (C kernel KVM) |
|--------------------------|--------------------------|
| `a3::command` | `struct pvegpu_cmd` |
| `a3::context` | `struct pvegpu_vm_ctx` |
| `a3::band_scheduler_t` | `struct pvegpu_scheduler` + `pve_gpu_sched_band.c` |
| `a3::device_t` | `struct pvegpu_device` |
| Xen MMIO trap | `vfio_device_ops.read/write` |
| `device_bar1::write()` | `pvegpu_write_bar1()` |
| `libxl / xen_foreign_memory` | `pci_iomap / vfio mdev` |

---

## Ce qui reste à faire

### Compilation

```bash
# Sur le host Proxmox
apt install pve-headers-$(uname -r)
make -C /lib/modules/$(uname -r)/build M=$(pwd) modules
insmod pve_gpu_sched.ko
```

### 1. Shadow page tables complètes — `pve_gpu_sched_bar.c`

C'est le travail le plus important restant. Actuellement la translation BAR0 (PFIFO_CTX_TABLE) est partielle — il faut implémenter :

- Le tracking complet des page directories GPU par VM
- La shadow page table BAR1 (équivalent de `device_bar1::shadow()` dans a3)
- Le BAR3 remapping pour les VRAM > 4GB (option `--bar3-remapping` de gxen)
- La translation RAMIN complète lors des context switches

Référence : `gxen/tools/a3/device_bar1.cc`, `device_bar3.cc`, `shadow_page_table.cc`

### 2. Intégration Proxmox — `PCI.pm`

Côté Proxmox, il faut modifier deux fichiers Perl :

**`src/PVE/QemuServer/PCI.pm`** — ajouter dans `$hostpci_fmt` :
```perl
'gpu-sched' => {
    type => 'boolean',
    description => "Enable PVE GPU scheduler (time-sharing)",
    optional => 1,
    default => 0,
},
'gpu-weight' => {
    type => 'integer',
    description => "GPU scheduler weight (1-100)",
    optional => 1,
    default => 50,
},
```

Et dans `print_hostpci_devices()`, quand `gpu-sched=1` est détecté, forcer `mdev=pvegpu-sched` au lieu du passthrough classique.

**`src/PVE/QemuServer.pm`** — dans `vm_start()`, appeler le scheduler pour initialiser le contexte VM avant de lancer QEMU.

### 3. Gestion des interruptions GPU → VM

Actuellement les IRQs ne sont pas passées aux VMs (`VFIO_DEVICE_SET_IRQS` retourne 0 sans action). Il faut implémenter le forwarding des interruptions GPU via `eventfd` + `irqbypass` pour que les VMs reçoivent les signaux de fin de commande GPU.

Référence : `linux/drivers/vfio/pci/vfio_pci_intrs.c`

### 4. Weighted Fair Queuing par VM

L'algorithme BAND actuel distribue le temps GPU équitablement entre toutes les VMs. Pour implémenter le WFQ selon le `gpu-weight` configuré dans Proxmox, il faut modifier `pvegpu_ctx_replenish()` dans `pve_gpu_sched_band.c` :

```c
/* Au lieu de : budget = period / n_contexts */
/* Faire :      budget = period * (weight / total_weight) */
budget = ktime_divns(
    ktime_mul(period, ctx->weight),
    total_weight
);
```

### 5. Tests et validation

- Test minimal : 2 VMs, 1 GPU, vérifier que les deux reçoivent le GPU en alternance
- Benchmark : comparer les performances GPU VM vs bare metal (objectif < 10% overhead)
- Test de stabilité : démarrage/arrêt de VMs pendant que d'autres utilisent le GPU
- Valider le reset GPU entre deux VMs (Function Level Reset)

### 6. Support AMD

Le module détecte actuellement les GPUs AMD via la table PCI mais les handlers BAR sont calibrés pour NVIDIA NVC0. Le portage AMD nécessite d'adapter les offsets de registres (PFIFO → CP sur AMD, registres différents) en s'appuyant sur le driver `amdgpu` du kernel.
