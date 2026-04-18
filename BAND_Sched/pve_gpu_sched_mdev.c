// SPDX-License-Identifier: GPL-2.0-only
/*
 * pve_gpu_sched_mdev.c — Interface VFIO Mediated Device
 *
 * Ce fichier est le pont entre le kernel KVM/VFIO et le scheduler GPU.
 * Il implemente le driver mdev qui :
 *
 *  1. S'enregistre comme driver PCI sur le GPU physique
 *  2. Expose N devices virtuels dans sysfs via mdev_register_parent()
 *  3. Quand Proxmox cree un mdev pour une VM, cree un pvegpu_vm_ctx
 *  4. Implemente vfio_device_ops avec :
 *     - Interception MMIO read/write
 *     - IRQ forwarding via eventfd
 *     - FLR (Function Level Reset) virtuel
 *     - Initialisation des canaux PFIFO shadow
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/pci.h>
#include <linux/pci_ids.h>
#include <linux/vfio.h>
#include <linux/mdev.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/eventfd.h>
#include <linux/iommufd.h>
#include <linux/interrupt.h>
#include <uapi/linux/vfio.h>

#include "pve_gpu_sched.h"

MODULE_LICENSE("GPL v2");
MODULE_AUTHOR("PVE GPU Scheduler Project");
MODULE_DESCRIPTION("Native GPU time-sharing scheduler for Proxmox VE / KVM");
MODULE_VERSION("0.2.0");

/* ============================================================
 * CONSTANTES MDEV
 * ============================================================ */

#define PVEGPU_MDEV_TYPE_NAME    "pvegpu-sched"
#define PVEGPU_MDEV_DESCRIPTION  "PVE GPU Scheduler - shared GPU access"

/* Table des GPUs NVIDIA et AMD supportes */
static const struct pci_device_id pvegpu_pci_table[] = {
    /* NVIDIA Fermi */
    { PCI_DEVICE(PVEGPU_VENDOR_NVIDIA, PCI_ANY_ID), .class = 0x030200,
      .class_mask = 0xffff00 },
    /* NVIDIA Kepler / Maxwell / Pascal / Turing / Ampere / Ada */
    { PCI_DEVICE(PVEGPU_VENDOR_NVIDIA, PCI_ANY_ID), .class = 0x030000,
      .class_mask = 0xffff00 },
    /* AMD GCN / RDNA */
    { PCI_DEVICE(PVEGPU_VENDOR_AMD, PCI_ANY_ID), .class = 0x030000,
      .class_mask = 0xffff00 },
    { 0 }
};
MODULE_DEVICE_TABLE(pci, pvegpu_pci_table);

/* ============================================================
 * IRQ FORWARDING
 * Forwarding des interruptions GPU vers les VMs via eventfd.
 *
 * Quand QEMU configure les IRQs avec VFIO_DEVICE_SET_IRQS,
 * il fournit un eventfd que nous signalons a chaque interruption GPU.
 *
 * Reference : linux/drivers/vfio/pci/vfio_pci_intrs.c
 * ============================================================ */

/*
 * pvegpu_irq_init — initialise l'etat IRQ d'un contexte
 */
int pvegpu_irq_init(struct pvegpu_vm_ctx *ctx)
{
    spin_lock_init(&ctx->irq.lock);
    ctx->irq.trigger  = NULL;
    ctx->irq.unmask   = NULL;
    ctx->irq.irq_type = -1;
    ctx->irq.enabled  = false;
    return 0;
}

/*
 * pvegpu_irq_fini — libere les ressources IRQ
 */
void pvegpu_irq_fini(struct pvegpu_vm_ctx *ctx)
{
    unsigned long flags;

    spin_lock_irqsave(&ctx->irq.lock, flags);

    if (ctx->irq.trigger) {
        eventfd_ctx_put(ctx->irq.trigger);
        ctx->irq.trigger = NULL;
    }
    if (ctx->irq.unmask) {
        eventfd_ctx_put(ctx->irq.unmask);
        ctx->irq.unmask = NULL;
    }
    ctx->irq.enabled = false;

    spin_unlock_irqrestore(&ctx->irq.lock, flags);
}

/*
 * pvegpu_irq_set — configure les IRQs via VFIO_DEVICE_SET_IRQS
 *
 * Supporte les operations :
 *  - SET_DATA_EVENTFD : enregistre un eventfd pour le trigger
 *  - SET_DATA_NONE + ACTION_TRIGGER : declenche une IRQ manuellement
 *  - UNMASK : demasque l'interruption
 */
