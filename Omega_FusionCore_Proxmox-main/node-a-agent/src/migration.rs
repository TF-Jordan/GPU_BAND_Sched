//! Démon de recherche de migration et compaction du cluster.
//!
//! # Critère de migration (comparaison relative)
//!
//! Un nœud candidat est retenu quand il est **meilleur que le nœud courant** :
//!   - RAM   : `target.available_mib > local_available_mib()`
//!   - vCPU  : `target.vcpu_free > local_vcpu_free` (lu depuis omega-vcpu-pool.json)
//!   - GPU   : veto absolu — si la VM requiert un GPU et que le nœud cible n'en a pas,
//!             il est rejeté quelle que soit sa RAM ou ses vCPUs.
//!
//! Il suffit qu'**un seul** critère soit meilleur (RAM OU vCPU) pour retenir le candidat
//! (sous réserve que le veto GPU ne s'applique pas).
//! On préfère ensuite le nœud qui a le plus de RAM parmi les candidats retenus.
//!
//! Si aucun nœud n'est meilleur sur aucune dimension → pas de migration.
//!
//! # Compaction deux VMs (point 5)
//!
//! Si aucun nœud direct ne convient, on cherche une paire de VMs (vm_A, vm_B)
//! hébergées sur un même nœud T tiers, telles que :
//!   - vm_A peut migrer vers un nœud Z1
//!   - vm_B peut migrer vers un nœud Z2 (Z1 == Z2 autorisé si capacité suffisante)
//!   - Après ces deux migrations, T a assez de RAM libre pour héberger notre VM
//!
//! On essaie d'abord les paires, puis les triplettes si besoin (max 3 VMs).

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::cluster::{local_available_mib, ClusterState, NodeSnapshot, NodeStatus};
use crate::memory::{MemoryRegion, PAGE_SIZE};

pub struct MigrationAgent {
    vm_id: u32,
    current_node: String,
    cluster: Arc<ClusterState>,
    region: Arc<MemoryRegion>,
    check_interval: Duration,
    compaction_enabled: bool,
    vm_vcpus: u32,
    vm_requested_mib: u64,
    vm_needs_gpu: bool,
    uffd_fd: RawFd,
    /// Levé par le scheduler vCPU quand le pool est saturé.
    /// Influence le niveau d'urgence de la recherche (interval réduit).
    cpu_pressure: Arc<AtomicBool>,
}

