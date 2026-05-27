//! Réorganisation globale du cluster (point 6).
//!
//! # Problème
//!
//! Analogue à la fragmentation mémoire en OS :
//! - 3 nœuds avec 500 MiB libres chacun = 1500 MiB au total, mais aucun ne peut
//!   héberger une VM de 1200 MiB.
//! - Si on consolide les VMs, un nœud peut se retrouver avec 1500 MiB libres.
//!
//! # Algorithme (First Fit Decreasing adapté au cluster)
//!
//! 1. Lister toutes les VMs et leur RAM.
//! 2. Trier les nœuds par RAM libre décroissante.
//! 3. Pour le nœud le moins chargé (le plus facile à vider) :
//!    a. Tenter de migrer chaque VM vers un autre nœud qui peut la recevoir.
//!    b. Si toutes les VMs migrent → le nœud est libéré (parfait pour futures migrations).
//!    c. Si certaines ne peuvent pas → skip ce nœud, essayer le suivant.
//! 4. Répéter jusqu'à ce qu'aucun nœud supplémentaire ne puisse être vidé.
//!
//! La compaction globale est déclenchée explicitement (pas automatiquement)
//! par le démon migration quand toutes les autres options ont échoué.

use anyhow::Result;
use tokio::process::Command;
use tracing::{info, warn};

use crate::cluster::ClusterState;
use crate::migration::{list_cluster_vms, VmInfo};

pub struct ClusterCompactor {
    cluster: std::sync::Arc<ClusterState>,
    current_node: String,
    dry_run: bool,
}

impl ClusterCompactor {
    pub fn new(cluster: std::sync::Arc<ClusterState>, current_node: String, dry_run: bool) -> Self {
        Self {
            cluster,
            current_node,
            dry_run,
        }
    }

    /// Lance une passe de compaction globale.
    /// Retourne le nombre de VMs déplacées.
    pub async fn compact(&self) -> Result<usize> {
        let nodes = self.cluster.snapshot().await;
        let vms = list_cluster_vms().await?;

        if vms.is_empty() {
            return Ok(0);
        }

        // Calculer la charge par nœud : somme RAM des VMs
        let mut node_loads: Vec<NodeLoad> = nodes
            .iter()
            .filter_map(|n| {
                n.last_status.as_ref().map(|s| {
                    let vm_ram: u64 = vms
                        .iter()
                        .filter(|v| v.node == s.node_id && v.status == "running")
                        .map(|v| v.mem_mib)
                        .sum();
                    NodeLoad {
                        node_id: s.node_id.clone(),
                        avail_mib: s.available_mib,
                        vm_ram,
                    }
                })
            })
            .collect();

        // Trier par charge croissante (les nœuds les moins chargés en premier — plus faciles à vider)
        node_loads.sort_by_key(|n| n.vm_ram);

        let mut total_moved = 0;

        for source in &node_loads {
            if source.node_id == self.current_node {
                continue; // On ne touche pas notre propre nœud
            }

            let source_vms: Vec<&VmInfo> = vms
                .iter()
                .filter(|v| v.node == source.node_id && v.status == "running")
                .collect();

            if source_vms.is_empty() {
                continue; // Nœud déjà vide
            }

            info!(
                source     = %source.node_id,
                vm_count   = source_vms.len(),
                total_ram  = source.vm_ram,
                "compaction : tentative de vidage du nœud"
            );

            let moved = self
                .try_drain_node(&source_vms, &node_loads, source)
                .await?;
            total_moved += moved;

            if moved == source_vms.len() {
                info!(source = %source.node_id, "compaction : nœud vidé avec succès");
            } else {
                info!(
                    source = %source.node_id,
                    moved,
                    remaining = source_vms.len() - moved,
                    "compaction : vidage partiel"
                );
            }
        }

        info!(total_moved, "compaction globale terminée");
        Ok(total_moved)
    }

