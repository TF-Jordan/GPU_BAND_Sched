// SPDX-License-Identifier: GPL-2.0-only
/*
 * pve_gpu_sched_core.c — PVE GPU Scheduler — Device & Context
 *
 * Couvre :
 *  1. Module init/exit
 *  2. pvegpu_device_init / fini
 *  3. pvegpu_ctx_create / destroy
 *  4. Accesseurs BAR physiques
 *  5. GPU ops vendor-specific (NVIDIA NVC0, AMD GCN)
 *
 * La logique de scheduling BAND est dans pve_gpu_sched_band.c
 * L'interface mdev (VFIO) est dans pve_gpu_sched_mdev.c
 * Les handlers BAR sont dans pve_gpu_sched_bar.c
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/pci.h>
#include <linux/io.h>
#include <linux/slab.h>
#include <linux/bitmap.h>
#include <linux/spinlock.h>
#include <linux/interrupt.h>

#include "pve_gpu_sched.h"

/*
 * Note : MODULE_LICENSE, module_init/exit sont dans pve_gpu_sched_mdev.c
 * car c'est le fichier qui enregistre les drivers PCI et mdev.
 */

/* ============================================================
 * SINGLETON DEVICE
 * ============================================================ */

static struct pvegpu_device *g_pvegpu_dev;

struct pvegpu_device *pvegpu_device_get(void)
{
    return g_pvegpu_dev;
}

/* ============================================================
 * ACCESSEURS BAR PHYSIQUES
 * ============================================================ */

uint32_t pvegpu_bar_read32(struct pvegpu_device *gdev, int bar, uint32_t offset)
{
    if (WARN_ON(!gdev->bar[bar]))
        return 0xffffffff;
    if (WARN_ON(offset >= gdev->bar_size[bar]))
        return 0xffffffff;
    return ioread32(gdev->bar[bar] + offset);
}

void pvegpu_bar_write32(struct pvegpu_device *gdev, int bar,
                         uint32_t offset, uint32_t val)
{
    if (WARN_ON(!gdev->bar[bar]))
        return;
    if (WARN_ON(offset >= gdev->bar_size[bar]))
        return;
    iowrite32(val, gdev->bar[bar] + offset);
}

/* ============================================================
 * NVIDIA NVC0 GPU OPS
 * ============================================================ */

static uint32_t nvc0_get_channel_range(const struct pvegpu_device *gdev)
{
    if ((gdev->chipset_type & 0x1f0) == 0x0c0)
        return 0x1000;
    return 0x200;
}

static uint32_t nvc0_get_status_reg(void)
{
    return 0x400700;  /* PGRAPH_STATUS */
}

static uint32_t nvc0_get_ctx_table_reg(void)
{
    return 0x001700;  /* PFIFO_CTX_TABLE */
}

static uint32_t nvc0_detect_channels(struct pvegpu_device *gdev)
{
    uint32_t chan_reg = pvegpu_bar_read32(gdev, 0, 0x002600);

    if ((gdev->chipset_type & 0x1f0) == 0x0c0) {
        uint32_t n = chan_reg & 0xff;
        return (n > 0 && n <= 128) ? n : 128;
    }
    {
        uint32_t n = chan_reg & 0xfff;
        return (n > 0 && n <= 4096) ? n : 4096;
    }
}

static uint32_t nvc0_ramin_translate(const struct pvegpu_vm_ctx *ctx,
                                      uint32_t virt_ramin)
{
    uint32_t channel_offset = ctx->id * PVEGPU_DOMAIN_CHANNELS
                              * PVEGPU_RAMIN_PER_CHANNEL;
    return virt_ramin + channel_offset;
}

static int nvc0_flush_tlb(struct pvegpu_vm_ctx *ctx, uint32_t engine)
{
    return pvegpu_flush_tlb(ctx, engine);
}

static int nvc0_context_reset(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint32_t i;

    PVEGPU_INFO("FLR: resetting context vmid=%d slot=%u\n",
                ctx->vmid, ctx->id);

    spin_lock_irqsave(&gdev->mutex, flags);

    for (i = 0; i < PVEGPU_DOMAIN_CHANNELS; i++) {
        struct pvegpu_channel *ch = &ctx->channels[i];
        uint32_t range = nvc0_get_channel_range(gdev);
        uint32_t phys_offset = ch->phys_id * range;

        if (ch->enabled) {
            pvegpu_bar_write32(gdev, 1, phys_offset + 0x08c, 0);
            ch->enabled = false;
            ch->put = 0;
            ch->get = 0;
            ch->ib_put = 0;
            ch->ib_get = 0;
        }
    }

    spin_unlock_irqrestore(&gdev->mutex, flags);

    kfifo_reset(&ctx->suspended);

    spin_lock_irqsave(&ctx->band_lock, flags);
    ctx->budget = ktime_set(0, 0);
    ctx->bandwidth_used = ktime_set(0, 0);
    spin_unlock_irqrestore(&ctx->band_lock, flags);

    if (gdev->gpu_ops && gdev->gpu_ops->flush_tlb)
        gdev->gpu_ops->flush_tlb(ctx, 0x1 | 0x4);

    PVEGPU_INFO("FLR: context reset complete vmid=%d\n", ctx->vmid);
    return 0;
}

