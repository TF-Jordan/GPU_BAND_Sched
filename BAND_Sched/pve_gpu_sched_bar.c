// SPDX-License-Identifier: GPL-2.0-only
/*
 * pve_gpu_sched_bar.c — Handlers BAR complets avec shadow page tables
 *
 * Implementation complete des handlers d'acces aux BARs GPU avec :
 *  - Filtrage des registres critiques (BAR0)
 *  - Translation des canaux PFIFO (BAR1) + shadow page table
 *  - Translation VRAM avec BAR3 remapping pour VRAM > 4GB
 *  - Gestion du RAMIN avec shadow page directory
 *  - Serialisation des flushes TLB multi-VM
 *
 * References :
 *  - gxen/tools/a3/context.cc : write_bar0/1/3()
 *  - gxen/tools/a3/device_bar1.cc : shadow(), write(), map()
 *  - gxen/tools/a3/shadow_page_table.cc
 *  - envytools documentation : PFIFO, RAMIN, page tables NVC0
 */

#include <linux/io.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/vmalloc.h>

#include "pve_gpu_sched.h"

/* ============================================================
 * SHADOW PAGE TABLES
 *
 * Chaque VM a son propre shadow page directory (PD) qui contient
 * des Page Directory Entries (PDEs), chacune pointant vers une
 * shadow Page Table (PT) avec des PTEs traduits.
 *
 * Quand une VM ecrit un PDE/PTE via BAR0, on intercepte l'ecriture
 * et on met a jour la shadow PT avec les adresses translatees.
 *
 * Equivalent de shadow_page_table.cc dans gxen/a3.
 * ============================================================ */

/*
 * pvegpu_shadow_pd_init — alloue et initialise le shadow page directory
 */
int pvegpu_shadow_pd_init(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_shadow_pd *spd = &ctx->shadow_pd;
    int i;

    spin_lock_init(&spd->lock);
    memset(spd->pde, 0, sizeof(spd->pde));

    for (i = 0; i < PVEGPU_PD_ENTRIES; i++)
        spd->pt[i] = NULL;

    /* Adresse physique du shadow PD = base RAMIN du contexte.
     * Chaque contexte a un slot RAMIN qui commence a :
     *   id * DOMAIN_CHANNELS * RAMIN_PER_CHANNEL
     * Le premier bloc RAMIN (canal 0) contient le PD.
     */
    spd->phys_addr = (uint64_t)ctx->id * PVEGPU_DOMAIN_CHANNELS
                     * PVEGPU_RAMIN_PER_CHANNEL;
    spd->initialized = true;

    PVEGPU_LOG("shadow_pd_init: vmid=%d phys_addr=0x%llx\n",
               ctx->vmid, spd->phys_addr);
    return 0;
}

/*
 * pvegpu_shadow_pd_fini — libere toutes les shadow page tables
 */
void pvegpu_shadow_pd_fini(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_shadow_pd *spd = &ctx->shadow_pd;
    int i;

    if (!spd->initialized)
        return;

    for (i = 0; i < PVEGPU_PD_ENTRIES; i++) {
        if (spd->pt[i]) {
            kfree(spd->pt[i]);
            spd->pt[i] = NULL;
        }
    }

    spd->initialized = false;
}

/*
 * pvegpu_shadow_pd_update_pde — met a jour un PDE dans le shadow PD
 *
 * Quand une VM ecrit un PDE, on :
 *  1. Sauvegarde le PDE traduit dans le shadow PD
 *  2. Alloue une shadow PT si necessaire
 *  3. Traduit l'adresse physique de la PT cible
 *
 * Format PDE NVC0 (8 bytes) :
 *   bits [1:0]   = type (0=invalid, 1=small PT, 3=large PT)
 *   bits [39:12] = adresse physique de la PT >> 12
 *   bits [63:40] = flags
 */
