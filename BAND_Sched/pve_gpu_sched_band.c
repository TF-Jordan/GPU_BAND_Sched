// SPDX-License-Identifier: GPL-2.0-only
/*
 * pve_gpu_sched_band.c — Algorithme BAND (Bandwidth-Aware Non-preemptive
 * Dispatcher) pour le partage GPU entre VMs Proxmox.
 *
 * Traduction directe de gxen/tools/a3/band_scheduler.cc en C kernel.
 *
 * Trois threads kernel :
 *  - pvegpu_run_thread      : attend une commande -> choisit la VM -> soumet
 *  - pvegpu_replenish_thread: recharge les budgets avec WFQ
 *  - pvegpu_sampler_thread  : mesure l'utilisation reelle GPU
 *
 * Reference :
 *   Yusuke Suzuki et al. "GPUvm: GPU Virtualization at the Hypervisor"
 *   USENIX ATC 2014.
 */

#include <linux/kthread.h>
#include <linux/delay.h>
#include <linux/sched.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/wait.h>
#include <linux/ktime.h>
#include <linux/hrtimer.h>

#include "pve_gpu_sched.h"

/* ============================================================
 * GESTION DU BUDGET PAR CONTEXTE
 * Equivalent de context::replenish() et update_budget() dans a3.
 * ============================================================ */

/*
 * pvegpu_ctx_update_budget — debite le budget apres execution
 *
 * Appele par submit() apres chaque commande GPU.
 * Equivalent de context::update_budget() dans a3.
 */
void pvegpu_ctx_update_budget(struct pvegpu_vm_ctx *ctx, ktime_t duration)
{
    unsigned long flags;

    spin_lock_irqsave(&ctx->band_lock, flags);
    ctx->budget        = ktime_sub(ctx->budget, duration);
    ctx->bandwidth_used = ktime_add(ctx->bandwidth_used, duration);
    ctx->sampling_bw_used = ktime_add(ctx->sampling_bw_used, duration);
    spin_unlock_irqrestore(&ctx->band_lock, flags);
}

/*
 * pvegpu_ctx_replenish_wfq — recharge le budget avec Weighted Fair Queuing
 *
 * Au lieu de diviser le budget equitablement entre toutes les VMs,
 * on le distribue proportionnellement au poids (weight) de chaque VM.
 *
 * budget = period * (weight / total_weight)
 *
 * Cela permet a une VM avec weight=80 d'obtenir 4x plus de temps GPU
 * qu'une VM avec weight=20, dans une config a 2 VMs.
 */
static void pvegpu_ctx_replenish_wfq(struct pvegpu_vm_ctx *ctx,
                                       ktime_t period,
                                       uint32_t total_weight,
                                       ktime_t defaults,
                                       bool idle)
{
    unsigned long flags;
    ktime_t credit;

    spin_lock_irqsave(&ctx->band_lock, flags);

    if (idle) {
        /* GPU etait inactif : reinitialiser avec le budget par defaut */
        ctx->budget = defaults;
    } else {
        /* WFQ : budget = period * (weight / total_weight)
         * Calcul en nanosecondes pour eviter les pertes de precision.
         */
        if (total_weight > 0) {
            int64_t period_ns = ktime_to_ns(period);
            int64_t credit_ns = (period_ns * ctx->weight) / total_weight;
            credit = ns_to_ktime(credit_ns);
        } else {
            credit = defaults;
        }
        ctx->budget = ktime_add(ctx->budget, credit);
    }

    /* Reinitialiser bandwidth_used pour le prochain cycle */
    ctx->bandwidth_used = ktime_set(0, 0);

    spin_unlock_irqrestore(&ctx->band_lock, flags);

    PVEGPU_LOG("replenish vmid=%d weight=%u budget=%lld ns\n",
               ctx->vmid, ctx->weight, ktime_to_ns(ctx->budget));
}

/* ============================================================
 * LOGIQUE DE SELECTION
 * Traduction directe de band_scheduler_t::select_next_context()
 * ============================================================ */

/*
 * pvegpu_utilization_over_bandwidth — test de depassement de quota
 *
 * Equivalent exact de band_scheduler_t::utilization_over_bandwidth().
 *
 * Retourne true si la VM a consomme plus que sa part ponderee,
 * ce qui la degrade dans la hierarchie de selection.
 */
static bool pvegpu_utilization_over_bandwidth(struct pvegpu_scheduler *sched,
                                               struct pvegpu_vm_ctx *ctx)
{
    ktime_t bw = sched->bandwidth;
    int64_t fair_share_ns;

