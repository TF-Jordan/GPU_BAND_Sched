//! Nettoyage des pages orphelines sur le store.
//!
//! ## Problème
//!
//! Quand un agent crash (SIGKILL, panne hôte) sans passer par l'arrêt propre,
//! `purge_remote_pages()` n'est jamais appelé. Les pages de cette VM restent
//! sur le store indéfiniment, consommant de la RAM ou du disque.
//!
//! ## Détection
//!
//! Toutes les `check_interval` secondes :
//! 1. Lister les vm_ids qui ont des pages dans ce store.
//! 2. Interroger Proxmox via `pvesh` pour savoir quels vmids existent encore
//!    dans le cluster (running, stopped, paused — tous états).
//! 3. Un vmid est "orphelin" s'il est dans le store mais ABSENT de Proxmox.
//!
//! ## Grâce
//!
//! Un vmid orphelin n'est supprimé qu'après `grace_period` secondes consécutives
//! d'absence dans Proxmox. Cela couvre :
//! - Les migrations en cours (la VM est temporairement absente du cluster vus d'un nœud)
//! - Les redémarrages rapides
//!
//! ## Limitation Ceph
//!
//! `AnyStore::list_vm_ids()` retourne vide pour le backend Ceph (listing RADOS
//! nécessite une API spécifique non encore câblée). Le cleaner est donc actif
//! uniquement pour le backend RAM.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::server::AnyStore;

pub struct OrphanCleaner {
    store: Arc<AnyStore>,
    check_interval: Duration,
    grace_period: Duration,
}

impl OrphanCleaner {
    pub fn new(store: Arc<AnyStore>, check_interval_secs: u64, grace_secs: u64) -> Self {
        Self {
            store,
            check_interval: Duration::from_secs(check_interval_secs.max(60)),
            grace_period: Duration::from_secs(grace_secs),
        }
    }

    pub async fn run(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        // vm_id → première fois qu'on l'a vu absent de Proxmox
        let mut absent_since: HashMap<u32, Instant> = HashMap::new();

        info!(
            check_interval_s = self.check_interval.as_secs(),
            grace_s = self.grace_period.as_secs(),
            "démon nettoyage orphelins démarré"
        );

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            sleep(self.check_interval).await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            self.cleanup_pass(&mut absent_since).await;
        }

        info!("démon nettoyage orphelins arrêté");
    }

    async fn cleanup_pass(&self, absent_since: &mut HashMap<u32, Instant>) {
        // 1. VM ids présents dans le store
        let store_vmids = self.store.list_vm_ids();
        if store_vmids.is_empty() {
            debug!("store vide — aucun orphelin possible");
            return;
        }

        // 2. VM ids connus de Proxmox (tous états)
        let cluster_vmids = match list_proxmox_vmids().await {
            Ok(ids) => ids,
            Err(e) => {
                warn!(error = %e, "impossible d'interroger Proxmox — nettoyage reporté");
                return;
            }
        };

        let now = Instant::now();

        // 3a. Retirer les VMs qui sont revenues dans Proxmox
        absent_since.retain(|vmid, _| !cluster_vmids.contains(vmid));

        // 3b. Enregistrer les nouveaux orphelins potentiels
        for &vmid in &store_vmids {
            if !cluster_vmids.contains(&vmid) {
                absent_since.entry(vmid).or_insert(now);
                debug!(vmid, "vmid absent de Proxmox — décompte grâce");
            }
        }

        // 4. Supprimer ceux dont le délai de grâce est dépassé
        let to_delete: Vec<u32> = absent_since
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= self.grace_period)
            .map(|(&vmid, _)| vmid)
            .collect();

        for vmid in to_delete {
            let n = self.store.delete_vm(vmid).await;
            if n > 0 {
                info!(
                    vmid,
                    pages_deleted = n,
                    "pages orphelines supprimées (VM absente de Proxmox depuis ≥ grâce)"
                );
            }
            absent_since.remove(&vmid);
        }

        if !absent_since.is_empty() {
            debug!(
                candidates = absent_since.len(),
                "vmids orphelins en attente de grâce"
            );
        }
    }
}

// ─── Interrogation Proxmox ────────────────────────────────────────────────────

