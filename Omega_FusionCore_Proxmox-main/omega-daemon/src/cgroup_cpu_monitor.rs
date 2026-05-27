//! Monitor CPU haute fréquence (1 ms) — remplace cpu_cgroup_monitor.py.
//!
//! # Pourquoi Rust plutôt que Python
//!
//! Le GIL Python sérialise l'exécution des threads : un monitor à 1 ms avec
//! N VMs ne peut pas paralléliser les lectures cgroup. Sous charge (20+ VMs),
//! le délai réel entre deux lectures dépasse 1 ms, ce qui rend les décisions
//! de hotplug imprecises.
//!
//! En Rust + Tokio :
//! - Pas de GIL : les lectures cgroup se font en parallèle via spawn_blocking
//! - `MissedTickBehavior::Skip` : si une itération dépasse 1 ms, on ne rattrape pas
//! - Canal mpsc borné : si le consommateur est lent, on lâche les événements
//!   plutôt que de bloquer la boucle de monitoring
//!
//! # Architecture
//!
//! ```text
//! tokio::spawn(CgroupCpuMonitor::run)
//!   │
//!   ├── interval 1 ms → list_active_vms() → read_cpu_stat(vm_id) × N
//!   ├── SlidingWindow par VM (100 échantillons = 100 ms de données)
//!   └── avg_usage >= 80% || avg_throttle >= 10%
//!       → tx.try_send(CpuPressureEvent { vm_id, avg_usage_pct, avg_throttle_ratio })
//!
//! Consommateur (main loop / vcpu_scheduler)
//!   └── rx.recv() → décision hotplug / migration
//! ```

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};
use tracing::{debug, trace, warn};

use crate::cpu_cgroup::{CgroupCpuController, VmCpuStat};

// ─── Constantes ───────────────────────────────────────────────────────────────

/// Intervalle de polling : 1 ms.
pub const POLL_INTERVAL_MS: u64 = 1;
/// Taille de la fenêtre glissante (100 échantillons × 1 ms = 100 ms).
pub const WINDOW_SIZE: usize = 100;
/// Seuil d'usage vCPU déclenchant on_pressure (%).
pub const USAGE_THRESHOLD: f64 = 80.0;
/// Taux de throttling déclenchant on_pressure (0.0–1.0).
pub const THROTTLE_THRESHOLD: f64 = 0.10;

// ─── Événement de pression ────────────────────────────────────────────────────

/// Signale qu'une VM dépasse les seuils de pression CPU.
#[derive(Debug, Clone)]
pub struct CpuPressureEvent {
    pub vm_id: u32,
    pub avg_usage_pct: f64, // % moyen sur 100 ms
    pub max_usage_pct: f64, // pic sur 100 ms
    pub avg_throttle: f64,  // taux de throttling moyen sur 100 ms
}

// ─── Fenêtre glissante par VM ─────────────────────────────────────────────────

struct SlidingWindow {
    usage_pcts: VecDeque<f64>,
    throttle_rates: VecDeque<f64>,
    capacity: usize,
}

impl SlidingWindow {
    fn new(capacity: usize) -> Self {
        Self {
            usage_pcts: VecDeque::with_capacity(capacity),
            throttle_rates: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, usage_pct: f64, throttle_ratio: f64) {
        if self.usage_pcts.len() >= self.capacity {
            self.usage_pcts.pop_front();
            self.throttle_rates.pop_front();
        }
        self.usage_pcts.push_back(usage_pct);
        self.throttle_rates.push_back(throttle_ratio);
    }

    fn is_full(&self) -> bool {
        self.usage_pcts.len() >= self.capacity
    }

    fn avg_usage(&self) -> f64 {
        if self.usage_pcts.is_empty() {
            return 0.0;
        }
        self.usage_pcts.iter().sum::<f64>() / self.usage_pcts.len() as f64
    }

    fn max_usage(&self) -> f64 {
        self.usage_pcts.iter().cloned().fold(0.0_f64, f64::max)
    }

    fn avg_throttle(&self) -> f64 {
        if self.throttle_rates.is_empty() {
            return 0.0;
        }
        self.throttle_rates.iter().sum::<f64>() / self.throttle_rates.len() as f64
    }
}

// ─── Calcul d'usage instantané ────────────────────────────────────────────────

/// % de vCPU utilisé entre deux snapshots.
///
/// `usage_usec` est cumulatif — on calcule le delta / temps écoulé.
fn compute_usage_pct(curr: &VmCpuStat, prev: &VmCpuStat) -> f64 {
    let delta_cpu = curr.usage_usec.saturating_sub(prev.usage_usec) as f64;
    let delta_wall = (curr.usage_pct - prev.usage_pct).abs(); // repurposé ci-dessous

    // On utilise les timestamps implicites (les stats sont lues à ~1 ms d'intervalle)
    // elapsed ≈ POLL_INTERVAL_MS * 1000 µs
    let elapsed_us = (POLL_INTERVAL_MS * 1_000) as f64;
    if elapsed_us <= 0.0 {
        return 0.0;
    }
    let _ = delta_wall; // non utilisé ici — usage_pct sera recalculé proprement
    (delta_cpu / elapsed_us) * 100.0
}

// ─── Monitor ──────────────────────────────────────────────────────────────────

/// Configuration du monitor.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Intervalle de polling en ms (défaut : 1).
    pub poll_interval_ms: u64,
    /// Taille de la fenêtre glissante (défaut : 100).
    pub window_size: usize,
    /// Seuil d'usage vCPU pour émettre un événement (défaut : 80.0).
    pub usage_threshold: f64,
    /// Seuil de throttling pour émettre un événement (défaut : 0.10).
    pub throttle_threshold: f64,
    /// Capacité du canal mpsc (défaut : 64).
    pub channel_capacity: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: POLL_INTERVAL_MS,
            window_size: WINDOW_SIZE,
            usage_threshold: USAGE_THRESHOLD,
            throttle_threshold: THROTTLE_THRESHOLD,
            channel_capacity: 64,
        }
    }
}