int pvegpu_irq_set(struct pvegpu_vm_ctx *ctx,
                    uint32_t flags_arg, uint32_t index,
                    uint32_t start, uint32_t count,
                    void *data)
{
    unsigned long flags;
    uint32_t action = flags_arg & VFIO_IRQ_SET_ACTION_TYPE_MASK;
    uint32_t data_type = flags_arg & VFIO_IRQ_SET_DATA_TYPE_MASK;

    if (index >= VFIO_PCI_NUM_IRQS)
        return -EINVAL;

    spin_lock_irqsave(&ctx->irq.lock, flags);

    switch (action) {
    case VFIO_IRQ_SET_ACTION_MASK:
        ctx->irq.enabled = false;
        PVEGPU_LOG("irq_set: masked vmid=%d index=%u\n",
                   ctx->vmid, index);
        break;

    case VFIO_IRQ_SET_ACTION_UNMASK:
        ctx->irq.enabled = true;
        PVEGPU_LOG("irq_set: unmasked vmid=%d index=%u\n",
                   ctx->vmid, index);
        break;

    case VFIO_IRQ_SET_ACTION_TRIGGER:
        if (data_type == VFIO_IRQ_SET_DATA_EVENTFD) {
            int32_t fd = *(int32_t *)data;

            /* Liberer l'ancien eventfd si present */
            if (ctx->irq.trigger) {
                eventfd_ctx_put(ctx->irq.trigger);
                ctx->irq.trigger = NULL;
            }

            if (fd >= 0) {
                ctx->irq.trigger = eventfd_ctx_fdget(fd);
                if (IS_ERR(ctx->irq.trigger)) {
                    int ret = PTR_ERR(ctx->irq.trigger);
                    ctx->irq.trigger = NULL;
                    spin_unlock_irqrestore(&ctx->irq.lock, flags);
                    PVEGPU_ERR("irq_set: eventfd_ctx_fdget failed: %d\n",
                               ret);
                    return ret;
                }
                ctx->irq.irq_type = index;
                ctx->irq.enabled = true;
                PVEGPU_INFO("irq_set: eventfd registered vmid=%d "
                            "index=%u fd=%d\n",
                            ctx->vmid, index, fd);
            } else {
                /* fd < 0 : desactiver les IRQs */
                ctx->irq.enabled = false;
            }
        } else if (data_type == VFIO_IRQ_SET_DATA_NONE) {
            /* Trigger manuel */
            if (ctx->irq.trigger && ctx->irq.enabled) {
                pvegpu_eventfd_signal(ctx->irq.trigger);
                PVEGPU_LOG("irq_set: manual trigger vmid=%d\n",
                           ctx->vmid);
            }
        }
        break;

    default:
        spin_unlock_irqrestore(&ctx->irq.lock, flags);
        return -EINVAL;
    }

    spin_unlock_irqrestore(&ctx->irq.lock, flags);
    return 0;
}

/*
 * pvegpu_irq_trigger — signale une interruption a la VM
 *
 * Appele depuis le handler d'interruption GPU physique
 * pour forwarder l'IRQ a la bonne VM.
 */
void pvegpu_irq_trigger(struct pvegpu_vm_ctx *ctx)
{
    unsigned long flags;

    spin_lock_irqsave(&ctx->irq.lock, flags);

    if (ctx->irq.trigger && ctx->irq.enabled) {
        eventfd_signal(ctx->irq.trigger);
        PVEGPU_LOG("irq_trigger: signaled vmid=%d\n", ctx->vmid);
    }

    spin_unlock_irqrestore(&ctx->irq.lock, flags);
}

/*
 * pvegpu_gpu_irq_handler — handler d'interruption GPU physique
 *
 * Determine quelle VM a genere l'interruption et forwarde
 * via eventfd. Sur NVIDIA, on lit PFIFO_INTR_0 et PGRAPH_INTR
 * pour identifier la source.
 */
/*
 * pvegpu_gpu_irq_handler — handler d'interruption GPU ameliore
 *
 * Forwarde l'interruption a la VM courante ET notifie les VMs
 * en attente si elles ont des commandes en cours. Cela permet
 * un meilleur support multi-VM pour les workloads compute.
 *
 * Sur NVIDIA : analyse PFIFO_INTR et PGRAPH_INTR pour identifier
 * la source. Sur AMD : analyse GRBM interrupt status.
 */
