//! Migration de VMs — à chaud (live) et à froid (cold).
//!
//! # Quand utiliser quelle stratégie
//!
//! ```text
//! État de la VM        Pression nœud       → Stratégie
//! ─────────────────    ─────────────────    ─────────────────────────────
//! Running, CPU > 5%    RAM > 85%            LIVE  — la VM tourne, on ne peut
//!                                           pas se permettre un arrêt
//! Running, CPU < 5%    RAM > 85%            COLD  — la VM est idle, l'arrêt
//!                      (> 60s)              court est acceptable
//! Stopped              peu importe          COLD  — VM déjà arrêtée
//! Running, CPU > 5%    RAM > 95% (critique) COLD  — urgence : live trop lent
//!                                           (trop de dirty pages à transférer)
//! Running, throttle    vCPU saturé          LIVE  — déplacer pour libérer
//!  > 30% persistant    nœud complet         des vCPU sur ce nœud
//! ```
//!
//! # Flux d'exécution
//!
//! ```text
//! MigrationPolicy.evaluate()           → MigrationRequest { vm, target, type }
//!     │
//! MigrationExecutor.spawn(req)         → task Tokio de fond (ID retourné immédiatement)
//!     │
//!     ├─ LIVE: "qm migrate {vm} {target} --online"
//!     │        Ceph RBD : disques partagés, seule la RAM est transférée.
//!     │        après succès : cleanup source (pages store + vm_tracker)
//!     │
//!     └─ COLD: "qm migrate {vm} {target}"
//!              VM stoppée, transfert RAM, redémarrage sur nœud cible.
//!              après succès : cleanup source
//!
//! MigrationExecutor.status(task_id)    → MigrationStatus { state, elapsed, result }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{error, info};

