/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * pve_gpu_sched.h — PVE GPU Scheduler
 *
 * Hyperviseur GPU natif pour Proxmox VE / KVM.
 * Inspire de gxen/tools/a3 (Yusuke Suzuki, CPFL) et Gdev (Shinpei Kato).
 *
 * Architecture : module kernel mdev (VFIO mediated device) qui intercepte
 * les acces MMIO des VMs vers le GPU physique et les ordonnance via
 * l'algorithme BAND (Bandwidth-Aware Non-preemptive Dispatcher).
 *
 * Correspondance avec a3 (gxen) :
 *   a3::command          -> struct pvegpu_cmd
 *   a3::context          -> struct pvegpu_vm_ctx
 *   a3::band_scheduler_t -> struct pvegpu_scheduler
 *   a3::device_t         -> struct pvegpu_device
 */

#ifndef PVE_GPU_SCHED_H
#define PVE_GPU_SCHED_H

#include <linux/types.h>
#include <linux/spinlock.h>
#include <linux/mutex.h>
#include <linux/list.h>
#include <linux/kfifo.h>
#include <linux/ktime.h>
#include <linux/wait.h>
#include <linux/kthread.h>
#include <linux/atomic.h>
#include <linux/pci.h>
#include <linux/vfio.h>
#include <linux/mdev.h>
#include <linux/eventfd.h>

#include "pve_gpu_sched_compat.h"

/* ============================================================
 * CONSTANTES DE CONFIGURATION
 * Equivalent de a3_config.h dans gxen
 * ============================================================ */

/* Nombre maximum de VMs pouvant partager le GPU simultanement */
#define PVEGPU_MAX_DOMAINS        16

/* Nombre de canaux GPU (PFIFO channels) par VM.
 * Sur NVC0 Fermi : 128 canaux physiques au total.
 * Avec 4 VMs : 32 canaux chacune. */
#define PVEGPU_DOMAIN_CHANNELS    32

/* Taille VRAM allouee par VM (en bytes).
 * Avec une GPU de 4GB et 4 VMs : 1GB chacune. */
#define PVEGPU_DOMAIN_VRAM_SIZE   (1ULL << 30)  /* 1 GB par defaut */

/* Periode du scheduler BAND en microsecondes.
 * Identique au reglage par defaut de a3 (30ms). */
#define PVEGPU_SCHED_PERIOD_US    30000          /* 30 ms */

/* Periode d'echantillonnage pour les stats d'utilisation */
#define PVEGPU_SAMPLE_PERIOD_US   500000         /* 500 ms */

/* Taille de la file de commandes suspendues par VM */
#define PVEGPU_CMD_QUEUE_SIZE     256

/* Poids par defaut pour le WFQ (Weighted Fair Queuing) */
#define PVEGPU_DEFAULT_WEIGHT     50

/* Nombre d'entrees dans une Page Directory GPU (NVC0) */
#define PVEGPU_PD_ENTRIES         2048

/* Nombre d'entrees dans une Page Table GPU (NVC0, small pages 4KB) */
#define PVEGPU_PT_ENTRIES         8192

/* Taille d'un bloc RAMIN par canal (NVC0) */
#define PVEGPU_RAMIN_PER_CHANNEL  0x1000

/* Taille de la fenetre glissante BAR3 pour VRAM > 4GB */
#define PVEGPU_BAR3_WINDOW_SIZE   (256ULL << 20)  /* 256 MB */

/* ============================================================
 * TYPES DE BASE
 * ============================================================ */

/*
 * struct pvegpu_cmd — une commande interceptee d'une VM vers le GPU
 *
 * Equivalent de a3::command dans gxen.
 * Represente un acces MMIO (lecture ou ecriture) intercepte
 * par le driver mdev depuis une VM.
 */
struct pvegpu_cmd {
    uint32_t type;      /* PVEGPU_CMD_TYPE_READ ou WRITE */
    uint32_t value;     /* valeur a ecrire, ou resultat de lecture */
    uint32_t offset;    /* offset dans le BAR */
    uint8_t  bar;       /* numero du BAR : 0, 1, ou 3 */
    uint8_t  size;      /* taille de l'acces : 1, 2, ou 4 bytes */
    uint8_t  _pad[2];
};