static irqreturn_t pvegpu_gpu_irq_handler(int irq, void *data)
{
    struct pvegpu_device *gdev = data;
    struct pvegpu_vm_ctx *ctx;
    uint32_t intr_status;
    uint32_t pfifo_intr = 0;
    unsigned long flags;
    int i;

    /* Lire le registre d'interruption global */
    if (gdev->vendor_id == PVEGPU_VENDOR_NVIDIA) {
        intr_status = pvegpu_bar_read32(gdev, 0, 0x000100);
        /* Lire aussi PFIFO_INTR pour determiner le canal source */
        if (intr_status & 0x00000100)  /* bit 8 = PFIFO */
            pfifo_intr = pvegpu_bar_read32(gdev, 0, 0x002100);
    } else {
        intr_status = pvegpu_bar_read32(gdev, 0, 0x44);
    }

    if (intr_status == 0)
        return IRQ_NONE;

    /* Forwarder a la VM courante (prioritaire) */
    spin_lock_irqsave(&gdev->scheduler.sched_lock, flags);
    ctx = gdev->scheduler.current_ctx;
    spin_unlock_irqrestore(&gdev->scheduler.sched_lock, flags);

    if (ctx)
        pvegpu_irq_trigger(ctx);

    /* Pour les interruptions PFIFO, notifier aussi les VMs
     * qui ont des canaux actifs correspondants. Cela permet
     * aux VMs en attente de completion de recevoir leur signal.
     */
    if (pfifo_intr) {
        for (i = 0; i < PVEGPU_MAX_DOMAINS; i++) {
            struct pvegpu_vm_ctx *vm = gdev->contexts[i];
            if (!vm || vm == ctx || !vm->initialized)
                continue;
            if (vm->irq.enabled && vm->irq.trigger &&
                pvegpu_ctx_is_suspended(vm)) {
                pvegpu_irq_trigger(vm);
            }
        }
    }

    /* Acquitter l'interruption */
    if (gdev->vendor_id == PVEGPU_VENDOR_NVIDIA) {
        pvegpu_bar_write32(gdev, 0, 0x000100, intr_status);
        if (pfifo_intr)
            pvegpu_bar_write32(gdev, 0, 0x002100, pfifo_intr);
    } else {
        pvegpu_bar_write32(gdev, 0, 0x44, intr_status);
    }

    return IRQ_HANDLED;
}

/* ============================================================
 * VFIO DEVICE OPS
 * ============================================================ */

/*
 * pvegpu_mdev_open_device — une VM ouvre son GPU virtuel
 *
 * Initialise les canaux PFIFO shadow pour cette VM.
 */
static int pvegpu_mdev_open_device(struct vfio_device *vdev)
{
    struct pvegpu_vm_ctx *ctx = container_of(vdev, struct pvegpu_vm_ctx,
                                              vdev);
    struct pvegpu_device *gdev = ctx->dev;
    uint32_t i, range;

    PVEGPU_INFO("open_device: vmid=%d slot=%u\n", ctx->vmid, ctx->id);

    /* Initialiser les canaux PFIFO shadow.
     * Pour chaque canal virtuel, on configure le canal physique
     * correspondant avec l'adresse RAMIN translatee.
     */
    range = gdev->gpu_ops ? gdev->gpu_ops->get_channel_range(gdev) : 0x1000;

    for (i = 0; i < PVEGPU_DOMAIN_CHANNELS; i++) {
        struct pvegpu_channel *ch = &ctx->channels[i];
        uint32_t phys_offset = ch->phys_id * range;

        /* Calculer l'adresse RAMIN physique pour ce canal */
        if (gdev->gpu_ops && gdev->gpu_ops->ramin_translate) {
            ch->ramin_addr = gdev->gpu_ops->ramin_translate(
                ctx, i * PVEGPU_RAMIN_PER_CHANNEL);
        } else {
            ch->ramin_addr = (ctx->id * PVEGPU_DOMAIN_CHANNELS + i)
                             * PVEGPU_RAMIN_PER_CHANNEL;
        }

        /* Configurer le pointeur page directory dans RAMIN */
        ch->page_dir_addr = ctx->shadow_pd.phys_addr;

        PVEGPU_LOG("open: init channel %u -> phys %u ramin=0x%llx "
                   "range=0x%x phys_offset=0x%x\n",
                   i, ch->phys_id, ch->ramin_addr, range, phys_offset);
    }

    return 0;
}

static void pvegpu_mdev_close_device(struct vfio_device *vdev)
{
    struct pvegpu_vm_ctx *ctx = container_of(vdev, struct pvegpu_vm_ctx,
                                              vdev);

    PVEGPU_INFO("close_device: vmid=%d slot=%u\n", ctx->vmid, ctx->id);

    kfifo_reset(&ctx->suspended);
}

static ssize_t pvegpu_mdev_read(struct vfio_device *vdev,
                                 char __user *buf,
                                 size_t count,
                                 loff_t *ppos)
{
    struct pvegpu_vm_ctx *ctx = container_of(vdev, struct pvegpu_vm_ctx,
                                              vdev);
    struct pvegpu_cmd cmd = {};
    unsigned int bar;
    loff_t pos = *ppos & VFIO_PCI_OFFSET_MASK;

    bar = VFIO_PCI_OFFSET_TO_INDEX(*ppos);

    if (bar != 0 && bar != 1 && bar != 3) {
        /* Config space : utiliser l'emulation per-VM */
        if (bar == VFIO_PCI_CONFIG_REGION_INDEX) {
            uint32_t val = 0;
            if (pos + count > PVEGPU_CFG_SPACE_SIZE)
                return -EINVAL;
            pvegpu_cfg_read(ctx, (int)pos, (int)count, &val);
            if (copy_to_user(buf, &val, count))
                return -EFAULT;
            return count;
        }
        PVEGPU_ERR("read: unsupported BAR %u\n", bar);
        return -EINVAL;
    }

    if (count != 1 && count != 2 && count != 4) {
        PVEGPU_ERR("read: unsupported size %zu\n", count);
        return -EINVAL;
    }

    cmd.type   = PVEGPU_CMD_TYPE_READ;
    cmd.bar    = (uint8_t)bar;
    cmd.offset = (uint32_t)pos;
    cmd.size   = (uint8_t)count;
    cmd.value  = 0;

    switch (bar) {
    case PVEGPU_BAR0:
        pvegpu_read_bar0(ctx, &cmd);
        break;
    case PVEGPU_BAR1:
        pvegpu_read_bar1(ctx, &cmd);
        break;
    case PVEGPU_BAR3:
        pvegpu_read_bar3(ctx, &cmd);
        break;
    }

    if (copy_to_user(buf, &cmd.value, count))
        return -EFAULT;

    return count;
}