/* ============================================================
 * NVIDIA NVC0 COMPUTE STATE SAVE/RESTORE
 *
 * Pour les workloads CUDA, le GPU sauvegarde automatiquement le
 * contexte compute dans le RAMIN du canal actif. Notre role est de :
 *  1. Sauvegarder les registres PFIFO et PGRAPH necessaires
 *  2. Attendre que le GPU soit idle
 *  3. Pointer le PFIFO vers le bon RAMIN au restore
 *
 * Registres NVC0 sauvegardes :
 *  - 0x001700 : PFIFO_CTX_TABLE (adresse RAMIN du canal actif)
 *  - 0x400700 : PGRAPH_STATUS (lecture seule, idle detection)
 *  - 0x400100 : PGRAPH_INTR (interruptions PGRAPH)
 *  - 0x40060c : PGRAPH_CTXCTL_CUR (contexte PGRAPH courant)
 *  - 0x400824 : PGRAPH_DISPATCH (dispatch state)
 *  - 0x419860 : GPC_CWD_FS (compute warp dispatch)
 *  - 0x419a04 : GPC_CWD_BUNDLE_INIT (compute bundle)
 *  - 0x405844 : TPC_COMPUTE_REG (compute register file)
 * ============================================================ */

/* Registres PGRAPH compute a sauvegarder (NVC0) */
static const uint32_t nvc0_pgraph_compute_regs[] = {
    0x400100,   /* PGRAPH_INTR */
    0x40060c,   /* PGRAPH_CTXCTL_CUR */
    0x400824,   /* PGRAPH_DISPATCH */
    0x419860,   /* GPC_CWD_FS */
    0x419a04,   /* GPC_CWD_BUNDLE_INIT */
    0x405844,   /* TPC_COMPUTE_REG */
    0x40584c,   /* TPC_COMPUTE_REG_2 */
    0x400708,   /* PGRAPH_VSTATUS (vertex status) */
};

static void nvc0_compute_save(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    int i;

    spin_lock_irqsave(&gdev->mutex, flags);

    /* Sauvegarder PFIFO_CTX_TABLE */
    ctx->compute.pfifo_ctx_table =
        pvegpu_bar_read32(gdev, 0, 0x001700);

    /* Sauvegarder les registres PGRAPH compute */
    for (i = 0; i < PVEGPU_NV_PGRAPH_SAVE_REGS &&
                i < ARRAY_SIZE(nvc0_pgraph_compute_regs); i++) {
        ctx->compute.pgraph_regs[i] =
            pvegpu_bar_read32(gdev, 0, nvc0_pgraph_compute_regs[i]);
    }

    /* Sauvegarder le status du context switch PGRAPH */
    ctx->compute.pgraph_ctxsw_status =
        pvegpu_bar_read32(gdev, 0, 0x400604);

    spin_unlock_irqrestore(&gdev->mutex, flags);

    ctx->compute.saved = true;

    PVEGPU_LOG("compute_save: vmid=%d pfifo_ctx=0x%x\n",
               ctx->vmid, ctx->compute.pfifo_ctx_table);
}

static void nvc0_compute_restore(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint32_t ramin_phys;

    if (!ctx->compute.saved)
        return;

    spin_lock_irqsave(&gdev->mutex, flags);

    /* Restaurer PFIFO_CTX_TABLE avec l'adresse RAMIN translatee.
     * Le RAMIN contient le page directory et le contexte compute
     * de la VM. Le GPU charge automatiquement le reste.
     */
    ramin_phys = ctx->id * PVEGPU_DOMAIN_CHANNELS
                 * PVEGPU_RAMIN_PER_CHANNEL;
    pvegpu_bar_write32(gdev, 0, 0x001700,
                       (ramin_phys >> 12) | 0x80000000);

    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("compute_restore: vmid=%d ramin_phys=0x%x\n",
               ctx->vmid, ramin_phys);
}