/* Valeurs pour pvegpu_cmd.type */
#define PVEGPU_CMD_TYPE_READ     0
#define PVEGPU_CMD_TYPE_WRITE    1

/* Valeurs pour pvegpu_cmd.bar */
#define PVEGPU_BAR0              0  /* registres de controle GPU */
#define PVEGPU_BAR1              1  /* canaux PFIFO */
#define PVEGPU_BAR3              3  /* VRAM */

/* ============================================================
 * VENDOR IDS
 * ============================================================ */

#define PVEGPU_VENDOR_NVIDIA     0x10de
#define PVEGPU_VENDOR_AMD        0x1002

/* ============================================================
 * FORWARD DECLARATIONS
 * ============================================================ */

struct pvegpu_device;
struct pvegpu_scheduler;

/* ============================================================
 * SHADOW PAGE TABLE GPU
 * Equivalent de shadow_page_table dans gxen/a3
 *
 * Sur NVC0 (Fermi), la hierarchie de page tables GPU est :
 *   RAMIN -> Page Directory (2048 PDEs)
 *         -> Page Table (8192 PTEs, small pages 4KB)
 *
 * Chaque VM a son propre shadow PD qui pointe vers des
 * shadow PTs. Les adresses VRAM dans les PTEs sont translatees
 * pour mapper vers la tranche physique de la VM.
 * ============================================================ */

/*
 * struct pvegpu_shadow_pt — shadow page table
 *
 * Copie kernel des PTEs GPU d'une VM.
 * Les adresses physiques dans les PTEs sont translatees
 * de l'espace virtuel VM vers l'espace physique GPU.
 */
struct pvegpu_shadow_pt {
    uint64_t entries[PVEGPU_PT_ENTRIES];  /* shadow PTEs */
    bool     valid;
};

/*
 * struct pvegpu_shadow_pd — shadow page directory
 *
 * Equivalent de shadow_page_directory dans a3.
 * Contient les PDEs et les pointeurs vers les shadow PTs.
 */
struct pvegpu_shadow_pd {
    uint64_t             pde[PVEGPU_PD_ENTRIES];  /* shadow PDEs */
    struct pvegpu_shadow_pt *pt[PVEGPU_PD_ENTRIES]; /* shadow page tables */
    uint64_t             phys_addr;     /* adresse physique du shadow PD sur GPU */
    bool                 initialized;
    spinlock_t           lock;
};

/* ============================================================
 * ETAT IRQ PAR VM
 * Gestion du forwarding des interruptions GPU -> VM via eventfd.
 * Equivalent du mecanisme irqbypass de vfio-pci.
 * ============================================================ */

/* Types d'interruption supportes */
#define PVEGPU_IRQ_INTX    0
#define PVEGPU_IRQ_MSI     1
#define PVEGPU_IRQ_MSIX    2
#define PVEGPU_NUM_IRQ_TYPES 3

struct pvegpu_irq_state {
    struct eventfd_ctx  *trigger;       /* eventfd pour signaler QEMU */
    struct eventfd_ctx  *unmask;        /* eventfd pour unmask */
    int                  irq_type;      /* INTX, MSI, ou MSI-X */
    bool                 enabled;
    spinlock_t           lock;
};

/* ============================================================
 * EMULATION CONFIG SPACE PCI (PAR VM)
 *
 * Chaque VM a sa propre copie du config space PCI.
 * Les lectures retournent les valeurs emulees, les ecritures
 * sont filtrees pour eviter de corrompre le device physique.
 * ============================================================ */

#define PVEGPU_CFG_SPACE_SIZE   256

struct pvegpu_config_space {
    uint8_t  regs[PVEGPU_CFG_SPACE_SIZE];   /* shadow config space */
    bool     initialized;
};

/* ============================================================
 * ETAT GPU COMPUTE (SAVE/RESTORE SIMPLIFIE)
 *
 * Pour les workloads compute (CUDA/OpenCL), le GPU sauvegarde
 * et restaure le contexte de calcul automatiquement via RAMIN
 * quand on change de canal PFIFO.
 *
 * Cette structure maintient les registres supplementaires
 * necessaires au context switch compute :
 *  - PFIFO_CTX_TABLE : pointe vers le RAMIN du canal actif
 *  - PGRAPH registers : etat du moteur graphique/compute
 *  - Compute Class registers : etat specifique CUDA/OpenCL
 * ============================================================ */