static ssize_t pvegpu_mdev_write(struct vfio_device *vdev,
                                  const char __user *buf,
                                  size_t count,
                                  loff_t *ppos)
{
    struct pvegpu_vm_ctx *ctx = container_of(vdev, struct pvegpu_vm_ctx,
                                              vdev);
    struct pvegpu_cmd cmd = {};
    unsigned int bar;
    loff_t pos = *ppos & VFIO_PCI_OFFSET_MASK;
    uint32_t val = 0;

    bar = VFIO_PCI_OFFSET_TO_INDEX(*ppos);

    if (bar != 0 && bar != 1 && bar != 3) {
        /* Config space : utiliser l'emulation per-VM */
        if (bar == VFIO_PCI_CONFIG_REGION_INDEX) {
            if (pos + count > PVEGPU_CFG_SPACE_SIZE)
                return -EINVAL;
            if (copy_from_user(&val, buf, count))
                return -EFAULT;
            pvegpu_cfg_write(ctx, (int)pos, (int)count, val);
            return count;
        }
        PVEGPU_ERR("write: unsupported BAR %u\n", bar);
        return -EINVAL;
    }

    if (count != 1 && count != 2 && count != 4) {
        PVEGPU_ERR("write: unsupported size %zu\n", count);
        return -EINVAL;
    }

    if (copy_from_user(&val, buf, count))
        return -EFAULT;

    cmd.type   = PVEGPU_CMD_TYPE_WRITE;
    cmd.bar    = (uint8_t)bar;
    cmd.offset = (uint32_t)pos;
    cmd.size   = (uint8_t)count;
    cmd.value  = val;

    pvegpu_enqueue(ctx, &cmd);

    return count;
}