static int nvc0_wait_idle(struct pvegpu_device *gdev, int timeout_us)
{
    int i;
    uint32_t status;

    for (i = 0; i < timeout_us; i++) {
        status = pvegpu_bar_read32(gdev, 0, 0x400700);
        if (status == 0)
            return 0;
        udelay(1);
    }

    PVEGPU_ERR("nvc0_wait_idle: timeout (status=0x%08x)\n", status);
    return -ETIMEDOUT;
}

const struct pvegpu_gpu_ops pvegpu_nvidia_nvc0_ops = {
    .name              = "NVIDIA NVC0+",
    .get_channel_range = nvc0_get_channel_range,
    .get_status_reg    = nvc0_get_status_reg,
    .get_ctx_table_reg = nvc0_get_ctx_table_reg,
    .detect_channels   = nvc0_detect_channels,
    .flush_tlb         = nvc0_flush_tlb,
    .context_reset     = nvc0_context_reset,
    .ramin_translate   = nvc0_ramin_translate,
    .compute_save      = nvc0_compute_save,
    .compute_restore   = nvc0_compute_restore,
    .wait_idle         = nvc0_wait_idle,
};

/* ============================================================
 * AMD GCN GPU OPS
 * ============================================================ */

#define AMD_GRBM_STATUS         0x8010
#define AMD_CP_RB_BASE          0x8040
#define AMD_SRBM_STATUS         0x0E50
#define AMD_VM_CONTEXT0_CNTL    0x1410
#define AMD_VM_INVALIDATE_ENG0  0x1490
#define AMD_CONFIG_MEMSIZE      0x5428

static uint32_t amd_get_channel_range(const struct pvegpu_device *gdev)
{
    return 0x1000;
}

static uint32_t amd_get_status_reg(void)
{
    return AMD_GRBM_STATUS;
}

static uint32_t amd_get_ctx_table_reg(void)
{
    return AMD_CP_RB_BASE;
}

static uint32_t amd_detect_channels(struct pvegpu_device *gdev)
{
    return 64;
}

static uint32_t amd_ramin_translate(const struct pvegpu_vm_ctx *ctx,
                                     uint32_t virt_addr)
{
    uint32_t offset = ctx->id * PVEGPU_DOMAIN_CHANNELS * 0x1000;
    return virt_addr + offset;
}

static int amd_flush_tlb(struct pvegpu_vm_ctx *ctx, uint32_t engine)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    int timeout = 1000;
    uint32_t status;
    uint32_t vm_id = ctx->id;

    spin_lock_irqsave(&gdev->mutex, flags);

    pvegpu_bar_write32(gdev, 0, AMD_VM_INVALIDATE_ENG0, 1 << vm_id);

    do {
        status = pvegpu_bar_read32(gdev, 0, AMD_VM_INVALIDATE_ENG0 + 4);
        if ((status & (1 << vm_id)) == 0)
            break;
        udelay(1);
    } while (--timeout > 0);

    spin_unlock_irqrestore(&gdev->mutex, flags);

    if (timeout == 0) {
        PVEGPU_ERR("AMD flush_tlb: timeout vmid=%d\n", ctx->vmid);
        return -ETIMEDOUT;
    }
    return 0;
}

static int amd_context_reset(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    uint32_t i;

    PVEGPU_INFO("AMD FLR: resetting context vmid=%d\n", ctx->vmid);

    spin_lock_irqsave(&gdev->mutex, flags);
    for (i = 0; i < PVEGPU_DOMAIN_CHANNELS; i++) {
        struct pvegpu_channel *ch = &ctx->channels[i];
        if (ch->enabled) {
            ch->enabled = false;
            ch->put = 0;
            ch->get = 0;
        }
    }
    pvegpu_bar_write32(gdev, 0,
                       AMD_VM_CONTEXT0_CNTL + ctx->id * 4, 0);
    spin_unlock_irqrestore(&gdev->mutex, flags);

    kfifo_reset(&ctx->suspended);

    spin_lock_irqsave(&ctx->band_lock, flags);
    ctx->budget = ktime_set(0, 0);
    ctx->bandwidth_used = ktime_set(0, 0);
    spin_unlock_irqrestore(&ctx->band_lock, flags);

    amd_flush_tlb(ctx, 0);
    return 0;
}

/* ============================================================
 * AMD GCN COMPUTE STATE SAVE/RESTORE
 *
 * AMD utilise des ring buffers (CP_RB) pour les commandes compute.
 * On sauvegarde/restaure les pointeurs de ring buffer et les
 * registres de contexte VM pour chaque VM.
 * ============================================================ */