/* Nombre de registres PGRAPH a sauvegarder (NVC0 compute-essentiels) */
#define PVEGPU_NV_PGRAPH_SAVE_REGS   8
/* Nombre de registres compute AMD a sauvegarder */
#define PVEGPU_AMD_COMPUTE_SAVE_REGS 8

struct pvegpu_compute_state {
    /* NVIDIA NVC0+ : registres PGRAPH/PFIFO necessaires au compute */
    uint32_t pfifo_ctx_table;        /* PFIFO_CTX_TABLE (0x001700) */
    uint32_t pgraph_ctxsw_status;    /* PGRAPH context switch status */
    uint32_t pgraph_regs[PVEGPU_NV_PGRAPH_SAVE_REGS];

    /* AMD GCN+ : registres compute */
    uint32_t amd_cp_rb_base;         /* CP ring buffer base */
    uint32_t amd_cp_rb_wptr;         /* CP ring buffer write pointer */
    uint32_t amd_cp_rb_rptr;         /* CP ring buffer read pointer */
    uint32_t amd_vm_context_cntl;    /* VM context control */
    uint32_t amd_compute_regs[PVEGPU_AMD_COMPUTE_SAVE_REGS];

    bool     saved;                  /* etat valide ? */
};

/* ============================================================
 * CANAL GPU VIRTUALISE
 * Equivalent de a3::channel dans gxen
 * ============================================================ */

/*
 * struct pvegpu_channel — un canal PFIFO virtualise
 *
 * Chaque VM dispose de PVEGPU_DOMAIN_CHANNELS canaux virtuels.
 * Ces canaux sont mappes vers des canaux physiques via une translation :
 *   phys_id = virt_id + vm->id * PVEGPU_DOMAIN_CHANNELS
 *
 * C'est exactement get_phys_channel_id() dans a3::context.
 */
struct pvegpu_channel {
    uint32_t virt_id;       /* ID virtuel (vu par la VM) */
    uint32_t phys_id;       /* ID physique (reel sur le GPU) */
    bool     enabled;       /* canal actif ? */
    uint64_t ramin_addr;    /* adresse shadow RAMIN */
    uint64_t page_dir_addr; /* adresse page directory GPU */
    /* Shadow state du canal pour le context switch */
    uint32_t put;           /* PUT pointer (dernier cmd soumis) */
    uint32_t get;           /* GET pointer (dernier cmd execute) */
    uint32_t ib_put;        /* indirect buffer PUT */
    uint32_t ib_get;        /* indirect buffer GET */
};

/* ============================================================
 * CONTEXTE VM — LE TYPE CENTRAL
 * Equivalent de a3::context dans gxen
 * ============================================================ */

/*
 * struct pvegpu_vm_ctx — contexte d'une VM utilisant le GPU
 *
 * Cree quand une VM demarre avec gpu-sched=1 dans sa config Proxmox.
 * Detruit quand la VM s'arrete.
 */
struct pvegpu_vm_ctx {
    /* --- Identite --- */
    uint32_t id;        /* slot GPU : 0..PVEGPU_MAX_DOMAINS-1 */
    int      vmid;      /* VMID Proxmox (= domid dans a3) */
    bool     initialized;

    /* --- Poids WFQ ---
     * Determine la proportion de temps GPU allouee a cette VM.
     * Configure via gpu-weight dans la config Proxmox.
     * 1-100, defaut 50.
     */
    uint32_t weight;

    /* --- Translation d'adresse VRAM ---
     * Chaque VM a une tranche de VRAM :
     *   VM 0 : [0,          vram_size)
     *   VM 1 : [vram_size,  2*vram_size)
     *   VM N : [N*vram_size, (N+1)*vram_size)
     *
     * get_phys_address(virt) = virt + id * vram_size
     * Equivalent de a3::context::get_phys_address()
     */
    uint64_t vram_size;     /* taille VRAM allouee a cette VM */