    if (ktime_to_ns(bw) == 0)
        return true;

    if (sched->n_contexts == 0 || sched->total_weight == 0)
        return false;

    /* Part equitable ponderee = previous_bandwidth * weight / total_weight */
    fair_share_ns = (ktime_to_ns(sched->previous_bandwidth) * ctx->weight)
                    / sched->total_weight;

    if (ktime_to_ns(ctx->bandwidth_used) > fair_share_ns)
        return true;

    return false;
}

/*
 * pvegpu_select_next_context — choisit la prochaine VM a servir
 *
 * Traduction directe de band_scheduler_t::select_next_context().
 *
 * Algorithme BAND :
 *  1. Penalise la VM courante si elle a depasse son quota
 *  2. Classe les VMs en 3 categories selon leur etat
 *  3. Priorite : under < band < over (under = meilleure priorite)
 *
 * Appele avec sched_lock deja acquis.
 */
static struct pvegpu_vm_ctx *
pvegpu_select_next_context(struct pvegpu_scheduler *sched, bool idle)
{
    struct pvegpu_vm_ctx *ctx;
    struct pvegpu_vm_ctx *band  = NULL;
    struct pvegpu_vm_ctx *under = NULL;
    struct pvegpu_vm_ctx *over  = NULL;
    struct pvegpu_vm_ctx *next  = NULL;
    ktime_t now;

    if (idle) {
        now = ktime_get();
        sched->gpu_idle = ktime_add(sched->gpu_idle,
                                     ktime_sub(now, sched->gpu_idle_start));
    }

    /* Penalise la VM courante si elle a depasse son budget ET quota */
    if (sched->current_ctx) {
        ctx = sched->current_ctx;
        if (ktime_to_ns(ctx->budget) < 0 &&
            pvegpu_utilization_over_bandwidth(sched, ctx)) {
            list_del(&ctx->list);
            list_add_tail(&ctx->list, &sched->contexts);
        }
    }

    /* Parcourir toutes les VMs et les classer en 3 categories */
    list_for_each_entry(ctx, &sched->contexts, list) {
        if (!pvegpu_ctx_is_suspended(ctx))
            continue;

        if (ktime_to_ns(ctx->budget) < 0) {
            if (!over)
                over = ctx;
        } else if (pvegpu_utilization_over_bandwidth(sched, ctx)) {
            if (!band)
                band = ctx;
        } else {
            if (!under)
                under = ctx;
        }

        if (over && under && band)
            break;
    }

    /* Selection : priorite under > band > over */
    next = under ? under : (band ? band : over);

    if (!sched->current_ctx)
        return next;

    /* Logique de "yield chance" */
    if (next && next != sched->current_ctx &&
        pvegpu_utilization_over_bandwidth(sched, next) &&
        !pvegpu_utilization_over_bandwidth(sched, sched->current_ctx) &&
        ktime_compare(next->bandwidth_used,
                      sched->current_ctx->bandwidth_used) > 0) {
        if (pvegpu_ctx_is_suspended(sched->current_ctx))
            return sched->current_ctx;
    }

    return next;
}

/* ============================================================
 * SOUMISSION AU GPU
 * Traduction de band_scheduler_t::submit() dans a3.
 * ============================================================ */

/*
 * pvegpu_submit — soumet une commande au GPU physique
 *
 * Equivalent direct de band_scheduler_t::submit() dans a3.
 * Utilise le registre de status GPU pour attendre la fin d'execution
 * au lieu de polling aveugle.
 *
 * Appele avec fire_lock acquis.
 */
static void pvegpu_submit(struct pvegpu_scheduler *sched,
                           struct pvegpu_vm_ctx *ctx)
{
    struct pvegpu_cmd cmd;
    ktime_t t_start, t_end, duration;
    int ret;

    ret = kfifo_out(&ctx->suspended, &cmd, sizeof(cmd));
    if (ret != sizeof(cmd)) {
        PVEGPU_ERR("submit: empty queue for vmid=%d\n", ctx->vmid);
        return;
    }

    t_start = ktime_get();

    /* Soumet la commande selon le BAR cible */
    switch (cmd.bar) {
    case PVEGPU_BAR0:
        if (cmd.type == PVEGPU_CMD_TYPE_WRITE)
            pvegpu_write_bar0(ctx, &cmd);
        else
            pvegpu_read_bar0(ctx, &cmd);
        break;
    case PVEGPU_BAR1:
        if (cmd.type == PVEGPU_CMD_TYPE_WRITE)
            pvegpu_write_bar1(ctx, &cmd);
        else
            pvegpu_read_bar1(ctx, &cmd);
        break;
    case PVEGPU_BAR3:
        if (cmd.type == PVEGPU_CMD_TYPE_WRITE)
            pvegpu_write_bar3(ctx, &cmd);
        else
            pvegpu_read_bar3(ctx, &cmd);
        break;
    default:
        PVEGPU_ERR("submit: unknown BAR %u for vmid=%d\n",
                   cmd.bar, ctx->vmid);
        return;
    }

