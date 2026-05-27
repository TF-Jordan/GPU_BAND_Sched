//! Point d'entrée de l'agent nœud A.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use node_a_agent::balloon::BalloonManager;
use node_a_agent::cluster::{local_available_mib, ClusterState};
use node_a_agent::compaction::ClusterCompactor;
use node_a_agent::config::Config;
use node_a_agent::disk_scheduler::DiskScheduler;
use node_a_agent::gpu_placement::GpuPlacementDaemon;
use node_a_agent::gpu_scheduler::GpuScheduler;
use node_a_agent::memory::{MemoryRegion, PAGE_SIZE};
use node_a_agent::metrics::AgentMetrics;
use node_a_agent::metrics_server;
use node_a_agent::migration::MigrationAgent;
use node_a_agent::prefetch::PrefetchEngine;
use node_a_agent::remote::RemoteStorePool;
use node_a_agent::shared_memory::{MemoryBackendKind, MemoryBackendOptions};
use node_a_agent::uffd::{spawn_fault_handler_thread, UffdHandle};
use node_a_agent::vcpu_scheduler::VCpuScheduler;

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 ne choisit pas de provider automatiquement.
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap_or(());

    let cfg = Config::parse();

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    match cfg.log_format.as_str() {
        "json" => fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(false)
            .init(),
        _ => fmt().with_env_filter(filter).with_target(false).init(),
    }

    info!(
        vm_id              = cfg.vm_id,
        stores             = ?cfg.stores,
        region_mib         = cfg.region_mib,
        vm_requested_mib   = cfg.vm_requested_mib,
        vm_vcpus           = cfg.vm_vcpus,
        vm_initial_vcpus   = cfg.vm_initial_vcpus,
        vcpu_overcommit    = cfg.vcpu_overcommit_ratio,
        backend            = %cfg.backend,
        mode               = %cfg.mode,
        "agent démarré"
    );

    // ── Pool de stores ────────────────────────────────────────────────────────
    let tls_fps: Vec<String> = cfg
        .tls_fingerprints
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let store = Arc::new(RemoteStorePool::new(
        cfg.stores.clone(),
        cfg.store_timeout_ms,
        tls_fps,
    ));

    info!("vérification de la connectivité aux stores...");
    let ping_results = store.ping_all().await;
    let mut stores_ok = 0;
    for (idx, ok) in &ping_results {
        if *ok {
            info!(store_idx = idx, addr = %cfg.stores[*idx], "store: PONG ok");
            stores_ok += 1;
        } else {
            warn!(store_idx = idx, addr = %cfg.stores[*idx], "store: pas de réponse");
        }
    }
    if stores_ok == 0 {
        bail!("aucun store disponible");
    }

    // ── État cluster ──────────────────────────────────────────────────────────
    let status_addrs = if cfg.status_addrs.is_empty() || cfg.status_addrs == [""] {
        cfg.stores
            .iter()
            .map(|s| format!("{}:9200", s.splitn(2, ':').next().unwrap_or("127.0.0.1")))
            .collect()
    } else {
        cfg.status_addrs.clone()
    };

    let cluster = Arc::new(ClusterState::new(cfg.stores.clone(), status_addrs));
    cluster.refresh().await;

    // ── Tokio handle ──────────────────────────────────────────────────────────
    let tokio_handle = tokio::runtime::Handle::current();

    // ── Métriques ─────────────────────────────────────────────────────────────
    let metrics = Arc::new(AgentMetrics::default());

    // ── Région mémoire ────────────────────────────────────────────────────────
    let region_size = cfg.region_mib * 1024 * 1024;
    let backend_kind = MemoryBackendKind::parse(&cfg.backend)?;
    let replication_flag = Arc::new(AtomicBool::new(cfg.replication_enabled));
    let region = Arc::new(
        MemoryRegion::allocate(
            region_size,
            cfg.vm_id,
            cfg.vm_requested_mib as u64,
            store.clone(),
            metrics.clone(),
            tokio_handle.clone(),
            MemoryBackendOptions {
                kind: backend_kind,
                memfd_name: cfg.memfd_name.clone(),
            },
            cluster.clone(),
            replication_flag,
        )
        .context("allocation région mémoire échouée")?,
    );

    if let Some(p) = region.backend_proc_fd_path() {
        info!(path = %p.display(), "backend partageable prêt");
    }
    if let Some(metadata_path) = &cfg.export_metadata {
        region.write_backend_metadata(metadata_path)?;
        info!(path = %metadata_path.display(), "métadonnées backend exportées");
    }

    // ── userfaultfd ───────────────────────────────────────────────────────────
    let uffd = UffdHandle::open().context("ouverture userfaultfd échouée")?;
    uffd.register_region(region.base_ptr(), region_size)?;
    let uffd_fd = uffd.fd();

    // ── Moteur de préfetch séquentiel ─────────────────────────────────────────
    let prefetch_engine: Option<Arc<PrefetchEngine>> = if cfg.prefetch_enabled {
        let engine = Arc::new(PrefetchEngine::new(
            (cfg.region_mib * 1024 * 1024 / node_a_agent::memory::PAGE_SIZE) as u64,
            cfg.prefetch_lookahead,
            cfg.prefetch_max_cached,
        ));
        info!(
            lookahead = cfg.prefetch_lookahead,
            max_cached = cfg.prefetch_max_cached,
            "préfetch séquentiel activé"
        );
        Some(engine)
    } else {
        None
    };

    // ── Thread handler uffd ───────────────────────────────────────────────────
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let region_start = region.base_ptr() as u64;
    let region_handler = region.clone();
    let prefetch_ref = prefetch_engine.clone();
    let fault_handler = Box::new(move |_vm_id: u32, page_id: u64, _addr: u64, _write: bool| {
        let result = region_handler.fetch_page(page_id);
        // Après un fetch réussi, enregistrer l'accès pour la détection de stride.
        // L'exécution réelle du prefetch (UFFDIO_COPY préventif) nécessite un support
        // dédié dans MemoryRegion — la détection est active, le cache reste en attente.
        if result.is_ok() {
            if let Some(engine) = &prefetch_ref {
                engine.record_access(page_id);
            }
        }
        result
    });

    let _handler_thread = spawn_fault_handler_thread(
        uffd,
        region_start,
        PAGE_SIZE as u64,
        cfg.vm_id,
        fault_handler,
        shutdown_flag.clone(),
        metrics.clone(),
    );

    // ── Serveur métriques HTTP (item 2) ───────────────────────────────────────
    {
        let m = metrics.clone();
        let c = cluster.clone();
        let r = region.clone();
        let addr = cfg.metrics_listen.clone();
        tokio::spawn(async move {
            if let Err(e) = metrics_server::run(addr, m, c, r).await {
                warn!(error = %e, "serveur métriques arrêté");
            }
        });
    }

    // ── Démon de placement GPU ────────────────────────────────────────────────
    // Auto-détection : config explicite OU passthrough PCI vers GPU détecté via qm config.
    let needs_gpu = cfg.gpu_required || detect_vm_gpu_passthrough(cfg.vm_id).await;
    if needs_gpu {
        // Démon de placement : migre la VM vers un nœud GPU si besoin
        let daemon = Arc::new(GpuPlacementDaemon::new(
            cfg.vm_id,
            cfg.current_node.clone(),
            cluster.clone(),
            cfg.vm_requested_mib as u64,
            cfg.gpu_placement_interval_secs,
        ));
        let sd = shutdown_flag.clone();
        tokio::spawn(async move { daemon.run(sd).await });
        info!(
            vm_id = cfg.vm_id,
            gpu_explicit = cfg.gpu_required,
            "démon GPU placement activé"
        );

        // Scheduler de partage GPU : round-robin QMP entre les VMs GPU du nœud
        // Actif uniquement si la VM a déjà hostpci0 configuré (passthrough prêt)
        if let Some(pci_id) = read_vm_hostpci0(cfg.vm_id).await {
            let scheduler = Arc::new(GpuScheduler::new(
                pci_id.clone(),
                cfg.gpu_quantum_secs,
                cfg.current_node.clone(),
            ));
            let sd = shutdown_flag.clone();
            tokio::spawn(async move { scheduler.run(sd).await });
            info!(
                pci       = %pci_id,
                quantum_s = cfg.gpu_quantum_secs,
                "scheduler GPU partage démarré (leader election via flock)"
            );
        } else {
            info!(
                vm_id = cfg.vm_id,
                "hostpci0 pas encore configuré — scheduler GPU en attente du placement"
            );
        }
    }

    // ── Scheduler vCPU élastique ──────────────────────────────────────────────
    let initial_vcpus = cfg.vm_initial_vcpus.min(cfg.vm_vcpus).max(1);
    let vcpu_sched = Arc::new(VCpuScheduler::new(
        cfg.vm_id,
        cfg.vm_vcpus,
        initial_vcpus,
        cfg.vcpu_high_threshold_pct,
        cfg.vcpu_low_threshold_pct,
        cfg.vcpu_scale_interval_secs,
        cfg.vcpu_overcommit_ratio,
    ));
    let cpu_pressure = vcpu_sched.cpu_pressure.clone();
    {
        let sd = shutdown_flag.clone();
        let vs = vcpu_sched.clone();
        tokio::spawn(async move { vs.run(sd).await });
    }
    info!(
        vm_id = cfg.vm_id,
        initial_vcpus,
        max_vcpus = cfg.vm_vcpus,
        overcommit = cfg.vcpu_overcommit_ratio,
        scale_interval = cfg.vcpu_scale_interval_secs,
        "scheduler vCPU élastique activé"
    );

    // ── Balloon thin-provisioning ─────────────────────────────────────────────
    let mut balloon_current_mib: Option<Arc<AtomicU64>> = None;
    if cfg.balloon_enabled {
        let initial_mib = cfg.balloon_initial_mib.min(cfg.region_mib as u64);
        let balloon_mgr = Arc::new(BalloonManager::new(
            cfg.vm_id,
            initial_mib,
            cfg.region_mib as u64,
            cfg.balloon_step_mib,
            cfg.balloon_interval_secs,
            cfg.balloon_grow_faults_per_sec,
            cfg.balloon_shrink_faults_per_sec,
            metrics.clone(),
        ));
        balloon_current_mib = Some(balloon_mgr.current_mib_handle());
        let sd = shutdown_flag.clone();
        tokio::spawn(async move { balloon_mgr.run(sd).await });
        info!(
            vm_id = cfg.vm_id,
            initial_mib,
            max_mib = cfg.region_mib,
            step_mib = cfg.balloon_step_mib,
            interval_s = cfg.balloon_interval_secs,
            "balloon thin-provisioning activé"
        );
    }

    // ── Scheduler I/O disque ─────────────────────────────────────────────────
    if cfg.disk_scheduler_enabled {
        let ds = Arc::new(DiskScheduler::new(
            vec![cfg.vm_id],
            cfg.disk_interval_secs,
            cfg.disk_psi_threshold,
            cfg.disk_active_bytes_mib * 1024 * 1024,
        ));
        let sd = shutdown_flag.clone();
        tokio::spawn(async move { ds.run(sd).await });
        info!(
            vm_id = cfg.vm_id,
            psi_threshold = cfg.disk_psi_threshold,
            active_mib = cfg.disk_active_bytes_mib,
            interval_s = cfg.disk_interval_secs,
            "scheduler I/O disque activé"
        );
    }

    // ── Exécution ─────────────────────────────────────────────────────────────
    match cfg.mode.as_str() {
        "demo" => run_demo(&region, &cfg, &metrics, &store, uffd_fd).await?,
        "daemon" => {
            run_daemon(
                &shutdown_flag,
                &region,
                &cluster,
                &metrics,
                &cfg,
                uffd_fd,
                cpu_pressure,
                needs_gpu,
                balloon_current_mib,
            )
            .await
        }
        m => bail!("mode inconnu : {m} (valides : demo, daemon)"),
    }

    shutdown_flag.store(true, Ordering::Relaxed);

    let snap = metrics.snapshot();
    info!(
        faults = snap.fault_count,
        served = snap.fault_served,
        errors = snap.fault_errors,
        evicted = snap.pages_evicted,
        fetched = snap.pages_fetched,
        recalled = snap.pages_recalled,
        zeros = snap.fetch_zeros,
        local_present = snap.local_present,
        alerts = snap.eviction_alerts,
        migrations = snap.migration_searches,
        "métriques finales"
    );

    Ok(())
}