/// Liste tous les vmids existant dans le cluster Proxmox (tous états confondus).
///
/// Utilise `pvesh get /cluster/resources --type vm --output-format json`.
/// Retourne une erreur si `pvesh` n'est pas disponible ou si la sortie est invalide.
async fn list_proxmox_vmids() -> anyhow::Result<Vec<u32>> {
    let out = tokio::process::Command::new("pvesh")
        .args([
            "get",
            "/cluster/resources",
            "--type",
            "vm",
            "--output-format",
            "json",
        ])
        .output()
        .await?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("pvesh cluster/resources : {err}");
    }

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let ids = json
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|item| item.get("vmid")?.as_u64())
        .map(|id| id as u32)
        .collect();

    Ok(ids)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grace_period_not_yet_elapsed() {
        let mut absent_since: HashMap<u32, Instant> = HashMap::new();
        let grace = Duration::from_secs(600);
        let now = Instant::now();

        // VM 42 absente depuis 1 seconde (< grâce)
        absent_since.insert(42, now - Duration::from_secs(1));

        let to_delete: Vec<u32> = absent_since
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= grace)
            .map(|(&vmid, _)| vmid)
            .collect();

        assert!(
            to_delete.is_empty(),
            "pas encore supprimée dans le délai de grâce"
        );
    }

    #[test]
    fn test_grace_period_elapsed() {
        let mut absent_since: HashMap<u32, Instant> = HashMap::new();
        let grace = Duration::from_secs(60);
        let now = Instant::now();

        // VM 99 absente depuis 120 s (> grâce)
        absent_since.insert(99, now - Duration::from_secs(120));

        let to_delete: Vec<u32> = absent_since
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= grace)
            .map(|(&vmid, _)| vmid)
            .collect();

        assert_eq!(to_delete, vec![99]);
    }

    #[test]
    fn test_returning_vm_removed_from_candidates() {
        let mut absent_since: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();

        absent_since.insert(10, now - Duration::from_secs(30));
        absent_since.insert(20, now - Duration::from_secs(30));

        // VM 10 est revenue dans Proxmox
        let cluster_vmids = vec![10u32, 30];
        absent_since.retain(|vmid, _| !cluster_vmids.contains(vmid));

        assert!(
            !absent_since.contains_key(&10),
            "VM 10 revenue → retirée des candidats"
        );
        assert!(absent_since.contains_key(&20), "VM 20 toujours absente");
    }

    #[test]
    fn test_new_orphan_registered_once() {
        let mut absent_since: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();

        // Deux passes : le timestamp ne doit pas être écrasé à la 2e passe
        absent_since
            .entry(55)
            .or_insert(now - Duration::from_secs(100));
        let first_ts = absent_since[&55];

        // Simule une 2e passe
        absent_since.entry(55).or_insert(now); // or_insert ne remplace pas si existant
        assert_eq!(
            absent_since[&55], first_ts,
            "timestamp préservé entre les passes"
        );
    }

    #[test]
    fn test_pvesh_json_parsing() {
        let json = serde_json::json!([
            {"vmid": 100, "type": "qemu", "status": "running"},
            {"vmid": 101, "type": "qemu", "status": "stopped"},
            {"vmid": 200, "type": "lxc",  "status": "running"},
        ]);
        let ids: Vec<u32> = json
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("vmid")?.as_u64())
            .map(|id| id as u32)
            .collect();
        assert_eq!(ids, vec![100, 101, 200]);
    }

    #[test]
    fn test_check_interval_minimum_60s() {
        let cleaner = OrphanCleaner::new(
            Arc::new(crate::server::AnyStore::Ram(Arc::new(
                crate::store::PageStore::new(Arc::new(crate::metrics::StoreMetrics::default())),
            ))),
            0, // 0 → clamped à 60
            300,
        );
        assert_eq!(cleaner.check_interval, Duration::from_secs(60));
    }

    #[test]
    fn test_grace_period_zero_means_immediate() {
        let grace = Duration::from_secs(0);
        let now = Instant::now();
        let since = now; // absent depuis exactement maintenant

        // >= 0 → éligible immédiatement
        assert!(now.duration_since(since) >= grace);
    }
}