    /*
     * Attend la fin d'execution GPU.
     * Utilise le registre de status specifique au vendor (NVIDIA/AMD).
     */
    {
        struct pvegpu_device *gdev = ctx->dev;
        int timeout = 10000;
        uint32_t status;
        uint32_t status_reg;

        status_reg = gdev->gpu_ops ?
                     gdev->gpu_ops->get_status_reg() : 0x400700;

        do {
            status = pvegpu_bar_read32(gdev, 0, status_reg);
            if (status == 0)
                break;
            udelay(1);
        } while (--timeout > 0);

        if (timeout == 0)
            PVEGPU_ERR("GPU timeout waiting for idle (vmid=%d status=0x%x)\n",
                       ctx->vmid, status);
    }

    t_end = ktime_get();
    duration = ktime_sub(t_end, t_start);

    sched->bandwidth = ktime_add(sched->bandwidth, duration);
    sched->total_cmds_processed++;
    pvegpu_ctx_update_budget(ctx, duration);

    PVEGPU_LOG("submit vmid=%d bar=%u dur=%lld ns budget=%lld ns\n",
               ctx->vmid, cmd.bar,
               ktime_to_ns(duration),
               ktime_to_ns(ctx->budget));
}

/* ============================================================
 * THREAD PRINCIPAL — run()
 * Traduction de band_scheduler_t::run() dans a3.
 * ============================================================ */

static int pvegpu_run_thread(void *data)
{
    struct pvegpu_scheduler *sched = data;
    struct pvegpu_vm_ctx *next;
    struct pvegpu_vm_ctx *prev_ctx = NULL;
    bool idle;
    unsigned long flags;

    PVEGPU_INFO("run_thread started\n");

    while (!kthread_should_stop()) {
        idle = false;

        sched->gpu_idle_start = ktime_get();

        wait_event_interruptible(sched->wq,
            atomic64_read((atomic64_t *)&sched->counter) > 0 ||
            kthread_should_stop());

        if (kthread_should_stop())
            break;

        spin_lock_irqsave(&sched->counter_lock, flags);
        if (sched->counter == 0) {
            spin_unlock_irqrestore(&sched->counter_lock, flags);
            idle = true;
            schedule();
            continue;
        }
        spin_unlock_irqrestore(&sched->counter_lock, flags);

        /* Selectionner la prochaine VM */
        spin_lock_irqsave(&sched->sched_lock, flags);
        next = pvegpu_select_next_context(sched, idle);
        prev_ctx = sched->current_ctx;
        sched->current_ctx = next;
        spin_unlock_irqrestore(&sched->sched_lock, flags);

        if (!next) {
            schedule();
            continue;
        }

        /* Context switch : resynchroniser BAR1 et flusher TLB */
        if (next != prev_ctx && prev_ctx != NULL) {
            pvegpu_shadow_bar1(next);

            if (sched->dev->gpu_ops &&
                sched->dev->gpu_ops->flush_tlb) {
                mutex_lock(&sched->tlb_flush_mutex);
                sched->dev->gpu_ops->flush_tlb(next, 0x1 | 0x4);
                mutex_unlock(&sched->tlb_flush_mutex);
            }
        }

        spin_lock_irqsave(&sched->counter_lock, flags);
        if (sched->counter > 0)
            sched->counter--;
        spin_unlock_irqrestore(&sched->counter_lock, flags);

        /* Soumettre au GPU */
        spin_lock_irqsave(&sched->fire_lock, flags);
        pvegpu_submit(sched, next);
        spin_unlock_irqrestore(&sched->fire_lock, flags);
    }

    PVEGPU_INFO("run_thread stopped\n");
    return 0;
}

/* ============================================================
 * THREAD DE RECHARGE — replenish()
 * Utilise le WFQ pour distribuer le budget proportionnellement.
 * ============================================================ */

