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
 * VFIO DEVICE INITIALIZATION
 *
 * Kernel 5.15-6.7  : vfio_init_group_dev() + vfio_register_group_dev()
 * Kernel 6.8+      : vfio_init_group_dev() removed.
 *                     Use vfio_alloc_device() or manual init.
 *
 * Pour notre cas mdev, on utilise simplement
 * vfio_register_emulated_iommu_dev() (6.1+) qui gere aussi l'init.
 * On fournit un wrapper no-op pour vfio_init_group_dev sur 6.8+.
 * ============================================================ */

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 8, 0)
  /* vfio_init_group_dev() n'existe plus. Le device est initialise
   * directement par vfio_register_emulated_iommu_dev().
   * On fournit un no-op inline pour eviter de changer tout le code.
   */
  static inline void pvegpu_vfio_init_dev(struct vfio_device *vdev,
                                           struct device *dev,
                                           const struct vfio_device_ops *ops)
  {
      /* Le vfio_device doit etre initialise manuellement */
      vdev->dev = dev;
      vdev->ops = ops;
  }
#else
  static inline void pvegpu_vfio_init_dev(struct vfio_device *vdev,
                                           struct device *dev,
                                           const struct vfio_device_ops *ops)
  {
      vfio_init_group_dev(vdev, dev, ops);
  }
#endif

/* ============================================================
 * VFIO DEVICE REGISTRATION
 *
 * Kernel 5.15 : vfio_register_group_dev()
 * Kernel 6.1+ : vfio_register_emulated_iommu_dev() pour mdev
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
 * VFIO DEVICE CLEANUP
 *
 * Kernel 5.15 : vfio_uninit_group_dev()
 * Kernel 6.1+ : vfio_put_device() ou simplement rien
 *               (le cleanup se fait via release callback)
 * ============================================================ */

static inline void pvegpu_vfio_cleanup_dev(struct vfio_device *vdev)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
    vfio_put_device(vdev);
#else
    vfio_uninit_group_dev(vdev);
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