void pvegpu_shadow_pd_update_pde(struct pvegpu_vm_ctx *ctx,
                                  uint32_t pde_index, uint64_t pde_value)
{
    struct pvegpu_shadow_pd *spd = &ctx->shadow_pd;
    unsigned long flags;
    uint64_t translated_pde;
    uint64_t pt_phys_addr;
    uint32_t pde_type;

    if (pde_index >= PVEGPU_PD_ENTRIES) {
        PVEGPU_ERR("shadow_pd: PDE index %u out of range\n", pde_index);
        return;
    }

    spin_lock_irqsave(&spd->lock, flags);

    pde_type = pde_value & 0x3;

    if (pde_type == 0) {
        /* PDE invalide : liberer la shadow PT si elle existe */
        if (spd->pt[pde_index]) {
            kfree(spd->pt[pde_index]);
            spd->pt[pde_index] = NULL;
        }
        spd->pde[pde_index] = 0;
        spin_unlock_irqrestore(&spd->lock, flags);
        return;
    }

    /* Allouer une shadow PT si necessaire */
    if (!spd->pt[pde_index]) {
        spin_unlock_irqrestore(&spd->lock, flags);
        {
            struct pvegpu_shadow_pt *new_pt;
            new_pt = kzalloc(sizeof(*new_pt), GFP_KERNEL);
            if (!new_pt) {
                PVEGPU_ERR("shadow_pd: failed to alloc PT for PDE %u\n",
                           pde_index);
                return;
            }
            spin_lock_irqsave(&spd->lock, flags);
            if (!spd->pt[pde_index]) {
                spd->pt[pde_index] = new_pt;
            } else {
                kfree(new_pt);
            }
        }
    }

    /* Traduire l'adresse physique de la PT
     * phys_pt_addr = virt_pt_addr + ctx->id * vram_size
     */
    pt_phys_addr = (pde_value & 0x000FFFFFFFFFF000ULL);
    pt_phys_addr += pvegpu_ctx_addr_shift(ctx);

    /* Reconstruire le PDE traduit avec la bonne adresse */
    translated_pde = (pde_value & 0xFFF0000000000003ULL) |
                     (pt_phys_addr & 0x000FFFFFFFFFF000ULL);

    spd->pde[pde_index] = translated_pde;
    spd->pt[pde_index]->valid = true;

    spin_unlock_irqrestore(&spd->lock, flags);

    PVEGPU_LOG("shadow_pd: vmid=%d PDE[%u] virt=0x%llx -> phys=0x%llx "
               "type=%u\n",
               ctx->vmid, pde_index, pde_value, translated_pde, pde_type);
}

/*
 * pvegpu_shadow_pt_update_pte — met a jour un PTE dans une shadow PT
 *
 * Quand une VM ecrit un PTE, on traduit l'adresse VRAM cible.
 *
 * Format PTE NVC0 (8 bytes, small page 4KB) :
 *   bit [0]      = valid
 *   bits [39:12] = adresse physique >> 12
 *   bits [63:40] = flags (cacheable, RO, etc.)
 */
void pvegpu_shadow_pt_update_pte(struct pvegpu_vm_ctx *ctx,
                                  uint32_t pde_index, uint32_t pte_index,
                                  uint64_t pte_value)
{
    struct pvegpu_shadow_pd *spd = &ctx->shadow_pd;
    struct pvegpu_shadow_pt *pt;
    unsigned long flags;
    uint64_t translated_pte;
    uint64_t page_phys;

    if (pde_index >= PVEGPU_PD_ENTRIES || pte_index >= PVEGPU_PT_ENTRIES)
        return;

    spin_lock_irqsave(&spd->lock, flags);

    pt = spd->pt[pde_index];
    if (!pt) {
        spin_unlock_irqrestore(&spd->lock, flags);
        return;
    }

    if (!(pte_value & 0x1)) {
        /* PTE invalide */
        pt->entries[pte_index] = 0;
        spin_unlock_irqrestore(&spd->lock, flags);
        return;
    }

    /* Traduire l'adresse physique de la page VRAM */
    page_phys = (pte_value & 0x000FFFFFFFFFF000ULL);
    page_phys += pvegpu_ctx_addr_shift(ctx);

    translated_pte = (pte_value & 0xFFF0000000000001ULL) |
                     (page_phys & 0x000FFFFFFFFFF000ULL);

    pt->entries[pte_index] = translated_pte;

    spin_unlock_irqrestore(&spd->lock, flags);

    PVEGPU_LOG("shadow_pt: vmid=%d PDE[%u] PTE[%u] 0x%llx -> 0x%llx\n",
               ctx->vmid, pde_index, pte_index, pte_value, translated_pte);
}

/* ============================================================
 * REGISTRES GPU CRITIQUES — TABLE DE FILTRAGE BAR0
 * ============================================================ */

static const uint32_t pvegpu_bar0_blocked[] = {
    0x000200,   /* PMC_ENABLE   : reset partiel du GPU */
    0x000260,   /* PMC_INTR_EN  : masque global d'interruptions */
    0x000640,   /* PMC_ENGINE   : activation des engines */
};