#define AMD_CP_RB_WPTR          0x8060
#define AMD_CP_RB_RPTR          0x8064
#define AMD_COMPUTE_STATIC_THREAD_MGMT_SE0  0x8E20
#define AMD_COMPUTE_STATIC_THREAD_MGMT_SE1  0x8E24
#define AMD_COMPUTE_STATIC_THREAD_MGMT_SE2  0x8E28
#define AMD_COMPUTE_STATIC_THREAD_MGMT_SE3  0x8E2C
#define AMD_COMPUTE_RESOURCE_LIMITS         0x8E30
#define AMD_COMPUTE_TMPRING_SIZE            0x8E34
#define AMD_SPI_COMPUTE_QUEUE_RESET         0x8E44
#define AMD_SPI_COMPUTE_WF_CTX_SAVE         0x8E48

static const uint32_t amd_compute_regs[] = {
    AMD_COMPUTE_STATIC_THREAD_MGMT_SE0,
    AMD_COMPUTE_STATIC_THREAD_MGMT_SE1,
    AMD_COMPUTE_RESOURCE_LIMITS,
    AMD_COMPUTE_TMPRING_SIZE,
    AMD_SPI_COMPUTE_QUEUE_RESET,
    AMD_SPI_COMPUTE_WF_CTX_SAVE,
    0x8E00,     /* COMPUTE_DISPATCH_INITIATOR */
    0x8E10,     /* COMPUTE_NUM_THREAD_X */
};

static void amd_compute_save(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    int i;

    spin_lock_irqsave(&gdev->mutex, flags);

    ctx->compute.amd_cp_rb_base =
        pvegpu_bar_read32(gdev, 0, AMD_CP_RB_BASE);
    ctx->compute.amd_cp_rb_wptr =
        pvegpu_bar_read32(gdev, 0, AMD_CP_RB_WPTR);
    ctx->compute.amd_cp_rb_rptr =
        pvegpu_bar_read32(gdev, 0, AMD_CP_RB_RPTR);
    ctx->compute.amd_vm_context_cntl =
        pvegpu_bar_read32(gdev, 0, AMD_VM_CONTEXT0_CNTL + ctx->id * 4);

    for (i = 0; i < PVEGPU_AMD_COMPUTE_SAVE_REGS &&
                i < ARRAY_SIZE(amd_compute_regs); i++) {
        ctx->compute.amd_compute_regs[i] =
            pvegpu_bar_read32(gdev, 0, amd_compute_regs[i]);
    }

    spin_unlock_irqrestore(&gdev->mutex, flags);

    ctx->compute.saved = true;

    PVEGPU_LOG("amd_compute_save: vmid=%d rb_base=0x%x\n",
               ctx->vmid, ctx->compute.amd_cp_rb_base);
}

static void amd_compute_restore(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev = ctx->dev;
    unsigned long flags;
    int i;

    if (!ctx->compute.saved)
        return;

    spin_lock_irqsave(&gdev->mutex, flags);

    /* Restaurer le contexte VM pour cette VM */
    pvegpu_bar_write32(gdev, 0,
                       AMD_VM_CONTEXT0_CNTL + ctx->id * 4,
                       ctx->compute.amd_vm_context_cntl);

    /* Restaurer les registres compute */
    for (i = 0; i < PVEGPU_AMD_COMPUTE_SAVE_REGS &&
                i < ARRAY_SIZE(amd_compute_regs); i++) {
        pvegpu_bar_write32(gdev, 0, amd_compute_regs[i],
                           ctx->compute.amd_compute_regs[i]);
    }

    spin_unlock_irqrestore(&gdev->mutex, flags);

    PVEGPU_LOG("amd_compute_restore: vmid=%d\n", ctx->vmid);
}

static int amd_wait_idle(struct pvegpu_device *gdev, int timeout_us)
{
    int i;
    uint32_t status;

    for (i = 0; i < timeout_us; i++) {
        status = pvegpu_bar_read32(gdev, 0, AMD_GRBM_STATUS);
        /* GRBM_STATUS: bits indiquant l'activite GPU.
         * Si les engines sont idle, status & activity_mask == 0.
         */
        if ((status & 0x80000000) == 0)
            return 0;
        udelay(1);
    }

    PVEGPU_ERR("amd_wait_idle: timeout (status=0x%08x)\n", status);
    return -ETIMEDOUT;
}

const struct pvegpu_gpu_ops pvegpu_amd_gcn_ops = {
    .name              = "AMD GCN+",
    .get_channel_range = amd_get_channel_range,
    .get_status_reg    = amd_get_status_reg,
    .get_ctx_table_reg = amd_get_ctx_table_reg,
    .detect_channels   = amd_detect_channels,
    .flush_tlb         = amd_flush_tlb,
    .context_reset     = amd_context_reset,
    .ramin_translate   = amd_ramin_translate,
    .compute_save      = amd_compute_save,
    .compute_restore   = amd_compute_restore,
    .wait_idle         = amd_wait_idle,
};

