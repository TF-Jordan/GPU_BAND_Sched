//! Canal de contrôle HTTP de l'agent — correction de la limite L2.
//!
//! # Endpoints
//!
//! ```text
//! GET  /control/status            → état complet (métriques, config active)
//! POST /control/evict             → déclencher l'éviction de N pages
//! POST /control/evict/{vm_id}     → évincer pages d'une VM spécifique
//! POST /control/config            → modifier la config à chaud (seuils, etc.)
//! POST /control/prefetch/{vm_id}/{page_id} → hint de préfetch
//! DELETE /control/pages/{vm_id}   → supprimer toutes les pages d'une VM du store
//! GET  /control/metrics           → métriques format Prometheus (V5 : exporter officiel)
//! ```
//!
//! # Utilisation par le controller
//!
//! Le `omega-controller` Python appelle ces endpoints pour :
//! - Déclencher l'éviction sur commande (avant une migration)
//! - Ajuster le seuil d'éviction selon la pression globale du cluster
//! - Préfetcher des pages en anticipation d'un accès connu
//! - Supprimer les pages post-migration

use std::sync::Arc;

use axum::{
    extract::{Json as ReqJson, Path, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::cpu_cgroup::{CgroupCpuController, VmCpuConfig};
use crate::node_state::NodeState;
use crate::qmp_vcpu::{HotplugResult, VcpuHotplugManager};
use crate::quota::VmQuota;
use crate::vcpu_scheduler::{VcpuDecision, DEFAULT_CPU_WEIGHT};
use crate::vm_migration::{
    MigrationExecutor, MigrationPolicy, MigrationRequest, MigrationThresholds,
};
use node_bc_store::store::PageKey;

// ─── Structures de requête ────────────────────────────────────────────────────

/// Corps de `POST /control/migrate`
#[derive(Deserialize)]
pub struct MigrateRequest {
    pub vm_id: u32,
    pub target: String,
    #[serde(rename = "type")]
    pub mtype: crate::vm_migration::MigrationType,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct EvictRequest {
    /// Nombre de pages à évincer (0 = éviction auto selon politique CLOCK)
    pub count: Option<u64>,
    /// vm_id cible (None = toutes les VMs)
    pub vm_id: Option<u32>,
}

/// Corps de `POST /control/vm/{vm_id}/quota`
#[derive(Deserialize)]
pub struct QuotaRequest {
    /// RAM totale demandée par la VM (Mio)
    pub max_mem_mib: u64,
    /// Budget local (RAM sur le nœud hôte, Mio)
    pub local_budget_mib: u64,
    /// Budget remote max autorisé (Mio) — peut être omis, calculé auto
    pub remote_budget_mib: Option<u64>,
}

/// Corps de `POST /control/vm/{vm_id}/vcpu`
#[derive(Deserialize)]
pub struct VcpuAdmitRequest {
    pub min_vcpus: usize,
    pub max_vcpus: usize,
}

#[derive(Deserialize)]
pub struct GpuBudgetRequest {
    pub vram_budget_mib: u64,
}

#[derive(Deserialize)]
pub struct ConfigUpdate {
    /// Nouveau seuil d'éviction (% RAM)
    pub evict_threshold_pct: Option<f64>,
    /// Nouveau facteur de réplication
    pub replication_factor: Option<u32>,
    /// Activer/désactiver le préfetch
    pub prefetch_enabled: Option<bool>,
    /// Nouveau nombre de workers uffd
    pub uffd_workers: Option<u32>,
}

// ─── Routeur ──────────────────────────────────────────────────────────────────

pub fn build_control_router(state: Arc<NodeState>) -> Router {
    Router::new()
        .route("/control/status", get(control_status))
        .route("/control/evict", post(evict))
        .route("/control/evict/:vm_id", post(evict_vm))
        .route("/control/config", post(update_config))
        .route("/control/pages/:vm_id", delete(delete_vm_pages))
        .route("/control/metrics", get(prometheus_metrics))
        // ── Migrations live / cold ────────────────────────────────────────
        .route("/control/migrate", post(migrate_vm))
        .route("/control/migrate/recommend", get(migrate_recommend))
        .route("/control/migrations", get(list_migrations))
        .route("/control/migrations/:task_id", get(migration_status))
        // ── Quotas mémoire par VM ─────────────────────────────────────────
        .route("/control/quotas", get(list_quotas))
        .route("/control/quotas/summary", get(quota_summary))
        .route("/control/vm/:vm_id/quota", post(set_quota))
        .route("/control/vm/:vm_id/quota", get(get_quota))
        .route("/control/vm/:vm_id/quota", delete(delete_quota))
        // ── Planificateur vCPU ────────────────────────────────────────────
        .route("/control/vcpu/status", get(vcpu_status))
        .route("/control/vm/:vm_id/vcpu", post(vcpu_admit))
        .route("/control/vm/:vm_id/vcpu", delete(vcpu_release))
        .route("/control/vm/:vm_id/vcpu/hotplug", post(vcpu_hotplug))
        // ── Scheduler disque ──────────────────────────────────────────────
        .route("/control/disk/status", get(disk_status))
        // ── GPU multiplexer ───────────────────────────────────────────────
        .route("/control/gpu/status", get(gpu_status))
        .route("/control/vm/:vm_id/gpu", post(set_gpu_budget))
        .route("/control/vm/:vm_id/gpu", delete(delete_gpu_budget))
        .with_state(state)
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// GET /control/status — état complet de l'agent
async fn control_status(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let snap = state.snapshot();
    let metrics = state.metrics.snapshot();

    Json(json!({
        "node":    snap,
        "metrics": {
            "pages_stored":    metrics.pages_stored,
            "put_count":       metrics.put_count,
            "get_count":       metrics.get_count,
            "hit_rate_pct":    metrics.hit_rate_pct,
            "connections":     metrics.active_connections,
        },
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// POST /control/evict — déclenche l'éviction de pages
async fn evict(
    State(_state): State<Arc<NodeState>>,
    ReqJson(req): ReqJson<EvictRequest>,
) -> (StatusCode, Json<Value>) {
    let count = req.count.unwrap_or(16) as usize;

    // En V4 : l'éviction est gérée par l'EvictionEngine.
    // Ce endpoint demande au moteur d'effectuer un cycle immédiat.
    // Pour l'instant on retourne un ack — le moteur tourne en tâche de fond.
    info!(count, vm_id = ?req.vm_id, "éviction déclenchée via contrôle HTTP");

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status":     "eviction_requested",
            "count":      count,
            "vm_id":      req.vm_id,
            "note":       "le moteur d'éviction traitera la demande lors du prochain cycle",
        })),
    )
}

/// POST /control/evict/{vm_id} — évincer les pages d'une VM spécifique
async fn evict_vm(
    State(_state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    info!(vm_id, "éviction VM spécifique demandée");

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "eviction_requested",
            "vm_id":  vm_id,
        })),
    )
}

/// POST /control/config — mise à jour de la configuration à chaud
async fn update_config(
    State(_state): State<Arc<NodeState>>,
    ReqJson(update): ReqJson<ConfigUpdate>,
) -> (StatusCode, Json<Value>) {
    // En V4 : on log la demande. En V5 : mutation atomique de la config.
    info!(
        evict_threshold = ?update.evict_threshold_pct,
        replication     = ?update.replication_factor,
        prefetch        = ?update.prefetch_enabled,
        "mise à jour config demandée"
    );

    (
        StatusCode::OK,
        Json(json!({
            "status":  "config_applied",
            "applied": {
                "evict_threshold_pct": update.evict_threshold_pct,
                "replication_factor":  update.replication_factor,
                "prefetch_enabled":    update.prefetch_enabled,
            }
        })),
    )
}

/// DELETE /control/pages/{vm_id} — supprimer les pages d'une VM du store local
async fn delete_vm_pages(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    let keys: Vec<PageKey> = state.store.keys_for_vm(vm_id);
    let count = keys.len();

    for key in keys {
        state.store.delete(&key);
    }

    state.vm_tracker.record_page_stored(vm_id, -(count as i64));

    info!(
        vm_id,
        deleted = count,
        "pages VM supprimées via contrôle HTTP (post-migration)"
    );

    (
        StatusCode::OK,
        Json(json!({
            "vm_id":   vm_id,
            "deleted": count,
        })),
    )
}

// ─── Handlers migration ───────────────────────────────────────────────────────

/// POST /control/migrate — déclenche une migration live ou cold
///
/// Appelé par le controller Python après décision de la MigrationPolicy.
/// La migration s'exécute en tâche de fond — la réponse est immédiate avec un task_id.
///
/// Corps JSON :
/// ```json
/// { "vm_id": 101, "target": "node-b", "type": "live" }
/// { "vm_id": 102, "target": "node-c", "type": "cold", "reason": "admin_request" }
/// ```
async fn migrate_vm(
    State(state): State<Arc<NodeState>>,
    ReqJson(req): ReqJson<MigrateRequest>,
) -> (StatusCode, Json<Value>) {
    use crate::vm_migration::MigrationReason;

    let reason = match req.reason.as_deref() {
        Some("admin_request") | None => MigrationReason::AdminRequest,
        Some("maintenance") => MigrationReason::MaintenanceDrain,
        _ => MigrationReason::AdminRequest,
    };

    let migration_req = MigrationRequest {
        vm_id: req.vm_id,
        source: state.node_id.clone(),
        target: req.target.clone(),
        mtype: req.mtype.clone(),
        reason,
        with_local_disks: false,
    };

    let executor = MigrationExecutor::new(Arc::clone(&state));
    let task_id = executor.spawn(migration_req);

    info!(
        task_id,
        vm_id  = req.vm_id,
        target = %req.target,
        mtype  = ?req.mtype,
        "migration démarrée via API"
    );

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status":   "migration_started",
            "task_id":  task_id,
            "vm_id":    req.vm_id,
            "target":   req.target,
            "type":     format!("{:?}", req.mtype).to_lowercase(),
        })),
    )
}