static long pvegpu_mdev_ioctl(struct vfio_device *vdev,
                               unsigned int cmd,
                               unsigned long arg)
{
    struct pvegpu_vm_ctx *ctx = container_of(vdev, struct pvegpu_vm_ctx,
                                              vdev);
    struct pvegpu_device *gdev = ctx->dev;

    switch (cmd) {

    case VFIO_DEVICE_GET_INFO: {
        struct vfio_device_info info = {};

        info.argsz = sizeof(info);
        info.flags = VFIO_DEVICE_FLAGS_PCI | VFIO_DEVICE_FLAGS_RESET;
        info.num_regions = VFIO_PCI_NUM_REGIONS;
        info.num_irqs    = VFIO_PCI_NUM_IRQS;

        if (copy_to_user((void __user *)arg, &info, sizeof(info)))
            return -EFAULT;
        return 0;
    }

    case VFIO_DEVICE_GET_REGION_INFO: {
        struct vfio_region_info info = {};
        struct vfio_region_info __user *uinfo =
            (struct vfio_region_info __user *)arg;

        if (copy_from_user(&info, uinfo, sizeof(info)))
            return -EFAULT;

        switch (info.index) {
        case VFIO_PCI_BAR0_REGION_INDEX:
            info.size  = gdev->bar_size[0] ? gdev->bar_size[0] :
                         (16ULL << 20);
            info.flags = VFIO_REGION_INFO_FLAG_READ |
                         VFIO_REGION_INFO_FLAG_WRITE;
            info.offset = VFIO_PCI_INDEX_TO_OFFSET(VFIO_PCI_BAR0_REGION_INDEX);
            break;

        case VFIO_PCI_BAR1_REGION_INDEX:
            info.size  = gdev->bar_size[1] ? gdev->bar_size[1] :
                         (32ULL << 20);
            info.flags = VFIO_REGION_INFO_FLAG_READ |
                         VFIO_REGION_INFO_FLAG_WRITE;
            info.offset = VFIO_PCI_INDEX_TO_OFFSET(VFIO_PCI_BAR1_REGION_INDEX);
            break;

        case VFIO_PCI_BAR3_REGION_INDEX:
            /* BAR3 = fenetre VRAM. Taille = taille physique reelle de la BAR,
             * PAS ctx->vram_size. On ne peut pas mapper plus que ce que le
             * GPU expose physiquement. Le mmap direct est desactive : mapper
             * bar3_phys + id*vram_size sortirait de la BAR physique et
             * corromprait la memoire host (IOMMU fault ou pire).
             * Les acces VRAM passent par read/write emules.
             */
            info.size  = gdev->bar_size[3] ? gdev->bar_size[3] :
                         (256ULL << 20);
            info.flags = VFIO_REGION_INFO_FLAG_READ |
                         VFIO_REGION_INFO_FLAG_WRITE;
            info.offset = VFIO_PCI_INDEX_TO_OFFSET(VFIO_PCI_BAR3_REGION_INDEX);
            break;

        case VFIO_PCI_CONFIG_REGION_INDEX:
            info.size  = PVEGPU_CFG_SPACE_SIZE;
            info.flags = VFIO_REGION_INFO_FLAG_READ |
                         VFIO_REGION_INFO_FLAG_WRITE;
            info.offset = VFIO_PCI_INDEX_TO_OFFSET(VFIO_PCI_CONFIG_REGION_INDEX);
            break;

        default:
            info.size  = 0;
            info.flags = 0;
            break;
        }

        info.argsz = sizeof(info);
        if (copy_to_user(uinfo, &info, sizeof(info)))
            return -EFAULT;
        return 0;
    }

    case VFIO_DEVICE_GET_IRQ_INFO: {
        struct vfio_irq_info irq_info = {};
        struct vfio_irq_info __user *uinfo =
            (struct vfio_irq_info __user *)arg;

        if (copy_from_user(&irq_info, uinfo, sizeof(irq_info)))
            return -EFAULT;

        if (irq_info.index >= VFIO_PCI_NUM_IRQS)
            return -EINVAL;

        irq_info.argsz = sizeof(irq_info);
        irq_info.count = 1;
        irq_info.flags = VFIO_IRQ_INFO_EVENTFD |
                         VFIO_IRQ_INFO_MASKABLE;

        if (copy_to_user(uinfo, &irq_info, sizeof(irq_info)))
            return -EFAULT;
        return 0;
    }

    case VFIO_DEVICE_RESET:
        /* FLR (Function Level Reset) virtuel */
        PVEGPU_INFO("device reset (FLR): vmid=%d\n", ctx->vmid);

        if (gdev->gpu_ops && gdev->gpu_ops->context_reset)
            return gdev->gpu_ops->context_reset(ctx);

        /* Fallback generique */
        kfifo_reset(&ctx->suspended);
        return 0;

    case VFIO_DEVICE_SET_IRQS: {
        struct vfio_irq_set hdr;
        void *irq_data = NULL;
        int ret;
        size_t data_size;

        if (copy_from_user(&hdr, (void __user *)arg, sizeof(hdr)))
            return -EFAULT;

        /* Calculer la taille des donnees supplementaires */
        data_size = 0;
        if ((hdr.flags & VFIO_IRQ_SET_DATA_TYPE_MASK) ==
            VFIO_IRQ_SET_DATA_EVENTFD) {
            data_size = hdr.count * sizeof(int32_t);
        } else if ((hdr.flags & VFIO_IRQ_SET_DATA_TYPE_MASK) ==
                   VFIO_IRQ_SET_DATA_BOOL) {
            data_size = hdr.count * sizeof(uint8_t);
        }

        if (data_size > 0) {
            if (hdr.argsz < sizeof(hdr) + data_size)
                return -EINVAL;

            irq_data = kmalloc(data_size, GFP_KERNEL);
            if (!irq_data)
                return -ENOMEM;

            if (copy_from_user(irq_data,
                               (void __user *)arg + sizeof(hdr),
                               data_size)) {
                kfree(irq_data);
                return -EFAULT;
            }
        }

        ret = pvegpu_irq_set(ctx, hdr.flags, hdr.index,
                              hdr.start, hdr.count, irq_data);

        kfree(irq_data);
        return ret;
    }

    default:
        PVEGPU_ERR("unknown ioctl 0x%x for vmid=%d\n", cmd, ctx->vmid);
        return -ENOTTY;
    }
}

static int pvegpu_mdev_mmap(struct vfio_device *vdev,
                             struct vm_area_struct *vma)
{
    struct pvegpu_vm_ctx *ctx = container_of(vdev, struct pvegpu_vm_ctx,
                                              vdev);
    /* Le mmap direct de BAR3 est desactive.
     *
     * Raison : mapper bar3_phys + (ctx->id * vram_size) est invalide en KVM.
     * La BAR3 est une fenetre physique de taille fixe ; l'arithmetique
     * d'offset sort de cette fenetre et pointe dans la memoire host.
     * Resultat : IOMMU fault (kernel panic) ou corruption memoire silencieuse.
     *
     * Correction correcte : allouer des pages noyau par VM et les exposer
     * via remap_pfn_range. A implémenter quand la baseline est stable.
     *
     * Pour l'instant : les acces VRAM passent par read/write emules.
     * VFIO_REGION_INFO_FLAG_MMAP est retire de GET_REGION_INFO donc
     * QEMU ne doit pas appeler ce chemin, mais on le refuse explicitement.
     */
    PVEGPU_ERR("mmap: direct BAR3 mmap disabled (emulated VRAM only) "
               "vmid=%d\n", ctx->vmid);
    return -ENODEV;
}

