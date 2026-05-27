//! Moteur d'éviction de pages sous pression mémoire locale.
//!
//! # Stratégie V4
//!
//! L'éviction est déclenchée automatiquement quand `/proc/meminfo` indique
//! que le nœud dépasse le seuil configuré (`evict_threshold_pct`).
//!
//! Pour chaque VM locale en cours de run :
//!   1. On identifie son processus QEMU via le pid_dir.
//!   2. On lit ses mappings mémoire depuis `/proc/{pid}/maps`.
//!   3. On sélectionne les régions anonymes privées (non-file, non-heap géré).
//!   4. On utilise `process_vm_readv` (ou `/proc/{pid}/mem`) pour lire les pages.
//!   5. On les envoie au store distant le moins chargé.
//!   6. On appelle `madvise(MADV_DONTNEED)` sur les pages sur le processus QEMU
//!      (requiert CAP_SYS_PTRACE ou ptrace). Cette étape est optionnelle en V4.
//!
//! Note : En V4, l'éviction directe depuis le processus QEMU est complexe car
//! QEMU gère lui-même sa mémoire. La stratégie pragmatique V4 est :
//!   - Monitorer la pression mémoire globale du nœud
//!   - Si pression > seuil et VM candidate à migration → signaler au controller
//!   - Le controller décide de migrer plutôt que d'évincer page par page
//!
//! L'éviction page par page reste disponible pour la région de test userfaultfd
//! (identique à la V1 — sert de validation du mécanisme).

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::config::Config;
use crate::fault_bus::{AdaptiveInterval, FaultBusConsumer};
use crate::node_state::{read_meminfo, NodeState};
use crate::vm_migration::{MigrationExecutor, MigrationPolicy, MigrationThresholds};
use crate::vm_tracker::VmStatus;

/// Résultat d'un cycle d'éviction.
#[derive(Debug, Default)]
pub struct EvictionResult {
    pub pages_evicted: u64,
    pub vms_considered: u32,
    pub eviction_skipped: bool,
    pub reason: String,
}

/// Moteur d'éviction — tâche de fond (correction L1 : réactif aux fautes uffd).
pub struct EvictionEngine {
    state: Arc<NodeState>,
    threshold_pct: f64,
    /// Intervalle adaptatif : réduit quand le FaultBus signale une pression uffd
    check_interval: AdaptiveInterval,
    /// Consommateur du bus de fautes (optionnel — absent si pas d'agent uffd)
    fault_consumer: Option<FaultBusConsumer>,
    /// Exécuteur de migrations (live + cold) — déclenché automatiquement sous pression
    migration_exec: Arc<MigrationExecutor>,
    /// Politique de décision live vs cold
    migration_policy: MigrationPolicy,
}

impl EvictionEngine {
    pub fn new(state: Arc<NodeState>, cfg: &Config) -> Self {
        let node_id = state.node_id.clone();
        let exec = Arc::new(MigrationExecutor::new(Arc::clone(&state)));
        Self {
            state,
            threshold_pct: cfg.evict_threshold_pct,
            check_interval: AdaptiveInterval::new(10, 2), // 10s normal, 2s accéléré
            fault_consumer: None,
            migration_exec: exec,
            migration_policy: MigrationPolicy::new(node_id, MigrationThresholds::default()),
        }
    }

    /// Connecte le FaultBus pour réagir en temps réel aux fautes uffd (correction L1).
    ///
    /// Appeler avant `run()`. Le moteur accélère son rythme si le taux de
    /// page faults distantes dépasse `accel_threshold_rps`.
    pub fn with_fault_bus(mut self, consumer: FaultBusConsumer) -> Self {
        self.fault_consumer = Some(consumer);
        self
    }