impl MigrationAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vm_id: u32,
        current_node: String,
        cluster: Arc<ClusterState>,
        region: Arc<MemoryRegion>,
        check_interval_secs: u64,
        compaction_enabled: bool,
        vm_vcpus: u32,
        vm_requested_mib: u64,
        vm_needs_gpu: bool,
        uffd_fd: RawFd,
        cpu_pressure: Arc<AtomicBool>,
    ) -> Self {
        Self {
            vm_id,
            current_node,
            cluster,
            region,
            check_interval: Duration::from_secs(check_interval_secs.max(10)),
            compaction_enabled,
            vm_vcpus,
            vm_requested_mib,
            vm_needs_gpu,
            uffd_fd,
            cpu_pressure,
        }
    }

    /// Boucle principale. S'arrête dès qu'une migration réussit ou sur shutdown.
    pub async fn run(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        info!(vm_id = self.vm_id, node = %self.current_node, "démon migration démarré");

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            match self.search_and_migrate().await {
                Ok(true) => {
                    info!(vm_id = self.vm_id, "migration déclenchée — démon arrêté");
                    break;
                }
                Ok(false) => debug!(vm_id = self.vm_id, "pas de candidat — réessai planifié"),
                Err(e) => warn!(vm_id = self.vm_id, error = %e, "erreur recherche migration"),
            }

            // Sous pression CPU, on relance plus vite (interval / 3).
            let interval = if self.cpu_pressure.load(Ordering::Relaxed) {
                self.check_interval / 3
            } else {
                self.check_interval
            };
            sleep(interval).await;
        }

        info!(vm_id = self.vm_id, "démon migration terminé");
    }

    async fn search_and_migrate(&self) -> Result<bool> {
        let local_avail = local_available_mib();
        let local_vcpu_free = read_local_vcpu_free();
        let remote_count = self.region.remote_count();
        let remote_mib = (remote_count * PAGE_SIZE / 1024 / 1024) as u64;
        let nodes = self.cluster.snapshot().await;

        info!(
            vm_id = self.vm_id,
            remote_pages = remote_count,
            remote_mib,
            local_avail_mib = local_avail,
            local_vcpu_free,
            vm_needs_gpu = self.vm_needs_gpu,
            "recherche migration"
        );

        // Candidats : différents du nœud courant ET meilleurs sur au moins une dimension
        let best = nodes
            .iter()
            .filter_map(|n| n.last_status.as_ref().map(|s| (n, s)))
            .filter(|(_, s)| s.node_id != self.current_node)
            .filter(|(_, s)| self.is_better_candidate(s, local_avail, local_vcpu_free))
            .max_by_key(|(_, s)| s.available_mib);

        if let Some((_, status)) = best {
            info!(
                vm_id           = self.vm_id,
                target          = %status.node_id,
                target_ram      = status.available_mib,
                target_vcpu_free = status.vcpu_free,
                local_avail,
                local_vcpu_free,
                remote_mib,
                "nœud candidat retenu — déclenchement migration"
            );
            return self.trigger_migration(&status.node_id).await.map(|_| true);
        }

        // Compaction si activée et aucun nœud directement meilleur
        if self.compaction_enabled {
            return self.try_compaction(&nodes).await;
        }

        Ok(false)
    }

    /// Retourne `true` si le nœud cible est meilleur que le nœud courant.
    ///
    /// - Veto absolu : GPU requis ET absent sur le cible → rejeté.
    /// - Sinon : meilleur sur RAM OU meilleur sur vCPU libres.
    fn is_better_candidate(
        &self,
        target: &NodeStatus,
        local_avail: u64,
        local_vcpu_free: u32,
    ) -> bool {
        // Veto GPU
        if self.vm_needs_gpu && !target.has_gpu {
            debug!(node = %target.node_id, "rejeté : GPU requis mais absent");
            return false;
        }

        let better_ram = target.available_mib > local_avail;
        let better_vcpu = if target.vcpu_total > 0 {
            target.vcpu_free > local_vcpu_free
        } else {
            // Nœud sans tracking omega-vcpu — fallback statique
            target.cpu_count >= self.vm_vcpus
        };

        if !better_ram && !better_vcpu {
            debug!(
                node         = %target.node_id,
                target_ram   = target.available_mib,
                local_ram    = local_avail,
                target_vcpu  = target.vcpu_free,
                local_vcpu   = local_vcpu_free,
                "rejeté : pas meilleur que le nœud courant"
            );
            return false;
        }

        true
    }

    async fn trigger_migration(&self, target_node: &str) -> Result<()> {
        // ── 1. Gel de l'éviction ──────────────────────────────────────────────
        // Plus aucune page ne sera évinvée à partir d'ici.
        // L'eviction_ticker du daemon recevra Ok(()) silencieux sur les pages gelées.
        self.region.freeze_eviction();

        // ── 2. Recall complet ─────────────────────────────────────────────────
        // Rapatrier toutes les pages encore sur les stores dans le mmap local.
        // Après cette étape, le mmap est complet : chaque page contient ses vraies données.
        // QEMU peut alors le transférer sans risque de zéros corrompus.
        let remote_count = self.region.remote_count();
        if remote_count > 0 {
            info!(
                vm_id = self.vm_id,
                remote_count, "recall complet avant migration"
            );
            let region = self.region.clone();
            let uffd_fd = self.uffd_fd;
            match tokio::task::spawn_blocking(move || region.recall_all_pages(uffd_fd))
                .await
                .unwrap()
            {
                Ok(n) => info!(
                    vm_id = self.vm_id,
                    recalled = n,
                    "recall pré-migration terminé"
                ),
                Err(e) => warn!(vm_id = self.vm_id, error = %e, "recall pré-migration partiel"),
            }
        }

        // ── 3. Migration live ─────────────────────────────────────────────────
        // Le mmap est maintenant cohérent. QEMU transfère les pages réelles.
        // Le nouvel agent sur la destination démarre proprement avec page_locations vide.
        info!(
            vm_id = self.vm_id,
            target = target_node,
            "lancement qm migrate --online"
        );
        let out = Command::new("qm")
            .args(["migrate", &self.vm_id.to_string(), target_node, "--online"])
            .output()
            .await?;

        if out.status.success() {
            info!(
                vm_id = self.vm_id,
                target = target_node,
                "migration réussie"
            );
            // Remettre cpu.weight au défaut : la VM redémarre sur le nœud cible
            // avec un scheduler propre, le boost n'a plus de raison d'être ici.
            crate::vcpu_scheduler::reset_cpu_weight(self.vm_id);
            crate::disk_scheduler::reset_io_weight(self.vm_id);
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("qm migrate échoué : {err}");
        }
    }

    /// Compaction : cherche une paire (ou triplette) de VMs à déplacer depuis un nœud tiers T
    /// de façon à libérer assez de RAM sur T pour que notre VM puisse y migrer.
    async fn try_compaction(&self, nodes: &[NodeSnapshot]) -> Result<bool> {
        let vms = match list_cluster_vms().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "impossible de lister les VMs cluster (pvesh)");
                return Ok(false);
            }
        };

        let needed_mib = self.vm_requested_mib;

        // Pour chaque nœud tiers T (différent du nœud courant)
        for target_node in nodes {
            let Some(target_status) = &target_node.last_status else {
                continue;
            };
            if target_status.node_id == self.current_node {
                continue;
            }

            let already_free = target_status.available_mib;
            if already_free >= needed_mib {
                // Le nœud a déjà assez — le trigger migration aurait dû le trouver
                // (resource check a peut-être échoué pour autre raison)
                continue;
            }

            let deficit = needed_mib.saturating_sub(already_free);

            // VMs qui tournent sur ce nœud T et pourraient être déplacées
            let candidates: Vec<&VmInfo> = vms
                .iter()
                .filter(|v| v.status == "running" && v.node == target_status.node_id)
                .collect();

            // Chercher une combinaison de 1, 2 ou 3 VMs dont la somme de RAM ≥ deficit
            if let Some(combo) = find_migration_combo(&candidates, deficit, nodes, &vms) {
                info!(
                    vm_id   = self.vm_id,
                    target  = %target_status.node_id,
                    n_moves = combo.len(),
                    "compaction : {} VM(s) à déplacer pour créer {} MiB",
                    combo.len(), deficit
                );
                for (vm, dest) in &combo {
                    let out = Command::new("qm")
                        .args(["migrate", &vm.vm_id.to_string(), dest, "--online"])
                        .output()
                        .await?;
                    if out.status.success() {
                        info!(small_vm = vm.vm_id, dest, "compaction : migration ok");
                    } else {
                        let err = String::from_utf8_lossy(&out.stderr);
                        warn!(small_vm = vm.vm_id, error = %err, "compaction : migration échouée");
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
        }

        error!(
            vm_id = self.vm_id,
            "ALERTE : compaction impossible — cluster saturé, aucune combinaison trouvée"
        );
        Ok(false)
    }
}

// ─── Bin-packing pour la compaction ──────────────────────────────────────────

/// Cherche une combinaison de 1, 2 ou 3 VMs sur le nœud T dont :
///   - la somme de RAM ≥ deficit
///   - chaque VM peut migrer vers un nœud Z qui a assez de RAM libre
///
/// Retourne Vec<(vm, destination_node_id)> ou None.
fn find_migration_combo<'a>(
    candidates: &[&'a VmInfo],
    deficit: u64,
    nodes: &[NodeSnapshot],
    _all_vms: &[VmInfo],
) -> Option<Vec<(&'a VmInfo, String)>> {
    // Trouver les destinations possibles pour une VM de taille mem_mib
    let destinations = |mem_mib: u64| -> Vec<String> {
        nodes
            .iter()
            .filter_map(|n| n.last_status.as_ref())
            .filter(|s| s.available_mib >= mem_mib)
            .map(|s| s.node_id.clone())
            .collect()
    };

    // Essai 1 : une seule VM
    for &vm in candidates {
        if vm.mem_mib >= deficit {
            let dests = destinations(vm.mem_mib);
            if let Some(dest) = dests.into_iter().next() {
                return Some(vec![(vm, dest)]);
            }
        }
    }

    // Essai 2 : deux VMs
    for i in 0..candidates.len() {
        for j in i + 1..candidates.len() {
            let vm_a = candidates[i];
            let vm_b = candidates[j];
            if vm_a.mem_mib + vm_b.mem_mib < deficit {
                continue;
            }

            let dests_a = destinations(vm_a.mem_mib);
            let dests_b = destinations(vm_b.mem_mib);

            if let (Some(da), Some(db)) = (dests_a.first(), dests_b.first()) {
                return Some(vec![(vm_a, da.clone()), (vm_b, db.clone())]);
            }
        }
    }

    // Essai 3 : trois VMs
    for i in 0..candidates.len() {
        for j in i + 1..candidates.len() {
            for k in j + 1..candidates.len() {
                let vm_a = candidates[i];
                let vm_b = candidates[j];
                let vm_c = candidates[k];
                if vm_a.mem_mib + vm_b.mem_mib + vm_c.mem_mib < deficit {
                    continue;
                }

                let da = destinations(vm_a.mem_mib);
                let db = destinations(vm_b.mem_mib);
                let dc = destinations(vm_c.mem_mib);

                if let (Some(da), Some(db), Some(dc)) = (da.first(), db.first(), dc.first()) {
                    return Some(vec![
                        (vm_a, da.clone()),
                        (vm_b, db.clone()),
                        (vm_c, dc.clone()),
                    ]);
                }
            }
        }
    }

    None
}

// ─── Helpers locaux ───────────────────────────────────────────────────────────

/// Lit le pool vCPU partagé et retourne le nombre de vCPUs libres sur ce nœud.
/// Retourne 0 si le fichier n'existe pas encore (pool non initialisé).
fn read_local_vcpu_free() -> u32 {
    use crate::vcpu_scheduler::NodeVCpuPool;
    std::fs::read_to_string(crate::vcpu_scheduler::POOL_PATH)
        .ok()
        .and_then(|s| serde_json::from_str::<NodeVCpuPool>(&s).ok())
        .map(|p| p.free_vcpus())
        .unwrap_or(0)
}

// ─── Inventaire cluster ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct VmInfo {
    pub vm_id: u32,
    pub node: String,
    pub mem_mib: u64,
    pub status: String,
}