/* Registres necessitant une translation d'adresse */
#define PFIFO_CTX_TABLE   0x001700
#define TLB_FLUSH_PDE     0x100cb8
#define TLB_FLUSH_TRIGGER 0x100cbc

/* Plage des registres page directory dans RAMIN
 * BAR0 offset 0x800000..0x810000 = acces RAMIN via PRAMIN window
 */
#define PRAMIN_BASE       0x700000
#define PRAMIN_END        0x800000

static bool pvegpu_bar0_is_blocked(uint32_t offset)
{
    int i;
    for (i = 0; i < ARRAY_SIZE(pvegpu_bar0_blocked); i++) {
        if (pvegpu_bar0_blocked[i] == offset)
            return true;
    }
    return false;
}

/* ============================================================
 * GESTION DU RAMIN
 * ============================================================ */

static uint32_t pvegpu_ramin_virt_to_phys(const struct pvegpu_vm_ctx *ctx,
                                           uint32_t virt_ramin)
{
    if (ctx->dev->gpu_ops && ctx->dev->gpu_ops->ramin_translate)
        return ctx->dev->gpu_ops->ramin_translate(ctx, virt_ramin);

    return virt_ramin + ctx->id * PVEGPU_DOMAIN_CHANNELS
                        * PVEGPU_RAMIN_PER_CHANNEL;
}

static uint32_t pvegpu_translate_pfifo_ctx(const struct pvegpu_vm_ctx *ctx,
                                            uint32_t val)
{
    uint32_t virt_addr = (val & 0x0fffffff) << 12;
    uint32_t phys_addr = pvegpu_ramin_virt_to_phys(ctx, virt_addr);
    return (val & 0xf0000000) | ((phys_addr >> 12) & 0x0fffffff);
}

/* ============================================================
 * HANDLER BAR0 COMPLET
 * ============================================================ */