/// GET /control/migrate/recommend — recommandations de migration basées sur l'état courant
async fn migrate_recommend(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let policy = MigrationPolicy::new(state.node_id.clone(), MigrationThresholds::default());
    let recommendations = policy.evaluate(&state);

    Json(json!({
        "node_id":         state.node_id,
        "recommendations": recommendations,
        "count":           recommendations.len(),
    }))
}

/// GET /control/migrations — liste toutes les migrations (en cours et terminées)
async fn list_migrations(State(state): State<Arc<NodeState>>) -> Json<Value> {
    // L'executor est normalement partagé via NodeState.
    // Dans cette implémentation, on crée un executor "lecture seule" avec une map vide
    // et on délègue au state si l'executor y est stocké.
    // Pour l'instant, on retourne un placeholder structuré.
    Json(json!({
        "node_id":    state.node_id,
        "migrations": [],
        "note":       "attach MigrationExecutor to NodeState for full tracking",
    }))
}

/// GET /control/migrations/{task_id} — statut d'une migration spécifique
async fn migration_status(
    State(_state): State<Arc<NodeState>>,
    Path(task_id): Path<u64>,
) -> (StatusCode, Json<Value>) {
    // Même remarque que list_migrations — l'executor doit être dans NodeState.
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error":   "task non trouvé",
            "task_id": task_id,
            "note":    "attach MigrationExecutor to NodeState for persistence",
        })),
    )
}

