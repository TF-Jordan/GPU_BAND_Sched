//! Serveur HTTP minimal exposant l'état du nœud store au reste du cluster.
//!
//! Port par défaut : STORE_STATUS_LISTEN (0.0.0.0:9200)
//!
//! GET /status → JSON {
//!   node_id, available_mib, total_mib, cpu_count,
//!   has_gpu, gpu_count,
//!   disk_available_mib, disk_total_mib,   ← Ceph si activé, sinon statvfs
//!   ceph_enabled
//! }
//!
//! Quand Ceph est activé, disk_available_mib/disk_total_mib reflètent la
//! capacité **du pool Ceph** (ressource partagée cluster), pas le disque local.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use crate::ceph_store::CephStore;
use crate::hardware;
use crate::metrics::StoreMetrics;

pub async fn run(
    bind_addr: String,
    node_id: String,
    data_path: String,
    ceph_store: Option<Arc<CephStore>>,
    metrics: Arc<StoreMetrics>,
) -> Result<()> {
    let listener = TcpListener::bind(&bind_addr).await?;
    info!(addr = %bind_addr, node = %node_id, "serveur HTTP status démarré");

    let gpu_info = hardware::detect_gpus();
    let has_gpu = gpu_info.present;
    let gpu_count = gpu_info.count;
    if has_gpu {
        info!(gpu_count, summary = %gpu_info.summary, "GPU(s) détecté(s) sur ce nœud");
    }

    let ceph_enabled = ceph_store.is_some();

    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let node_id = node_id.clone();
                let data_path = data_path.clone();
                let ceph = ceph_store.clone();
                let m = metrics.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 512];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]);

                    if !req.starts_with("GET /status") {
                        let _ = stream
                            .write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        return;
                    }

                    let page_count = m.pages_stored.load(Ordering::Relaxed);
                    let body = build_status_json(
                        &node_id,
                        has_gpu,
                        gpu_count,
                        &data_path,
                        ceph.as_deref(),
                        ceph_enabled,
                        page_count,
                    );
                    let resp = format!(
                        "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    debug!(peer = %peer, "status servi");
                });
            }
            Err(e) => warn!(error = %e, "accept status server échoué"),
        }
    }
}

fn build_status_json(
    node_id: &str,
    has_gpu: bool,
    gpu_count: u32,
    data_path: &str,
    ceph: Option<&CephStore>,
    ceph_enabled: bool,
    page_count: u64,
) -> String {
    let (avail_mib, total_mib) = read_mem_info_mib().unwrap_or((0, 0));
    let cpu_count = read_cpu_count();

    // Espace disque : pool Ceph (partagé cluster) ou disque local (par nœud)
    let (disk_avail_mib, disk_tot_mib) = if let Some(cs) = ceph {
        cs.cluster_stats_mib()
    } else {
        hardware::disk_space_mib(data_path)
    };

    // vCPUs : total = cœurs × 3 (overcommit ratio par défaut).
    // vcpu_free lu depuis le pool partagé si des agents tournent sur ce nœud.
    let (vcpu_total, vcpu_free) = read_vcpu_pool_info(cpu_count);

    format!(
        r#"{{"node_id":"{node_id}","available_mib":{avail_mib},"total_mib":{total_mib},"cpu_count":{cpu_count},"has_gpu":{has_gpu},"gpu_count":{gpu_count},"disk_available_mib":{disk_avail_mib},"disk_total_mib":{disk_tot_mib},"ceph_enabled":{ceph_enabled},"vcpu_total":{vcpu_total},"vcpu_free":{vcpu_free},"page_count":{page_count}}}"#
    )
}

const VCPU_OVERCOMMIT_RATIO: u32 = 3;
const VCPU_POOL_PATH: &str = "/run/omega-vcpu-pool.json";

fn read_vcpu_pool_info(cpu_count: u32) -> (u32, u32) {
    let total = cpu_count * VCPU_OVERCOMMIT_RATIO;
    // Tente de lire le pool si des agents tournent sur ce nœud
    let pool_json = std::fs::read_to_string(VCPU_POOL_PATH).ok();
    let free = pool_json
        .as_deref()
        .and_then(|s| {
            let v: serde_json::Value = serde_json::from_str(s).ok()?;
            let assigned: u32 = v["vms"]
                .as_object()?
                .values()
                .filter_map(|e| e["current_vcpus"].as_u64())
                .map(|n| n as u32)
                .sum();
            Some(total.saturating_sub(assigned))
        })
        .unwrap_or(total); // pas de pool → tout libre
    (total, free)
}

fn read_mem_info_mib() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut avail_kb = None;
    let mut total_kb = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        }
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
        }
        if avail_kb.is_some() && total_kb.is_some() {
            break;
        }
    }
    Some((avail_kb? / 1024, total_kb? / 1024))
}

fn read_cpu_count() -> u32 {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u32)
        .unwrap_or(1)
}