void pvegpu_write_bar0(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint32_t phys_val = cmd->value;

    /* 1. Bloquer les registres dangereux */
    if (pvegpu_bar0_is_blocked(cmd->offset)) {
        PVEGPU_LOG("BAR0 BLOCKED write vmid=%d offset=0x%x val=0x%x\n",
                   ctx->vmid, cmd->offset, cmd->value);
        return;
    }

    /* 2. Translater les registres selon leur type */
    switch (cmd->offset) {
    case PFIFO_CTX_TABLE:
        /* PFIFO_CTX_TABLE : adresse RAMIN du canal courant */
        phys_val = pvegpu_translate_pfifo_ctx(ctx, cmd->value);
        PVEGPU_LOG("BAR0 TRANSLATE PFIFO_CTX vmid=%d virt=0x%x phys=0x%x\n",
                   ctx->vmid, cmd->value, phys_val);
        break;

    case TLB_FLUSH_PDE:
        /* TLB_FLUSH_PDE : adresse du shadow page directory.
         * Traduire vers l'adresse physique du shadow PD de ce contexte.
         */
        {
            uint64_t shadow_pd_addr = ctx->shadow_pd.phys_addr;
            phys_val = (uint32_t)(shadow_pd_addr >> 8);
        }
        PVEGPU_LOG("BAR0 TLB_FLUSH_PDE vmid=%d shadow_pd=0x%x\n",
                   ctx->vmid, phys_val);
        break;

    case TLB_FLUSH_TRIGGER:
        /* TLB_FLUSH_TRIGGER : serialiser les flushes entre VMs.
         * Spinlock obligatoire ici : cette fonction peut etre appelee
         * depuis pvegpu_submit() qui tient fire_lock (spinlock).
         * Un mutex dormirait sous spinlock -> BUG kernel.
         */
        {
            unsigned long tlb_flags;
            spin_lock_irqsave(&gdev->scheduler.tlb_flush_lock, tlb_flags);
            pvegpu_bar_write32(gdev, 0, cmd->offset, cmd->value);
            spin_unlock_irqrestore(&gdev->scheduler.tlb_flush_lock, tlb_flags);
        }
        PVEGPU_LOG("BAR0 TLB_FLUSH_TRIGGER vmid=%d val=0x%x (serialized)\n",
                   ctx->vmid, cmd->value);
        return;  /* deja ecrit */

    default:
        /* Intercepter les ecritures dans la fenetre PRAMIN
         * pour mettre a jour les shadow page tables.
         */
        if (cmd->offset >= PRAMIN_BASE && cmd->offset < PRAMIN_END) {
            uint32_t ramin_offset = cmd->offset - PRAMIN_BASE;
            uint32_t ramin_phys = pvegpu_ramin_virt_to_phys(ctx, ramin_offset);

            /* Detecter si c'est une ecriture PDE ou PTE
             * dans le page directory du canal.
             *
             * Layout RAMIN (NVC0) :
             *   0x000 - 0x1FF : channel header
             *   0x200 - 0x4200 : Page Directory (2048 PDEs * 8 bytes)
             *   0x4200+       : Page Tables (si inline)
             *
             * Les PDEs et PTEs font 8 bytes (64 bits) mais sont
             * ecrits en 2 acces de 4 bytes. On utilise un buffer
             * temporaire par PDE pour accumuler les deux moities.
             */
            uint32_t local_offset = ramin_offset %
                                    PVEGPU_RAMIN_PER_CHANNEL;

            /* --- Interception des ecritures PDE (64-bit) --- */
            if (local_offset >= 0x200 &&
                local_offset < 0x200 + PVEGPU_PD_ENTRIES * 8) {
                uint32_t pde_byte = local_offset - 0x200;
                uint32_t pde_index = pde_byte / 8;
                uint32_t half = pde_byte % 8;  /* 0=low, 4=high */

                if (pde_index < PVEGPU_PD_ENTRIES) {
                    struct pvegpu_shadow_pd *spd = &ctx->shadow_pd;
                    unsigned long pd_flags;
                    uint64_t full_pde;

                    spin_lock_irqsave(&spd->lock, pd_flags);

                    /* Ecrire la moitie dans le shadow PDE */
                    if (half == 0) {
                        /* Mot bas : bits [31:0] */
                        spd->pde[pde_index] =
                            (spd->pde[pde_index] & 0xFFFFFFFF00000000ULL) |
                            (uint64_t)cmd->value;
                    } else {
                        /* Mot haut : bits [63:32] */
                        spd->pde[pde_index] =
                            (spd->pde[pde_index] & 0x00000000FFFFFFFFULL) |
                            ((uint64_t)cmd->value << 32);
                    }
                    full_pde = spd->pde[pde_index];

                    spin_unlock_irqrestore(&spd->lock, pd_flags);

                    /* Quand on recoit le mot haut (2eme ecriture),
                     * on a le PDE complet — mettre a jour le shadow
                     */
                    if (half == 4) {
                        pvegpu_shadow_pd_update_pde(ctx, pde_index,
                                                    full_pde);
                    }
                }
            }

            /* --- Interception des ecritures PTE --- */
            /* Les PTEs sont dans les Page Tables pointees par les PDEs.
             * Si l'offset est dans la zone PT (apres le PD dans le RAMIN),
             * on intercepte pour mettre a jour les shadow PTs.
             *
             * Zone PT : offset >= 0x4200 dans le RAMIN du canal.
             * Chaque PT a 8192 entries * 8 bytes = 0x10000.
             * Le PDE_index est determine par l'adresse de la PT.
             */
            {
                uint32_t pt_base = 0x200 + PVEGPU_PD_ENTRIES * 8;

                if (local_offset >= pt_base) {
                    uint32_t pt_offset = local_offset - pt_base;
                    /* Chaque PT = PVEGPU_PT_ENTRIES * 8 bytes */
                    uint32_t pt_size = PVEGPU_PT_ENTRIES * 8;
                    uint32_t pde_index = pt_offset / pt_size;
                    uint32_t pte_byte = pt_offset % pt_size;
                    uint32_t pte_index = pte_byte / 8;
                    uint32_t pte_half = pte_byte % 8;

                    if (pde_index < PVEGPU_PD_ENTRIES &&
                        pte_index < PVEGPU_PT_ENTRIES) {
                        struct pvegpu_shadow_pd *spd = &ctx->shadow_pd;
                        struct pvegpu_shadow_pt *pt;
                        unsigned long pt_flags;

                        spin_lock_irqsave(&spd->lock, pt_flags);
                        pt = spd->pt[pde_index];
                        if (pt) {
                            if (pte_half == 0) {
                                pt->entries[pte_index] =
                                    (pt->entries[pte_index] &
                                     0xFFFFFFFF00000000ULL) |
                                    (uint64_t)cmd->value;
                            } else {
                                pt->entries[pte_index] =
                                    (pt->entries[pte_index] &
                                     0x00000000FFFFFFFFULL) |
                                    ((uint64_t)cmd->value << 32);

                                /* Mot haut recu : PTE complet,
                                 * mettre a jour le shadow */
                                pvegpu_shadow_pt_update_pte(
                                    ctx, pde_index, pte_index,
                                    pt->entries[pte_index]);
                            }
                        }
                        spin_unlock_irqrestore(&spd->lock, pt_flags);
                    }
                }
            }

            /* Ecrire avec l'offset RAMIN traduit */
            spin_lock_irqsave(&gdev->mutex, flags);
            pvegpu_bar_write32(gdev, 0,
                               PRAMIN_BASE + ramin_phys, phys_val);
            spin_unlock_irqrestore(&gdev->mutex, flags);

            PVEGPU_LOG("BAR0 PRAMIN write vmid=%d ramin_virt=0x%x "
                       "ramin_phys=0x%x val=0x%x\n",
                       ctx->vmid, ramin_offset, ramin_phys, cmd->value);
            return;
        }
        break;
    }

    /* 3. Ecriture physique */
    spin_lock_irqsave(&gdev->mutex, flags);
    pvegpu_bar_write32(gdev, 0, cmd->offset, phys_val);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("BAR0 write vmid=%d offset=0x%x val=0x%x->0x%x\n",
               ctx->vmid, cmd->offset, cmd->value, phys_val);
}