/*
 * Kernel 6.8+ : vfio_alloc_device() requiert un callback .release
 * dans vfio_device_ops. Ce callback est appele par le VFIO core
 * quand la derniere reference au device est relachee (vfio_put_device).
 * La memoire est liberee par kvfree() dans le VFIO core APRES .release.
 */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
static void pvegpu_vfio_dev_release(struct vfio_device *vdev)
{
    /* pvegpu_ctx_destroy() est deja appele dans pvegpu_mdev_remove().
     * Le VFIO core fait kvfree() apres ce callback.
     */
}
#endif

static const struct vfio_device_ops pvegpu_vfio_ops = {
    .name         = "pvegpu-sched",
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
    .release      = pvegpu_vfio_dev_release,
#endif
    /*
     * Kernel 6.2+ : callbacks iommufd pour les devices mdev.
     * vfio_iommufd_emulated_* sont des helpers kernel pour les devices
     * a IOMMU emule (exactement notre cas : mdev sans vrai IOMMU).
     * Sans ces callbacks, vfio_register_emulated_iommu_dev() peut
     * retourner -EINVAL sur les kernels avec iommufd actif.
     * C'est le meme pattern que le driver Intel GVT (i915/gvt/kvmgt.c).
     */
    /*
     * Le champ a ete renomme dans le cycle 6.6+ :
     *   6.2-6.5 : bind_iommufd_device / unbind_iommufd_device
     *   6.6+    : bind_iommufd / unbind_iommufd
     */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 6, 0)
    .bind_iommufd          = vfio_iommufd_emulated_bind,
    .unbind_iommufd        = vfio_iommufd_emulated_unbind,
    .attach_ioas           = vfio_iommufd_emulated_attach_ioas,
    .detach_ioas           = vfio_iommufd_emulated_detach_ioas,
#elif LINUX_VERSION_CODE >= KERNEL_VERSION(6, 2, 0)
    .bind_iommufd_device   = vfio_iommufd_emulated_bind,
    .unbind_iommufd_device = vfio_iommufd_emulated_unbind,
    .attach_ioas           = vfio_iommufd_emulated_attach_ioas,
    .detach_ioas           = vfio_iommufd_emulated_detach_ioas,
#endif
    .open_device  = pvegpu_mdev_open_device,
    .close_device = pvegpu_mdev_close_device,
    .read         = pvegpu_mdev_read,
    .write        = pvegpu_mdev_write,
    .ioctl        = pvegpu_mdev_ioctl,
    .mmap         = pvegpu_mdev_mmap,
};

/* ============================================================
 * MDEV DRIVER OPS
 *
 * Lifecycle VFIO par version kernel :
 *
 *  Kernel < 6.8 :
 *    kzalloc(ctx) → vfio_init_group_dev() → vfio_register_*_dev()
 *    vfio_unregister_group_dev() → vfio_uninit_group_dev() → kfree()
 *
 *  Kernel 6.8+ :
 *    vfio_alloc_device() → pvegpu_ctx_init() → vfio_register_*_dev()
 *    vfio_unregister_group_dev() → vfio_put_device() → kvfree()
 *
 * vfio_alloc_device() appelle _vfio_alloc_device() qui fait :
 *   kvzalloc + vfio_init_device (init_completion, device_initialize, etc.)
 * C'est obligatoire sur 6.8+ car vfio_init_group_dev() n'existe plus.
 * ============================================================ */