static int pvegpu_replenish_thread(void *data)
{
    struct pvegpu_scheduler *sched = data;
    struct pvegpu_vm_ctx *ctx;
    unsigned long flags;

    PVEGPU_INFO("replenish_thread started\n");

    while (!kthread_should_stop()) {
        spin_lock_irqsave(&sched->sched_lock, flags);
        spin_lock(&sched->fire_lock);

        if (!list_empty(&sched->contexts)) {
            ktime_t period, defaults;
            bool was_idle;

            period = ktime_add(sched->bandwidth, sched->gpu_idle);
            was_idle = (ktime_to_ns(sched->bandwidth) == 0);

            /* Budget par defaut : periode / nombre de VMs
             * Utilise seulement quand le GPU etait inactif
             */
            defaults = ktime_divns(sched->period, sched->n_contexts ?
                                   sched->n_contexts : 1);

            if (ktime_to_ns(period) > 0) {
                /* WFQ replenish : chaque VM recoit un budget proportionnel
                 * a son poids */
                list_for_each_entry(ctx, &sched->contexts, list) {
                    pvegpu_ctx_replenish_wfq(ctx, period,
                                              sched->total_weight,
                                              defaults, was_idle);
                }
            }

            sched->previous_bandwidth = period;
            sched->bandwidth  = ktime_set(0, 0);
            sched->gpu_idle   = ktime_set(0, 0);
        }

        spin_unlock(&sched->fire_lock);
        spin_unlock_irqrestore(&sched->sched_lock, flags);

        usleep_range(ktime_to_us(sched->period),
                     ktime_to_us(sched->period) + 100);

        schedule();
    }

    PVEGPU_INFO("replenish_thread stopped\n");
    return 0;
}

/* ============================================================
 * THREAD SAMPLER — mesure l'utilisation reelle GPU
 * Traduction de band_scheduler_t::sampler() dans a3.
 *
 * Mesure periodiquement la bande passante GPU utilisee par
 * chaque VM et met a jour les statistiques. Ces stats sont
 * exposees via sysfs pour le monitoring.
 * ============================================================ */

static int pvegpu_sampler_thread(void *data)
{
    struct pvegpu_scheduler *sched = data;
    struct pvegpu_vm_ctx *ctx;
    unsigned long flags;

    PVEGPU_INFO("sampler_thread started (period=%lld us)\n",
                ktime_to_us(sched->sample_period));

    while (!kthread_should_stop()) {
        spin_lock_irqsave(&sched->sched_lock, flags);

        list_for_each_entry(ctx, &sched->contexts, list) {
            unsigned long ctx_flags;

            spin_lock_irqsave(&ctx->band_lock, ctx_flags);

            /* Enregistrer la bande passante mesuree depuis le dernier
             * echantillon. sampling_bw_used est incremente par
             * pvegpu_ctx_update_budget() a chaque submit().
             */
            ctx->bandwidth = ctx->sampling_bw_used;
            ctx->sampling_bw_used = ktime_set(0, 0);

            spin_unlock_irqrestore(&ctx->band_lock, ctx_flags);

            PVEGPU_LOG("sampler vmid=%d bw=%lld ns\n",
                       ctx->vmid, ktime_to_ns(ctx->bandwidth));
        }

        spin_unlock_irqrestore(&sched->sched_lock, flags);

        usleep_range(ktime_to_us(sched->sample_period),
                     ktime_to_us(sched->sample_period) + 1000);

        schedule();
    }

    PVEGPU_INFO("sampler_thread stopped\n");
    return 0;
}

/* ============================================================
 * POINT D'ENTREE PUBLIC — enqueue()
 * Traduction de band_scheduler_t::enqueue() dans a3.
 * ============================================================ */

void pvegpu_enqueue(struct pvegpu_vm_ctx *ctx, const struct pvegpu_cmd *cmd)
{
    struct pvegpu_scheduler *sched = &ctx->dev->scheduler;
    unsigned long flags;
    int ret;

    ret = kfifo_in(&ctx->suspended, cmd, sizeof(*cmd));
    if (ret != sizeof(*cmd)) {
        PVEGPU_ERR("enqueue: queue full for vmid=%d, dropping command\n",
                   ctx->vmid);
        return;
    }

    spin_lock_irqsave(&sched->counter_lock, flags);
    sched->counter++;
    spin_unlock_irqrestore(&sched->counter_lock, flags);

    wake_up_interruptible(&sched->wq);

    PVEGPU_LOG("enqueue vmid=%d bar=%u offset=0x%x val=0x%x counter=%llu\n",
               ctx->vmid, cmd->bar, cmd->offset, cmd->value,
               sched->counter);
}

/* ============================================================
 * INIT / FINI DU SCHEDULER
 * ============================================================ */