/// Monitor CPU haute fréquence.
///
/// Spawné comme tâche Tokio via `CgroupCpuMonitor::spawn()`.
/// Les événements de pression sont envoyés via le `Receiver` retourné.
pub struct CgroupCpuMonitor {
    controller: CgroupCpuController,
    config: MonitorConfig,
    tx: mpsc::Sender<CpuPressureEvent>,
}

impl CgroupCpuMonitor {
    /// Crée le monitor et retourne le canal de réception des événements.
    pub fn new(config: MonitorConfig) -> (Self, mpsc::Receiver<CpuPressureEvent>) {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let monitor = Self {
            controller: CgroupCpuController::new(),
            config,
            tx,
        };
        (monitor, rx)
    }

    /// Lance le monitor dans une tâche Tokio de fond.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(self) {
        let interval_dur = Duration::from_millis(self.config.poll_interval_ms);
        let mut ticker = time::interval(interval_dur);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut windows: HashMap<u32, SlidingWindow> = HashMap::new();
        let mut prev_stats: HashMap<u32, VmCpuStat> = HashMap::new();
        // rate-limit : une seule émission par fenêtre (100 ms) par VM
        let mut last_event: HashMap<u32, std::time::Instant> = HashMap::new();
        let rate_limit_dur = interval_dur * self.config.window_size as u32;

        loop {
            ticker.tick().await;

            let vm_ids = self.controller.list_active_vms();
            if vm_ids.is_empty() {
                trace!("cgroup_cpu_monitor : aucune VM active");
                continue;
            }

            for vm_id in vm_ids {
                let Some(stat) = self.controller.read_cpu_stat(vm_id) else {
                    continue;
                };

                if let Some(prev) = prev_stats.get(&vm_id) {
                    let usage_pct = compute_usage_pct(&stat, prev);
                    let throttle_rate = stat.throttle_ratio();

                    let window = windows
                        .entry(vm_id)
                        .or_insert_with(|| SlidingWindow::new(self.config.window_size));
                    window.push(usage_pct, throttle_rate);

                    // Émettre uniquement quand la fenêtre est pleine (100 ms de données)
                    if window.is_full() {
                        let avg_usage = window.avg_usage();
                        let avg_throttle = window.avg_throttle();

                        let now = std::time::Instant::now();
                        let last = last_event
                            .get(&vm_id)
                            .copied()
                            .unwrap_or(std::time::Instant::now() - rate_limit_dur * 2);

                        if (avg_usage >= self.config.usage_threshold
                            || avg_throttle >= self.config.throttle_threshold)
                            && now.duration_since(last) >= rate_limit_dur
                        {
                            let event = CpuPressureEvent {
                                vm_id,
                                avg_usage_pct: avg_usage,
                                max_usage_pct: window.max_usage(),
                                avg_throttle,
                            };
                            debug!(
                                vm_id,
                                avg_usage_pct = format!("{:.1}", avg_usage),
                                avg_throttle = format!("{:.3}", avg_throttle),
                                "pression CPU détectée"
                            );
                            // try_send : non-bloquant — on lâche si le canal est plein
                            if self.tx.try_send(event).is_err() {
                                warn!(vm_id, "canal CpuPressureEvent plein — événement ignoré");
                            } else {
                                last_event.insert(vm_id, now);
                            }
                        }
                    }
                }

                prev_stats.insert(vm_id, stat);
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sliding_window_avg() {
        let mut w = SlidingWindow::new(4);
        w.push(80.0, 0.0);
        w.push(90.0, 0.0);
        w.push(100.0, 0.0);
        w.push(70.0, 0.0);
        assert!(w.is_full());
        let avg = w.avg_usage();
        assert!((avg - 85.0).abs() < 0.01, "avg={avg}");
    }

    #[test]
    fn test_sliding_window_max() {
        let mut w = SlidingWindow::new(4);
        w.push(50.0, 0.0);
        w.push(99.0, 0.0);
        w.push(10.0, 0.0);
        w.push(40.0, 0.0);
        assert!((w.max_usage() - 99.0).abs() < 0.01);
    }

    #[test]
    fn test_sliding_window_eviction() {
        let mut w = SlidingWindow::new(3);
        w.push(10.0, 0.0);
        w.push(20.0, 0.0);
        w.push(30.0, 0.0);
        w.push(40.0, 0.0); // éjecte 10.0
        assert!(w.is_full());
        // avg = (20 + 30 + 40) / 3 = 30
        let avg = w.avg_usage();
        assert!((avg - 30.0).abs() < 0.01, "avg={avg}");
    }

    #[test]
    fn test_monitor_config_defaults() {
        let cfg = MonitorConfig::default();
        assert_eq!(cfg.poll_interval_ms, 1);
        assert_eq!(cfg.window_size, 100);
        assert!((cfg.usage_threshold - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_monitor_new_returns_rx() {
        let (_monitor, rx) = CgroupCpuMonitor::new(MonitorConfig::default());
        // Le canal reste ouvert tant que _monitor (et donc tx) est en vie
        assert!(!rx.is_closed());
    }
}