/* ============================================================
 * DEVICE INIT / FINI
 * ============================================================ */

static int pvegpu_detect_chipset(struct pvegpu_device *gdev)
{
    uint32_t boot0;

    if (!gdev->bar[0]) {
        PVEGPU_ERR("BAR0 not mapped, cannot detect chipset\n");
        return -EIO;
    }

    if (gdev->vendor_id == PVEGPU_VENDOR_AMD) {
        boot0 = pvegpu_bar_read32(gdev, 0, 0x0);
        gdev->chipset_type = (boot0 >> 16) & 0xffff;
        gdev->gpu_ops = &pvegpu_amd_gcn_ops;
        PVEGPU_INFO("detected AMD chipset 0x%x\n", gdev->chipset_type);
        return 0;
    }

    boot0 = pvegpu_bar_read32(gdev, 0, 0x0);
    gdev->chipset_type = (boot0 >> 20) & 0x1ff;
    gdev->gpu_ops = &pvegpu_nvidia_nvc0_ops;

    PVEGPU_INFO("detected NVIDIA chipset 0x%x from PMC_BOOT_0=0x%08x\n",
                gdev->chipset_type, boot0);
    return 0;
}

static int pvegpu_detect_vram(struct pvegpu_device *gdev)
{
    uint32_t vram_mb;

    if (gdev->vendor_id == PVEGPU_VENDOR_AMD) {
        uint32_t memsize = pvegpu_bar_read32(gdev, 0, AMD_CONFIG_MEMSIZE);
        vram_mb = memsize >> 20;
        if (vram_mb == 0)
            vram_mb = (uint32_t)(gdev->bar_size[0] >> 20);
    } else {
        vram_mb = pvegpu_bar_read32(gdev, 0, 0x10f20c) & 0xff;
        if (vram_mb == 0)
            vram_mb = pvegpu_bar_read32(gdev, 0, 0x10f200) & 0xfff;
    }

    if (vram_mb == 0) {
        PVEGPU_ERR("could not detect VRAM size\n");
        return -EIO;
    }

    gdev->vram_total = (uint64_t)vram_mb << 20;
    PVEGPU_INFO("detected %u MB VRAM total\n", vram_mb);
    return 0;
}

int pvegpu_device_init(struct pvegpu_device *gdev, struct pci_dev *pdev)
{
    int ret;
    int bar;

    memset(gdev, 0, sizeof(*gdev));
    gdev->pdev = pdev;
    gdev->vendor_id = pdev->vendor;
    spin_lock_init(&gdev->mutex);
    bitmap_zero(gdev->virt_slots, PVEGPU_MAX_DOMAINS);

    PVEGPU_ASSERT_CONFIG();

    ret = pci_enable_device(pdev);
    if (ret) {
        PVEGPU_ERR("pci_enable_device failed: %d\n", ret);
        return ret;
    }

    ret = pci_request_regions(pdev, "pve_gpu_sched");
    if (ret) {
        PVEGPU_ERR("pci_request_regions failed: %d\n", ret);
        goto err_disable;
    }

    pci_set_master(pdev);

    for (bar = 0; bar < 6; bar++) {
        if (pci_resource_len(pdev, bar) == 0)
            continue;
        gdev->bar[bar] = pci_iomap(pdev, bar, 0);
        if (!gdev->bar[bar]) {
            PVEGPU_ERR("pci_iomap BAR%d failed\n", bar);
            ret = -ENOMEM;
            goto err_unmap;
        }
        gdev->bar_size[bar] = pci_resource_len(pdev, bar);
        PVEGPU_INFO("BAR%d mapped at %p size 0x%llx\n",
                    bar, gdev->bar[bar],
                    (unsigned long long)gdev->bar_size[bar]);
    }

    ret = pvegpu_detect_chipset(gdev);
    if (ret)
        goto err_unmap;

    ret = pvegpu_detect_vram(gdev);
    if (ret)
        goto err_unmap;

    gdev->vram_per_vm = gdev->vram_total / PVEGPU_MAX_DOMAINS;
    PVEGPU_INFO("VRAM per VM: %llu MB\n",
                (unsigned long long)(gdev->vram_per_vm >> 20));

    if (gdev->gpu_ops && gdev->gpu_ops->detect_channels)
        gdev->total_channels = gdev->gpu_ops->detect_channels(gdev);
    else
        gdev->total_channels = 128;

    PVEGPU_INFO("detected %u GPU channels\n", gdev->total_channels);

    gdev->gpu_irq = pdev->irq;
    gdev->irq_registered = false;

    ret = pvegpu_sched_init(&gdev->scheduler, gdev,
                             ktime_set(0, PVEGPU_SCHED_PERIOD_US * 1000));
    if (ret) {
        PVEGPU_ERR("pvegpu_sched_init failed: %d\n", ret);
        goto err_unmap;
    }

    PVEGPU_INFO("device initialized: vendor=%s chipset=0x%x vram=%llu MB "
                "channels=%u\n",
                gdev->gpu_ops ? gdev->gpu_ops->name : "unknown",
                gdev->chipset_type,
                (unsigned long long)(gdev->vram_total >> 20),
                gdev->total_channels);

    g_pvegpu_dev = gdev;
    return 0;

err_unmap:
    for (bar = 0; bar < 6; bar++) {
        if (gdev->bar[bar]) {
            pci_iounmap(pdev, gdev->bar[bar]);
            gdev->bar[bar] = NULL;
        }
    }
    pci_release_regions(pdev);
err_disable:
    pci_disable_device(pdev);
    return ret;
}

