//! État du cluster — RAM disponible sur chaque nœud store.
//!
//! Interroge chaque nœud via le serveur HTTP /status (port 9200 par défaut).
//! La liste des `status_addrs` est parallèle à la liste des `stores` TCP :
//! status_addrs[i] correspond au store d'index i dans RemoteStorePool.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatus {
    pub node_id: String,
    pub available_mib: u64,
    pub total_mib: u64,
    pub cpu_count: u32,

    /// Le nœud store possède au moins un GPU (détection PCI générique).
    #[serde(default)]
    pub has_gpu: bool,
    /// Nombre de GPUs PCI détectés sur ce nœud (0 si aucun).
    #[serde(default)]
    pub gpu_count: u32,
    /// Espace disque disponible sur le répertoire de données du store (en Mio).
    #[serde(default)]
    pub disk_available_mib: u64,
    /// Espace disque total du répertoire de données du store (en Mio).
    #[serde(default)]
    pub disk_total_mib: u64,
    /// Le store utilise Ceph RADOS comme backend (auto-détecté au démarrage).
    #[serde(default)]
    pub ceph_enabled: bool,
    /// vCPUs totaux du nœud (physical_cores × overcommit_ratio).
    /// 0 = non reporté par ce nœud (compatibilité ancienne version).
    #[serde(default)]
    pub vcpu_total: u32,
    /// vCPUs libres sur ce nœud (non encore alloués à une VM).
    /// 0 = non reporté.
    #[serde(default)]
    pub vcpu_free: u32,
}

#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    pub store_idx: usize,
    pub store_addr: String,  // host:port TCP store
    pub status_addr: String, // host:port HTTP status
    pub last_status: Option<NodeStatus>,
}

pub struct ClusterState {
    nodes: Arc<RwLock<Vec<NodeSnapshot>>>,
    /// Ajustement local de capacité (en pages) par store, mis à jour
    /// immédiatement après chaque éviction/recall sans attendre le prochain
    /// refresh HTTP (item 3). Remis à zéro à chaque refresh.
    local_delta: Arc<Vec<AtomicI64>>,
}

impl ClusterState {
    pub fn new(store_addrs: Vec<String>, status_addrs: Vec<String>) -> Self {
        let n = store_addrs.len();
        let nodes = store_addrs
            .into_iter()
            .zip(status_addrs)
            .enumerate()
            .map(|(i, (store, status))| NodeSnapshot {
                store_idx: i,
                store_addr: store,
                status_addr: status,
                last_status: None,
            })
            .collect();
        let local_delta = Arc::new((0..n).map(|_| AtomicI64::new(0)).collect());
        Self {
            nodes: Arc::new(RwLock::new(nodes)),
            local_delta,
        }
    }