// ─── Mode daemon ──────────────────────────────────────────────────────────────

async fn run_daemon(
    shutdown_flag: &Arc<AtomicBool>,
    region: &Arc<MemoryRegion>,
    cluster: &Arc<ClusterState>,
    metrics: &Arc<AgentMetrics>,
    cfg: &Config,
    uffd_fd: std::os::unix::io::RawFd,
    cpu_pressure: Arc<AtomicBool>,
    needs_gpu: bool,
    balloon_current_mib: Option<Arc<AtomicU64>>,
) {
    use tokio::signal::unix::{signal, SignalKind};
    use tokio::time::interval;

    let eviction_enabled = cfg.eviction_threshold_mib > 0;
    let recall_enabled = cfg.recall_threshold_mib > 0;

    let recall_priority_delay = priority_delay(cfg.recall_priority);
    // Item 6 : auto-détection du nombre de VMs si vm_count_hint est à sa valeur par défaut.
    let vm_count = if cfg.vm_count_hint == 1 {
        let detected = detect_vm_count();
        if detected > 1 {
            info!(detected, "vm_count_hint auto-détecté via qm list");
        }
        detected
    } else {
        cfg.vm_count_hint
    };
    let fair_recall_batch = fair_batch(cfg.recall_batch_size, vm_count);

    info!(
        eviction_threshold_mib = cfg.eviction_threshold_mib,
        recall_threshold_mib = cfg.recall_threshold_mib,
        recall_priority = cfg.recall_priority,
        recall_priority_delay_ms = recall_priority_delay.as_millis(),
        fair_recall_batch,
        vm_count,
        migration_enabled = cfg.migration_enabled,
        compaction_enabled = cfg.compaction_enabled,
        "mode daemon démarré"
    );

    let mut sigterm = signal(SignalKind::terminate()).expect("impossible d'écouter SIGTERM");
    let mut eviction_ticker = interval(Duration::from_secs(cfg.eviction_interval_secs.max(1)));
    let mut recall_ticker = interval(Duration::from_secs(cfg.recall_interval_secs.max(1)));
    let mut cluster_ticker = interval(Duration::from_secs(cfg.cluster_refresh_secs.max(5)));
    // Compaction globale : vérification toutes les 5 minutes si des alertes se sont accumulées
    let mut compact_ticker = interval(Duration::from_secs(300));
    // Surveillance pression CPU : déclenche migration si vCPU saturé
    let mut cpu_ticker = interval(Duration::from_secs(cfg.vcpu_scale_interval_secs.max(10)));
    // Surveillance balloon : si le guest consomme durablement une grosse part du plafond,
    // on escalade vers la recherche de migration au lieu de rester en croissance locale.
    let mut balloon_ticker = interval(Duration::from_secs(cfg.balloon_interval_secs.max(5)));
    let balloon_migration_threshold_mib = if cfg.balloon_enabled {
        let max_mib = cfg.region_mib as u64;
        let half_max = max_mib.saturating_mul(50) / 100;
        let min_plus_steps = cfg
            .balloon_initial_mib
            .saturating_add(cfg.balloon_step_mib.saturating_mul(2));
        half_max.max(min_plus_steps).min(max_mib)
    } else {
        u64::MAX
    };

    let mut migration_spawned = false;
    let mut consecutive_alerts = 0u32;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT reçu — arrêt");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM reçu — arrêt");
                break;
            }
            _ = cluster_ticker.tick() => {
                cluster.refresh().await;
                // Désactiver la réplication si tous les stores utilisent Ceph
                // (Ceph assure la redondance via min_size/size du pool).
                if cluster.all_ceph_enabled().await {
                    region.set_replication_enabled(false);
                }
            }
            _ = eviction_ticker.tick(), if eviction_enabled => {
                let avail_mib = local_available_mib();
                if avail_mib < cfg.eviction_threshold_mib {
                    let is_suffering = evict_batch(region, cluster, metrics, cfg).await;
                    if is_suffering {
                        consecutive_alerts += 1;
                        if cfg.migration_enabled && !migration_spawned {
                            spawn_migration_daemon(region, cluster, cfg, shutdown_flag, uffd_fd, cpu_pressure.clone(), needs_gpu);
                            metrics.migration_searches.fetch_add(1, Ordering::Relaxed);
                            migration_spawned = true;
                        }
                    } else {
                        consecutive_alerts = 0;
                    }
                } else {
                    debug!(avail_mib, "RAM OK — pas d'éviction");
                    consecutive_alerts = 0;
                }
            }
            _ = cpu_ticker.tick(), if cfg.migration_enabled && !migration_spawned => {
                if cpu_pressure.load(Ordering::Relaxed) {
                    info!("pression vCPU détectée — déclenchement recherche migration");
                    spawn_migration_daemon(region, cluster, cfg, shutdown_flag, uffd_fd, cpu_pressure.clone(), needs_gpu);
                    metrics.migration_searches.fetch_add(1, Ordering::Relaxed);
                    migration_spawned = true;
                }
            }
            _ = balloon_ticker.tick(), if cfg.migration_enabled && cfg.balloon_enabled && !migration_spawned => {
                if let Some(current) = balloon_current_mib.as_ref() {
                    let current_mib = current.load(Ordering::Relaxed);
                    if current_mib >= balloon_migration_threshold_mib {
                        info!(
                            vm_id = cfg.vm_id,
                            current_mib,
                            threshold_mib = balloon_migration_threshold_mib,
                            max_mib = cfg.region_mib,
                            "balloon proche du plafond — déclenchement recherche migration"
                        );
                        spawn_migration_daemon(region, cluster, cfg, shutdown_flag, uffd_fd, cpu_pressure.clone(), needs_gpu);
                        metrics.migration_searches.fetch_add(1, Ordering::Relaxed);
                        migration_spawned = true;
                    }
                }
            }
            _ = recall_ticker.tick(), if recall_enabled => {
                let avail_mib = local_available_mib();
                if avail_mib > cfg.recall_threshold_mib && region.remote_count() > 0 {
                    // Point 4 : délai de priorité avant le recall
                    if !recall_priority_delay.is_zero() {
                        tokio::time::sleep(recall_priority_delay).await;
                    }
                    match region.recall_n_pages(fair_recall_batch, uffd_fd) {
                        Ok(n) if n > 0 => info!(
                            recalled    = n,
                            avail_mib,
                            batch_share = fair_recall_batch,
                            priority    = cfg.recall_priority,
                            "recall LIFO effectué"
                        ),
                        Ok(_)  => {}
                        Err(e) => warn!(error = %e, "recall LIFO échoué"),
                    }
                }
            }
            // Point 6 : compaction globale si beaucoup d'alertes accumulées
            _ = compact_ticker.tick(), if cfg.compaction_enabled && consecutive_alerts >= 3 => {
                info!(consecutive_alerts, "déclenchement compaction globale du cluster");
                let compactor = ClusterCompactor::new(
                    cluster.clone(),
                    cfg.current_node.clone(),
                    false, // dry_run = false
                );
                match compactor.compact().await {
                    Ok(n) => {
                        info!(vms_moved = n, "compaction globale terminée");
                        if n > 0 {
                            consecutive_alerts = 0; // la situation s'est améliorée
                            // Relancer la recherche de migration maintenant que des nœuds sont libérés
                            if cfg.migration_enabled {
                                spawn_migration_daemon(region, cluster, cfg, shutdown_flag, uffd_fd, cpu_pressure.clone(), needs_gpu);
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "compaction globale échouée"),
                }
            }
        }
    }

    // La VM s'éteint : la RAM est volatile, supprimer toutes les pages distantes.
    // spawn_blocking évite d'appeler block_on depuis un contexte async.
    info!("arrêt — purge des pages distantes sur les stores");
    let region_purge = region.clone();
    let _ = tokio::task::spawn_blocking(move || region_purge.purge_remote_pages()).await;

    shutdown_flag.store(true, Ordering::Relaxed);
}