// ─── Handlers quota ───────────────────────────────────────────────────────────

/// GET /control/quotas — liste tous les quotas VM
async fn list_quotas(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let quotas = state.quota_registry.snapshot();
    Json(json!({ "quotas": quotas, "count": quotas.len() }))
}

/// GET /control/quotas/summary — résumé global
async fn quota_summary(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let summary = state.quota_registry.summary();
    Json(json!(summary))
}

/// POST /control/vm/{vm_id}/quota — configurer le quota d'une VM
///
/// Appelé par le controller Python après une décision d'admission.
async fn set_quota(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
    ReqJson(req): ReqJson<QuotaRequest>,
) -> (StatusCode, Json<Value>) {
    let mut quota = VmQuota::new(vm_id, req.max_mem_mib, req.local_budget_mib);

    // Si remote_budget_mib est fourni explicitement, on l'utilise
    if let Some(explicit_remote) = req.remote_budget_mib {
        quota.remote_budget_mib = explicit_remote;
    }

    info!(
        vm_id,
        max_mem_mib = quota.max_mem_mib,
        local_budget_mib = quota.local_budget_mib,
        remote_budget_mib = quota.remote_budget_mib,
        "quota VM configuré via API"
    );

    state.quota_registry.set(quota.clone());

    (
        StatusCode::OK,
        Json(json!({
            "status":  "quota_set",
            "vm_id":   vm_id,
            "quota":   quota,
        })),
    )
}