void pvegpu_device_fini(struct pvegpu_device *gdev)
{
    int bar;

    if (!gdev || !gdev->pdev)
        return;

    pvegpu_sched_fini(&gdev->scheduler);

    for (bar = 0; bar < 6; bar++) {
        if (gdev->bar[bar]) {
            pci_iounmap(gdev->pdev, gdev->bar[bar]);
            gdev->bar[bar] = NULL;
        }
    }

    pci_release_regions(gdev->pdev);
    pci_disable_device(gdev->pdev);
    g_pvegpu_dev = NULL;

    PVEGPU_INFO("device finalized\n");
}

/* ============================================================
 * GESTION DES SLOTS VIRTUELS
 * ============================================================ */

static int pvegpu_acquire_slot(struct pvegpu_device *gdev)
{
    unsigned long slot;
    unsigned long flags;

    spin_lock_irqsave(&gdev->mutex, flags);
    slot = find_first_zero_bit(gdev->virt_slots, PVEGPU_MAX_DOMAINS);
    if (slot >= PVEGPU_MAX_DOMAINS) {
        spin_unlock_irqrestore(&gdev->mutex, flags);
        return -ENOSPC;
    }
    set_bit(slot, gdev->virt_slots);
    gdev->n_contexts++;
    spin_unlock_irqrestore(&gdev->mutex, flags);

    return (int)slot;
}

static void pvegpu_release_slot(struct pvegpu_device *gdev, int slot)
{
    unsigned long flags;

    spin_lock_irqsave(&gdev->mutex, flags);
    clear_bit(slot, gdev->virt_slots);
    gdev->n_contexts--;
    spin_unlock_irqrestore(&gdev->mutex, flags);
}

/* ============================================================
 * CONTEXT INIT / CREATE / DESTROY
 * ============================================================ */

/*
 * pvegpu_ctx_init — initialise un pvegpu_vm_ctx deja alloue.
 *
 * Utilise par pvegpu_mdev_probe() avec vfio_alloc_device() sur kernel 6.8+,
 * ou par pvegpu_ctx_create() (kzalloc) sur kernels plus anciens.
 */