// ─── Éviction batch ───────────────────────────────────────────────────────────

/// Retourne `true` si la VM souffre (aucun store disponible ou cap atteint).
async fn evict_batch(
    region: &Arc<MemoryRegion>,
    cluster: &Arc<ClusterState>,
    metrics: &Arc<AgentMetrics>,
    cfg: &Config,
) -> bool {
    let targets = cluster.select_eviction_targets().await;
    if targets.is_empty() {
        error!(vm_id = cfg.vm_id, "ALERTE : aucun nœud store disponible");
        metrics.eviction_alerts.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    // Vérification disque : exclure les stores avec < 5 % d'espace libre.
    // Le cluster.rs logue déjà une alerte ; ici on filtre les cibles.
    let snapshots = cluster.snapshot().await;
    for snap in &snapshots {
        if let Some(s) = &snap.last_status {
            if s.disk_total_mib > 0 {
                let pct = s.disk_available_mib * 100 / s.disk_total_mib;
                if pct < 5 {
                    warn!(
                        store_idx = snap.store_idx,
                        disk_avail_mib = s.disk_available_mib,
                        disk_pct = pct,
                        "store exclu de l'éviction : disque presque plein"
                    );
                }
            }
        }
    }

    // Ne pas évincer vers des stores dont le disque est critique (< 5 %).
    let targets: Vec<_> = targets
        .into_iter()
        .filter(|(idx, _)| {
            snapshots
                .iter()
                .find(|n| n.store_idx == *idx)
                .and_then(|n| n.last_status.as_ref())
                .map(|s| {
                    s.disk_total_mib == 0 || // RAM pure (pas de disque à surveiller)
                s.disk_available_mib * 100 / s.disk_total_mib >= 5
                })
                .unwrap_or(true)
        })
        .collect();

    let cold_pages = region.select_cold_pages(cfg.eviction_batch_size);
    if cold_pages.is_empty() {
        debug!("aucune page froide à évincer");
        return false;
    }

    let assignments = assign_pages_to_stores(&cold_pages, &targets);
    let avail_before = local_available_mib();
    let mut evicted = 0usize;
    let mut failed = 0usize;
    let mut cap_hit = false;

    // Stores encore opérationnels pour ce batch ; on les retire au fur et à mesure
    // qu'ils refusent une page (store plein ou inaccessible).
    let mut available: Vec<(usize, usize)> = targets.clone();

    'pages: for (page_id, preferred_store) in assignments {
        if available.is_empty() {
            warn!("tous les stores épuisés — batch interrompu");
            failed += 1;
            break 'pages;
        }

        // Ordre d'essai : store préféré d'abord, puis les autres disponibles.
        let store_order: Vec<usize> = std::iter::once(preferred_store)
            .chain(
                available
                    .iter()
                    .map(|(i, _)| *i)
                    .filter(|&i| i != preferred_store),
            )
            .collect();

        let mut placed = false;
        for store_idx in store_order {
            // Vérifier que ce store n'a pas été retiré par une itération précédente.
            if !available.iter().any(|(i, _)| *i == store_idx) {
                continue;
            }

            match region.evict_page_to(page_id, store_idx) {
                Ok(()) => {
                    evicted += 1;
                    placed = true;
                    break;
                }
                Err(e) if e.to_string().contains("cap vm_requested") => {
                    cap_hit = true;
                    debug!(page_id, "cap vm_requested atteint — éviction stoppée");
                    break 'pages;
                }
                Err(e) => {
                    warn!(
                        page_id,
                        store_idx,
                        error = %e,
                        "store indisponible — rerouting vers prochain"
                    );
                    available.retain(|(i, _)| *i != store_idx);
                }
            }
        }
        if !placed {
            failed += 1;
        }
    }

    let avail_after = local_available_mib();
    info!(
        evicted,
        failed,
        cap_hit,
        avail_before_mib = avail_before,
        avail_after_mib = avail_after,
        threshold_mib = cfg.eviction_threshold_mib,
        "éviction batch terminée"
    );

    failed > 0 || cap_hit
}

