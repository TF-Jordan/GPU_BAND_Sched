//! Serveur HTTP minimal exposant les métriques de l'agent (item 2).
//!
//! GET /metrics → JSON (métriques atomiques + état cluster)
//! GET /health  → 200 OK si l'agent est vivant
//!
//! Port par défaut : 0.0.0.0:9300

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::cluster::ClusterState;
use crate::memory::MemoryRegion;
use crate::metrics::AgentMetrics;

pub async fn run(
    bind_addr: String,
    metrics: Arc<AgentMetrics>,
    cluster: Arc<ClusterState>,
    region: Arc<MemoryRegion>,
) -> Result<()> {
    let listener = TcpListener::bind(&bind_addr).await?;
    info!(addr = %bind_addr, "serveur métriques démarré");

    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let metrics = metrics.clone();
                let cluster = cluster.clone();
                let region = region.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 256];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]);

                    let (status, body) = if req.starts_with("GET /metrics") {
                        let body = build_metrics_json(&metrics, &cluster, &region).await;
                        ("200 OK", body)
                    } else if req.starts_with("GET /health") {
                        ("200 OK", r#"{"status":"ok"}"#.to_string())
                    } else {
                        ("404 Not Found", r#"{"error":"not found"}"#.to_string())
                    };

                    let resp = format!(
                        "HTTP/1.0 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    debug!(peer = %peer, "métriques servies");
                });
            }
            Err(e) => warn!(error = %e, "accept métriques échoué"),
        }
    }
}

async fn build_metrics_json(
    metrics: &AgentMetrics,
    cluster: &ClusterState,
    region: &MemoryRegion,
) -> String {
    let snap = metrics.snapshot();

    // Snapshot cluster : capacités actuelles des stores + GPU + disque
    let targets = cluster.select_eviction_targets().await;
    let snapshots = cluster.snapshot().await;
    let stores_json: Vec<String> = snapshots
        .iter()
        .map(|n| {
            let pages = targets.iter().find(|(i, _)| *i == n.store_idx)
                .map(|(_, p)| *p)
                .unwrap_or(0);
            let (has_gpu, gpu_count, disk_avail, disk_total) = n.last_status
                .as_ref()
                .map(|s| (s.has_gpu, s.gpu_count, s.disk_available_mib, s.disk_total_mib))
                .unwrap_or((false, 0, 0, 0));
            format!(
                r#"{{"store_idx":{idx},"pages_available":{pages},"has_gpu":{has_gpu},"gpu_count":{gpu_count},"disk_available_mib":{disk_avail},"disk_total_mib":{disk_total}}}"#,
                idx = n.store_idx,
            )
        })
        .collect();

    let remote_count = region.remote_count();
    let remote_cap = region.remote_cap();
    let vm_id = region.vm_id;

    format!(
        r#"{{"vm_id":{vm_id},"fault_count":{fault_count},"fault_served":{fault_served},"fault_errors":{fault_errors},"pages_evicted":{pages_evicted},"pages_fetched":{pages_fetched},"pages_recalled":{pages_recalled},"fetch_zeros":{fetch_zeros},"local_present":{local_present},"remote_count":{remote_count},"remote_cap":{remote_cap},"eviction_alerts":{eviction_alerts},"migration_searches":{migration_searches},"stores":[{stores}]}}"#,
        fault_count = snap.fault_count,
        fault_served = snap.fault_served,
        fault_errors = snap.fault_errors,
        pages_evicted = snap.pages_evicted,
        pages_fetched = snap.pages_fetched,
        pages_recalled = snap.pages_recalled,
        fetch_zeros = snap.fetch_zeros,
        local_present = snap.local_present,
        eviction_alerts = snap.eviction_alerts,
        migration_searches = snap.migration_searches,
        stores = stores_json.join(","),
    )
}