    /* --- Canaux GPU shadow ---
     * Equivalent de a3::context::channels_[]
     */
    struct pvegpu_channel channels[PVEGPU_DOMAIN_CHANNELS];

    /* --- Shadow page tables ---
     * Maintient une copie des page tables GPU de la VM
     * avec les adresses translatees.
     * Equivalent de shadow_page_table dans a3.
     */
    struct pvegpu_shadow_pd shadow_pd;

    /* --- Etat IRQ ---
     * Forwarding des interruptions GPU vers QEMU via eventfd.
     */
    struct pvegpu_irq_state irq;

    /* --- Config space PCI emule --- */
    struct pvegpu_config_space cfg;

    /* --- Etat GPU compute (save/restore) --- */
    struct pvegpu_compute_state compute;

    /* --- BAND Scheduler ---
     * Ces champs ne sont touches que par le scheduler.
     * Proteges par band_lock.
     * Equivalent des champs "// only touched by BAND scheduler"
     * dans a3::context.
     */
    spinlock_t  band_lock;
    ktime_t     budget;              /* budget restant dans la periode */
    ktime_t     bandwidth;           /* quota total alloue (WFQ weight) */
    ktime_t     bandwidth_used;      /* consomme dans la periode courante */
    ktime_t     sampling_bw_used;    /* consomme mesure par le sampler */

    /* File de commandes suspendues (budget epuise).
     * Equivalent de std::queue<command> suspended_ dans a3::context.
     * kfifo est thread-safe pour 1 producteur / 1 consommateur.
     * Utilise DECLARE_KFIFO_PTR + kfifo_alloc pour allocation dynamique.
     */
    DECLARE_KFIFO_PTR(suspended, struct pvegpu_cmd);

    /* --- Liste dans le scheduler ---
     * Equivalent de boost::intrusive::list_base_hook<> dans a3::context.
     * Permet d'inserer ce contexte dans scheduler->contexts sans alloc.
     */
    struct list_head list;

    /* --- Reference au device parent --- */
    struct pvegpu_device *dev;

    /* --- Interface mdev (VFIO) ---
     * Un mdev_device est cree par VM dans /sys/bus/mdev/devices/
     */
    struct mdev_device  *mdev;
    struct vfio_device   vdev;

    /* --- BAR3 fenetre glissante ---
     * Pour VRAM > 4GB, on utilise une fenetre glissante dans BAR3.
     * window_base = offset actuel de la fenetre dans la VRAM physique.
     */
    uint64_t bar3_window_base;
};

/* Accesseurs inline — equivalent des methodes inline de a3::context */

static inline uint64_t pvegpu_ctx_addr_shift(const struct pvegpu_vm_ctx *ctx)
{
    return (uint64_t)ctx->id * ctx->vram_size;
}

static inline uint64_t pvegpu_virt_to_phys_addr(const struct pvegpu_vm_ctx *ctx,
                                                  uint64_t virt)
{
    return virt + pvegpu_ctx_addr_shift(ctx);
}

static inline uint64_t pvegpu_phys_to_virt_addr(const struct pvegpu_vm_ctx *ctx,
                                                  uint64_t phys)
{
    return phys - pvegpu_ctx_addr_shift(ctx);
}

static inline uint32_t pvegpu_virt_to_phys_channel(const struct pvegpu_vm_ctx *ctx,
                                                     uint32_t virt_cid)
{
    return virt_cid + ctx->id * PVEGPU_DOMAIN_CHANNELS;
}

static inline uint32_t pvegpu_phys_to_virt_channel(const struct pvegpu_vm_ctx *ctx,
                                                     uint32_t phys_cid)
{
    return phys_cid - ctx->id * PVEGPU_DOMAIN_CHANNELS;
}

static inline bool pvegpu_ctx_addr_valid(const struct pvegpu_vm_ctx *ctx,
                                          uint64_t phys)
{
    uint64_t shift = pvegpu_ctx_addr_shift(ctx);
    return (phys >= shift) && (phys < shift + ctx->vram_size);
}

