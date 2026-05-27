//! omega-daemon V4 — daemon unifié multi-rôle pour cluster Proxmox.
//!
//! # Rôles simultanés
//!
//!   1. **Store TCP**    (port 9100) → accepte pages distantes de tout nœud
//!   2. **API HTTP**     (port 9200) → état nœud exposé au cluster
//!   3. **Control HTTP** (port 9300) → canal contrôle controller↔agent (fix L2)
//!   4. **VM Monitor**               → VMs QEMU locales via filesystem Proxmox
//!   5. **Eviction Engine**          → CLOCK + balloon trigger (fix L1, feat V4)
//!   6. **Balloon Monitor**          → stats virtio-balloon via QMP (V4)
//!
//! # Démarrage
//!
//! ```bash
//! omega-daemon \
//!     --node-id pve-node1 \
//!     --node-addr 192.168.1.1 \
//!     --peers 192.168.1.2:9200,192.168.1.3:9200
//! ```

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

extern crate num_cpus;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use node_bc_store::metrics::StoreMetrics;
use node_bc_store::store::PageStore;

use omega_daemon::balloon::{BalloonManager, BalloonMonitor, BalloonStats};
use omega_daemon::cgroup_cpu_monitor::{CgroupCpuMonitor, MonitorConfig};
use omega_daemon::cluster_api::run_api_server;
use omega_daemon::config::Config;
use omega_daemon::control_api::build_control_router;
use omega_daemon::cpu_cgroup::{CgroupCpuController, VmCpuConfig, VmCpuStat};
use omega_daemon::disk_io_scheduler::{BOOSTED_IO_WEIGHT, DEFAULT_IO_WEIGHT, DONOR_IO_WEIGHT};
use omega_daemon::eviction_engine::EvictionEngine;
use omega_daemon::fault_bus::{FaultBus, FaultBusConsumer};
use omega_daemon::gpu_drm_backend::DrmGpuBackend;
use omega_daemon::gpu_multiplexer::{GpuBackend, GpuMultiplexer};
use omega_daemon::gpu_runtime::GpuRuntime;
use omega_daemon::io_cgroup::{CgroupIoController, VmDiskConfig, VmDiskStat};
use omega_daemon::node_state::NodeState;
use omega_daemon::qmp_vcpu::{HotplugResult, VcpuHotplugManager};
use omega_daemon::store_server::run_store_server;
use omega_daemon::tls::{TlsContext, TlsPaths};
use omega_daemon::vm_tracker::VmTracker;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::parse();
    setup_logging(&cfg);

    info!(
        version      = env!("CARGO_PKG_VERSION"),
        node_id      = %cfg.node_id,
        addr         = %cfg.node_addr,
        store_port   = cfg.store_port,
        api_port     = cfg.api_port,
        peers        = cfg.peers.len(),
        monitor_vms  = cfg.monitor_vms,
        "omega-daemon V4 démarré"
    );

    // ─── Composants partagés ──────────────────────────────────────────────

    let metrics = Arc::new(StoreMetrics::default());
    let store = Arc::new(PageStore::new(metrics.clone()));
    let vm_tracker = Arc::new(VmTracker::new(
        cfg.qemu_pid_dir.clone(),
        cfg.qemu_conf_dir.clone(),
    ));
    let num_pcpus = num_cpus::get();
    let gpu_runtime = initialize_gpu_runtime(&cfg).await;
    let node_state = Arc::new(NodeState::new(
        cfg.node_id.clone(),
        cfg.store_public_addr(),
        cfg.api_public_addr(),
        store.clone(),
        metrics.clone(),
        vm_tracker.clone(),
        num_pcpus,
        cfg.qemu_pid_dir.clone(),
        gpu_runtime.clone(),
    ));

    // ─── Tâche 1 : Store TCP ──────────────────────────────────────────────
    {
        let store = store.clone();
        let metrics = metrics.clone();
        let vm_tracker = vm_tracker.clone();
        let listen = cfg.store_addr();
        let max_pages = cfg.max_pages;
        let node_id = cfg.node_id.clone();

        tokio::spawn(async move {
            if let Err(e) =
                run_store_server(listen, store, metrics, vm_tracker, max_pages, node_id).await
            {
                error!(error = %e, "store TCP terminé avec erreur");
            }
        });
    }

    // ─── Tâche 2 : API HTTP cluster (/api/*) ─────────────────────────────
    {
        let state = node_state.clone();
        let api_addr = cfg.api_addr();

        tokio::spawn(async move {
            if let Err(e) = run_api_server(state, api_addr).await {
                error!(error = %e, "API cluster HTTP terminée avec erreur");
            }
        });
    }

    // ─── Tâche 3 : Canal de contrôle HTTP (/control/*) — fix L2 ──────────
    {
        let state = node_state.clone();
        let control_addr = format!("0.0.0.0:{}", cfg.api_port + 100); // port 9300

        tokio::spawn(async move {
            let app = build_control_router(state);
            let Ok(listener) = tokio::net::TcpListener::bind(&control_addr).await else {
                error!(addr = %control_addr, "impossible de démarrer le canal contrôle");
                return;
            };
            info!(addr = %control_addr, "canal de contrôle HTTP démarré");
            let _ = axum::serve(listener, app).await;
        });
    }

    // ─── Tâche 4 : Découverte des VMs locales (périodique) ───────────────
    if cfg.monitor_vms {
        let tracker = vm_tracker.clone();
        let interval = Duration::from_secs(15);

        tokio::spawn(async move {
            info!("monitoring VMs locales actif (intervalle 15s)");
            loop {
                if let Err(e) = tracker.refresh_local_vms() {
                    warn!(error = %e, "refresh VMs échoué");
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // ─── Tâche 4b.0 : Monitor CPU haute fréquence 1 ms ──────────────────
    //
    // Le CgroupCpuMonitor lit cpu.stat toutes les 1 ms en Rust (sans GIL).
    // Il émet des CpuPressureEvent quand avg_usage >= 80% ou throttle >= 10%.
    // La boucle 4b consomme ces événements pour déclencher hotplug immédiatement,
    // sans attendre l'intervalle de polling (100ms+).
    let mut cpu_pressure_rx = if cfg.monitor_vms {
        let (monitor, rx) = CgroupCpuMonitor::new(MonitorConfig::default());
        monitor.spawn();
        Some(rx)
    } else {
        None
    };

    // ─── Tâche 4b : Monitoring CPU local + hotplug réel ─────────────────
    if cfg.monitor_vms {
        let state = node_state.clone();
        let tracker = vm_tracker.clone();
        let interval = Duration::from_millis(cfg.cpu_monitor_interval_ms.max(100));
        let qmp_dir = cfg.qemu_pid_dir.clone();
        let qemu_conf_dir = cfg.qemu_conf_dir.clone();
        // Ownership du Receiver 1ms transféré dans la tâche
        let mut cpu_pressure_rx = cpu_pressure_rx.take();

        tokio::spawn(async move {
            let cpu_ctrl = CgroupCpuController::new();
            let io_ctrl = CgroupIoController::new();
            let hotplug = VcpuHotplugManager::new(qmp_dir);
            let mut previous: HashMap<u32, (VmCpuStat, Instant)> = HashMap::new();
            let mut previous_io: HashMap<u32, (VmDiskStat, Instant)> = HashMap::new();

            info!(
                interval_ms = interval.as_millis() as u64,
                "monitoring CPU local actif"
            );

            loop {
                // Drainer les événements haute fréquence du monitor 1 ms.
                // Chaque CpuPressureEvent signale une VM sous pression —
                // on met à jour ses métriques et on tente un hotplug immédiat,
                // sans attendre la prochaine itération du polling 100ms.
                if let Some(rx) = cpu_pressure_rx.as_mut() {
                    while let Ok(ev) = rx.try_recv() {
                        state.vcpu_scheduler.update_from_cgroup(
                            ev.vm_id,
                            ev.avg_usage_pct,
                            ev.avg_throttle,
                        );
                        // Tentative hotplug immédiate si pression détectée
                        if ev.avg_usage_pct >= omega_daemon::cgroup_cpu_monitor::USAGE_THRESHOLD
                            || ev.avg_throttle
                                >= omega_daemon::cgroup_cpu_monitor::THROTTLE_THRESHOLD
                        {
                            apply_hotplug_if_possible(&state, &hotplug, &cpu_ctrl, ev.vm_id);
                        }
                    }
                }

                let local_vms = tracker.local_running_vms_snapshot();
                let local_ids: HashSet<u32> = local_vms.iter().map(|vm| vm.vmid).collect();

                for vm_state in state.vcpu_scheduler.vm_snapshot() {
                    if !local_ids.contains(&vm_state.vm_id) {
                        previous.remove(&vm_state.vm_id);
                        previous_io.remove(&vm_state.vm_id);
                        state.vcpu_scheduler.release_vm(vm_state.vm_id);
                        state.disk_io_scheduler.release_vm(vm_state.vm_id);
                        state.quota_registry.record_delete_vm(vm_state.vm_id);
                        state.quota_registry.remove(vm_state.vm_id);
                        if let Some(gpu) = &state.gpu_runtime {
                            gpu.release_vm(vm_state.vm_id).await;
                        }
                        info!(
                            vm_id = vm_state.vm_id,
                            "VM non running localement — ressources CPU/RAM/GPU/I/O nettoyées"
                        );
                    }
                }

                state
                    .disk_io_scheduler
                    .set_node_pressure_pct(io_ctrl.read_node_pressure_pct());

                for vm in &local_vms {
                    ensure_vm_registered(&state, &hotplug, &cpu_ctrl, &qemu_conf_dir, vm.vmid);
                    state.disk_io_scheduler.ensure_vm(vm.vmid);

                    let now = Instant::now();
                    if let Some(stat) = cpu_ctrl.read_cpu_stat(vm.vmid) {
                        if let Some((before, started_at)) = previous.get(&vm.vmid) {
                            let elapsed = started_at.elapsed().as_micros() as u64;
                            let usage_pct =
                                CgroupCpuController::compute_usage_pct(before, &stat, elapsed);
                            state.vcpu_scheduler.update_from_cgroup(
                                vm.vmid,
                                usage_pct,
                                stat.throttle_ratio(),
                            );
                        }
                        previous.insert(vm.vmid, (stat, now));
                    }

                    let now_io = Instant::now();
                    if let Some(stat) = io_ctrl.read_io_stat(vm.vmid) {
                        if let Some((before, started_at)) = previous_io.get(&vm.vmid) {
                            let elapsed = started_at.elapsed().as_micros() as u64;
                            let (read_bps, write_bps) =
                                CgroupIoController::compute_bps(before, &stat, elapsed);
                            state
                                .disk_io_scheduler
                                .update_vm_io(vm.vmid, read_bps, write_bps);
                        } else {
                            state.disk_io_scheduler.update_vm_io(vm.vmid, 0.0, 0.0);
                        }
                        previous_io.insert(vm.vmid, (stat, now_io));
                    }
                }

                for vm_id in state.vcpu_scheduler.vms_needing_hotplug() {
                    apply_hotplug_if_possible(&state, &hotplug, &cpu_ctrl, vm_id);
                }

                for vm_id in state.vcpu_scheduler.vms_needing_downscale() {
                    apply_downscale_if_idle(&state, &hotplug, &cpu_ctrl, vm_id, false);
                }

                reconcile_local_cpu_sharing(&state, &hotplug, &cpu_ctrl);
                reconcile_local_disk_sharing(&state, &io_ctrl);

                for (vm_id, reason) in state.vcpu_scheduler.vms_needing_migration() {
                    warn!(vm_id, reason = %reason, "VM candidate à la migration CPU");
                }

                tokio::time::sleep(interval).await;
            }
        });
    }

    // ─── Tâche 5 : Moteur d'éviction CLOCK + FaultBus (L1) ───────────────
    {
        // Créer le FaultBus — les handlers uffd posteront leurs événements ici
        let mut fault_bus = FaultBus::new(4096);

        // Publier l'émetteur dans NodeState pour que les handlers uffd y accèdent
        // (en V5 : stocké dans NodeState ; en V4 : exposé via variable statique)
        let _fault_sender = fault_bus.sender(); // utilisé par node-a-agent

        let consumer = FaultBusConsumer::new(
            fault_bus.take_receiver().unwrap(),
            10,  // fenêtre 10 secondes
            5.0, // accélération si > 5 fautes/sec
        );

        let engine = EvictionEngine::new(node_state.clone(), &cfg).with_fault_bus(consumer);

        tokio::spawn(engine.run());
    }

    // ─── Initialisation TLS (L3) ─────────────────────────────────────────
    {
        let tls_paths = TlsPaths::new("/etc/omega-store/tls");
        match TlsContext::generate_or_load(tls_paths, &cfg.node_id) {
            Ok(ctx) => {
                info!(
                    fingerprint = %ctx.fingerprint,
                    "TLS initialisé — empreinte à distribuer aux pairs pour TOFU"
                );
            }
            Err(e) => {
                warn!(error = %e, "TLS non disponible — canal de paging non chiffré");
            }
        }
    }

    // ─── Tâche 6 : Balloon Monitor — V4 ──────────────────────────────────
    if cfg.monitor_vms {
        let _qmp_dir = cfg.qemu_pid_dir.replace("qemu-server", "qemu-server"); // même dossier
        let vm_tracker_ref = vm_tracker.clone();
        let _threshold = cfg.evict_threshold_pct;
        let balloon_state = node_state.clone();

        // Le monitor balloon tourne dans un thread dédié (read QMP = bloquant)
        tokio::task::spawn_blocking(move || {
            // Découverte initiale des vmids locaux
            let vmids: Vec<u32> = vm_tracker_ref
                .local_vms_snapshot()
                .iter()
                .map(|vm| vm.vmid)
                .collect();

            if vmids.is_empty() {
                info!("aucune VM locale — balloon monitor en attente");
                // On laisse le thread tourner, il se relancera quand des VMs apparaissent
                std::thread::sleep(Duration::from_secs(60));
                return;
            }

            let monitor = BalloonMonitor::new(
                // On utilise le répertoire QMP standard Proxmox
                "/var/run/qemu-server".to_string(),
                15,   // poll toutes les 15s
                20.0, // alerte si RAM libre guest < 20%
                move |vmid: u32, stats: BalloonStats| {
                    warn!(
                        vmid,
                        free_pct = format!("{:.1}%", stats.free_pct()),
                        total_mib = stats.total_bytes / 1024 / 1024,
                        major_faults = stats.major_faults,
                        "BALLOON : pression mémoire guest → éviction accélérée recommandée"
                    );
                    // En V4 : le controller lira cet état via /api/status et décidera
                    // En V5 : déclencher directement l'éviction CLOCK
                },
            )
            .with_sample_hook({
                let state = balloon_state.clone();
                move |vmid: u32, stats: BalloonStats| {
                    state
                        .quota_registry
                        .apply_balloon_update(vmid, stats.actual_bytes / 1024 / 1024);
                }
            });
            monitor.run_blocking(vmids);
        });
    }

    // ─── Tâche 8 : Balloon Manager proactif — V5 ─────────────────────────
    // Gonfle le balloon des guests sur-provisionnés pour récupérer la RAM inutilisée,
    // et le dégonfle dès qu'un guest est sous pression (scénario S5).
    if cfg.monitor_vms {
        let vm_tracker_ref = vm_tracker.clone();
        let quota_ref = node_state.quota_registry.clone();

        tokio::task::spawn_blocking(move || {
            let manager = BalloonManager::new(
                "/var/run/qemu-server".to_string(),
                30, // réconciliation toutes les 30s
            );

            manager.run_blocking(
                move || {
                    vm_tracker_ref
                        .local_vms_snapshot()
                        .iter()
                        .map(|vm| (vm.vmid, vm.max_mem_mib))
                        .collect()
                },
                move |vmid, new_actual_mib| {
                    quota_ref.apply_balloon_update(vmid, new_actual_mib);
                },
            );
        });
    }

    // ─── Tâche 7 : Stats périodiques ─────────────────────────────────────
    {
        let state = node_state.clone();
        let interval = cfg.stats_interval;
        let node_id = cfg.node_id.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(interval));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let snap = state.snapshot();
                info!(
                    node_id           = %node_id,
                    mem_usage_pct     = format!("{:.1}%", snap.mem_usage_pct),
                    mem_available_mib = snap.mem_available_kb / 1024,
                    pages_stored      = snap.pages_stored,
                    store_used_mib    = snap.store_used_kb / 1024,
                    local_vms         = snap.local_vms.len(),
                    "stats périodiques"
                );
                for vm in &snap.local_vms {
                    if vm.remote_pages > 0 {
                        warn!(
                            vmid = vm.vmid,
                            remote_pages = vm.remote_pages,
                            remote_mib = vm.remote_mem_mib,
                            "VM avec pages distantes → candidat migration"
                        );
                    }
                }
            }
        });
    }

    // ─── Attente SIGTERM / SIGINT ─────────────────────────────────────────
    info!("daemon opérationnel — CTRL+C pour arrêter");
    tokio::signal::ctrl_c().await?;
    info!("signal d'arrêt reçu");
    Ok(())
}