int pvegpu_sched_init(struct pvegpu_scheduler *sched,
                       struct pvegpu_device *gdev,
                       ktime_t period)
{
    INIT_LIST_HEAD(&sched->contexts);
    sched->n_contexts       = 0;
    sched->total_weight     = 0;
    sched->dev              = gdev;
    sched->period           = period;
    sched->bandwidth        = ktime_set(0, 0);
    sched->previous_bandwidth = ktime_set(0, 0);
    sched->gpu_idle         = ktime_set(0, 0);
    sched->counter          = 0;
    sched->current_ctx      = NULL;
    sched->running          = false;
    sched->total_cmds_processed = 0;
    sched->sample_period    = ktime_set(0, PVEGPU_SAMPLE_PERIOD_US * 1000);

    spin_lock_init(&sched->fire_lock);
    spin_lock_init(&sched->sched_lock);
    spin_lock_init(&sched->counter_lock);
    mutex_init(&sched->tlb_flush_mutex);
    init_waitqueue_head(&sched->wq);

    /* Demarrer le run_thread */
    sched->run_thread = kthread_run(pvegpu_run_thread, sched,
                                    "pvegpu_run");
    if (IS_ERR(sched->run_thread)) {
        PVEGPU_ERR("failed to start run_thread\n");
        return PTR_ERR(sched->run_thread);
    }

    /* Demarrer le replenish_thread */
    sched->replenish_thread = kthread_run(pvegpu_replenish_thread, sched,
                                           "pvegpu_replenish");
    if (IS_ERR(sched->replenish_thread)) {
        PVEGPU_ERR("failed to start replenish_thread\n");
        kthread_stop(sched->run_thread);
        return PTR_ERR(sched->replenish_thread);
    }

    /* Demarrer le sampler_thread */
    sched->sampler_thread = kthread_run(pvegpu_sampler_thread, sched,
                                         "pvegpu_sampler");
    if (IS_ERR(sched->sampler_thread)) {
        PVEGPU_ERR("failed to start sampler_thread\n");
        kthread_stop(sched->replenish_thread);
        kthread_stop(sched->run_thread);
        return PTR_ERR(sched->sampler_thread);
    }

    sched->running = true;

    PVEGPU_INFO("scheduler started: period=%lld us sample_period=%lld us "
                "WFQ=enabled\n",
                ktime_to_us(period),
                ktime_to_us(sched->sample_period));
    return 0;
}

void pvegpu_sched_fini(struct pvegpu_scheduler *sched)
{
    if (!sched->running)
        return;

    sched->running = false;

    if (sched->sampler_thread) {
        kthread_stop(sched->sampler_thread);
        sched->sampler_thread = NULL;
    }

    if (sched->replenish_thread) {
        kthread_stop(sched->replenish_thread);
        sched->replenish_thread = NULL;
    }

    if (sched->run_thread) {
        wake_up_interruptible(&sched->wq);
        kthread_stop(sched->run_thread);
        sched->run_thread = NULL;
    }

    mutex_destroy(&sched->tlb_flush_mutex);

    PVEGPU_INFO("scheduler stopped (total cmds processed: %llu)\n",
                sched->total_cmds_processed);
}

/* ============================================================
 * REGISTER / UNREGISTER CONTEXT
 * ============================================================ */

void pvegpu_sched_register_ctx(struct pvegpu_scheduler *sched,
                                 struct pvegpu_vm_ctx *ctx)
{
    unsigned long flags;

    spin_lock_irqsave(&sched->sched_lock, flags);
    list_add_tail(&ctx->list, &sched->contexts);
    sched->n_contexts++;
    sched->total_weight += ctx->weight;
    spin_unlock_irqrestore(&sched->sched_lock, flags);

    PVEGPU_INFO("registered vmid=%d weight=%u (total=%d total_weight=%u)\n",
                ctx->vmid, ctx->weight,
                sched->n_contexts, sched->total_weight);
}

void pvegpu_sched_unregister_ctx(struct pvegpu_scheduler *sched,
                                   struct pvegpu_vm_ctx *ctx)
{
    unsigned long flags;

    spin_lock_irqsave(&sched->sched_lock, flags);
    list_del_init(&ctx->list);
    sched->n_contexts--;
    sched->total_weight -= ctx->weight;
    if (sched->current_ctx == ctx)
        sched->current_ctx = NULL;
    spin_unlock_irqrestore(&sched->sched_lock, flags);

    PVEGPU_INFO("unregistered vmid=%d (total=%d total_weight=%u)\n",
                ctx->vmid, sched->n_contexts, sched->total_weight);
}