static inline bool pvegpu_ctx_is_suspended(const struct pvegpu_vm_ctx *ctx)
{
    return !kfifo_is_empty(&ctx->suspended);
}

/* ============================================================
 * GPU OPS — ABSTRACTION VENDOR (NVIDIA / AMD)
 *
 * Permet de supporter differents vendeurs GPU en abstraant
 * les offsets de registres et les operations specifiques.
 *
 * NVIDIA NVC0 (Fermi) : PFIFO, PGRAPH, BAR0/1/3
 * AMD GCN+ : CP (Command Processor), GRBM, MMIO
 * ============================================================ */

struct pvegpu_gpu_ops {
    const char *name;

    /* Taille d'un canal dans BAR1 */
    uint32_t (*get_channel_range)(const struct pvegpu_device *gdev);

    /* Registre de status GPU (idle detection) */
    uint32_t (*get_status_reg)(void);

    /* Offset du registre PFIFO_CTX_TABLE (ou equivalent) */
    uint32_t (*get_ctx_table_reg)(void);

    /* Detection du nombre total de canaux */
    uint32_t (*detect_channels)(struct pvegpu_device *gdev);

    /* Flush TLB GPU */
    int (*flush_tlb)(struct pvegpu_vm_ctx *ctx, uint32_t engine);

    /* Reset d'un contexte (FLR virtuel) */
    int (*context_reset)(struct pvegpu_vm_ctx *ctx);

    /* Translation d'adresse RAMIN */
    uint32_t (*ramin_translate)(const struct pvegpu_vm_ctx *ctx,
                                uint32_t virt_ramin);

    /* Save/restore d'etat compute (CUDA/OpenCL) */
    void (*compute_save)(struct pvegpu_vm_ctx *ctx);
    void (*compute_restore)(struct pvegpu_vm_ctx *ctx);

    /* Attente GPU idle */
    int (*wait_idle)(struct pvegpu_device *gdev, int timeout_us);
};

/* ============================================================
 * SCHEDULER BAND
 * Equivalent de a3::band_scheduler_t dans gxen
 * ============================================================ */

/*
 * struct pvegpu_scheduler — le scheduler BAND
 *
 * Tourne en 3 threads kernel :
 *  - run_thread      : boucle principale (attend -> choisit -> soumet)
 *  - replenish_thread: recharge les budgets a chaque periode
 *  - sampler_thread  : mesure l'utilisation reelle GPU
 *
 * Equivalent de a3::band_scheduler_t.
 */
struct pvegpu_scheduler {
    /* --- Liste des VMs enregistrees ---
     * Equivalent de contexts_t contexts_ dans a3::scheduler_t.
     * Chaque pvegpu_vm_ctx est dans cette liste via son champ ->list.
     */
    struct list_head   contexts;
    int                n_contexts;    /* nombre de VMs actives */

    /* --- Poids total pour WFQ ---
     * Somme des poids de toutes les VMs enregistrees.
     * Utilise pour calculer le budget proportionnel.
     */
    uint32_t           total_weight;

    /* --- Locks ---
     * fire_lock  : protege l'acces physique au GPU (un seul a la fois)
     * sched_lock : protege la liste contexts et la decision
     * Equivalent de fire_mutex_ / sched_mutex_ dans a3::scheduler_t.
     */
    spinlock_t         fire_lock;
    spinlock_t         sched_lock;

    /* --- Etat BAND ---
     * Equivalent des champs prives de a3::band_scheduler_t.
     */
    ktime_t            period;           /* duree d'une periode (30ms) */
    ktime_t            bandwidth;        /* temps GPU utilise ce cycle */
    ktime_t            previous_bandwidth;
    ktime_t            gpu_idle;         /* temps GPU inactif ce cycle */
    ktime_t            gpu_idle_start;   /* timestamp debut inactivite */
    atomic64_t         counter;          /* nb commandes en attente (atomique) */
    struct pvegpu_vm_ctx *current_ctx;   /* VM qui a le GPU maintenant */

    /* --- Synchronisation ---
     * wq  : la VM en attente dort ici (equivalent de cond_ dans a3)
     */
    wait_queue_head_t  wq;