fn setup_logging(cfg: &Config) {
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
}

async fn initialize_gpu_runtime(cfg: &Config) -> Option<Arc<GpuRuntime>> {
    if !cfg.gpu_enabled {
        info!("GPU désactivé par configuration");
        return None;
    }

    let backend_result = if cfg.gpu_render_node.trim().is_empty() {
        DrmGpuBackend::open_default()
    } else {
        DrmGpuBackend::open(std::path::Path::new(&cfg.gpu_render_node)).map(Arc::new)
    };

    let backend = match backend_result {
        Ok(backend) => backend,
        Err(e) => {
            warn!(error = %e, "GPU indisponible — multiplexeur désactivé");
            return None;
        }
    };

    let total_vram_mib = if cfg.gpu_total_vram_mib > 0 {
        cfg.gpu_total_vram_mib
    } else {
        backend.total_vram_mib()
    };

    let render_node = Some(backend.render_node().display().to_string());
    let backend_name = backend.name().to_string();
    let backend_trait: Arc<dyn GpuBackend> = backend;
    let mux = Arc::new(GpuMultiplexer::new(
        std::path::PathBuf::from(&cfg.gpu_socket_path),
        backend_trait,
    ));
    let runtime = Arc::new(GpuRuntime::new(
        Arc::clone(&mux),
        backend_name,
        render_node,
        cfg.gpu_socket_path.clone(),
        total_vram_mib,
    ));

    tokio::spawn({
        let mux = Arc::clone(&mux);
        async move {
            if let Err(e) = mux.run().await {
                error!(error = %e, "multiplexeur GPU terminé avec erreur");
            }
        }
    });

    info!(
        socket = %cfg.gpu_socket_path,
        total_vram_mib,
        "multiplexeur GPU initialisé"
    );

    Some(runtime)
}