static int pvegpu_mdev_probe(struct mdev_device *mdev)
{
    struct pvegpu_device *gdev = pvegpu_device_get();
    struct pvegpu_vm_ctx *ctx;
    const char *uuid_str;
    int vmid = 0;
    uint32_t weight = PVEGPU_DEFAULT_WEIGHT;
    int ret;

    if (!gdev) {
        PVEGPU_ERR("probe: no GPU device initialized\n");
        return -ENODEV;
    }

    /* Extraire le vmid depuis l'UUID mdev.
     * Format Proxmox : "%08d-0000-0000-0000-%012d" (index, vmid)
     */
    uuid_str = dev_name(mdev_dev(mdev));
    if (uuid_str && strlen(uuid_str) >= 36) {
        ret = kstrtoint(uuid_str + 24, 10, &vmid);
        if (ret) {
            PVEGPU_ERR("probe: cannot parse vmid from UUID '%s'\n",
                       uuid_str);
            vmid = -1;
        }
    }

    PVEGPU_INFO("probe: creating GPU context for vmid=%d uuid=%s "
                "weight=%u\n",
                vmid, uuid_str ? uuid_str : "unknown", weight);

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
    /* Kernel 6.8+ : vfio_alloc_device() alloue le pvegpu_vm_ctx
     * et initialise le vfio_device interne (device_initialize,
     * init_completion, etc.). Sans ca, vfio_register_emulated_iommu_dev
     * retourne -EINVAL.
     */
    ctx = vfio_alloc_device(pvegpu_vm_ctx, vdev,
                            mdev_dev(mdev), &pvegpu_vfio_ops);
    if (IS_ERR(ctx)) {
        PVEGPU_ERR("probe: vfio_alloc_device failed: %ld\n",
                   PTR_ERR(ctx));
        return PTR_ERR(ctx);
    }

    ret = pvegpu_ctx_init(ctx, gdev, vmid, weight);
    if (ret) {
        PVEGPU_ERR("probe: pvegpu_ctx_init failed: %d\n", ret);
        vfio_put_device(&ctx->vdev);
        return ret;
    }
#else
    /* Kernel < 6.8 : kzalloc + vfio_init_group_dev classique */
    ret = pvegpu_ctx_create(gdev, vmid, weight, &ctx);
    if (ret) {
        PVEGPU_ERR("probe: pvegpu_ctx_create failed: %d\n", ret);
        return ret;
    }

    vfio_init_group_dev(&ctx->vdev, mdev_dev(mdev), &pvegpu_vfio_ops);
#endif

    ctx->mdev = mdev;

    ret = pvegpu_vfio_register_dev(&ctx->vdev);
    if (ret) {
        PVEGPU_ERR("probe: vfio register dev failed: %d\n", ret);
        pvegpu_ctx_destroy(ctx);
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
        vfio_put_device(&ctx->vdev);
#else
        kfree(ctx);
#endif
        return ret;
    }

    dev_set_drvdata(mdev_dev(mdev), ctx);

    PVEGPU_INFO("probe: GPU context ready vmid=%d slot=%u weight=%u\n",
                ctx->vmid, ctx->id, ctx->weight);
    return 0;
}

static void pvegpu_mdev_remove(struct mdev_device *mdev)
{
    struct pvegpu_vm_ctx *ctx = dev_get_drvdata(mdev_dev(mdev));

    if (!ctx) {
        PVEGPU_ERR("remove: no context found\n");
        return;
    }

    PVEGPU_INFO("remove: vmid=%d slot=%u\n", ctx->vmid, ctx->id);

    vfio_unregister_group_dev(&ctx->vdev);
    pvegpu_ctx_destroy(ctx);

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
    /* vfio_put_device relache la reference. Le VFIO core appelle
     * .release puis kvfree(ctx) quand le refcount atteint 0.
     */
    vfio_put_device(&ctx->vdev);
#else
    vfio_uninit_group_dev(&ctx->vdev);
    kfree(ctx);
#endif

    dev_set_drvdata(mdev_dev(mdev), NULL);
}

/* ============================================================
 * SYSFS
 * ============================================================ */

static unsigned int pvegpu_mdev_available_instances(struct mdev_type *mtype)
{
    struct pvegpu_device *gdev = pvegpu_device_get();

    if (!gdev)
        return 0;

    return PVEGPU_MAX_DOMAINS - gdev->n_contexts;
}

static ssize_t pvegpu_mdev_show_description(struct mdev_type *mtype,
                                             char *buf)
{
    struct pvegpu_device *gdev = pvegpu_device_get();

    if (!gdev)
        return sysfs_emit(buf, "%s\n", PVEGPU_MDEV_DESCRIPTION);

    return sysfs_emit(buf,
        "%s\n"
        "vendor: %s\n"
        "chipset: 0x%x\n"
        "vram_total: %llu MB\n"
        "vram_per_vm: %llu MB\n"
        "max_vms: %d\n"
        "active_vms: %d\n"
        "channels: %u\n"
        "scheduler: BAND+WFQ period=%lld us\n"
        "features: shadow_pt, irq_fwd, flr, nvidia+amd\n",
        PVEGPU_MDEV_DESCRIPTION,
        gdev->gpu_ops ? gdev->gpu_ops->name : "unknown",
        gdev->chipset_type,
        gdev->vram_total >> 20,
        gdev->vram_per_vm >> 20,
        PVEGPU_MAX_DOMAINS,
        gdev->n_contexts,
        gdev->total_channels,
        ktime_to_us(gdev->scheduler.period));
}

static struct mdev_driver pvegpu_mdev_driver = {
    .device_api         = VFIO_DEVICE_API_PCI_STRING,
    .probe              = pvegpu_mdev_probe,
    .remove             = pvegpu_mdev_remove,
    .get_available      = pvegpu_mdev_available_instances,
    .show_description   = pvegpu_mdev_show_description,
    .driver = {
        .name           = "pvegpu_sched",
        .owner          = THIS_MODULE,
    },
};

/* ============================================================
 * MDEV TYPE DEFINITION
 *
 * Definit le type mdev "pvegpu-sched" expose dans sysfs :
 *   /sys/bus/pci/devices/<GPU>/mdev_supported_types/pvegpu-sched/
 *
 * Kernel 6.6+ : mdev_register_parent attend struct mdev_type **
 * Le mdev core initialise le kobject et cree les entrees sysfs.
 * Les callbacks get_available/show_description du mdev_driver
 * fournissent les infos pour available_instances et description.
 * ============================================================ */