    /// Décrémente la capacité estimée d'un store après éviction d'une page (item 3).
    pub fn track_eviction(&self, store_idx: usize) {
        if let Some(d) = self.local_delta.get(store_idx) {
            d.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Incrémente la capacité estimée d'un store après recall ou purge d'une page (item 3).
    pub fn track_recall(&self, store_idx: usize) {
        if let Some(d) = self.local_delta.get(store_idx) {
            d.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Rafraîchit les statuts de tous les nœuds en parallèle.
    pub async fn refresh(&self) {
        let snapshots: Vec<(usize, String)> = self
            .nodes
            .read()
            .await
            .iter()
            .map(|n| (n.store_idx, n.status_addr.clone()))
            .collect();

        let futs: Vec<_> = snapshots
            .into_iter()
            .map(|(idx, addr)| async move { (idx, addr.clone(), http_get_status(&addr).await) })
            .collect();

        let results = futures::future::join_all(futs).await;

        let mut nodes = self.nodes.write().await;
        for (idx, addr, result) in results {
            match result {
                Ok(status) => {
                    debug!(idx, addr = %addr, avail = status.available_mib, "status ok");

                    // Alerte disque : < 5 % d'espace disponible
                    if status.disk_total_mib > 0 {
                        let pct = status.disk_available_mib * 100 / status.disk_total_mib;
                        if pct < 5 {
                            warn!(
                                idx,
                                addr           = %addr,
                                disk_avail_mib = status.disk_available_mib,
                                disk_total_mib = status.disk_total_mib,
                                disk_pct       = pct,
                                "ALERTE DISQUE : store bientôt plein (< 5 % disponible)"
                            );
                        } else {
                            debug!(
                                idx,
                                disk_avail_mib = status.disk_available_mib,
                                disk_pct = pct,
                                "disque store ok"
                            );
                        }
                    }

                    // Info GPU au premier refresh
                    if status.has_gpu {
                        info!(
                            idx,
                            addr      = %addr,
                            gpu_count = status.gpu_count,
                            "GPU détecté sur ce store"
                        );
                    }

                    if let Some(n) = nodes.iter_mut().find(|n| n.store_idx == idx) {
                        n.last_status = Some(status);
                        // Refresh reçu : les deltas locaux sont absorbés dans la nouvelle valeur.
                        if let Some(d) = self.local_delta.get(idx) {
                            d.store(0, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => warn!(idx, addr = %addr, error = %e, "status inaccessible"),
            }
        }
    }

    /// Retourne les stores triés par RAM disponible décroissante.
    /// Tuple : (store_idx, pages_disponibles_sur_ce_nœud)
    pub async fn select_eviction_targets(&self) -> Vec<(usize, usize)> {
        let nodes = self.nodes.read().await;
        let mut targets: Vec<(usize, usize)> = nodes
            .iter()
            .filter_map(|n| {
                n.last_status.as_ref().map(|s| {
                    // Capacité brute depuis le dernier refresh HTTP
                    let base = (s.available_mib.saturating_mul(1024) / 4) as i64;
                    // Delta local : pages évincées/rappelées depuis ce refresh
                    let delta = self
                        .local_delta
                        .get(n.store_idx)
                        .map(|d| d.load(Ordering::Relaxed))
                        .unwrap_or(0);
                    let pages = (base + delta).max(0) as usize;
                    (n.store_idx, pages)
                })
            })
            .filter(|(_, p)| *p > 0)
            .collect();
        targets.sort_by(|a, b| b.1.cmp(&a.1));
        targets
    }

    /// Snapshot complet pour la recherche de migration.
    pub async fn snapshot(&self) -> Vec<NodeSnapshot> {
        self.nodes.read().await.clone()
    }

    /// Retourne `true` si TOUS les nœuds connus ont `ceph_enabled = true`.
    ///
    /// Utilisé pour auto-désactiver la réplication write-through quand Ceph
    /// assure lui-même la disponibilité (min_size/size du pool RADOS).
    /// Retourne `false` si aucun nœud n'a encore reporté son statut.
    pub async fn all_ceph_enabled(&self) -> bool {
        let nodes = self.nodes.read().await;
        let reported: Vec<_> = nodes
            .iter()
            .filter_map(|n| n.last_status.as_ref())
            .collect();
        if reported.is_empty() {
            return false;
        }
        reported.iter().all(|s| s.ceph_enabled)
    }

    /// Nœuds dont le statut indique la présence d'un GPU.
    pub async fn gpu_nodes(&self) -> Vec<NodeSnapshot> {
        self.nodes
            .read()
            .await
            .iter()
            .filter(|n| n.last_status.as_ref().map(|s| s.has_gpu).unwrap_or(false))
            .cloned()
            .collect()
    }

    /// RAM disponible localement (sur le nœud courant).
    pub fn local_available_mib() -> u64 {
        local_available_mib()
    }
}

/// Lit /proc/meminfo, retourne MemAvailable en Mio.
pub fn local_available_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| kb / 1024)
        })
        .unwrap_or(0)
}

/// GET http://{addr}/status — client HTTP minimal sans dépendance externe.
async fn http_get_status(addr: &str) -> Result<NodeStatus> {
    let mut stream = timeout(Duration::from_secs(3), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("timeout connexion {addr}"))?
        .map_err(|e| anyhow::anyhow!("connexion {addr} : {e}"))?;

    let req = format!("GET /status HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    timeout(Duration::from_secs(2), stream.write_all(req.as_bytes()))
        .await
        .map_err(|_| anyhow::anyhow!("timeout write {addr}"))??;

    let mut buf = Vec::with_capacity(1024);
    timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout read {addr}"))??;

    let resp = String::from_utf8_lossy(&buf);
    let body = resp
        .split("\r\n\r\n")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("réponse HTTP malformée de {addr}"))?;

    serde_json::from_str::<NodeStatus>(body)
        .map_err(|e| anyhow::anyhow!("JSON invalide de {addr} : {e}"))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_select_eviction_targets_sorted_desc() {
        let state = ClusterState::new(
            vec!["10.10.0.12:9100".into(), "10.10.0.13:9100".into()],
            vec!["10.10.0.12:9200".into(), "10.10.0.13:9200".into()],
        );
        {
            let mut nodes = state.nodes.write().await;
            nodes[0].last_status = Some(NodeStatus {
                node_id: "pve2".into(),
                available_mib: 1024,
                total_mib: 8192,
                cpu_count: 4,
                has_gpu: false,
                gpu_count: 0,
                disk_available_mib: 0,
                disk_total_mib: 0,
                ceph_enabled: false,
                vcpu_total: 0,
                vcpu_free: 0,
            });
            nodes[1].last_status = Some(NodeStatus {
                node_id: "pve3".into(),
                available_mib: 4096,
                total_mib: 8192,
                cpu_count: 4,
                has_gpu: false,
                gpu_count: 0,
                disk_available_mib: 0,
                disk_total_mib: 0,
                ceph_enabled: false,
                vcpu_total: 0,
                vcpu_free: 0,
            });
        }
        let targets = state.select_eviction_targets().await;
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].0, 1, "pve3 (4096 MiB) doit être en premier");
        assert_eq!(targets[1].0, 0, "pve2 (1024 MiB) doit être en second");
    }

    #[tokio::test]
    async fn test_no_targets_when_statuses_unknown() {
        let state = ClusterState::new(
            vec!["10.10.0.12:9100".into()],
            vec!["10.10.0.12:9200".into()],
        );
        let targets = state.select_eviction_targets().await;
        assert!(targets.is_empty());
    }

    #[tokio::test]
    async fn test_zero_available_filtered_out() {
        let state = ClusterState::new(
            vec!["10.10.0.12:9100".into()],
            vec!["10.10.0.12:9200".into()],
        );
        {
            let mut nodes = state.nodes.write().await;
            nodes[0].last_status = Some(NodeStatus {
                node_id: "pve2".into(),
                available_mib: 0,
                total_mib: 8192,
                cpu_count: 4,
                has_gpu: false,
                gpu_count: 0,
                disk_available_mib: 0,
                disk_total_mib: 0,
                ceph_enabled: false,
                vcpu_total: 0,
                vcpu_free: 0,
            });
        }
        let targets = state.select_eviction_targets().await;
        assert!(targets.is_empty(), "nœud plein ne doit pas être cible");
    }

    #[test]
    fn test_local_available_mib_plausible() {
        let mib = local_available_mib();
        assert!(mib > 0);
        assert!(mib < 4 * 1024 * 1024);
    }
}