fn ensure_vm_registered(
    state: &Arc<NodeState>,
    hotplug: &VcpuHotplugManager,
    cpu_ctrl: &CgroupCpuController,
    conf_dir: &str,
    vm_id: u32,
) {
    if state.vcpu_scheduler.has_vm(vm_id) {
        return;
    }

    let qmp_vcpus = hotplug.vcpu_info(vm_id);
    let online_vcpus = qmp_vcpus
        .as_ref()
        .map(|info| info.online_count.max(1))
        .unwrap_or(1);

    let (conf_min_vcpus, conf_max_vcpus) = read_vm_cpu_profile(conf_dir, vm_id).unwrap_or((1, 1));
    let max_vcpus = qmp_vcpus
        .map(|info| info.total_count)
        .unwrap_or(conf_max_vcpus);
    let (min_vcpus, current_vcpus, max_vcpus) =
        derive_runtime_vcpu_profile(conf_min_vcpus, conf_max_vcpus, online_vcpus, max_vcpus);

    match state
        .vcpu_scheduler
        .admit_vm(vm_id, current_vcpus, max_vcpus)
    {
        omega_daemon::vcpu_scheduler::VcpuDecision::Allocated { .. } => {
            if let Err(reason) = state
                .vcpu_scheduler
                .update_profile(vm_id, min_vcpus, max_vcpus)
            {
                warn!(
                    vm_id,
                    min_vcpus,
                    max_vcpus,
                    current_vcpus,
                    reason,
                    "échec normalisation du profil vCPU après auto-enregistrement"
                );
            }
            let _ = apply_cpu_envelope(state, cpu_ctrl, vm_id, current_vcpus);
            info!(
                vm_id,
                min_vcpus,
                max_vcpus,
                current_vcpus,
                "VM enregistrée automatiquement dans le scheduler vCPU"
            );
        }
        omega_daemon::vcpu_scheduler::VcpuDecision::MigrateRequired { reason, .. } => {
            warn!(
                vm_id,
                reason, "VM locale non enregistrée dans le scheduler vCPU faute de capacité"
            );
        }
        _ => {}
    }
}