    /* --- TLB flush serialization ---
     * Empeche les flushes TLB concurrents entre VMs.
     */
    struct mutex       tlb_flush_mutex;

    /* --- Threads kernel ---
     * Equivalent de thread_, replenisher_, sampler_ dans a3.
     */
    struct task_struct *run_thread;
    struct task_struct *replenish_thread;
    struct task_struct *sampler_thread;
    bool                running;

    /* --- Statistiques sampler --- */
    ktime_t            sample_period;
    uint64_t           total_cmds_processed;

    /* --- Reference au device parent --- */
    struct pvegpu_device *dev;
};

/* ============================================================
 * DEVICE — LE GPU PHYSIQUE
 * Equivalent de a3::device_t dans gxen
 * ============================================================ */

/*
 * struct pvegpu_device — represente le GPU physique et son etat global
 *
 * Singleton : un seul par GPU physique sur l'hote Proxmox.
 * Cree au chargement du module, detruit au dechargement.
 *
 * Equivalent de a3::device_t dans gxen.
 */
struct pvegpu_device {
    /* --- PCI --- */
    struct pci_dev     *pdev;
    void __iomem       *bar[6];          /* BARs mappes en memoire */
    resource_size_t     bar_size[6];

    /* --- VMs --- */
    struct pvegpu_vm_ctx *contexts[PVEGPU_MAX_DOMAINS];
    DECLARE_BITMAP(virt_slots, PVEGPU_MAX_DOMAINS); /* slots libres/pris */
    int                   n_contexts;

    /* --- Scheduler --- */
    struct pvegpu_scheduler scheduler;

    /* --- Lock global device ---
     * Equivalent de mutex_t mutex_ dans a3::device_t.
     * Protege les acces concurrents aux BARs physiques.
     */
    spinlock_t          mutex;

    /* --- Interface mdev ---
     * mdev_parent : enregistre dans sysfs, cree les VDs pour les VMs.
     */
    struct mdev_parent  mdev_parent;
    struct device      *dev;

    /* --- Infos GPU --- */
    uint32_t            vendor_id;       /* NVIDIA ou AMD */
    uint32_t            chipset_type;    /* NVC0, NVE4, etc. */
    uint64_t            vram_total;      /* VRAM totale du GPU */
    uint64_t            vram_per_vm;     /* = vram_total / n_max_vms */
    uint32_t            total_channels;  /* canaux PFIFO totaux */

    /* --- GPU Operations (vendor-specific) --- */
    const struct pvegpu_gpu_ops *gpu_ops;

    /* --- IRQ physique du GPU --- */
    int                 gpu_irq;         /* numero IRQ physique */
    bool                irq_registered;
};

/* ============================================================
 * API PUBLIQUE DU MODULE
 * ============================================================ */

/* Initialisation / destruction */
int  pvegpu_device_init(struct pvegpu_device *gdev, struct pci_dev *pdev);
void pvegpu_device_fini(struct pvegpu_device *gdev);

/* Gestion des contextes VM */
int  pvegpu_ctx_create(struct pvegpu_device *gdev, int vmid,
                       uint32_t weight,
                       struct pvegpu_vm_ctx **ctx_out);
void pvegpu_ctx_destroy(struct pvegpu_vm_ctx *ctx);

/* Scheduler */
int  pvegpu_sched_init(struct pvegpu_scheduler *sched,
                       struct pvegpu_device *gdev,
                       ktime_t period);
void pvegpu_sched_fini(struct pvegpu_scheduler *sched);
void pvegpu_sched_register_ctx(struct pvegpu_scheduler *sched,
                                struct pvegpu_vm_ctx *ctx);
void pvegpu_sched_unregister_ctx(struct pvegpu_scheduler *sched,
                                  struct pvegpu_vm_ctx *ctx);

/* Point d'entree principal : une VM veut envoyer une commande */
void pvegpu_enqueue(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd);

/* Handlers d'acces BAR (appeles depuis vfio_device_ops) */
void pvegpu_write_bar0(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd);
void pvegpu_write_bar1(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd);
void pvegpu_write_bar3(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd);
int  pvegpu_read_bar0(struct pvegpu_vm_ctx *ctx, struct pvegpu_cmd *cmd);
int  pvegpu_read_bar1(struct pvegpu_vm_ctx *ctx, struct pvegpu_cmd *cmd);
int  pvegpu_read_bar3(struct pvegpu_vm_ctx *ctx, struct pvegpu_cmd *cmd);