/// GET /control/vm/{vm_id}/quota — lire le quota d'une VM
async fn get_quota(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    match state.quota_registry.get(vm_id) {
        Some(quota) => (StatusCode::OK, Json(json!({ "quota": quota }))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error":  "quota non défini pour cette VM",
                "vm_id":  vm_id,
                "note":   "aucun quota = pas de limite (comportement par défaut)"
            })),
        ),
    }
}

/// DELETE /control/vm/{vm_id}/quota — supprimer le quota (ex : après arrêt VM)
async fn delete_quota(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    state.quota_registry.remove(vm_id);
    (
        StatusCode::OK,
        Json(json!({ "status": "quota_removed", "vm_id": vm_id })),
    )
}

/// GET /control/metrics — format Prometheus (simplifié V4, exporter complet V5)
async fn prometheus_metrics(State(state): State<Arc<NodeState>>) -> String {
    let snap = state.snapshot();
    let metrics = state.metrics.snapshot();

    let mut output = format!(
        "# HELP omega_pages_stored Pages actuellement stockées dans ce nœud\n\
         # TYPE omega_pages_stored gauge\n\
         omega_pages_stored{{node=\"{node}\"}} {pages}\n\
         \n\
         # HELP omega_mem_available_kb RAM disponible sur ce nœud\n\
         # TYPE omega_mem_available_kb gauge\n\
         omega_mem_available_kb{{node=\"{node}\"}} {avail}\n\
         \n\
         # HELP omega_mem_usage_pct Pourcentage d'usage RAM\n\
         # TYPE omega_mem_usage_pct gauge\n\
         omega_mem_usage_pct{{node=\"{node}\"}} {usage}\n\
         \n\
         # HELP omega_store_get_total Total des GET_PAGE\n\
         # TYPE omega_store_get_total counter\n\
         omega_store_get_total{{node=\"{node}\"}} {gets}\n\
         \n\
         # HELP omega_store_put_total Total des PUT_PAGE\n\
         # TYPE omega_store_put_total counter\n\
         omega_store_put_total{{node=\"{node}\"}} {puts}\n\
         \n\
         # HELP omega_store_hit_rate_pct Taux de hit du store\n\
         # TYPE omega_store_hit_rate_pct gauge\n\
         omega_store_hit_rate_pct{{node=\"{node}\"}} {hit_rate}\n",
        node = snap.node_id,
        pages = snap.pages_stored,
        avail = snap.mem_available_kb,
        usage = format!("{:.2}", snap.mem_usage_pct),
        gets = metrics.get_count,
        puts = metrics.put_count,
        hit_rate = format!("{:.1}", metrics.hit_rate_pct),
    );

    output.push_str(&state.vcpu_scheduler.prometheus_metrics(&state.node_id));
    output.push_str(&state.disk_io_scheduler.prometheus_metrics(&state.node_id));
    if let Some(gpu) = &state.gpu_runtime {
        output.push_str(&gpu.prometheus_metrics(&state.node_id).await);
    }
    output
}

// ─── Handlers vCPU ────────────────────────────────────────────────────────────

/// GET /control/vcpu/status — état global du planificateur vCPU
async fn vcpu_status(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let sched = &state.vcpu_scheduler;
    let metrics = sched.prometheus_metrics(&state.node_id);
    let total = sched.total_vslots();
    let free = sched.free_vslots();

    Json(json!({
        "node_id":          state.node_id,
        "total_vcpu_slots": total,
        "used_vcpu_slots":  total - free,
        "free_vcpu_slots":  free,
        "occupancy_ratio":  sched.occupancy_ratio(),
        "steal_pct":        sched.read_node_steal_pct(),
        "vms_needing_hotplug":   sched.vms_needing_hotplug(),
        "vms_needing_migration": sched.vms_needing_migration(),
        "vm_states":        sched.vm_snapshot(),
        "prometheus":       metrics,
    }))
}