int pvegpu_read_bar0(struct pvegpu_vm_ctx *ctx, struct pvegpu_cmd *cmd)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;

    /* Intercepter les lectures dans la fenetre PRAMIN */
    if (cmd->offset >= PRAMIN_BASE && cmd->offset < PRAMIN_END) {
        uint32_t ramin_offset = cmd->offset - PRAMIN_BASE;
        uint32_t ramin_phys = pvegpu_ramin_virt_to_phys(ctx, ramin_offset);

        spin_lock_irqsave(&gdev->mutex, flags);
        cmd->value = pvegpu_bar_read32(gdev, 0, PRAMIN_BASE + ramin_phys);
        spin_unlock_irqrestore(&gdev->mutex, flags);
        return 0;
    }

    /* Bloquer la lecture des registres dangereux */
    if (pvegpu_bar0_is_blocked(cmd->offset)) {
        cmd->value = 0;
        return 0;
    }

    spin_lock_irqsave(&gdev->mutex, flags);
    cmd->value = pvegpu_bar_read32(gdev, 0, cmd->offset);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("BAR0 read  vmid=%d offset=0x%x val=0x%x\n",
               ctx->vmid, cmd->offset, cmd->value);
    return 0;
}

/* ============================================================
 * HANDLER BAR1 COMPLET — Shadow Channel Table
 *
 * BAR1 expose les canaux PFIFO a l'espace CPU.
 * Translation : phys_offset = (virt_cid + id*CHANNELS) * range + in_ch
 *
 * En plus de la translation, on maintient l'etat shadow complet
 * de chaque canal (PUT, GET, IB_PUT, IB_GET, RAMIN addr).
 * ============================================================ */

static uint32_t pvegpu_get_bar1_range(const struct pvegpu_device *gdev)
{
    if (gdev->gpu_ops && gdev->gpu_ops->get_channel_range)
        return gdev->gpu_ops->get_channel_range(gdev);
    return 0x1000;
}

static uint32_t pvegpu_bar1_virt_to_phys_offset(
    const struct pvegpu_vm_ctx *ctx,
    const struct pvegpu_device *gdev,
    uint32_t virt_offset)
{
    uint32_t range = pvegpu_get_bar1_range(gdev);
    uint32_t virt_cid, in_channel_offset;
    uint32_t phys_cid;

    virt_cid         = virt_offset / range;
    in_channel_offset = virt_offset % range;
    phys_cid         = pvegpu_virt_to_phys_channel(ctx, virt_cid);

    return phys_cid * range + in_channel_offset;
}

static struct pvegpu_channel *
pvegpu_channel_from_bar1_offset(struct pvegpu_vm_ctx *ctx,
                                 const struct pvegpu_device *gdev,
                                 uint32_t virt_offset)
{
    uint32_t range = pvegpu_get_bar1_range(gdev);
    uint32_t virt_cid = virt_offset / range;

    if (virt_cid >= PVEGPU_DOMAIN_CHANNELS)
        return NULL;

    return &ctx->channels[virt_cid];
}