pub async fn list_cluster_vms() -> Result<Vec<VmInfo>> {
    let out = Command::new("pvesh")
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
        anyhow::bail!("pvesh cluster resources échoué : {err}");
    }

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let mut vms = Vec::new();

    if let Some(arr) = json.as_array() {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) != Some("qemu") {
                continue;
            }
            let vm_id = item.get("vmid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let node = item
                .get("node")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mem = item.get("maxmem").and_then(|v| v.as_u64()).unwrap_or(0);
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            vms.push(VmInfo {
                vm_id,
                node,
                mem_mib: mem / 1024 / 1024,
                status,
            });
        }
    }

    Ok(vms)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::NodeSnapshot;

    fn make_node(node_id: &str, avail: u64) -> NodeSnapshot {
        NodeSnapshot {
            store_idx: 0,
            store_addr: format!("10.0.0.1:9100"),
            status_addr: format!("10.0.0.1:9200"),
            last_status: Some(crate::cluster::NodeStatus {
                node_id: node_id.to_string(),
                available_mib: avail,
                total_mib: 8192,
                cpu_count: 4,
                has_gpu: false,
                gpu_count: 0,
                disk_available_mib: 0,
                disk_total_mib: 0,
                ceph_enabled: false,
                vcpu_total: 0,
                vcpu_free: 0,
            }),
        }
    }

    fn make_vm(vm_id: u32, node: &str, mem_mib: u64) -> VmInfo {
        VmInfo {
            vm_id,
            node: node.to_string(),
            mem_mib,
            status: "running".to_string(),
        }
    }

    #[test]
    fn test_combo_single_vm_sufficient() {
        let nodes = vec![make_node("pve2", 2048)];
        let vms = vec![make_vm(100, "pve3", 1024)];
        let candidates = vms.iter().collect::<Vec<_>>();
        let result = find_migration_combo(&candidates, 512, &nodes, &vms);
        assert!(result.is_some(), "une seule VM suffit");
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn test_combo_two_vms_needed() {
        let nodes = vec![make_node("pve2", 2048)];
        let vms = vec![make_vm(101, "pve3", 300), make_vm(102, "pve3", 400)];
        let candidates = vms.iter().collect::<Vec<_>>();
        // Aucune VM seule ne couvre 600 MiB, mais 300+400=700 ≥ 600
        let result = find_migration_combo(&candidates, 600, &nodes, &vms);
        assert!(result.is_some(), "deux VMs nécessaires");
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn test_combo_no_solution() {
        let nodes = vec![make_node("pve2", 100)]; // pve2 trop petit pour accueillir quoi que ce soit
        let vms = vec![make_vm(103, "pve3", 500)];
        let candidates = vms.iter().collect::<Vec<_>>();
        let result = find_migration_combo(&candidates, 600, &nodes, &vms);
        assert!(
            result.is_none(),
            "pas de solution si aucun nœud peut accueillir"
        );
    }
}