/// Répartit les pages sur les stores (greedy, proportiellement à leur capacité).
fn assign_pages_to_stores(pages: &[u64], targets: &[(usize, usize)]) -> Vec<(u64, usize)> {
    if targets.is_empty() || pages.is_empty() {
        return Vec::new();
    }

    // Capacité totale
    let total_cap: usize = targets.iter().map(|(_, c)| *c).sum();
    let mut result = Vec::with_capacity(pages.len());
    let mut counts = vec![0usize; targets.len()];

    for (i, &page_id) in pages.iter().enumerate() {
        // Choisir le store avec la plus grande "dette" (capacité non encore utilisée)
        let idx = if total_cap == 0 {
            i % targets.len()
        } else {
            targets
                .iter()
                .enumerate()
                .max_by_key(|(j, (_, cap))| {
                    // Score : capacité restante proportionnelle
                    cap.saturating_sub(counts[*j])
                })
                .map(|(j, _)| j)
                .unwrap_or(0)
        };
        counts[idx] += 1;
        result.push((page_id, targets[idx].0));
    }

    result
}

fn spawn_migration_daemon(
    region: &Arc<MemoryRegion>,
    cluster: &Arc<ClusterState>,
    cfg: &Config,
    shutdown_flag: &Arc<AtomicBool>,
    uffd_fd: std::os::unix::io::RawFd,
    cpu_pressure: Arc<AtomicBool>,
    needs_gpu: bool,
) {
    let agent = Arc::new(MigrationAgent::new(
        cfg.vm_id,
        cfg.current_node.clone(),
        cluster.clone(),
        region.clone(),
        cfg.migration_interval_secs,
        cfg.compaction_enabled,
        cfg.vm_vcpus,
        cfg.vm_requested_mib as u64,
        needs_gpu,
        uffd_fd,
        cpu_pressure,
    ));
    let shutdown = shutdown_flag.clone();
    info!(vm_id = cfg.vm_id, "spawning démon migration");
    tokio::spawn(async move { agent.run(shutdown).await });
}