fn derive_runtime_vcpu_profile(
    conf_min_vcpus: usize,
    conf_max_vcpus: usize,
    online_vcpus: usize,
    qmp_max_vcpus: usize,
) -> (usize, usize, usize) {
    let current_vcpus = online_vcpus.max(1);
    let max_vcpus = qmp_max_vcpus.max(conf_max_vcpus).max(current_vcpus);
    let min_vcpus = conf_min_vcpus.max(1).min(current_vcpus);
    (min_vcpus, current_vcpus, max_vcpus)
}

fn read_vm_cpu_profile(conf_dir: &str, vm_id: u32) -> Option<(usize, usize)> {
    let direct_conf = format!("{}/{}.conf", conf_dir, vm_id);
    if let Ok(content) = fs::read_to_string(&direct_conf) {
        if let Some(profile) = parse_vm_cpu_profile(&content) {
            return Some(profile);
        }
    }

    // Certaines installations exposent les confs locales sous /etc/pve/nodes/<node>/qemu-server.
    if conf_dir.ends_with("/qemu-server") {
        if let Some(nodes_root) = conf_dir.strip_suffix("/qemu-server") {
            let nodes_dir = format!("{}/nodes", nodes_root);
            if let Ok(entries) = fs::read_dir(nodes_dir) {
                for entry in entries.flatten() {
                    let candidate = entry
                        .path()
                        .join("qemu-server")
                        .join(format!("{vm_id}.conf"));
                    if let Ok(content) = fs::read_to_string(candidate) {
                        if let Some(profile) = parse_vm_cpu_profile(&content) {
                            return Some(profile);
                        }
                    }
                }
            }
        }
    }

    read_vm_cpu_profile_from_qm(vm_id)
}