int pvegpu_ctx_init(struct pvegpu_vm_ctx *ctx,
                    struct pvegpu_device *gdev, int vmid,
                    uint32_t weight)
{
    int slot;
    int ret;
    uint32_t i;

    slot = pvegpu_acquire_slot(gdev);
    if (slot < 0) {
        PVEGPU_ERR("no GPU slot available for vmid %d\n", vmid);
        return slot;
    }

    ctx->id             = (uint32_t)slot;
    ctx->vmid           = vmid;
    ctx->vram_size      = gdev->vram_per_vm;
    ctx->dev            = gdev;
    ctx->initialized    = false;
    ctx->weight         = (weight > 0 && weight <= 100) ?
                          weight : PVEGPU_DEFAULT_WEIGHT;

    spin_lock_init(&ctx->band_lock);
    INIT_LIST_HEAD(&ctx->list);

    ret = kfifo_alloc(&ctx->suspended, PVEGPU_CMD_QUEUE_SIZE, GFP_KERNEL);
    if (ret) {
        PVEGPU_ERR("kfifo_alloc failed for vmid %d\n", vmid);
        goto err_release_slot;
    }

    for (i = 0; i < PVEGPU_DOMAIN_CHANNELS; i++) {
        ctx->channels[i].virt_id = i;
        ctx->channels[i].phys_id = pvegpu_virt_to_phys_channel(ctx, i);
        ctx->channels[i].enabled = false;
        ctx->channels[i].put = 0;
        ctx->channels[i].get = 0;
        ctx->channels[i].ib_put = 0;
        ctx->channels[i].ib_get = 0;
    }

    ctx->budget           = ktime_set(0, 0);
    ctx->bandwidth        = ktime_set(0, 0);
    ctx->bandwidth_used   = ktime_set(0, 0);
    ctx->sampling_bw_used = ktime_set(0, 0);
    ctx->bar3_window_base = 0;
    ctx->compute.saved    = false;

    pvegpu_cfg_init(ctx);

    ret = pvegpu_shadow_pd_init(ctx);
    if (ret) {
        PVEGPU_ERR("shadow_pd_init failed for vmid %d\n", vmid);
        goto err_kfifo;
    }

    ret = pvegpu_irq_init(ctx);
    if (ret) {
        PVEGPU_ERR("irq_init failed for vmid %d\n", vmid);
        goto err_shadow;
    }

    gdev->contexts[slot] = ctx;
    ctx->initialized = true;

    pvegpu_sched_register_ctx(&gdev->scheduler, ctx);

    PVEGPU_INFO("context created: vmid=%d slot=%u weight=%u "
                "vram_offset=%llu MB\n",
                vmid, slot, ctx->weight,
                (unsigned long long)(pvegpu_ctx_addr_shift(ctx) >> 20));

    return 0;

err_shadow:
    pvegpu_shadow_pd_fini(ctx);
err_kfifo:
    kfifo_free(&ctx->suspended);
err_release_slot:
    pvegpu_release_slot(gdev, slot);
    return ret;
}

/*
 * pvegpu_ctx_create — alloue et initialise un contexte (kernel < 6.8)
 */
int pvegpu_ctx_create(struct pvegpu_device *gdev, int vmid,
                       uint32_t weight,
                       struct pvegpu_vm_ctx **ctx_out)
{
    struct pvegpu_vm_ctx *ctx;
    int ret;

    ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);
    if (!ctx)
        return -ENOMEM;

    ret = pvegpu_ctx_init(ctx, gdev, vmid, weight);
    if (ret) {
        kfree(ctx);
        return ret;
    }

    *ctx_out = ctx;
    return 0;
}

/*
 * pvegpu_ctx_destroy — libere les ressources d'un contexte.
 *
 * Ne fait PAS kfree(ctx). L'appelant gere la memoire :
 *  - kernel < 6.8 : kfree() apres ctx_destroy
 *  - kernel 6.8+  : vfio_put_device() (kvfree via release callback)
 */
void pvegpu_ctx_destroy(struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_device *gdev;

    if (!ctx)
        return;

    gdev = ctx->dev;

    PVEGPU_INFO("destroying context: vmid=%d slot=%u\n",
                ctx->vmid, ctx->id);

    pvegpu_sched_unregister_ctx(&gdev->scheduler, ctx);
    pvegpu_irq_fini(ctx);
    pvegpu_shadow_pd_fini(ctx);

    if (ctx->initialized)
        gdev->contexts[ctx->id] = NULL;

    pvegpu_release_slot(gdev, ctx->id);
    kfifo_free(&ctx->suspended);
}

/* ============================================================
 * GENERIC COMPUTE STATE / GPU WAIT / CONFIG SPACE
 * ============================================================ */

void pvegpu_compute_state_save(struct pvegpu_vm_ctx *ctx)
{
    if (ctx->dev->gpu_ops && ctx->dev->gpu_ops->compute_save)
        ctx->dev->gpu_ops->compute_save(ctx);
}

void pvegpu_compute_state_restore(struct pvegpu_vm_ctx *ctx)
{
    if (ctx->dev->gpu_ops && ctx->dev->gpu_ops->compute_restore)
        ctx->dev->gpu_ops->compute_restore(ctx);
}

int pvegpu_gpu_wait_idle(struct pvegpu_device *gdev, int timeout_us)
{
    if (gdev->gpu_ops && gdev->gpu_ops->wait_idle)
        return gdev->gpu_ops->wait_idle(gdev, timeout_us);

    /* Fallback generique : poll PGRAPH_STATUS (NVIDIA default) */
    {
        int i;
        uint32_t status_reg = (gdev->vendor_id == PVEGPU_VENDOR_NVIDIA)
                              ? 0x400700 : 0x8010;  /* GRBM_STATUS */

        for (i = 0; i < timeout_us; i++) {
            if (pvegpu_bar_read32(gdev, 0, status_reg) == 0)
                return 0;
            udelay(1);
        }
    }
    return -ETIMEDOUT;
}

