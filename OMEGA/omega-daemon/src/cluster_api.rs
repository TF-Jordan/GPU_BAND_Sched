//! API HTTP du cluster — exposée par chaque nœud via axum.
//!
//! # Endpoints
//!
//! ```text
//! GET  /api/status          → NodeStatus JSON (RAM, VMs, pages, etc.)
//! GET  /api/pages           → [{vmid, pages_stored}] pour ce nœud
//! GET  /api/health          → {"ok": true}
//! DELETE /api/pages/{vmid}  → supprime toutes les pages d'une VM (post-migration)
//! ```
//!
//! Ces endpoints sont consommés par le `omega-controller` Python pour construire
//! l'état global du cluster et prendre des décisions de migration.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get},
    Router,
};
use serde_json::{json, Value};
use tracing::info;

use crate::node_state::NodeState;
use node_bc_store::store::PageKey;

/// Construit le routeur axum.
pub fn build_router(state: Arc<NodeState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(node_status))
        .route("/api/pages", get(pages_list))
        .route("/api/pages/:vmid", delete(delete_vm_pages))
        // Compat avec node-bc-store : mêmes noms de champs que son /status
        .route("/status", get(node_status_compat))
        .with_state(state)
}

/// Lance le serveur HTTP sur l'adresse configurée.
pub async fn run_api_server(state: Arc<NodeState>, addr: String) -> anyhow::Result<()> {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(addr = %addr, "API HTTP cluster démarrée");
    axum::serve(listener, app).await?;
    Ok(())
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// GET /api/health — vérification de vie minimale
async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

/// GET /api/status — état complet du nœud
async fn node_status(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let snap = state.snapshot();
    Json(serde_json::to_value(snap).unwrap_or_else(|_| json!({"error": "serialization"})))
}

/// GET /status — compat node-bc-store : mêmes champs que status_server.rs
async fn node_status_compat(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let snap = state.snapshot();
    let available_mib = snap.mem_available_kb / 1024;
    let total_mib = snap.mem_total_kb / 1024;
    let (has_gpu, gpu_count) = snap
        .gpu
        .as_ref()
        .map(|g| (g.enabled, if g.enabled { 1u32 } else { 0u32 }))
        .unwrap_or((false, 0));
    Json(json!({
        "node_id":           snap.node_id,
        "available_mib":     available_mib,
        "total_mib":         total_mib,
        "cpu_count":         snap.vcpu_total,
        "has_gpu":           has_gpu,
        "gpu_count":         gpu_count,
        "disk_available_mib": 0u64,
        "disk_total_mib":    0u64,
        "ceph_enabled":      false,
        "vcpu_total":        snap.vcpu_total,
        "vcpu_free":         snap.vcpu_free,
        "page_count":        snap.pages_stored,
    }))
}

/// GET /api/pages — pages stockées sur ce nœud, par vm_id
async fn pages_list(State(state): State<Arc<NodeState>>) -> Json<Value> {
    let pages = state.pages_per_vm();
    let entries: Vec<Value> = pages
        .iter()
        .map(|(vmid, count)| {
            json!({
                "vmid":         vmid,
                "pages_stored": count,
                "mem_kb":       count * 4,
            })
        })
        .collect();
    Json(json!({"pages": entries, "node_id": state.node_id}))
}

/// DELETE /api/pages/{vmid} — supprime toutes les pages d'une VM
///
/// Appelé par le controller après une migration réussie pour libérer la mémoire
/// du store qui n'est plus utile (la VM est maintenant sur un autre nœud avec
/// toute sa RAM locale).
async fn delete_vm_pages(
    State(state): State<Arc<NodeState>>,
    Path(vmid): Path<u32>,
) -> (StatusCode, Json<Value>) {
    // On parcourt le store et supprime toutes les pages de ce vm_id.
    // Le DashMap ne supporte pas de scan par préfixe directement —
    // on collecte les clés puis on supprime.
    let keys_to_delete: Vec<PageKey> = {
        // Reconstruction des clés à partir des page_ids connus via le tracker
        let _pages_count = state.vm_tracker.pages_stored_for(vmid);
        // Note: pour une suppression complète en V4, on itère les clés du store.
        // Le DashMap expose iter() pour ça.
        state.store.keys_for_vm(vmid)
    };

    let deleted = keys_to_delete.len();
    for key in keys_to_delete {
        state.store.delete(&key);
    }

    // Mise à jour du tracker
    state.vm_tracker.record_page_stored(vmid, -(deleted as i64));

    info!(vmid, deleted, "pages VM supprimées post-migration");

    (
        StatusCode::OK,
        Json(json!({
            "vmid":    vmid,
            "deleted": deleted,
            "node_id": state.node_id,
        })),
    )
}
