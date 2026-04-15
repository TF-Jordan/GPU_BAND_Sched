/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * pve_gpu_sched_compat.h — Couche de compatibilite kernel
 *
 * Gere les differences d'API entre les versions du kernel Linux :
 *  - Proxmox VE 7.x : kernel 5.15 LTS
 *  - Proxmox VE 8.x : kernel 6.2 - 6.8
 *
 * APIs couvertes :
 *  - VFIO device lifecycle
 *  - mdev parent registration
 *  - eventfd_signal signature
 */

#ifndef PVE_GPU_SCHED_COMPAT_H
#define PVE_GPU_SCHED_COMPAT_H

#include <linux/version.h>
#include <linux/vfio.h>
#include <linux/mdev.h>
#include <linux/eventfd.h>

/* ============================================================
 * VFIO DEVICE REGISTRATION
 *
 * Kernel 5.15 : vfio_register_group_dev()
 * Kernel 6.1+ : vfio_register_emulated_iommu_dev() pour mdev
 * vfio_init_group_dev() existe dans les deux.
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
 * Kernel 6.1+     : mdev_register_parent(parent, dev, driver,
 *                                         types, nr_types)
 *
 * On fournit une macro pour gerer les deux cas.
 * ============================================================ */

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 1, 0)
  #define PVEGPU_MDEV_REGISTER_PARENT(parent, dev, drv, types, nr) \
      mdev_register_parent(parent, dev, drv, types, nr)
#else
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