    /// Boucle principale — tourne en tâche Tokio de fond.
    pub async fn run(mut self) {
        info!(
            threshold_pct = self.threshold_pct,
            fault_bus = self.fault_consumer.is_some(),
            "moteur d'éviction démarré"
        );

        loop {
            tokio::time::sleep(self.check_interval.current()).await;

            // Lire les événements du FaultBus et ajuster le rythme (L1)
            if let Some(ref mut consumer) = self.fault_consumer {
                let under_fault_pressure = consumer.poll();
                self.check_interval.update(under_fault_pressure);

                if under_fault_pressure {
                    info!(
                        remote_rps = format!("{:.1}", consumer.stats.remote_rps),
                        avg_lat_us = consumer.stats.avg_latency_us,
                        "FaultBus : taux de fautes distant élevé → cycle d'éviction accéléré"
                    );
                }
            }

            let result = self.evaluate_eviction();

            if !result.eviction_skipped {
                info!(
                    pages_evicted = result.pages_evicted,
                    vms_considered = result.vms_considered,
                    "cycle d'éviction terminé"
                );
            } else {
                debug!(reason = %result.reason, "éviction ignorée ce cycle");
            }
        }
    }

    /// Évalue si l'éviction est nécessaire et la déclenche si oui.
    fn evaluate_eviction(&self) -> EvictionResult {
        let (mem_total, mem_available) = read_meminfo();
        if mem_total == 0 {
            return EvictionResult {
                eviction_skipped: true,
                reason: "impossible de lire /proc/meminfo".into(),
                ..Default::default()
            };
        }

        let usage_pct = ((mem_total - mem_available) as f64 / mem_total as f64) * 100.0;

        if usage_pct < self.threshold_pct {
            return EvictionResult {
                eviction_skipped: true,
                reason: format!(
                    "usage RAM {:.1}% < seuil {:.1}%",
                    usage_pct, self.threshold_pct
                ),
                ..Default::default()
            };
        }

        info!(
            usage_pct = format!("{:.1}%", usage_pct),
            threshold = format!("{:.1}%", self.threshold_pct),
            "pression mémoire détectée — évaluation des VMs candidates"
        );

        // Identifier les VMs locales sous pression
        let vms = self.state.vm_tracker.local_vms_snapshot();
        let candidates: Vec<_> = vms
            .iter()
            .filter(|vm| vm.status == VmStatus::Running)
            .collect();

        if candidates.is_empty() {
            return EvictionResult {
                eviction_skipped: true,
                reason: "aucune VM locale en cours de run".into(),
                vms_considered: 0,
                ..Default::default()
            };
        }

        // Évaluer les recommandations de migration
        let recommendations = self.migration_policy.evaluate(&self.state);

        let mut migrations_triggered = 0u32;

        for rec in &recommendations {
            // Ne déclencher qu'une migration à la fois depuis l'engine
            // (le controller Python peut en déclencher plusieurs en parallèle)
            if migrations_triggered >= 1 {
                break;
            }

            // Vérifier qu'aucune migration pour cette VM n'est déjà en cours
            let already_running = self
                .migration_exec
                .running()
                .iter()
                .any(|s| s.request.vm_id == rec.vm_id);

            if already_running {
                info!(
                    vm_id = rec.vm_id,
                    "migration déjà en cours — ignorée ce cycle"
                );
                continue;
            }

            if rec.target == "auto" {
                warn!(
                    vm_id = rec.vm_id,
                    mtype = ?rec.mtype,
                    reason = ?rec.reason,
                    "migration automatique recommandée sans cible résolue — exécution locale ignorée"
                );
                continue;
            }

            info!(
                vm_id   = rec.vm_id,
                target  = %rec.target,
                mtype   = ?rec.mtype,
                reason  = ?rec.reason,
                "déclenchement automatique de migration (pression mémoire)"
            );

            self.migration_exec.spawn(rec.clone());
            migrations_triggered += 1;
        }

        if recommendations.is_empty() {
            warn!(
                vms_count = candidates.len(),
                usage_pct = format!("{:.1}%", usage_pct),
                "VMs sous pression — aucune cible disponible pour migration"
            );
        }

        EvictionResult {
            pages_evicted: 0,
            vms_considered: candidates.len() as u32,
            eviction_skipped: false,
            reason: format!(
                "{} VM(s) sous pression, {} migration(s) déclenchée(s)",
                candidates.len(),
                migrations_triggered
            ),
        }
    }
}