// ─── Mode demo ────────────────────────────────────────────────────────────────

async fn run_demo(
    region: &Arc<MemoryRegion>,
    cfg: &Config,
    metrics: &Arc<AgentMetrics>,
    _store: &Arc<RemoteStorePool>,
    uffd_fd: std::os::unix::io::RawFd,
) -> Result<()> {
    let demo_pages = region.num_pages.min(64);
    info!(demo_pages, "=== début du scénario de démonstration ===");

    // 1. Écriture
    info!("étape 1/5 : écriture des valeurs initiales dans {demo_pages} pages");
    for page_id in 0..demo_pages as u64 {
        let mut data = [0u8; PAGE_SIZE];
        data[..8].copy_from_slice(&page_id.to_be_bytes());
        for i in 8..PAGE_SIZE {
            data[i] = ((page_id as u8).wrapping_add(i as u8)) & 0xFF;
        }
        region.write_page_local(page_id, &data)?;
    }
    info!("étape 1/5 : ok");

    // 2. Éviction des pages paires
    info!("étape 2/5 : éviction des pages paires");
    let t0 = Instant::now();
    let mut evicted = 0u64;
    for page_id in (0..demo_pages as u64).step_by(2) {
        let store_idx = (page_id / 2) as usize % cfg.stores.len();
        region.evict_page_to(page_id, store_idx)?;
        evicted += 1;
    }
    info!(
        evicted,
        elapsed_ms = t0.elapsed().as_millis(),
        "étape 2/5 : ok"
    );

    // 3. Lecture + vérification d'intégrité
    info!("étape 3/5 : lecture + vérification d'intégrité");
    let t1 = Instant::now();
    let mut errors = 0u32;
    for page_id in 0..demo_pages as u64 {
        let ptr = unsafe { (region.base_ptr() as *const u8).add(page_id as usize * PAGE_SIZE) };
        let read_id = unsafe { u64::from_be_bytes(*(ptr as *const [u8; 8])) };
        if read_id != page_id {
            error!(
                page_id,
                got = read_id,
                "ERREUR INTÉGRITÉ : page_id incorrect"
            );
            errors += 1;
        }
        let ok = (8..24usize).all(|i| {
            let expected = ((page_id as u8).wrapping_add(i as u8)) & 0xFF;
            let got = unsafe { *ptr.add(i) };
            got == expected
        });
        if !ok {
            error!(page_id, "ERREUR INTÉGRITÉ : motif incorrect");
            errors += 1;
        }
    }
    info!(
        pages = demo_pages,
        elapsed_ms = t1.elapsed().as_millis(),
        errors,
        "étape 3/5 : ok"
    );

    // 4. Recall LIFO
    info!("étape 4/5 : recall LIFO des pages évinvées");
    let t2 = Instant::now();
    let recalled = region.recall_n_pages(evicted as usize, uffd_fd)?;
    info!(
        recalled,
        elapsed_ms = t2.elapsed().as_millis(),
        "étape 4/5 : ok"
    );

    // 5. Résultats
    let snap = metrics.snapshot();
    info!(
        pages_evicted = snap.pages_evicted,
        pages_recalled = snap.pages_recalled,
        faults_caught = snap.fault_count,
        faults_served = snap.fault_served,
        fetch_zeros = snap.fetch_zeros,
        integrity_ok = errors == 0,
        "étape 5/5 : résultats"
    );

    if errors > 0 {
        bail!("ÉCHEC : {errors} erreur(s) d'intégrité");
    }
    info!("SUCCÈS : toutes les pages lues avec intégrité correcte");
    Ok(())
}