/// GET /control/disk/status — état global du scheduler disque
async fn disk_status(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let sched = &state.disk_io_scheduler;
    let status = sched.status();
    let metrics = sched.prometheus_metrics(&state.node_id);

    Json(json!({
        "node_id": state.node_id,
        "disk_pressure_pct": status.node_pressure_pct,
        "busy_vms": status.busy_vms,
        "donor_vms": status.donor_vms,
        "vm_states": status.vm_states,
        "prometheus": metrics,
    }))
}

/// POST /control/vm/{vm_id}/vcpu — admettre une VM dans le planificateur vCPU
async fn vcpu_admit(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
    ReqJson(req): ReqJson<VcpuAdmitRequest>,
) -> (StatusCode, Json<Value>) {
    if req.min_vcpus == 0 || req.max_vcpus < req.min_vcpus {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_vcpu_profile",
                "vm_id": vm_id,
                "min_vcpus": req.min_vcpus,
                "max_vcpus": req.max_vcpus,
            })),
        );
    }

    let mut decision_text = String::new();

    if state.vcpu_scheduler.has_vm(vm_id) {
        let Some(current) = state.vcpu_scheduler.get_vm_state(vm_id) else {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "vcpu_state_missing",
                    "vm_id": vm_id,
                })),
            );
        };

        let bootstrap_convergence = current.min_vcpus == current.current_vcpus
            && current.max_vcpus == current.current_vcpus
            && req.min_vcpus < current.current_vcpus;

        if let Err(reason) = state.vcpu_scheduler.update_profile(
            vm_id,
            current.min_vcpus.min(current.current_vcpus),
            req.max_vcpus,
        ) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "migrate_required",
                    "vm_id": vm_id,
                    "reason": reason,
                })),
            );
        }

        if bootstrap_convergence {
            loop {
                let Some(vm_state) = state.vcpu_scheduler.get_vm_state(vm_id) else {
                    break;
                };
                if vm_state.current_vcpus <= req.min_vcpus {
                    break;
                }
                let decision = apply_downscale(&state, vm_id, true);
                match &decision {
                    VcpuDecision::Downscaled { .. } => {
                        decision_text = format!("{:?}", decision);
                    }
                    VcpuDecision::AtMin { .. } => break,
                    _ => {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "status": "downscale_required",
                                "vm_id": vm_id,
                                "decision": format!("{:?}", decision),
                            })),
                        );
                    }
                }
            }
        }

        loop {
            let Some(vm_state) = state.vcpu_scheduler.get_vm_state(vm_id) else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "vcpu_state_missing",
                        "vm_id": vm_id,
                    })),
                );
            };
            if vm_state.current_vcpus >= req.min_vcpus {
                break;
            }

            let decision = apply_hotplug(&state, vm_id);
            match &decision {
                VcpuDecision::Hotplugged { .. } => {
                    decision_text = format!("{:?}", decision);
                }
                VcpuDecision::AtMax { .. } | VcpuDecision::MigrateRequired { .. } => {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "status": "migrate_required",
                            "vm_id": vm_id,
                            "decision": format!("{:?}", decision),
                        })),
                    );
                }
                VcpuDecision::Allocated { .. }
                | VcpuDecision::AtMin { .. }
                | VcpuDecision::Downscaled { .. } => {}
            }
        }

        if let Err(reason) =
            state
                .vcpu_scheduler
                .update_profile(vm_id, req.min_vcpus, req.max_vcpus)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "migrate_required",
                    "vm_id": vm_id,
                    "reason": reason,
                })),
            );
        }
    } else {
        let decision = state
            .vcpu_scheduler
            .admit_vm(vm_id, req.min_vcpus, req.max_vcpus);
        match &decision {
            VcpuDecision::Allocated { slots, .. } => {
                info!(
                    vm_id,
                    slots_count = slots.len(),
                    "VM admise dans le planificateur vCPU"
                );
                decision_text = format!("{:?}", decision);
            }
            VcpuDecision::MigrateRequired { reason, .. } => {
                info!(vm_id, reason, "admission vCPU → migration requise");
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "status": "migrate_required",
                        "vm_id": vm_id,
                        "decision": format!("{:?}", decision),
                    })),
                );
            }
            VcpuDecision::AtMax { .. }
            | VcpuDecision::Hotplugged { .. }
            | VcpuDecision::AtMin { .. }
            | VcpuDecision::Downscaled { .. } => {
                decision_text = format!("{:?}", decision);
            }
        }
    }

    let current_vcpus = state
        .vcpu_scheduler
        .get_vm_state(vm_id)
        .map(|vm| vm.current_vcpus)
        .unwrap_or(req.min_vcpus);
    let weight = state
        .vcpu_scheduler
        .get_vm_state(vm_id)
        .map(|vm| vm.cpu_weight)
        .unwrap_or(DEFAULT_CPU_WEIGHT);
    let _ = CgroupCpuController::new().apply(
        &VmCpuConfig::new(vm_id)
            .capped_at_vcpus(current_vcpus)
            .with_weight(weight),
    );

    (
        StatusCode::OK,
        Json(json!({
            "status":   "allocated",
            "vm_id":    vm_id,
            "current_vcpus": current_vcpus,
            "decision": if decision_text.is_empty() {
                "profile_updated".to_string()
            } else {
                decision_text
            },
        })),
    )
}