static struct mdev_type pvegpu_mdev_type_obj = {
    .sysfs_name  = PVEGPU_MDEV_TYPE_NAME,
    .pretty_name = PVEGPU_MDEV_DESCRIPTION,
};

static struct mdev_type *pvegpu_mdev_types[] = {
    &pvegpu_mdev_type_obj,
};

/* ============================================================
 * PCI DRIVER
 * ============================================================ */

static int pvegpu_pci_probe(struct pci_dev *pdev,
                             const struct pci_device_id *id)
{
    struct pvegpu_device *gdev;
    int ret;

    PVEGPU_INFO("pci_probe: found GPU %04x:%04x\n",
                pdev->vendor, pdev->device);

    gdev = kzalloc(sizeof(*gdev), GFP_KERNEL);
    if (!gdev)
        return -ENOMEM;

    ret = pvegpu_device_init(gdev, pdev);
    if (ret) {
        PVEGPU_ERR("pci_probe: device_init failed: %d\n", ret);
        kfree(gdev);
        return ret;
    }

    gdev->dev = &pdev->dev;
    pci_set_drvdata(pdev, gdev);

    /* Enregistrer l'IRQ GPU pour le forwarding */
    ret = request_irq(pdev->irq, pvegpu_gpu_irq_handler,
                      IRQF_SHARED, "pvegpu_sched", gdev);
    if (ret) {
        PVEGPU_ERR("pci_probe: request_irq failed: %d "
                   "(IRQ forwarding disabled)\n", ret);
        /* Non-fatal : on continue sans IRQ forwarding */
    } else {
        gdev->irq_registered = true;
        PVEGPU_INFO("pci_probe: IRQ %d registered for forwarding\n",
                    pdev->irq);
    }

    ret = PVEGPU_MDEV_REGISTER_PARENT(&gdev->mdev_parent, &pdev->dev,
                                       &pvegpu_mdev_driver,
                                       pvegpu_mdev_types,
                                       ARRAY_SIZE(pvegpu_mdev_types));
    if (ret) {
        PVEGPU_ERR("pci_probe: mdev_register_parent failed: %d\n", ret);
        if (gdev->irq_registered)
            free_irq(pdev->irq, gdev);
        pvegpu_device_fini(gdev);
        kfree(gdev);
        return ret;
    }

    PVEGPU_INFO("pci_probe: GPU ready, mdev parent registered\n");
    return 0;
}

static void pvegpu_pci_remove(struct pci_dev *pdev)
{
    struct pvegpu_device *gdev = pci_get_drvdata(pdev);

    if (!gdev)
        return;

    PVEGPU_INFO("pci_remove: GPU %04x:%04x\n",
                pdev->vendor, pdev->device);

    mdev_unregister_parent(&gdev->mdev_parent);

    if (gdev->irq_registered) {
        free_irq(pdev->irq, gdev);
        gdev->irq_registered = false;
    }

    pvegpu_device_fini(gdev);
    kfree(gdev);
    pci_set_drvdata(pdev, NULL);
}

static struct pci_driver pvegpu_pci_driver = {
    .name     = "pvegpu_sched",
    .id_table = pvegpu_pci_table,
    .probe    = pvegpu_pci_probe,
    .remove   = pvegpu_pci_remove,
};

/* ============================================================
 * MODULE INIT / EXIT
 * ============================================================ */

static int __init pvegpu_mdev_init(void)
{
    int ret;

    PVEGPU_INFO("loading PVE GPU scheduler v0.2.0\n");
    PVEGPU_INFO("config: max_domains=%d channels_per_vm=%d period=%d us\n",
                PVEGPU_MAX_DOMAINS,
                PVEGPU_DOMAIN_CHANNELS,
                PVEGPU_SCHED_PERIOD_US);
    PVEGPU_INFO("features: WFQ scheduling, shadow page tables, "
                "IRQ forwarding, AMD+NVIDIA\n");
    PVEGPU_INFO("registering mdev driver\n");

    ret = mdev_register_driver(&pvegpu_mdev_driver);
    if (ret) {
        PVEGPU_ERR("mdev_register_driver failed: %d\n", ret);
        return ret;
    }

    ret = pci_register_driver(&pvegpu_pci_driver);
    if (ret) {
        PVEGPU_ERR("pci_register_driver failed: %d\n", ret);
        mdev_unregister_driver(&pvegpu_mdev_driver);
        return ret;
    }

    PVEGPU_INFO("mdev driver registered\n");
    return 0;
}

static void __exit pvegpu_mdev_exit(void)
{
    pci_unregister_driver(&pvegpu_pci_driver);
    mdev_unregister_driver(&pvegpu_mdev_driver);
    PVEGPU_INFO("mdev driver unregistered\n");
    PVEGPU_INFO("unloading PVE GPU scheduler\n");
}

module_init(pvegpu_mdev_init);
module_exit(pvegpu_mdev_exit);