// ─── Politiques de recall ─────────────────────────────────────────────────────

/// Délai avant recall selon la priorité de la VM.
///
/// priority=10 (haute) → 0 ms   — rappelle immédiatement
/// priority=1  (basse) → 900 ms — attend que les VMs prioritaires rappellent d'abord
pub fn priority_delay(priority: u32) -> Duration {
    let p = priority.clamp(1, 10);
    Duration::from_millis((10u32.saturating_sub(p)) as u64 * 100)
}

/// Pages rappelées par tick pour cette VM, selon le nombre de VMs sur le nœud.
///
/// Chaque VM reçoit une part égale du budget recall.
/// On garantit au moins 1 page par tick.
pub fn fair_batch(batch_size: usize, vm_count: usize) -> usize {
    (batch_size / vm_count.max(1)).max(1)
}

/// Lit l'adresse PCI configurée dans `hostpci0` pour `vm_id`.
///
/// Retourne `None` si la VM n'a pas de `hostpci0` (passthrough pas encore configuré).
async fn read_vm_hostpci0(vm_id: u32) -> Option<String> {
    let out = tokio::process::Command::new("qm")
        .args(["config", &vm_id.to_string()])
        .output()
        .await
        .ok()?;

    let config = String::from_utf8_lossy(&out.stdout);
    config
        .lines()
        .find(|l| l.starts_with("hostpci0:"))
        .and_then(|l| {
            let val = l["hostpci0:".len()..].trim();
            let pci_id = val.split(',').next()?.trim();
            let full = if pci_id.matches(':').count() == 1 {
                format!("0000:{pci_id}")
            } else {
                pci_id.to_string()
            };
            if full.is_empty() {
                None
            } else {
                Some(full)
            }
        })
}