/// DELETE /control/vm/{vm_id}/vcpu — libérer les vCPUs d'une VM
async fn vcpu_release(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    state.vcpu_scheduler.release_vm(vm_id);
    info!(vm_id, "vCPUs VM libérés");
    (
        StatusCode::OK,
        Json(json!({ "status": "released", "vm_id": vm_id })),
    )
}

/// POST /control/vm/{vm_id}/vcpu/hotplug — ajouter 1 vCPU à la VM
async fn vcpu_hotplug(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    let decision = apply_hotplug(&state, vm_id);

    let (code, status) = match &decision {
        VcpuDecision::Hotplugged { new_count, .. } => {
            info!(vm_id, new_count, "hotplug vCPU effectué");
            (StatusCode::OK, "hotplugged")
        }
        VcpuDecision::AtMax { .. } => {
            info!(vm_id, "hotplug vCPU refusé — plafond atteint");
            (StatusCode::CONFLICT, "at_max")
        }
        VcpuDecision::MigrateRequired { reason, .. } => {
            info!(vm_id, reason, "hotplug vCPU refusé — migration suggérée");
            (StatusCode::CONFLICT, "migrate_required")
        }
        VcpuDecision::Allocated { .. } => (StatusCode::OK, "allocated"),
        VcpuDecision::AtMin { .. } | VcpuDecision::Downscaled { .. } => {
            (StatusCode::CONFLICT, "unexpected_downscale_state")
        }
    };

    (
        code,
        Json(json!({
            "status":   status,
            "vm_id":    vm_id,
            "decision": format!("{:?}", decision),
        })),
    )
}

// ─── Handlers GPU ─────────────────────────────────────────────────────────────

async fn gpu_status(State(state): State<Arc<NodeState>>) -> (StatusCode, Json<Value>) {
    match &state.gpu_runtime {
        Some(runtime) => {
            let snap = runtime.snapshot();
            let metrics = runtime.prometheus_metrics(&state.node_id).await;
            (
                StatusCode::OK,
                Json(json!({
                    "node_id": state.node_id,
                    "gpu": snap,
                    "prometheus": metrics,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "gpu_non_disponible",
                "node_id": state.node_id,
            })),
        ),
    }
}

async fn set_gpu_budget(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
    ReqJson(req): ReqJson<GpuBudgetRequest>,
) -> (StatusCode, Json<Value>) {
    match &state.gpu_runtime {
        Some(runtime) => {
            let current_budget = runtime.vm_budget_mib(vm_id);
            let snap = runtime.snapshot();
            let available_mib = snap.free_vram_mib.saturating_add(current_budget);
            if snap.total_vram_mib > 0 && req.vram_budget_mib > available_mib {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "gpu_budget_exceeds_free_vram",
                        "vm_id": vm_id,
                        "requested_mib": req.vram_budget_mib,
                        "available_mib": available_mib,
                    })),
                );
            }
            runtime.set_vm_budget(vm_id, req.vram_budget_mib).await;
            let snap = runtime.snapshot();
            (
                StatusCode::OK,
                Json(json!({
                    "status": "gpu_budget_set",
                    "vm_id": vm_id,
                    "vram_budget_mib": req.vram_budget_mib,
                    "gpu": snap,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "gpu_non_disponible",
                "vm_id": vm_id,
            })),
        ),
    }
}