use crate::node_state::{read_meminfo, NodeState};
use crate::vm_tracker::{LocalVm, VmStatus};

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationType {
    /// VM reste allumée pendant le transfert (KVM pre-copy, dirty page tracking).
    /// Downtime < 1s typiquement. Nécessite que la VM tourne.
    Live,
    /// VM arrêtée avant le transfert, redémarrée sur le nœud cible.
    /// Downtime = temps de stop + transfert + démarrage. Plus simple, plus sûr.
    Cold,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum MigrationReason {
    /// RAM du nœud source > seuil critique
    MemoryPressure {
        node_used_pct: f64,
        target_free_pct: f64,
    },
    /// vCPU throttling persistant, pas de hotplug possible
    CpuSaturation {
        throttle_ratio: f64,
        target_vcpu_free: usize,
    },
    /// Trop de pages de la VM stockées à distance → rapatrier sur nœud moins chargé
    ExcessiveRemotePaging { remote_pct: f64 },
    /// Drainage pour maintenance planifiée
    MaintenanceDrain,
    /// Demande explicite de l'administrateur
    AdminRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRequest {
    pub vm_id: u32,
    pub source: String,
    pub target: String,
    pub mtype: MigrationType,
    pub reason: MigrationReason,
    /// Activer `--with-local-disks` pour les clusters sans Ceph (LVM, ZFS local).
    #[serde(default)]
    pub with_local_disks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationState {
    Pending,
    Running,
    Success,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct MigrationStatus {
    pub task_id: u64,
    pub request: MigrationRequest,
    pub state: MigrationState,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

// ─── Exécuteur ────────────────────────────────────────────────────────────────

/// Exécute des migrations via `qm migrate` et suit leur avancement.
///
/// Chaque migration tourne dans une tâche Tokio de fond.
/// Le résultat est accessible via `status(task_id)`.
///
/// Supporte Ceph RBD (défaut) et stockage local via `MigrationRequest::with_local_disks`.
pub struct MigrationExecutor {
    node_state: Arc<NodeState>,
    tasks: Arc<Mutex<HashMap<u64, MigrationStatus>>>,
    next_id: Arc<Mutex<u64>>,
}

impl MigrationExecutor {
    pub fn new(node_state: Arc<NodeState>) -> Self {
        Self {
            node_state,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Lance une migration en tâche de fond.
    /// Retourne le task_id immédiatement pour suivi asynchrone.
    pub fn spawn(&self, req: MigrationRequest) -> u64 {
        let task_id = {
            let mut n = self.next_id.lock().unwrap();
            let id = *n;
            *n += 1;
            id
        };

        let status = MigrationStatus {
            task_id,
            request: req.clone(),
            state: MigrationState::Pending,
            elapsed_ms: 0,
            error: None,
        };

        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.insert(task_id, status);
        }

        let tasks_ref = Arc::clone(&self.tasks);
        let state_ref = Arc::clone(&self.node_state);

        tokio::spawn(async move {
            Self::run_migration(task_id, req, tasks_ref, state_ref).await;
        });

        task_id
    }

    async fn run_migration(
        task_id: u64,
        req: MigrationRequest,
        tasks: Arc<Mutex<HashMap<u64, MigrationStatus>>>,
        node_state: Arc<NodeState>,
    ) {
        let started = Instant::now();

        {
            let mut t = tasks.lock().unwrap();
            if let Some(s) = t.get_mut(&task_id) {
                s.state = MigrationState::Running;
            }
        }

        let args = build_qm_args(&req);
        info!(
            task_id,
            vm_id   = req.vm_id,
            target  = %req.target,
            mtype   = ?req.mtype,
            command = %format!("qm {}", args.join(" ")),
            "migration démarrée"
        );

        let result = execute_qm_migrate(&req).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match &result {
            Ok(()) => {
                info!(task_id, vm_id = req.vm_id, elapsed_ms, "migration réussie");
                Self::cleanup_after_migration(&req, &node_state).await;
                let mut t = tasks.lock().unwrap();
                if let Some(s) = t.get_mut(&task_id) {
                    s.state = MigrationState::Success;
                    s.elapsed_ms = elapsed_ms;
                }
            }
            Err(e) => {
                error!(task_id, vm_id = req.vm_id, error = %e, "migration échouée");
                let mut t = tasks.lock().unwrap();
                if let Some(s) = t.get_mut(&task_id) {
                    s.state = MigrationState::Failed;
                    s.elapsed_ms = elapsed_ms;
                    s.error = Some(e.to_string());
                }
            }
        }
    }

    /// Nettoie les ressources source après une migration réussie :
    ///   - Supprime les pages de cette VM du store local
    ///   - Retire la VM du tracker
    async fn cleanup_after_migration(req: &MigrationRequest, state: &NodeState) {
        let keys = state.store.keys_for_vm(req.vm_id);
        let count = keys.len();
        for key in keys {
            state.store.delete(&key);
        }
        if count > 0 {
            info!(
                vm_id = req.vm_id,
                deleted_pages = count,
                "pages source supprimées post-migration"
            );
        }
        state.vcpu_scheduler.release_vm(req.vm_id);
        state.quota_registry.remove(req.vm_id);
        if let Some(gpu_runtime) = &state.gpu_runtime {
            gpu_runtime.release_vm(req.vm_id).await;
        }
        info!(
            vm_id = req.vm_id,
            "ressources source libérées post-migration"
        );
    }

    /// Consulte le statut d'une migration par son task_id.
    pub fn status(&self, task_id: u64) -> Option<MigrationStatus> {
        let tasks = self.tasks.lock().unwrap();
        tasks.get(&task_id).cloned()
    }

    /// Liste toutes les migrations (en cours et terminées).
    pub fn list_all(&self) -> Vec<MigrationStatus> {
        let tasks = self.tasks.lock().unwrap();
        let mut v: Vec<_> = tasks.values().cloned().collect();
        v.sort_by_key(|s| s.task_id);
        v
    }

    /// Migrations actuellement en cours.
    pub fn running(&self) -> Vec<MigrationStatus> {
        self.list_all()
            .into_iter()
            .filter(|s| s.state == MigrationState::Running || s.state == MigrationState::Pending)
            .collect()
    }
}

// ─── Construction des arguments qm migrate ───────────────────────────────────

/// Construit la liste d'arguments pour `qm migrate`.
///
/// - live : `migrate {vmid} {target} --online`
/// - cold : `migrate {vmid} {target}`
/// - stockage local : ajoute `--with-local-disks` (LVM, ZFS local — copie les disques)
pub fn build_qm_args(req: &MigrationRequest) -> Vec<String> {
    let mut args = vec![
        "migrate".to_string(),
        req.vm_id.to_string(),
        req.target.clone(),
    ];
    if req.mtype == MigrationType::Live {
        args.push("--online".to_string());
    }
    if req.with_local_disks {
        args.push("--with-local-disks".to_string());
    }
    args
}

/// Exécute `qm migrate` sur ce nœud.
pub async fn execute_qm_migrate(req: &MigrationRequest) -> Result<()> {
    if req.target == "auto" {
        return Err(anyhow::anyhow!(
            "cible de migration non résolue: le daemon ne peut pas exécuter qm migrate avec target=auto"
        ));
    }

    let args = build_qm_args(req);

    let output = Command::new("qm")
        .args(&args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("impossible de lancer qm migrate: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow::anyhow!(
            "qm migrate a échoué (code {:?}): {}",
            output.status.code(),
            stderr
        ))
    }
}

// ─── Politique de décision ────────────────────────────────────────────────────

/// Seuils de déclenchement de la politique de migration.
#[derive(Debug, Clone)]
pub struct MigrationThresholds {
    /// RAM > X% → pression haute → live migration (défaut : 85%)
    pub ram_high_pct: f64,
    /// RAM > X% → pression critique → cold migration forcée (défaut : 95%)
    pub ram_critical_pct: f64,
    /// Throttle ratio vCPU > X → migration vCPU (défaut : 0.30)
    pub vcpu_throttle_trigger: f64,
    /// % de RAM de la VM stocké à distance > X → excessive remote paging (défaut : 60%)
    pub remote_paging_pct: f64,
    /// CPU usage < X% pendant idle_duration_secs → VM idle → cold OK (défaut : 5%)
    pub idle_cpu_pct: f64,
    /// Durée d'idle CPU avant de classifier la VM comme idle (défaut : 60s)
    pub idle_duration_secs: u64,
}

impl Default for MigrationThresholds {
    fn default() -> Self {
        Self {
            ram_high_pct: 85.0,
            ram_critical_pct: 95.0,
            vcpu_throttle_trigger: 0.30,
            remote_paging_pct: 60.0,
            idle_cpu_pct: 5.0,
            idle_duration_secs: 60,
        }
    }
}

/// Évalue l'état du nœud et détermine si une VM doit être migrée et comment.
///
/// Cette structure est utilisée par l'`EvictionEngine` et par l'API HTTP.
/// Elle ne prend PAS de décisions autonomes — elle retourne une `MigrationRequest`
/// que le controller Python ou l'admin peut approuver ou rejeter.
pub struct MigrationPolicy {
    thresholds: MigrationThresholds,
    node_id: String,
    /// Cache des usages CPU par VM (vm_id → (avg_pct, last_seen))
    cpu_cache: Arc<Mutex<HashMap<u32, (f64, Instant)>>>,
}

impl MigrationPolicy {
    pub fn new(node_id: String, thresholds: MigrationThresholds) -> Self {
        Self {
            thresholds,
            node_id,
            cpu_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Met à jour le cache CPU pour une VM (appelé depuis le moniteur cgroup).
    pub fn update_cpu_usage(&self, vm_id: u32, avg_cpu_pct: f64) {
        let mut cache = self.cpu_cache.lock().unwrap();
        cache.insert(vm_id, (avg_cpu_pct, Instant::now()));
    }

    /// Évalue l'état courant et retourne une liste de migrations recommandées.
    ///
    /// Le résultat est trié par urgence décroissante.
    /// Le controller choisit combien en exécuter simultanément.
    pub fn evaluate(&self, state: &NodeState) -> Vec<MigrationRequest> {
        let (mem_total, mem_available) = read_meminfo();
        if mem_total == 0 {
            return vec![];
        }

        let usage_pct = (mem_total - mem_available) as f64 / mem_total as f64 * 100.0;
        let vms = state.vm_tracker.local_vms_snapshot();
        let mut recommendations = vec![];

        // ── 1. Excessive remote paging → migrer vers nœud avec plus de RAM ──
        for vm in &vms {
            if vm.remote_pct() > self.thresholds.remote_paging_pct {
                let mtype =
                    self.pick_migration_type(vm.vmid, vm.status == VmStatus::Running, false);
                recommendations.push(MigrationRequest {
                    vm_id: vm.vmid,
                    source: self.node_id.clone(),
                    target: String::from("auto"), // controller Python choisit la cible
                    mtype,
                    reason: MigrationReason::ExcessiveRemotePaging {
                        remote_pct: vm.remote_pct(),
                    },
                    with_local_disks: false,
                });
            }
        }

        // ── 2. Pression RAM critique → cold migration de la VM la plus lourde ──
        if usage_pct >= self.thresholds.ram_critical_pct {
            if let Some(heaviest) = vms
                .iter()
                .filter(|v| v.status == VmStatus::Running || v.status == VmStatus::Stopped)
                .max_by_key(|v| v.rss_kb)
            {
                let is_critical = true;
                let mtype = self.pick_migration_type(
                    heaviest.vmid,
                    heaviest.status == VmStatus::Running,
                    is_critical,
                );
                recommendations.push(MigrationRequest {
                    vm_id: heaviest.vmid,
                    source: self.node_id.clone(),
                    target: String::from("auto"),
                    mtype,
                    reason: MigrationReason::MemoryPressure {
                        node_used_pct: usage_pct,
                        target_free_pct: 0.0, // rempli par le controller
                    },
                    with_local_disks: false,
                });
            }
        }
        // ── 3. Pression RAM haute → live migration de la VM la plus lourde ───
        else if usage_pct >= self.thresholds.ram_high_pct {
            if let Some(heaviest) = vms
                .iter()
                .filter(|v| v.status == VmStatus::Running)
                .max_by_key(|v| v.rss_kb)
            {
                let mtype = self.pick_migration_type(heaviest.vmid, true, false);
                recommendations.push(MigrationRequest {
                    vm_id: heaviest.vmid,
                    source: self.node_id.clone(),
                    target: String::from("auto"),
                    mtype,
                    reason: MigrationReason::MemoryPressure {
                        node_used_pct: usage_pct,
                        target_free_pct: 0.0,
                    },
                    with_local_disks: false,
                });
            }
        }

        // ── 4. Saturation vCPU → live migration de la VM la plus throttlée ──
        let throttle_candidates = self.vms_with_high_throttle(&vms);
        for (vm_id, throttle_ratio) in throttle_candidates {
            let free_slots = state.vcpu_scheduler.free_vslots();
            if free_slots == 0 {
                recommendations.push(MigrationRequest {
                    vm_id,
                    source: self.node_id.clone(),
                    target: String::from("auto"),
                    mtype: MigrationType::Live,
                    reason: MigrationReason::CpuSaturation {
                        throttle_ratio,
                        target_vcpu_free: free_slots,
                    },
                    with_local_disks: false,
                });
            }
        }

        recommendations
    }

    /// Choisit entre LIVE et COLD selon l'état de la VM.
    ///
    /// Règles :
    ///   - VM stopped → toujours COLD
    ///   - VM running + critique (RAM > 95%) → COLD (live trop lent sous forte pression)
    ///   - VM running + idle (CPU < 5% depuis 60s) → COLD (acceptable)
    ///   - VM running + active → LIVE (minimiser le downtime)
    fn pick_migration_type(
        &self,
        vm_id: u32,
        is_running: bool,
        is_critical: bool,
    ) -> MigrationType {
        if !is_running {
            return MigrationType::Cold;
        }
        if is_critical {
            return MigrationType::Cold; // urgence : ne pas attendre la convergence live
        }
        if self.is_vm_idle(vm_id) {
            return MigrationType::Cold; // idle → downtime court acceptable
        }
        MigrationType::Live
    }

    /// Retourne true si la VM est considérée idle (CPU < seuil depuis > durée).
    fn is_vm_idle(&self, vm_id: u32) -> bool {
        let cache = self.cpu_cache.lock().unwrap();
        if let Some((avg_pct, last_seen)) = cache.get(&vm_id) {
            let elapsed = last_seen.elapsed().as_secs();
            *avg_pct < self.thresholds.idle_cpu_pct
                && elapsed < self.thresholds.idle_duration_secs + 5
        } else {
            false
        }
    }

    /// Retourne les VMs avec un throttle ratio dépassant le seuil.
    fn vms_with_high_throttle(&self, vms: &[LocalVm]) -> Vec<(u32, f64)> {
        // Le throttle_ratio vient du vcpu_scheduler (metrics injectées par cpu_cgroup_monitor).
        // Ici on lit depuis le cache CPU — si throttle n'est pas dans le cache, on ignore.
        // En pratique le controller Python pousse les métriques via update_cpu_usage.
        let cache = self.cpu_cache.lock().unwrap();
        vms.iter()
            .filter(|v| v.status == VmStatus::Running)
            .filter_map(|v| {
                // On utilise avg_cpu_pct comme proxy : si très haut (> 150%) = throttling probable
                cache.get(&v.vmid).and_then(|(pct, _)| {
                    if *pct > 150.0 {
                        // 150% = 1.5 vCPU saturé en permanence
                        Some((v.vmid, (*pct - 100.0) / 100.0)) // ratio approximatif
                    } else {
                        None
                    }
                })
            })
            .collect()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_req(mtype: MigrationType) -> MigrationRequest {
        MigrationRequest {
            vm_id: 100,
            source: "node-a".into(),
            target: "node-b".into(),
            mtype,
            reason: MigrationReason::AdminRequest,
            with_local_disks: false,
        }
    }

    // ── build_qm_args : 4 combinaisons ──────────────────────────────────────

    // ── build_qm_args (Ceph — pas de --with-local-disks) ────────────────────

    #[test]
    fn test_args_live() {
        let args = build_qm_args(&make_req(MigrationType::Live));
        assert_eq!(args, vec!["migrate", "100", "node-b", "--online"]);
    }

    #[test]
    fn test_args_cold() {
        let args = build_qm_args(&make_req(MigrationType::Cold));
        assert_eq!(args, vec!["migrate", "100", "node-b"]);
    }

    #[test]
    fn test_args_no_local_disks_flag() {
        // Vérifie qu'on ne passe jamais --with-local-disks (Ceph = stockage partagé)
        for mtype in [MigrationType::Live, MigrationType::Cold] {
            let args = build_qm_args(&make_req(mtype));
            assert!(!args.contains(&"--with-local-disks".to_string()));
        }
    }

    #[tokio::test]
    async fn test_execute_qm_migrate_rejects_auto_target() {
        let mut req = make_req(MigrationType::Live);
        req.target = "auto".into();

        let err = execute_qm_migrate(&req).await.unwrap_err();
        assert!(err.to_string().contains("target=auto"));
    }

    fn make_policy() -> MigrationPolicy {
        MigrationPolicy::new("node-a".into(), MigrationThresholds::default())
    }

    #[test]
    fn test_pick_cold_for_stopped_vm() {
        let policy = make_policy();
        assert_eq!(
            policy.pick_migration_type(1, false, false),
            MigrationType::Cold
        );
    }

    #[test]
    fn test_pick_cold_for_critical_running_vm() {
        let policy = make_policy();
        assert_eq!(
            policy.pick_migration_type(1, true, true),
            MigrationType::Cold
        );
    }

    #[test]
    fn test_pick_live_for_active_vm() {
        let policy = make_policy();
        // VM active (pas idle, pas critique)
        policy.update_cpu_usage(1, 80.0); // haute charge
        assert_eq!(
            policy.pick_migration_type(1, true, false),
            MigrationType::Live
        );
    }

    #[test]
    fn test_pick_cold_for_idle_vm() {
        let policy = make_policy();
        policy.update_cpu_usage(1, 2.0); // très faible charge
                                         // is_vm_idle vérifie elapsed < idle_duration + 5s
                                         // On vient de pusher → elapsed ≈ 0s < 65s → condition remplie
        assert_eq!(
            policy.pick_migration_type(1, true, false),
            MigrationType::Cold
        );
    }

    #[test]
    fn test_migration_type_serialization() {
        let req = MigrationRequest {
            vm_id: 101,
            source: "node-a".into(),
            target: "node-b".into(),
            mtype: MigrationType::Live,
            reason: MigrationReason::AdminRequest,
            with_local_disks: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"live\""));
        assert!(json.contains("\"admin_request\""));
    }

    #[test]
    fn test_migration_reason_memory_pressure() {
        let reason = MigrationReason::MemoryPressure {
            node_used_pct: 92.0,
            target_free_pct: 40.0,
        };
        let json = serde_json::to_string(&reason).unwrap();
        assert!(json.contains("memory_pressure"));
        assert!(json.contains("92.0"));
    }

    #[test]
    fn test_update_cpu_usage_and_idle_detection() {
        let policy = make_policy();
        assert!(!policy.is_vm_idle(42)); // inconnu → pas idle

        policy.update_cpu_usage(42, 3.0); // < 5% seuil
        assert!(policy.is_vm_idle(42)); // maintenant idle

        policy.update_cpu_usage(42, 80.0); // charge élevée
        assert!(!policy.is_vm_idle(42)); // plus idle
    }

    #[tokio::test]
    async fn test_executor_tracks_tasks() {
        // Vérifie que spawn retourne des IDs consécutifs et que status est consultable.
        // Pas d'exécution réelle de qm — on teste la mécanique de tracking.
        let metrics = Arc::new(node_bc_store::metrics::StoreMetrics::default());
        let store = Arc::new(node_bc_store::store::PageStore::new(Arc::clone(&metrics)));
        let vm_tracker = Arc::new(crate::vm_tracker::VmTracker::new(
            "/var/run/qemu-server".into(),
            "/etc/pve/qemu-server".into(),
        ));
        let state = Arc::new(crate::node_state::NodeState::new(
            "node-test".into(),
            "0.0.0.0:7100".into(),
            "0.0.0.0:7200".into(),
            store,
            metrics,
            vm_tracker,
            4,
            "/var/run/qemu-server".into(),
            None,
        ));
        let exec = MigrationExecutor::new(state);

        let req1 = MigrationRequest {
            vm_id: 101,
            source: "node-a".into(),
            target: "node-b".into(),
            mtype: MigrationType::Cold,
            reason: MigrationReason::AdminRequest,
            with_local_disks: false,
        };
        let req2 = MigrationRequest {
            vm_id: 102,
            ..req1.clone()
        };

        let id1 = exec.spawn(req1);
        let id2 = exec.spawn(req2);

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert!(exec.status(id1).is_some());
        assert!(exec.status(id2).is_some());
        assert_eq!(exec.list_all().len(), 2);
    }
}