/// Détecte si la VM `vm_id` a un GPU en passthrough PCI.
///
/// Lit `qm config {vm_id}`, cherche les lignes `hostpciN`, et vérifie
/// la classe PCI dans `/sys/bus/pci/devices/*/class` (préfixe `0x03` = contrôleur d'affichage).
/// Retourne `false` si `qm` n'est pas disponible (hors Proxmox) ou si aucun GPU trouvé.
async fn detect_vm_gpu_passthrough(vm_id: u32) -> bool {
    let out = tokio::process::Command::new("qm")
        .args(["config", &vm_id.to_string()])
        .output()
        .await;

    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };

    let config = String::from_utf8_lossy(&out.stdout);

    for line in config.lines() {
        if !line.starts_with("hostpci") {
            continue;
        }
        // hostpci0: 0000:02:00.0,pcie=1,rombar=0
        let value = match line.splitn(2, ':').nth(1) {
            Some(v) => v.trim(),
            None => continue,
        };
        // Récupérer l'identifiant PCI (avant la première virgule)
        let pci_id = value.split(',').next().unwrap_or("").trim();
        // Normaliser la forme courte "02:00.0" → "0000:02:00.0"
        let full_id = if pci_id.matches(':').count() == 1 {
            format!("0000:{pci_id}")
        } else {
            pci_id.to_string()
        };
        let class_path = format!("/sys/bus/pci/devices/{full_id}/class");
        if let Ok(class_hex) = std::fs::read_to_string(&class_path) {
            let class =
                u32::from_str_radix(class_hex.trim().trim_start_matches("0x"), 16).unwrap_or(0);
            if (class >> 16) == 0x03 {
                info!(vm_id, pci = %full_id, "GPU passthrough détecté via qm config");
                return true;
            }
        }
    }

    false
}