/* Offsets des registres critiques dans un canal BAR1 (NVC0) */
#define CHAN_REG_PUT      0x08c  /* PUT pointer */
#define CHAN_REG_GET      0x090  /* GET pointer */
#define CHAN_REG_IB_PUT   0x0a0  /* Indirect buffer PUT */
#define CHAN_REG_IB_GET   0x0a4  /* Indirect buffer GET */
#define CHAN_REG_RAMIN    0x010  /* Adresse RAMIN du canal */
#define CHAN_REG_ENABLE   0x004  /* Bit d'activation du canal */

void pvegpu_write_bar1(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint32_t phys_offset;
    struct pvegpu_channel *ch;
    uint32_t range, in_ch;
    uint32_t translated_val = cmd->value;

    phys_offset = pvegpu_bar1_virt_to_phys_offset(ctx, gdev, cmd->offset);

    /* Mise a jour de l'etat shadow du canal */
    ch = pvegpu_channel_from_bar1_offset(ctx, gdev, cmd->offset);
    if (ch) {
        range = pvegpu_get_bar1_range(gdev);
        in_ch = cmd->offset % range;

        switch (in_ch) {
        case CHAN_REG_PUT:
            ch->put = cmd->value;
            ch->enabled = (cmd->value != 0);
            break;
        case CHAN_REG_GET:
            ch->get = cmd->value;
            break;
        case CHAN_REG_IB_PUT:
            ch->ib_put = cmd->value;
            break;
        case CHAN_REG_IB_GET:
            ch->ib_get = cmd->value;
            break;
        case CHAN_REG_RAMIN:
            /* Traduire l'adresse RAMIN */
            translated_val = pvegpu_ramin_virt_to_phys(ctx, cmd->value);
            ch->ramin_addr = translated_val;
            break;
        case CHAN_REG_ENABLE:
            ch->enabled = !!(cmd->value & 0x1);
            break;
        }
    }

    spin_lock_irqsave(&gdev->mutex, flags);
    pvegpu_bar_write32(gdev, 1, phys_offset, translated_val);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("BAR1 write vmid=%d virt=0x%x phys=0x%x val=0x%x->0x%x\n",
               ctx->vmid, cmd->offset, phys_offset,
               cmd->value, translated_val);
}

int pvegpu_read_bar1(struct pvegpu_vm_ctx *ctx, struct pvegpu_cmd *cmd)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint32_t phys_offset;

    phys_offset = pvegpu_bar1_virt_to_phys_offset(ctx, gdev, cmd->offset);

    spin_lock_irqsave(&gdev->mutex, flags);
    cmd->value = pvegpu_bar_read32(gdev, 1, phys_offset);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("BAR1 read  vmid=%d virt=0x%x phys=0x%x val=0x%x\n",
               ctx->vmid, cmd->offset, phys_offset, cmd->value);
    return 0;
}

/* ============================================================
 * HANDLER BAR3 COMPLET — Translation VRAM + Fenetre glissante
 *
 * BAR3 est la fenetre CPU vers la VRAM GPU.
 * Translation : phys = virt + id * vram_size
 *
 * Pour VRAM > 4GB : on utilise une fenetre glissante dans BAR3.
 * La VM ecrit dans un registre de controle pour deplacer la
 * fenetre. L'adresse physique finale est :
 *   phys = window_base + (virt_offset % window_size) + ctx_shift
 *
 * Equivalent du BAR3 remapping dans gxen (--bar3-remapping).
 * ============================================================ */

/* Registre de controle de la fenetre BAR3 (virtuel, intercepte) */
#define BAR3_WINDOW_CTRL  0xFFFF0000

