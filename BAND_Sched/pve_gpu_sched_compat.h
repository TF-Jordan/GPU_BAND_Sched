/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * pve_gpu_sched_compat.h — Couche de compatibilite kernel
 *
 * Gere les differences d'API entre les versions du kernel Linux :
 *  - Proxmox VE 7.x : kernel 5.15 LTS
 *  - Proxmox VE 8.x : kernel 6.2 - 6.8
 *  - Proxmox VE 8.x+: kernel 6.9 - 6.17+
 *
 * APIs couvertes :
 *  - VFIO device lifecycle (init/register/cleanup)
 *  - VFIO PCI offset macros (removed in 6.13+)
 *  - mdev parent registration
 *  - eventfd_signal signature
 *  - vm_flags (read-only since 6.3+)
 */

#ifndef PVE_GPU_SCHED_COMPAT_H
#define PVE_GPU_SCHED_COMPAT_H

#include <linux/version.h>
#include <linux/vfio.h>
#include <linux/mdev.h>
#include <linux/eventfd.h>
#include <linux/delay.h>
#include <linux/mm.h>

/* ============================================================
 * VFIO PCI OFFSET MACROS
 *
 * Ces macros ont ete retirees des headers UAPI/VFIO dans les
 * kernels recents (>= 6.13). On les redefinit ici si absentes.
 *
 * Elles servent a encoder/decoder le numero de BAR et l'offset
 * dans un seul loff_t (pour les ops read/write VFIO).
 * ============================================================ */

#ifndef VFIO_PCI_OFFSET_SHIFT
  #define VFIO_PCI_OFFSET_SHIFT   40
#endif

#ifndef VFIO_PCI_OFFSET_MASK
  #define VFIO_PCI_OFFSET_MASK    ((1ULL << VFIO_PCI_OFFSET_SHIFT) - 1)
#endif

#ifndef VFIO_PCI_OFFSET_TO_INDEX
  #define VFIO_PCI_OFFSET_TO_INDEX(off)  ((off) >> VFIO_PCI_OFFSET_SHIFT)
#endif

#ifndef VFIO_PCI_INDEX_TO_OFFSET
  #define VFIO_PCI_INDEX_TO_OFFSET(idx)  ((u64)(idx) << VFIO_PCI_OFFSET_SHIFT)
#endif

/* ============================================================
 * VM_FLAGS — lecture seule depuis kernel 6.3+
 *
 * Kernel < 6.3  : vma->vm_flags |= flags;
 * Kernel >= 6.3 : vm_flags_set(vma, flags);
 * ============================================================ */

#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 3, 0)
  static inline void pvegpu_vm_flags_set(struct vm_area_struct *vma,
                                          vm_flags_t flags)
  {
      vma->vm_flags |= flags;
  }
#else
  static inline void pvegpu_vm_flags_set(struct vm_area_struct *vma,
                                          vm_flags_t flags)
  {
      vm_flags_set(vma, flags);
  }
#endif

/* ============================================================
 * VFIO DEVICE LIFECYCLE
 *
 * Kernel < 6.8 :
 *   kzalloc(ctx)
 *   vfio_init_group_dev(&ctx->vdev, dev, ops)
 *   vfio_register_group_dev() ou vfio_register_emulated_iommu_dev()
 *   ...
 *   vfio_unregister_group_dev()
 *   vfio_uninit_group_dev() (< 6.1) ou vfio_put_device() (>= 6.1)
 *   kfree(ctx)
 *
 * Kernel 6.8+ :
 *   vfio_alloc_device(type, member, dev, ops)
 *     → kvzalloc + vfio_init_device (device_initialize, etc.)
 *   vfio_register_emulated_iommu_dev()
 *   ...
 *   vfio_unregister_group_dev()
 *   vfio_put_device()
 *     → .release callback + kvfree
 *
 * Les #ifdefs d'allocation/cleanup sont directement dans
 * pvegpu_mdev_probe() et pvegpu_mdev_remove(). Ici on fournit
 * seulement le wrapper pour la registration.
 * ============================================================ */

static inline int pvegpu_vfio_register_dev(struct vfio_device *vdev)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
    return vfio_register_emulated_iommu_dev(vdev);
#else
    return vfio_register_group_dev(vdev);
#endif
}

/* ============================================================
 * MDEV PARENT REGISTRATION
 *
 * Kernel 5.15-6.0 : mdev_register_parent(parent, dev, driver)
 * Kernel 6.1-6.5  : mdev_register_parent(parent, dev, driver,
 *                                         type_groups, nr_types)
 *                    4th arg = struct attribute_group **
 * Kernel 6.6+     : mdev_register_parent(parent, dev, driver,
 *                                         types, nr_types)
 *                    4th arg = struct mdev_type **
 *
 * On fournit une macro unique pour gerer les trois cas.
 * ============================================================ */

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 6, 0)
  /* Kernel 6.6+ : types = struct mdev_type **, nr = count */
  #define PVEGPU_MDEV_REGISTER_PARENT(parent, dev, drv, types, nr) \
      mdev_register_parent(parent, dev, drv, types, nr)
#elif LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
  /* Kernel 6.1-6.5 : types = struct attribute_group ** */
  #define PVEGPU_MDEV_REGISTER_PARENT(parent, dev, drv, types, nr) \
      mdev_register_parent(parent, dev, drv, types, nr)
#else
  /* Kernel < 6.1 : pas de types */
  #define PVEGPU_MDEV_REGISTER_PARENT(parent, dev, drv, types, nr) \
      mdev_register_parent(parent, dev, drv)
#endif

/* ============================================================
 * EVENTFD SIGNAL
 *
 * Kernel < 6.8  : eventfd_signal(ctx, 1)  (2 args)
 * Kernel >= 6.8 : eventfd_signal(ctx)     (1 arg)
 * ============================================================ */

static inline void pvegpu_eventfd_signal(struct eventfd_ctx *ctx)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
    eventfd_signal(ctx);
#else
    eventfd_signal(ctx, 1);
#endif
}

/* ============================================================
 * MDEV DEVICE ACCESS
 *
 * Kernel 5.15 : mdev_dev(mdev) retourne &mdev->dev
 * Kernel 6.x  : pareil, mais parfois c'est dev_to_mdev()
 * ============================================================ */

#ifndef mdev_dev
  #define mdev_dev(mdev) (&(mdev)->dev)
#endif

#endif /* PVE_GPU_SCHED_COMPAT_H */