/// Compte les VMs en état "running" sur ce nœud via `qm list`.
///
/// Retourne 1 si `qm` n'est pas disponible (hors Proxmox) ou si aucune VM ne tourne.
fn detect_vm_count() -> usize {
    let output = std::process::Command::new("qm").arg("list").output().ok();
    match output {
        Some(out) if out.status.success() => {
            let count = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| {
                    let cols: Vec<&str> = l.split_whitespace().collect();
                    cols.len() >= 3 && cols[0].parse::<u32>().is_ok() && cols[2] == "running"
                })
                .count();
            count.max(1)
        }
        _ => 1,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── priority_delay ────────────────────────────────────────────────────────

    #[test]
    fn test_priority_10_no_delay() {
        assert_eq!(priority_delay(10), Duration::ZERO);
    }

    #[test]
    fn test_priority_1_max_delay() {
        assert_eq!(priority_delay(1), Duration::from_millis(900));
    }

    #[test]
    fn test_priority_5_mid_delay() {
        assert_eq!(priority_delay(5), Duration::from_millis(500));
    }

    #[test]
    fn test_priority_clamped_above_10() {
        // Valeur hors plage vers le haut → traitée comme 10
        assert_eq!(priority_delay(99), Duration::ZERO);
    }

    #[test]
    fn test_priority_clamped_below_1() {
        // Valeur hors plage vers le bas → traitée comme 1
        assert_eq!(priority_delay(0), Duration::from_millis(900));
    }

    #[test]
    fn test_priority_monotone_decreasing() {
        // Plus la priorité est haute, plus le délai est court
        let delays: Vec<Duration> = (1..=10).map(priority_delay).collect();
        for w in delays.windows(2) {
            assert!(w[0] >= w[1], "délai non monotone : {:?} < {:?}", w[0], w[1]);
        }
    }

    // ── fair_batch ────────────────────────────────────────────────────────────

    #[test]
    fn test_fair_batch_single_vm() {
        assert_eq!(fair_batch(32, 1), 32);
    }

    #[test]
    fn test_fair_batch_four_vms() {
        assert_eq!(fair_batch(32, 4), 8);
    }

    #[test]
    fn test_fair_batch_more_vms_than_batch() {
        // batch=2, vms=10 → 2/10=0, plancher à 1
        assert_eq!(fair_batch(2, 10), 1);
    }

    #[test]
    fn test_fair_batch_zero_vms_clamped() {
        // vm_count=0 → traité comme 1
        assert_eq!(fair_batch(32, 0), 32);
    }

    #[test]
    fn test_fair_batch_decreases_with_more_vms() {
        let b4 = fair_batch(64, 4);
        let b8 = fair_batch(64, 8);
        assert!(b4 > b8, "plus de VMs doit réduire le batch : {b4} vs {b8}");
    }

    // ── assign_pages_to_stores ────────────────────────────────────────────────

    #[test]
    fn test_assign_empty_pages() {
        let result = assign_pages_to_stores(&[], &[(0, 100), (1, 100)]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_assign_single_store() {
        let pages = vec![0u64, 1, 2];
        let result = assign_pages_to_stores(&pages, &[(0, 1000)]);
        assert!(result.iter().all(|(_, s)| *s == 0));
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_assign_proportional_to_capacity() {
        // store 0 : cap=3, store 1 : cap=1 → 3/4 des pages vers store 0
        let pages: Vec<u64> = (0..4).collect();
        let result = assign_pages_to_stores(&pages, &[(0, 3), (1, 1)]);
        let to_0 = result.iter().filter(|(_, s)| *s == 0).count();
        let to_1 = result.iter().filter(|(_, s)| *s == 1).count();
        assert_eq!(to_0, 3, "3 pages vers store 0 (cap=3)");
        assert_eq!(to_1, 1, "1 page vers store 1 (cap=1)");
    }

    #[test]
    fn test_assign_equal_capacity_spreads_evenly() {
        let pages: Vec<u64> = (0..4).collect();
        let result = assign_pages_to_stores(&pages, &[(0, 2), (1, 2)]);
        let to_0 = result.iter().filter(|(_, s)| *s == 0).count();
        let to_1 = result.iter().filter(|(_, s)| *s == 1).count();
        assert_eq!(to_0, 2);
        assert_eq!(to_1, 2);
    }
}