fn parse_vm_cpu_profile(content: &str) -> Option<(usize, usize)> {
    let mut sockets = 1usize;
    let mut cores = 1usize;
    let mut boot_vcpus: Option<usize> = None;

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("sockets:") {
            sockets = rest
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|v| *v > 0)
                .unwrap_or(1);
        } else if let Some(rest) = line.strip_prefix("cores:") {
            cores = rest
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|v| *v > 0)
                .unwrap_or(1);
        } else if let Some(rest) = line.strip_prefix("vcpus:") {
            boot_vcpus = rest.trim().parse::<usize>().ok().filter(|v| *v > 0);
        }
    }

    let max_vcpus = sockets.saturating_mul(cores).max(1);
    let min_vcpus = boot_vcpus.unwrap_or(max_vcpus).clamp(1, max_vcpus);
    Some((min_vcpus, max_vcpus))
}

fn read_vm_cpu_profile_from_qm(vm_id: u32) -> Option<(usize, usize)> {
    let output = std::process::Command::new("qm")
        .arg("config")
        .arg(vm_id.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let content = String::from_utf8(output.stdout).ok()?;
    parse_vm_cpu_profile(&content)
}

fn apply_cpu_envelope(
    state: &Arc<NodeState>,
    cpu_ctrl: &CgroupCpuController,
    vm_id: u32,
    vcpu_cap: usize,
) -> Result<()> {
    let weight = state
        .vcpu_scheduler
        .get_vm_state(vm_id)
        .map(|vm| vm.cpu_weight)
        .unwrap_or(omega_daemon::vcpu_scheduler::DEFAULT_CPU_WEIGHT);

    cpu_ctrl.apply(
        &VmCpuConfig::new(vm_id)
            .capped_at_vcpus(vcpu_cap)
            .with_weight(weight),
    )
}

fn apply_hotplug_if_possible(
    state: &Arc<NodeState>,
    hotplug: &VcpuHotplugManager,
    cpu_ctrl: &CgroupCpuController,
    vm_id: u32,
) -> omega_daemon::vcpu_scheduler::VcpuDecision {
    use omega_daemon::vcpu_scheduler::VcpuDecision;

    let decision = state.vcpu_scheduler.try_hotplug(vm_id);
    let VcpuDecision::Hotplugged {
        new_count, slot, ..
    } = decision
    else {
        return decision;
    };

    let Some(vm_state) = state.vcpu_scheduler.get_vm_state(vm_id) else {
        return VcpuDecision::MigrateRequired {
            vm_id,
            reason: "état vCPU introuvable après réservation".into(),
        };
    };

    match hotplug.add_vcpu(vm_id, vm_state.min_vcpus, vm_state.max_vcpus) {
        HotplugResult::Added {
            new_count: qmp_count,
        } => {
            if let Err(e) = apply_cpu_envelope(state, cpu_ctrl, vm_id, qmp_count) {
                warn!(vm_id, error = %e, "échec mise à jour cpu.max après hotplug");
            }
            info!(
                vm_id,
                scheduler_count = new_count,
                qmp_count,
                "hotplug vCPU appliqué automatiquement"
            );
            VcpuDecision::Hotplugged {
                vm_id,
                new_count,
                slot,
            }
        }
        HotplugResult::NoSlots { current, max } => {
            state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
            warn!(
                vm_id,
                current, max, "hotplug QMP impossible — rollback scheduler, migration nécessaire"
            );
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: format!(
                    "aucun slot CPU hotpluggable hors-ligne : current={} max={}",
                    current, max
                ),
            }
        }
        HotplugResult::Unavailable { reason } => {
            state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
            warn!(
                vm_id,
                reason, "hotplug QMP indisponible — rollback scheduler"
            );
            VcpuDecision::MigrateRequired { vm_id, reason }
        }
        HotplugResult::Removed { .. } | HotplugResult::AtMin { .. } => {
            state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: "résultat QMP inattendu pendant hotplug".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vm_cpu_profile_uses_boot_vcpus_as_min() {
        let profile = parse_vm_cpu_profile(
            "boot: order=scsi0\ncores: 4\nsockets: 1\nvcpus: 1\nmemory: 512\n",
        )
        .unwrap();
        assert_eq!(profile, (1, 4));
    }

    #[test]
    fn test_parse_vm_cpu_profile_defaults_min_to_max_without_vcpus_line() {
        let profile = parse_vm_cpu_profile("cores: 2\nsockets: 2\nmemory: 512\n").unwrap();
        assert_eq!(profile, (4, 4));
    }

    #[test]
    fn test_derive_runtime_vcpu_profile_keeps_config_min_when_vm_is_currently_scaled_up() {
        let profile = derive_runtime_vcpu_profile(1, 4, 4, 4);
        assert_eq!(profile, (1, 4, 4));
    }

    #[test]
    fn test_derive_runtime_vcpu_profile_caps_min_to_current_when_runtime_is_lower() {
        let profile = derive_runtime_vcpu_profile(4, 8, 2, 8);
        assert_eq!(profile, (2, 2, 8));
    }
}

fn apply_downscale_if_idle(
    state: &Arc<NodeState>,
    hotplug: &VcpuHotplugManager,
    cpu_ctrl: &CgroupCpuController,
    vm_id: u32,
    force: bool,
) -> omega_daemon::vcpu_scheduler::VcpuDecision {
    use omega_daemon::vcpu_scheduler::VcpuDecision;

    let decision = state.vcpu_scheduler.try_downscale(vm_id, force);
    let VcpuDecision::Downscaled {
        new_count, slot, ..
    } = decision
    else {
        return decision;
    };

    let Some(vm_state) = state.vcpu_scheduler.get_vm_state(vm_id) else {
        let _ = state.vcpu_scheduler.rollback_downscale(vm_id, slot);
        return VcpuDecision::AtMin { vm_id };
    };

    match hotplug.remove_vcpu(vm_id, vm_state.min_vcpus) {
        HotplugResult::Removed {
            new_count: qmp_count,
        } => {
            if let Err(e) = apply_cpu_envelope(state, cpu_ctrl, vm_id, qmp_count) {
                warn!(vm_id, error = %e, "échec mise à jour cpu.max après hot-unplug");
            }
            info!(
                vm_id,
                scheduler_count = new_count,
                qmp_count,
                force,
                "downscale vCPU appliqué automatiquement"
            );
            VcpuDecision::Downscaled {
                vm_id,
                new_count,
                slot,
            }
        }
        HotplugResult::AtMin { .. } => {
            let _ = state.vcpu_scheduler.rollback_downscale(vm_id, slot);
            VcpuDecision::AtMin { vm_id }
        }
        HotplugResult::Unavailable { reason } => {
            let _ = state.vcpu_scheduler.rollback_downscale(vm_id, slot);
            warn!(
                vm_id,
                reason, "hot-unplug QMP indisponible — rollback scheduler"
            );
            VcpuDecision::MigrateRequired { vm_id, reason }
        }
        HotplugResult::NoSlots { .. } | HotplugResult::Added { .. } => {
            let _ = state.vcpu_scheduler.rollback_downscale(vm_id, slot);
            warn!(vm_id, "résultat QMP inattendu pendant hot-unplug");
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: "résultat QMP inattendu pendant hot-unplug".into(),
            }
        }
    }
}

fn reconcile_local_cpu_sharing(
    state: &Arc<NodeState>,
    hotplug: &VcpuHotplugManager,
    cpu_ctrl: &CgroupCpuController,
) {
    use omega_daemon::vcpu_scheduler::{
        VcpuDecision, BOOSTED_CPU_WEIGHT, DEFAULT_CPU_WEIGHT, DONOR_CPU_WEIGHT,
    };
    use std::collections::HashSet;

    let borrowers = state.vcpu_scheduler.local_share_borrowers();
    let mut donors = state.vcpu_scheduler.local_share_donors();

    for borrower in &borrowers {
        let Some(mut borrower_state) = state.vcpu_scheduler.get_vm_state(*borrower) else {
            continue;
        };

        while borrower_state.needs_more_vcpus()
            && borrower_state.current_vcpus < borrower_state.max_vcpus
        {
            let Some(idx) = donors.iter().position(|donor| *donor != *borrower) else {
                break;
            };
            let donor_vm = donors.remove(idx);

            match apply_downscale_if_idle(state, hotplug, cpu_ctrl, donor_vm, false) {
                VcpuDecision::Downscaled { .. } => {
                    info!(
                        borrower_vm = *borrower,
                        donor_vm, "réclamation locale d'un vCPU depuis une VM idle"
                    );
                    let _ = apply_hotplug_if_possible(state, hotplug, cpu_ctrl, *borrower);
                }
                _ => continue,
            }

            let Some(updated_state) = state.vcpu_scheduler.get_vm_state(*borrower) else {
                break;
            };
            borrower_state = updated_state;
        }
    }

    let stressed: HashSet<u32> = state
        .vcpu_scheduler
        .local_share_borrowers()
        .into_iter()
        .collect();
    let donor_peers: HashSet<u32> = state
        .vcpu_scheduler
        .local_share_idle_peers()
        .into_iter()
        .filter(|vm_id| !stressed.contains(vm_id))
        .collect();

    for vm_state in state.vcpu_scheduler.vm_snapshot() {
        let (target_weight, local_share_active) = if stressed.contains(&vm_state.vm_id) {
            (BOOSTED_CPU_WEIGHT, true)
        } else if donor_peers.contains(&vm_state.vm_id) {
            (DONOR_CPU_WEIGHT, true)
        } else {
            (DEFAULT_CPU_WEIGHT, false)
        };

        if !state
            .vcpu_scheduler
            .set_cpu_weight(vm_state.vm_id, target_weight, local_share_active)
        {
            continue;
        }

        if let Err(e) = apply_cpu_envelope(state, cpu_ctrl, vm_state.vm_id, vm_state.current_vcpus)
        {
            warn!(
                vm_id = vm_state.vm_id,
                error = %e,
                "échec mise à jour cpu.weight pendant le partage CPU local"
            );
            continue;
        }

        info!(
            vm_id = vm_state.vm_id,
            cpu_weight = target_weight,
            local_share_active,
            "politique de partage CPU local appliquée"
        );
    }
}

fn reconcile_local_disk_sharing(state: &Arc<NodeState>, io_ctrl: &CgroupIoController) {
    let stressed: HashSet<u32> = state
        .disk_io_scheduler
        .local_share_borrowers()
        .into_iter()
        .collect();
    let donor_peers: HashSet<u32> = state
        .disk_io_scheduler
        .idle_peers()
        .into_iter()
        .filter(|vm_id| !stressed.contains(vm_id))
        .collect();

    for vm_state in state.disk_io_scheduler.vm_snapshot() {
        let (target_weight, local_share_active) = if stressed.contains(&vm_state.vm_id) {
            (BOOSTED_IO_WEIGHT, true)
        } else if donor_peers.contains(&vm_state.vm_id) {
            (DONOR_IO_WEIGHT, true)
        } else {
            (DEFAULT_IO_WEIGHT, false)
        };

        if vm_state.io_weight == target_weight && vm_state.local_share_active == local_share_active
        {
            continue;
        }

        if matches!(io_ctrl.io_weight_supported(vm_state.vm_id), Some(false)) {
            let changed = state.disk_io_scheduler.mark_io_control_unsupported(
                vm_state.vm_id,
                "io.weight non disponible dans le cgroup VM (backend/cgroup non supporté)".into(),
            );
            if changed {
                info!(
                    vm_id = vm_state.vm_id,
                    "partage disque local désactivé automatiquement — io.weight indisponible"
                );
            }
            continue;
        }

        if let Err(e) = io_ctrl.apply(&VmDiskConfig::new(vm_state.vm_id).with_weight(target_weight))
        {
            let changed = state
                .disk_io_scheduler
                .mark_io_control_unsupported(vm_state.vm_id, e.to_string());
            if changed {
                warn!(
                    vm_id = vm_state.vm_id,
                    error = %e,
                    "io.weight indisponible — fallback vers télémétrie + migration"
                );
            }
            continue;
        }

        state
            .disk_io_scheduler
            .set_vm_weight(vm_state.vm_id, target_weight, local_share_active);

        info!(
            vm_id = vm_state.vm_id,
            io_weight = target_weight,
            local_share_active,
            "partage disque local réconcilié"
        );
    }
}