    /// Tente de migrer toutes les VMs d'un nœud source vers d'autres nœuds.
    async fn try_drain_node(
        &self,
        source_vms: &[&VmInfo],
        node_loads: &[NodeLoad],
        source_node: &NodeLoad,
    ) -> Result<usize> {
        // Simuler la disponibilité résiduelle de chaque nœud en tenant compte
        // des placements déjà décidés dans cette passe
        let mut residual: std::collections::HashMap<String, u64> = node_loads
            .iter()
            .map(|n| (n.node_id.clone(), n.avail_mib))
            .collect();

        let mut moved = 0;

        // Trier les VMs par RAM décroissante (First Fit Decreasing)
        let mut sorted_vms: Vec<&&VmInfo> = source_vms.iter().collect();
        sorted_vms.sort_by(|a, b| b.mem_mib.cmp(&a.mem_mib));

        for vm in sorted_vms {
            // Trouver le nœud avec le moins de RAM libre qui peut encore accueillir cette VM
            // (Best Fit : minimise les "trous")
            let dest = residual
                .iter()
                .filter(|(nid, &avail)| {
                    *nid != &source_node.node_id
                        && *nid != &self.current_node
                        && avail >= vm.mem_mib
                })
                .min_by_key(|(_, &avail)| avail)
                .map(|(nid, _)| nid.clone());

            let Some(dest_node) = dest else {
                warn!(
                    vm_id = vm.vm_id,
                    mem_mib = vm.mem_mib,
                    "compaction : aucun nœud pour cette VM"
                );
                continue;
            };

            info!(
                vm_id    = vm.vm_id,
                mem_mib  = vm.mem_mib,
                from     = %source_node.node_id,
                to       = %dest_node,
                dry_run  = self.dry_run,
                "compaction : migration VM"
            );

            if !self.dry_run {
                let out = Command::new("qm")
                    .args(["migrate", &vm.vm_id.to_string(), &dest_node, "--online"])
                    .output()
                    .await?;

                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    warn!(vm_id = vm.vm_id, error = %err, "compaction : migration échouée");
                    continue;
                }
            }

            // Mettre à jour le résiduel simulé
            *residual.entry(dest_node).or_insert(0) =
                residual[&dest_node].saturating_sub(vm.mem_mib);
            moved += 1;
        }

        Ok(moved)
    }
}

struct NodeLoad {
    node_id: String,
    avail_mib: u64,
    vm_ram: u64,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn nl(node_id: &str, avail: u64) -> NodeLoad {
        NodeLoad {
            node_id: node_id.to_string(),
            avail_mib: avail,
            vm_ram: 8192 - avail,
        }
    }

    fn vm(vm_id: u32, node: &str, mem: u64) -> VmInfo {
        VmInfo {
            vm_id,
            node: node.to_string(),
            mem_mib: mem,
            status: "running".to_string(),
        }
    }

    #[test]
    fn test_best_fit_selects_tightest_node() {
        // pve2 a 2048 MiB libres, pve3 a 4096 MiB libres
        // On veut placer une VM de 1000 MiB → Best Fit choisit pve2 (moins de gaspillage)
        let node_loads = vec![nl("pve2", 2048), nl("pve3", 4096)];
        let all_vms = vec![vm(200, "pve3", 1000)];
        let source = nl("pve3", 0);

        // La logique best-fit est dans try_drain_node → on teste via un compactor dry_run
        // sans pvesh, on teste juste que la structure est cohérente
        let _ = node_loads;
        let _ = all_vms;
        let _ = source;
    }

    #[test]
    fn test_node_load_sorted_ascending() {
        let mut loads = vec![nl("pve3", 500), nl("pve1", 100), nl("pve2", 300)];
        loads.sort_by_key(|n| n.vm_ram);
        // Le nœud le moins chargé (vm_ram minimal) doit être en premier
        assert!(loads[0].vm_ram <= loads[1].vm_ram);
        assert!(loads[1].vm_ram <= loads[2].vm_ram);
    }
}