/* Acces physique au GPU */
uint32_t pvegpu_bar_read32(struct pvegpu_device *gdev, int bar, uint32_t offset);
void     pvegpu_bar_write32(struct pvegpu_device *gdev, int bar,
                             uint32_t offset, uint32_t val);

/* Shadow page tables */
int  pvegpu_shadow_pd_init(struct pvegpu_vm_ctx *ctx);
void pvegpu_shadow_pd_fini(struct pvegpu_vm_ctx *ctx);
void pvegpu_shadow_pd_update_pde(struct pvegpu_vm_ctx *ctx,
                                  uint32_t pde_index, uint64_t pde_value);
void pvegpu_shadow_pt_update_pte(struct pvegpu_vm_ctx *ctx,
                                  uint32_t pde_index, uint32_t pte_index,
                                  uint64_t pte_value);

/* Shadow BAR1 / context switch */
void pvegpu_shadow_bar1(struct pvegpu_vm_ctx *ctx);
int  pvegpu_flush_tlb(struct pvegpu_vm_ctx *ctx, uint32_t engine);

/* IRQ forwarding */
int  pvegpu_irq_init(struct pvegpu_vm_ctx *ctx);
void pvegpu_irq_fini(struct pvegpu_vm_ctx *ctx);
int  pvegpu_irq_set(struct pvegpu_vm_ctx *ctx,
                     uint32_t flags, uint32_t index,
                     uint32_t start, uint32_t count,
                     void *data);
void pvegpu_irq_trigger(struct pvegpu_vm_ctx *ctx);

/* Config space emulation */
void pvegpu_cfg_init(struct pvegpu_vm_ctx *ctx);
int  pvegpu_cfg_read(struct pvegpu_vm_ctx *ctx, int offset,
                      int size, uint32_t *val);
int  pvegpu_cfg_write(struct pvegpu_vm_ctx *ctx, int offset,
                       int size, uint32_t val);

/* GPU compute state save/restore */
void pvegpu_compute_state_save(struct pvegpu_vm_ctx *ctx);
void pvegpu_compute_state_restore(struct pvegpu_vm_ctx *ctx);
int  pvegpu_gpu_wait_idle(struct pvegpu_device *gdev, int timeout_us);

/* GPU ops par vendor */
extern const struct pvegpu_gpu_ops pvegpu_nvidia_nvc0_ops;
extern const struct pvegpu_gpu_ops pvegpu_amd_gcn_ops;

/* Device singleton */
struct pvegpu_device *pvegpu_device_get(void);

/* ============================================================
 * MACROS UTILITAIRES
 * ============================================================ */

/* Logging — equivalent de A3_LOG dans gxen */
#define PVEGPU_LOG(fmt, ...)  \
    pr_debug("[pve_gpu_sched] " fmt, ##__VA_ARGS__)

#define PVEGPU_ERR(fmt, ...)  \
    pr_err("[pve_gpu_sched] " fmt, ##__VA_ARGS__)

#define PVEGPU_INFO(fmt, ...) \
    pr_info("[pve_gpu_sched] " fmt, ##__VA_ARGS__)

/* Assertion compile-time — equivalent de BOOST_STATIC_ASSERT */
#define PVEGPU_STATIC_ASSERT(cond) \
    BUILD_BUG_ON(!(cond))

/* Verification que les constantes sont coherentes */
#define PVEGPU_ASSERT_CONFIG() do {                                   \
    PVEGPU_STATIC_ASSERT(PVEGPU_DOMAIN_CHANNELS > 0);                \
    PVEGPU_STATIC_ASSERT(PVEGPU_MAX_DOMAINS > 0);                    \
    PVEGPU_STATIC_ASSERT(PVEGPU_MAX_DOMAINS * PVEGPU_DOMAIN_CHANNELS \
                         <= 4096); /* max canaux NVE4+ */             \
} while (0)

#endif /* PVE_GPU_SCHED_H */