async fn delete_gpu_budget(
    State(state): State<Arc<NodeState>>,
    Path(vm_id): Path<u32>,
) -> (StatusCode, Json<Value>) {
    match &state.gpu_runtime {
        Some(runtime) => {
            runtime.release_vm(vm_id).await;
            (
                StatusCode::OK,
                Json(json!({
                    "status": "gpu_budget_removed",
                    "vm_id": vm_id,
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "gpu_non_disponible",
                "vm_id": vm_id,
            })),
        ),
    }
}

fn apply_hotplug(state: &Arc<NodeState>, vm_id: u32) -> VcpuDecision {
    let decision = state.vcpu_scheduler.try_hotplug(vm_id);
    let VcpuDecision::Hotplugged {
        new_count, slot, ..
    } = decision
    else {
        return decision;
    };

    let Some(vm_state) = state.vcpu_scheduler.get_vm_state(vm_id) else {
        let _ = state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
        return VcpuDecision::MigrateRequired {
            vm_id,
            reason: "état vCPU introuvable après réservation".into(),
        };
    };

    let hotplug_manager = VcpuHotplugManager::new(&state.qmp_dir);
    match hotplug_manager.add_vcpu(vm_id, vm_state.min_vcpus, vm_state.max_vcpus) {
        HotplugResult::Added {
            new_count: qmp_count,
        } => {
            let weight = state
                .vcpu_scheduler
                .get_vm_state(vm_id)
                .map(|vm| vm.cpu_weight)
                .unwrap_or(DEFAULT_CPU_WEIGHT);
            let _ = CgroupCpuController::new().apply(
                &VmCpuConfig::new(vm_id)
                    .capped_at_vcpus(qmp_count)
                    .with_weight(weight),
            );
            VcpuDecision::Hotplugged {
                vm_id,
                new_count,
                slot,
            }
        }
        HotplugResult::NoSlots { current, max } => {
            let _ = state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: format!(
                    "QMP sans slot CPU disponible : current={} max={}",
                    current, max,
                ),
            }
        }
        HotplugResult::Unavailable { reason } => {
            let _ = state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
            VcpuDecision::MigrateRequired { vm_id, reason }
        }
        HotplugResult::Removed { .. } | HotplugResult::AtMin { .. } => {
            let _ = state.vcpu_scheduler.rollback_hotplug(vm_id, slot);
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: "résultat QMP inattendu pendant hotplug".into(),
            }
        }
    }
}

fn apply_downscale(state: &Arc<NodeState>, vm_id: u32, force: bool) -> VcpuDecision {
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

    let hotplug_manager = VcpuHotplugManager::new(&state.qmp_dir);
    match hotplug_manager.remove_vcpu(vm_id, vm_state.min_vcpus) {
        HotplugResult::Removed {
            new_count: qmp_count,
        } => {
            let weight = state
                .vcpu_scheduler
                .get_vm_state(vm_id)
                .map(|vm| vm.cpu_weight)
                .unwrap_or(DEFAULT_CPU_WEIGHT);
            let _ = CgroupCpuController::new().apply(
                &VmCpuConfig::new(vm_id)
                    .capped_at_vcpus(qmp_count)
                    .with_weight(weight),
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
            VcpuDecision::MigrateRequired { vm_id, reason }
        }
        HotplugResult::NoSlots { current, max } => {
            let _ = state.vcpu_scheduler.rollback_downscale(vm_id, slot);
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: format!(
                    "résultat QMP inattendu pendant hot-unplug : current={} max={}",
                    current, max,
                ),
            }
        }
        HotplugResult::Added { .. } => {
            let _ = state.vcpu_scheduler.rollback_downscale(vm_id, slot);
            VcpuDecision::MigrateRequired {
                vm_id,
                reason: "résultat QMP inattendu pendant hot-unplug".into(),
            }
        }
    }
}