/*
 * pvegpu_cfg_init — initialise le config space emule pour une VM
 *
 * Copie le config space reel du GPU, puis ajuste les valeurs
 * specifiques a la VM (BAR sizes, subsystem ID, etc.).
 */
void pvegpu_cfg_init(struct pvegpu_vm_ctx *ctx)
{
    struct pci_dev *pdev = ctx->dev->pdev;
    int i;

    /* Lire le config space reel en mots de 32 bits */
    for (i = 0; i < PVEGPU_CFG_SPACE_SIZE; i += 4) {
        uint32_t val;
        pci_read_config_dword(pdev, i, &val);
        memcpy(&ctx->cfg.regs[i], &val, 4);
    }

    /* Ajuster les BARs pour refleter la VRAM de cette VM */
    /* BAR0 et BAR1 : garder les tailles physiques */
    /* BAR3 (VRAM) : ajuster la taille a vram_per_vm */
    {
        uint32_t bar3_size_mask = ~((uint32_t)ctx->vram_size - 1);
        memcpy(&ctx->cfg.regs[0x1C], &bar3_size_mask, 4);
    }

    /* Masquer les capabilities MSI-X si presentes (simplification) */
    /* Laisser MSI fonctionnel pour l'IRQ forwarding */

    ctx->cfg.initialized = true;

    PVEGPU_LOG("cfg_init: vmid=%d config space emulated\n", ctx->vmid);
}

/*
 * pvegpu_cfg_read — lecture emule du config space PCI
 */
int pvegpu_cfg_read(struct pvegpu_vm_ctx *ctx, int offset,
                     int size, uint32_t *val)
{
    if (offset + size > PVEGPU_CFG_SPACE_SIZE)
        return -EINVAL;

    *val = 0;
    memcpy(val, &ctx->cfg.regs[offset], size);
    return 0;
}

/*
 * pvegpu_cfg_write — ecriture emule du config space PCI
 *
 * Filtre les ecritures dangereuses. Seuls certains registres
 * sont modifiables par la VM (command, status, latency timer,
 * interrupt line).
 */
int pvegpu_cfg_write(struct pvegpu_vm_ctx *ctx, int offset,
                      int size, uint32_t val)
{
    if (offset + size > PVEGPU_CFG_SPACE_SIZE)
        return -EINVAL;

    switch (offset) {
    case PCI_COMMAND:
        /* Autoriser seulement I/O, Memory, Bus Master */
        val &= (PCI_COMMAND_IO | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER);
        memcpy(&ctx->cfg.regs[offset], &val, size);
        break;

    case PCI_INTERRUPT_LINE:
        /* La VM peut ecrire sa ligne d'interruption */
        memcpy(&ctx->cfg.regs[offset], &val, size);
        break;

    case PCI_CACHE_LINE_SIZE:
    case PCI_LATENCY_TIMER:
        memcpy(&ctx->cfg.regs[offset], &val, size);
        break;

    case PCI_BASE_ADDRESS_0:
    case PCI_BASE_ADDRESS_1:
    case PCI_BASE_ADDRESS_2:
    case PCI_BASE_ADDRESS_3:
    case PCI_BASE_ADDRESS_4:
    case PCI_BASE_ADDRESS_5:
        /* BAR probing : la VM ecrit 0xFFFFFFFF pour detecter la taille.
         * On repond avec le masque qui indique la taille.
         * On ne modifie pas les BARs reels.
         */
        if (val == 0xFFFFFFFF) {
            int bar_idx = (offset - PCI_BASE_ADDRESS_0) / 4;
            resource_size_t bar_size;

            if (bar_idx == 3) {
                /* BAR3 = VRAM de cette VM */
                bar_size = ctx->vram_size;
            } else {
                bar_size = ctx->dev->bar_size[bar_idx];
            }

            if (bar_size > 0) {
                uint32_t mask = ~((uint32_t)bar_size - 1);
                /* Conserver les bits de type (I/O, mem, 64bit, prefetch) */
                mask |= (ctx->cfg.regs[offset] & 0x0F);
                memcpy(&ctx->cfg.regs[offset], &mask, 4);
            }
        } else {
            /* Ecriture d'adresse : stocker dans le shadow config */
            memcpy(&ctx->cfg.regs[offset], &val, size);
        }
        break;

    default:
        /* Bloquer les ecritures aux registres critiques */
        PVEGPU_LOG("cfg_write: blocked vmid=%d offset=0x%x val=0x%x\n",
                   ctx->vmid, offset, val);
        break;
    }

    return 0;
}

/* Module init/exit : voir pve_gpu_sched_mdev.c */