void pvegpu_write_bar3(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint64_t phys_offset;
    uint64_t virt_offset;

    /* Registre de controle de la fenetre glissante (virtuel) */
    if (cmd->offset == BAR3_WINDOW_CTRL) {
        uint64_t new_base = (uint64_t)cmd->value << 20;  /* en MB */
        if (new_base < ctx->vram_size) {
            ctx->bar3_window_base = new_base;
            PVEGPU_LOG("BAR3 window: vmid=%d base=%llu MB\n",
                       ctx->vmid, new_base >> 20);
        }
        return;
    }

    virt_offset = (uint64_t)cmd->offset;

    /* Pour VRAM > 4GB : appliquer la fenetre glissante */
    if (ctx->vram_size > 0xffffffffULL) {
        /* L'offset virtuel est relatif a la fenetre */
        if (virt_offset >= PVEGPU_BAR3_WINDOW_SIZE) {
            PVEGPU_ERR("BAR3 write: offset 0x%x beyond window\n",
                       cmd->offset);
            return;
        }
        virt_offset += ctx->bar3_window_base;
    }

    /* Verifier les limites */
    if (virt_offset >= ctx->vram_size) {
        PVEGPU_ERR("BAR3 write out of bounds: vmid=%d offset=0x%llx "
                   "vram_size=0x%llx\n",
                   ctx->vmid, virt_offset, ctx->vram_size);
        return;
    }

    /* Translation VRAM */
    phys_offset = virt_offset + pvegpu_ctx_addr_shift(ctx);

    /* Pour les offsets > 32 bits : utiliser un acces 64-bit
     * via une double ecriture BAR0 sur la fenetre PRAMIN.
     */
    if (phys_offset > 0xffffffffULL) {
        /* Utiliser la fenetre PRAMIN pour acceder a la haute VRAM.
         * 1. Configurer la fenetre PRAMIN vers l'adresse cible
         * 2. Ecrire via la fenetre
         */
        uint32_t pramin_window_reg = 0x001700;  /* PBUS_BAR3_WINDOW */
        uint32_t window_page = (uint32_t)(phys_offset >> 16);
        uint32_t in_page_offset = (uint32_t)(phys_offset & 0xFFFF);

        spin_lock_irqsave(&gdev->mutex, flags);
        /* Configurer la fenetre BAR3 physique */
        pvegpu_bar_write32(gdev, 0, pramin_window_reg,
                           window_page | 0x1);
        /* Ecrire via la fenetre */
        pvegpu_bar_write32(gdev, 3, in_page_offset, cmd->value);
        spin_unlock_irqrestore(&gdev->mutex, flags);

        PVEGPU_LOG("BAR3 write (hi) vmid=%d virt=0x%llx phys=0x%llx "
                   "val=0x%x\n",
                   ctx->vmid, virt_offset, phys_offset, cmd->value);
        return;
    }

    spin_lock_irqsave(&gdev->mutex, flags);
    pvegpu_bar_write32(gdev, 3, (uint32_t)phys_offset, cmd->value);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("BAR3 write vmid=%d virt=0x%llx phys=0x%llx val=0x%x\n",
               ctx->vmid, virt_offset, phys_offset, cmd->value);
}

int pvegpu_read_bar3(struct pvegpu_vm_ctx *ctx, struct pvegpu_cmd *cmd)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint64_t phys_offset;
    uint64_t virt_offset;

    virt_offset = (uint64_t)cmd->offset;

    /* Fenetre glissante pour VRAM > 4GB */
    if (ctx->vram_size > 0xffffffffULL) {
        if (virt_offset >= PVEGPU_BAR3_WINDOW_SIZE) {
            cmd->value = 0xffffffff;
            return -EFAULT;
        }
        virt_offset += ctx->bar3_window_base;
    }

    if (virt_offset >= ctx->vram_size) {
        PVEGPU_ERR("BAR3 read out of bounds: vmid=%d\n", ctx->vmid);
        cmd->value = 0xffffffff;
        return -EFAULT;
    }

    phys_offset = virt_offset + pvegpu_ctx_addr_shift(ctx);

    /* Acces haute VRAM via fenetre PRAMIN */
    if (phys_offset > 0xffffffffULL) {
        uint32_t pramin_window_reg = 0x001700;
        uint32_t window_page = (uint32_t)(phys_offset >> 16);
        uint32_t in_page_offset = (uint32_t)(phys_offset & 0xFFFF);

        spin_lock_irqsave(&gdev->mutex, flags);
        pvegpu_bar_write32(gdev, 0, pramin_window_reg,
                           window_page | 0x1);
        cmd->value = pvegpu_bar_read32(gdev, 3, in_page_offset);
        spin_unlock_irqrestore(&gdev->mutex, flags);
        return 0;
    }

    spin_lock_irqsave(&gdev->mutex, flags);
    cmd->value = pvegpu_bar_read32(gdev, 3, (uint32_t)phys_offset);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("BAR3 read  vmid=%d virt=0x%llx phys=0x%llx val=0x%x\n",
               ctx->vmid, virt_offset, phys_offset, cmd->value);
    return 0;
}

/* ============================================================
 * SHADOW BAR1 — synchronisation lors du context switch
 *
 * Quand le scheduler BAND transfere le GPU d'une VM a une autre,
 * on resynchronise la shadow table des canaux dans BAR1.
 *
 * Equivalent de device_bar1::shadow(context* ctx) dans a3.
 * ============================================================ */

void pvegpu_shadow_bar1(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    uint32_t range = pvegpu_get_bar1_range(gdev);
    unsigned long flags;
    uint32_t vcid;

    PVEGPU_LOG("shadow_bar1: vmid=%d\n", ctx->vmid);

    spin_lock_irqsave(&gdev->mutex, flags);

    for (vcid = 0; vcid < PVEGPU_DOMAIN_CHANNELS; vcid++) {
        struct pvegpu_channel *ch = &ctx->channels[vcid];
        uint32_t phys_offset;

        if (!ch->enabled)
            continue;

        phys_offset = ch->phys_id * range;

        /* Restaurer l'adresse RAMIN du canal
         * Equivalent de device_bar1::map() : entry_.write32(...)
         */
        if (ch->ramin_addr) {
            pvegpu_bar_write32(gdev, 1, phys_offset + CHAN_REG_RAMIN,
                               (uint32_t)ch->ramin_addr);
        }

        /* Restaurer le PUT pointer (active le canal) */
        pvegpu_bar_write32(gdev, 1, phys_offset + CHAN_REG_PUT,
                           ch->put);

        /* Restaurer le GET pointer */
        pvegpu_bar_write32(gdev, 1, phys_offset + CHAN_REG_GET,
                           ch->get);

        /* Restaurer les indirect buffers */
        pvegpu_bar_write32(gdev, 1, phys_offset + CHAN_REG_IB_PUT,
                           ch->ib_put);
        pvegpu_bar_write32(gdev, 1, phys_offset + CHAN_REG_IB_GET,
                           ch->ib_get);

        PVEGPU_LOG("shadow_bar1: restored ch %u -> phys %u "
                   "ramin=0x%llx put=0x%x get=0x%x\n",
                   vcid, ch->phys_id, ch->ramin_addr,
                   ch->put, ch->get);
    }

    spin_unlock_irqrestore(&gdev->mutex, flags);
}

/* ============================================================
 * FLUSH TLB GPU — serialise entre VMs
 *
 * Utilise le shadow page directory de chaque VM pour le flush.
 * Le tlb_flush_lock du scheduler empeche les flush concurrents.
 * ============================================================ */

int pvegpu_flush_tlb(struct pvegpu_vm_ctx *ctx, uint32_t engine)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    int timeout = 1000;
    uint32_t status;
    uint64_t shadow_pd_addr;

    /* Adresse du shadow page directory de cette VM */
    shadow_pd_addr = ctx->shadow_pd.phys_addr;

    spin_lock_irqsave(&gdev->mutex, flags);

    /* Attendre que le TLB soit disponible */
    do {
        status = pvegpu_bar_read32(gdev, 0, 0x100c80);
        if ((status & 0x00ff0000) == 0)
            break;
        udelay(1);
    } while (--timeout > 0);

    if (timeout == 0) {
        spin_unlock_irqrestore(&gdev->mutex, flags);
        PVEGPU_ERR("flush_tlb: timeout waiting for TLB vmid=%d\n",
                   ctx->vmid);
        return -ETIMEDOUT;
    }

    /* Ecrire l'adresse du shadow PD et declencher le flush */
    pvegpu_bar_write32(gdev, 0, 0x100cb8,
                       (uint32_t)(shadow_pd_addr >> 8));
    pvegpu_bar_write32(gdev, 0, 0x100cbc,
                       0x80000000 | engine);

    /* Attendre la fin du flush */
    timeout = 1000;
    do {
        status = pvegpu_bar_read32(gdev, 0, 0x100c80);
        if (status & 0x00008000)
            break;
        udelay(1);
    } while (--timeout > 0);

    spin_unlock_irqrestore(&gdev->mutex, flags);

    if (timeout == 0) {
        PVEGPU_ERR("flush_tlb: timeout waiting for flush done vmid=%d\n",
                   ctx->vmid);
        return -ETIMEDOUT;
    }

    PVEGPU_LOG("flush_tlb: done vmid=%d engine=0x%x shadow_pd=0x%llx\n",
               ctx->vmid, engine, shadow_pd_addr);
    return 0;
}
