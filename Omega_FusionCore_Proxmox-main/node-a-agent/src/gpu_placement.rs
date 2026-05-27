//! Démon de placement GPU.
//!
//! Garantit que les VMs qui ont besoin d'un GPU tournent sur un nœud possédant
//! un GPU ET ont le passthrough PCI configuré (`hostpci0`).
//!
//! ## Stratégie (par ordre de priorité)
//!
//! 1. **Nœud courant GPU** — s'assurer que `hostpci0` est configuré, rien d'autre.
//! 2. **Migration directe** — un nœud GPU a assez de RAM libre :
//!    configurer `hostpci0` avec un GPU libre sur la cible, puis migrer offline.
//! 3. **Libération de place** — déplacer une VM non-GPU du nœud GPU (libère de la RAM),
//!    puis configurer `hostpci0` et migrer notre VM vers ce nœud GPU.
//! 4. **Alerte** — aucun nœud GPU disponible, on réessaie au prochain tick.
//!
//! ## Passthrough PCI
//!
//! `configure_gpu_passthrough(vm_id, node)` :
//! 1. Liste les GPUs PCI du nœud via `pvesh get /nodes/{node}/hardware/pci`.
//! 2. Exclut ceux déjà assignés (scan des `hostpciN` de toutes les VMs du nœud).
//! 3. Appelle `qm set <vmid> --hostpci0 <pci_addr>,pcie=1,rombar=0`.
//!
//! La migration GPU utilise `qm migrate` **sans** `--online` (offline) car le
//! passthrough VFIO est incompatible avec la live migration.
//!
//! ## Découverte des VMs nécessitant un GPU
//!
//! Chaque agent appose le tag `omega-gpu` sur sa VM via `qm set --tags`.
//! Les autres agents détectent quelles VMs ont besoin d'un GPU en lisant les tags.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, error, info, warn};

use crate::cluster::{ClusterState, NodeSnapshot};

/// Tag Proxmox apposé sur toute VM nécessitant un accès GPU.
pub const GPU_TAG: &str = "omega-gpu";

// ─── PciDevice ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PciDevice {
    /// Adresse PCI complète, ex: "0000:02:00.0"
    id: String,
}

/// Marge de sécurité RAM (en Mio) ajoutée à la taille de la VM lors du choix
/// d'un nœud cible, pour éviter de placer la VM sur un nœud trop juste.
const SAFETY_MIB: u64 = 512;

// ─── VmInfo ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct VmInfo {
    vmid: u32,
    name: String,
    status: String,  // "running" | "stopped" | …
    memory: u64,     // Mio (maxmem / 1024 / 1024)
    needs_gpu: bool, // tag omega-gpu présent ?
}

// ─── GpuPlacementDaemon ───────────────────────────────────────────────────────

pub struct GpuPlacementDaemon {
    vm_id: u32,
    current_node: String,
    cluster: Arc<ClusterState>,
    vm_requested_mib: u64,
    interval_secs: u64,
}

impl GpuPlacementDaemon {
    pub fn new(
        vm_id: u32,
        current_node: String,
        cluster: Arc<ClusterState>,
        vm_requested_mib: u64,
        interval_secs: u64,
    ) -> Self {
        Self {
            vm_id,
            current_node,
            cluster,
            vm_requested_mib,
            interval_secs,
        }
    }

    /// Boucle principale du démon.
    pub async fn run(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        // Taguer notre VM pour qu'elle soit visible par les autres agents
        if let Err(e) = self.tag_our_vm().await {
            warn!(
                error    = %e,
                vm_id    = self.vm_id,
                tag      = GPU_TAG,
                "impossible de taguer la VM GPU — les autres agents ne pourront pas la détecter"
            );
        }

        info!(
            vm_id        = self.vm_id,
            current_node = %self.current_node,
            interval_s   = self.interval_secs,
            "démon GPU placement démarré"
        );

        let mut ticker = tokio::time::interval(Duration::from_secs(self.interval_secs));

        loop {
            ticker.tick().await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            match self.check_and_place().await {
                Ok(true) => {
                    // Migration lancée — pause pour laisser le temps à la migration
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                Ok(false) => {}
                Err(e) => error!(error = %e, vm_id = self.vm_id, "GPU placement check échoué"),
            }
        }

        info!(vm_id = self.vm_id, "démon GPU placement arrêté");
    }

    // ── Logique principale ────────────────────────────────────────────────────

    async fn check_and_place(&self) -> Result<bool> {
        let nodes = self.cluster.snapshot().await;

        // Le nœud courant possède-t-il un GPU ?
        let current_has_gpu = nodes
            .iter()
            .find(|n| {
                n.last_status
                    .as_ref()
                    .map(|s| s.node_id == self.current_node)
                    .unwrap_or(false)
            })
            .and_then(|n| n.last_status.as_ref())
            .map(|s| s.has_gpu)
            .unwrap_or(false);

        if current_has_gpu {
            // VM sur nœud GPU — s'assurer que hostpci0 est configuré.
            // (La VM peut avoir été migrée ici manuellement sans hostpci.)
            match self
                .configure_gpu_passthrough(self.vm_id, &self.current_node)
                .await
            {
                Ok(true) => debug!(vm_id = self.vm_id, "hostpci0 OK sur nœud GPU courant"),
                Ok(false) => warn!(vm_id = self.vm_id, "aucun GPU libre sur le nœud courant"),
                Err(e) => warn!(error = %e, vm_id = self.vm_id, "configure_gpu_passthrough échoué"),
            }
            return Ok(false);
        }

        // Lister les nœuds GPU connus
        let gpu_nodes: Vec<_> = nodes
            .iter()
            .filter(|n| n.last_status.as_ref().map(|s| s.has_gpu).unwrap_or(false))
            .collect();

        if gpu_nodes.is_empty() {
            warn!(
                vm_id = self.vm_id,
                "ALERTE GPU : aucun nœud GPU détecté dans le cluster"
            );
            return Ok(false);
        }

        info!(
            vm_id = self.vm_id,
            gpu_nodes = gpu_nodes.len(),
            "VM nécessite GPU — recherche de placement"
        );

        // Stratégie 1 : migration directe
        if let Some(placed) = self.try_direct_migration(&gpu_nodes).await? {
            return Ok(placed);
        }

        // Stratégie 2 : libérer de la place sur un nœud GPU
        for node in &gpu_nodes {
            if let Some(status) = &node.last_status {
                match self.try_make_room(&status.node_id, &nodes).await {
                    Ok(true) => {
                        info!(
                            vm_id  = self.vm_id,
                            target = %status.node_id,
                            "place libérée — configuration passthrough puis migration GPU dans 15 s"
                        );
                        tokio::time::sleep(Duration::from_secs(15)).await;
                        match self
                            .configure_gpu_passthrough(self.vm_id, &status.node_id)
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                warn!(vm_id = self.vm_id, node = %status.node_id, "aucun GPU libre — migration annulée");
                                return Ok(false);
                            }
                            Err(e) => {
                                warn!(error = %e, "configure_gpu_passthrough échoué — migration annulée");
                                return Ok(false);
                            }
                        }
                        self.migrate_vm(self.vm_id, &status.node_id, false).await?;
                        return Ok(true);
                    }
                    Ok(false) => continue,
                    Err(e) => {
                        warn!(error = %e, node = %status.node_id, "libération de place échouée");
                        continue;
                    }
                }
            }
        }

        // Aucune solution : tous les nœuds GPU ont leurs slots pris ou plus de RAM
        let total_gpu_slots: usize = gpu_nodes
            .iter()
            .filter_map(|n| n.last_status.as_ref())
            .map(|s| s.gpu_count as usize)
            .sum();
        warn!(
            vm_id = self.vm_id,
            gpu_nodes = gpu_nodes.len(),
            total_gpu_slots,
            retry_secs = self.interval_secs,
            "ALERTE GPU : aucun slot GPU disponible — VM en attente de placement"
        );
        Ok(false)
    }

    /// Tente une migration directe vers un nœud GPU ayant assez de RAM.
    ///
    /// Configure `hostpci0` sur la VM avant de migrer (offline — VFIO incompatible
    /// avec live migration). Si aucun GPU libre sur la cible, passe au nœud suivant.
    async fn try_direct_migration(&self, gpu_nodes: &[&NodeSnapshot]) -> Result<Option<bool>> {
        let needed = self.vm_requested_mib + SAFETY_MIB;
        for node in gpu_nodes {
            if let Some(status) = &node.last_status {
                if status.available_mib < needed {
                    continue;
                }

                match self
                    .configure_gpu_passthrough(self.vm_id, &status.node_id)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        info!(node = %status.node_id, "aucun GPU libre — nœud suivant");
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, node = %status.node_id, "configure_gpu_passthrough échoué — nœud suivant");
                        continue;
                    }
                }

                info!(
                    vm_id  = self.vm_id,
                    target = %status.node_id,
                    avail  = status.available_mib,
                    "migration directe vers nœud GPU (offline)"
                );
                self.migrate_vm(self.vm_id, &status.node_id, false).await?;
                return Ok(Some(true));
            }
        }
        Ok(None)
    }

    /// Tente de libérer de la place (RAM) sur `gpu_node` en déplaçant une VM non-GPU.
    ///
    /// Une fois la RAM libérée, la VM GPU peut migrer sur ce nœud et accéder au GPU.
    async fn try_make_room(&self, gpu_node: &str, cluster_snap: &[NodeSnapshot]) -> Result<bool> {
        let vms = self.list_vms_on_node(gpu_node).await?;

        let mut candidates: Vec<_> = vms
            .iter()
            .filter(|v| v.status == "running" && !v.needs_gpu && v.vmid != self.vm_id)
            .collect();

        if candidates.is_empty() {
            debug!(node = %gpu_node, "aucune VM non-GPU déplaçable sur ce nœud GPU");
            return Ok(false);
        }

        // La plus petite en mémoire d'abord pour minimiser l'impact
        candidates.sort_by_key(|v| v.memory);

        for candidate in &candidates {
            match self.find_non_gpu_target(candidate, cluster_snap).await {
                Ok(target) => {
                    info!(
                        candidate_vm = candidate.vmid,
                        name         = %candidate.name,
                        from         = %gpu_node,
                        to           = %target,
                        memory_mib   = candidate.memory,
                        "déplacement VM non-GPU pour libérer nœud GPU"
                    );
                    // VM non-GPU déplacée en live migration (pas de passthrough — compatible)
                    self.migrate_vm(candidate.vmid, &target, true).await?;
                    return Ok(true);
                }
                Err(e) => {
                    debug!(vm = candidate.vmid, error = %e, "pas de nœud cible — essai suivant");
                }
            }
        }

        Ok(false)
    }

    /// Trouve un nœud non-GPU avec assez de RAM pour accueillir `vm`.
    ///
    /// Tous les nœuds du cluster font tourner un store et sont donc présents
    /// dans le cluster snapshot. Pas besoin d'interroger pvesh /nodes séparément.
    async fn find_non_gpu_target(
        &self,
        vm: &VmInfo,
        cluster_snap: &[NodeSnapshot],
    ) -> Result<String> {
        let needed = vm.memory + SAFETY_MIB;

        // Trier par RAM disponible décroissante pour choisir le nœud le moins chargé
        let mut candidates: Vec<_> = cluster_snap
            .iter()
            .filter_map(|n| n.last_status.as_ref().map(|s| (n, s)))
            .filter(|(_, s)| {
                !s.has_gpu && s.node_id != self.current_node && s.available_mib >= needed
            })
            .collect();

        candidates.sort_by(|a, b| b.1.available_mib.cmp(&a.1.available_mib));

        if let Some((_, status)) = candidates.first() {
            return Ok(status.node_id.clone());
        }

        anyhow::bail!(
            "aucun nœud non-GPU avec {} Mio disponibles pour VM {} — \
             vérifiez que tous les nœuds ont leur store démarré",
            needed,
            vm.vmid
        )
    }

    // ── Appels Proxmox ────────────────────────────────────────────────────────

    /// Liste les VMs sur un nœud Proxmox via `pvesh get /nodes/{node}/qemu`.
    async fn list_vms_on_node(&self, node: &str) -> Result<Vec<VmInfo>> {
        let out = tokio::process::Command::new("pvesh")
            .args([
                "get",
                &format!("/nodes/{node}/qemu"),
                "--output-format",
                "json",
            ])
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("pvesh /nodes/{node}/qemu : {err}");
        }

        let arr: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();

        let vms = arr
            .iter()
            .filter_map(|item| {
                let vmid = item["vmid"].as_u64()? as u32;
                let status = item["status"].as_str().unwrap_or("unknown").to_string();
                let name = item["name"].as_str().unwrap_or("").to_string();
                // maxmem est en octets dans l'API Proxmox
                let memory = item["maxmem"].as_u64().unwrap_or(0) / (1024 * 1024);
                let tags = item["tags"].as_str().unwrap_or("");
                let needs_gpu = tags.split(';').any(|t| t.trim() == GPU_TAG);
                Some(VmInfo {
                    vmid,
                    name,
                    status,
                    memory,
                    needs_gpu,
                })
            })
            .collect();

        Ok(vms)
    }

    /// Migre `vmid` vers `target_node`.
    ///
    /// `online = true`  → `qm migrate --online` (live migration, incompatible GPU passthrough).
    /// `online = false` → migration offline : la VM est arrêtée, transférée, redémarrée.
    async fn migrate_vm(&self, vmid: u32, target_node: &str, online: bool) -> Result<()> {
        let mode = if online { "--online" } else { "(offline)" };
        info!(vmid, target = %target_node, mode, "lancement qm migrate");

        let mut args = vec![
            "migrate".to_string(),
            vmid.to_string(),
            target_node.to_string(),
        ];
        if online {
            args.push("--online".to_string());
        }

        let out = tokio::process::Command::new("qm")
            .args(&args)
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("qm migrate {vmid} → {target_node} : {err}");
        }

        info!(vmid, target = %target_node, "migration terminée");
        Ok(())
    }

    // ── Passthrough GPU ───────────────────────────────────────────────────────

    /// Liste les GPUs (classe PCI 0x03xx) disponibles sur `node` via pvesh.
    async fn list_gpus_on_node(&self, node: &str) -> Result<Vec<PciDevice>> {
        let out = tokio::process::Command::new("pvesh")
            .args([
                "get",
                &format!("/nodes/{node}/hardware/pci"),
                "--output-format",
                "json",
            ])
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("pvesh /nodes/{node}/hardware/pci : {err}");
        }

        let arr: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();

        let gpus = arr
            .iter()
            .filter_map(|item| {
                let id = item["id"].as_str()?.to_string();
                let class = u32::from_str_radix(
                    item["class"]
                        .as_str()
                        .unwrap_or("0x000000")
                        .trim_start_matches("0x"),
                    16,
                )
                .unwrap_or(0);
                if (class >> 16) == 0x03 {
                    Some(PciDevice { id })
                } else {
                    None
                }
            })
            .collect();

        Ok(gpus)
    }

    /// Retourne les adresses PCI déjà assignées en passthrough sur `node`.
    ///
    /// Scanne tous les `hostpciN` de toutes les VMs du nœud via pvesh.
    async fn list_assigned_gpus_on_node(&self, node: &str) -> Result<Vec<String>> {
        let out = tokio::process::Command::new("pvesh")
            .args([
                "get",
                &format!("/nodes/{node}/qemu"),
                "--output-format",
                "json",
            ])
            .output()
            .await?;

        if !out.status.success() {
            return Ok(Vec::new());
        }

        let vms: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let mut assigned = Vec::new();

        for vm in &vms {
            let vmid = match vm["vmid"].as_u64() {
                Some(id) => id,
                None => continue,
            };

            let cfg_out = tokio::process::Command::new("pvesh")
                .args([
                    "get",
                    &format!("/nodes/{node}/qemu/{vmid}/config"),
                    "--output-format",
                    "json",
                ])
                .output()
                .await
                .unwrap_or_else(|_| {
                    // En cas d'erreur IO, on retourne un Output "vide" avec échec.
                    // On passe par un Command factice qui échoue immédiatement.
                    std::process::Output {
                        status: std::process::Command::new("false")
                            .status()
                            .unwrap_or_else(|_| {
                                std::process::Command::new("sh")
                                    .args(["-c", "exit 1"])
                                    .status()
                                    .expect("sh doit exister")
                            }),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    }
                });

            if !cfg_out.status.success() {
                continue;
            }

            let cfg: serde_json::Value =
                serde_json::from_slice(&cfg_out.stdout).unwrap_or_default();

            // hostpci0 … hostpci7
            for i in 0..8u32 {
                let key = format!("hostpci{i}");
                if let Some(val) = cfg[&key].as_str() {
                    // "0000:02:00.0,pcie=1,..." → extraire l'ID PCI
                    let pci_id = val.split(',').next().unwrap_or("").trim();
                    let full = if pci_id.matches(':').count() == 1 {
                        format!("0000:{pci_id}")
                    } else {
                        pci_id.to_string()
                    };
                    if !full.is_empty() {
                        assigned.push(full);
                    }
                }
            }
        }

        Ok(assigned)
    }

    /// Configure `hostpci0` sur `vm_id` avec un GPU libre sur `target_node`.
    ///
    /// Retourne `true` si la configuration a réussi, `false` si aucun GPU libre.
    /// Si `hostpci0` est déjà configuré avec une adresse valide sur ce nœud,
    /// la remplace quand même pour garantir la cohérence après migration.
    async fn configure_gpu_passthrough(&self, vm_id: u32, target_node: &str) -> Result<bool> {
        let all_gpus = self.list_gpus_on_node(target_node).await?;
        if all_gpus.is_empty() {
            warn!(vm_id, node = %target_node, "pvesh hardware/pci : aucun GPU PCI détecté");
            return Ok(false);
        }

        let assigned = self.list_assigned_gpus_on_node(target_node).await?;

        let free_gpu = all_gpus
            .iter()
            .find(|g| !assigned.iter().any(|a| a == &g.id))
            .map(|g| g.id.clone());

        let gpu_id = match free_gpu {
            Some(id) => id,
            None => {
                warn!(
                    vm_id,
                    node        = %target_node,
                    total_gpus  = all_gpus.len(),
                    "tous les GPUs du nœud sont déjà assignés"
                );
                return Ok(false);
            }
        };

        // qm set <vmid> --hostpci0 <pci_id>,pcie=1,rombar=0
        let hostpci_val = format!("{gpu_id},pcie=1,rombar=0");
        let out = tokio::process::Command::new("qm")
            .args(["set", &vm_id.to_string(), "--hostpci0", &hostpci_val])
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("qm set --hostpci0 VM {vm_id} ({gpu_id}) : {err}");
        }

        info!(
            vm_id,
            pci  = %gpu_id,
            node = %target_node,
            "hostpci0 configuré — passthrough GPU actif au prochain démarrage"
        );
        Ok(true)
    }

    /// Appose le tag `omega-gpu` sur notre VM dans Proxmox.
    ///
    /// Lit les tags existants avant d'ajouter le nôtre, pour ne pas écraser.
    async fn tag_our_vm(&self) -> Result<()> {
        let out = tokio::process::Command::new("qm")
            .args(["config", &self.vm_id.to_string()])
            .output()
            .await?;

        let config = String::from_utf8_lossy(&out.stdout);
        let existing: Vec<&str> = config
            .lines()
            .find(|l| l.starts_with("tags:"))
            .map(|l| l["tags:".len()..].trim())
            .unwrap_or("")
            .split(';')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();

        if existing.contains(&GPU_TAG) {
            debug!(vm_id = self.vm_id, "tag {GPU_TAG} déjà présent");
            return Ok(());
        }

        let mut all_tags = existing;
        all_tags.push(GPU_TAG);
        let tags_str = all_tags.join(";");

        let out = tokio::process::Command::new("qm")
            .args(["set", &self.vm_id.to_string(), "--tags", &tags_str])
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("qm set --tags VM {} : {err}", self.vm_id);
        }

        info!(
            vm_id = self.vm_id,
            tag = GPU_TAG,
            "VM taguée GPU dans Proxmox"
        );
        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_tag_constant() {
        assert_eq!(GPU_TAG, "omega-gpu");
    }

    #[test]
    fn test_vm_needs_gpu_from_tags() {
        // Simule le parsing des tags depuis pvesh
        let tags = "web;omega-gpu;db";
        let needs = tags.split(';').any(|t| t.trim() == GPU_TAG);
        assert!(needs, "tag omega-gpu doit être détecté");
    }

    #[test]
    fn test_vm_no_gpu_tag() {
        let tags = "web;production";
        let needs = tags.split(';').any(|t| t.trim() == GPU_TAG);
        assert!(!needs, "pas de tag GPU");
    }

    #[test]
    fn test_vm_empty_tags() {
        let tags = "";
        let needs = tags.split(';').any(|t| t.trim() == GPU_TAG);
        assert!(!needs);
    }

    #[test]
    fn test_sort_by_memory_ascending() {
        let mut vms = vec![
            VmInfo {
                vmid: 1,
                name: "big".into(),
                status: "running".into(),
                memory: 4096,
                needs_gpu: false,
            },
            VmInfo {
                vmid: 2,
                name: "small".into(),
                status: "running".into(),
                memory: 512,
                needs_gpu: false,
            },
            VmInfo {
                vmid: 3,
                name: "medium".into(),
                status: "running".into(),
                memory: 2048,
                needs_gpu: false,
            },
        ];
        vms.sort_by_key(|v| v.memory);
        assert_eq!(vms[0].vmid, 2, "la plus petite VM doit être en premier");
        assert_eq!(vms[1].vmid, 3);
        assert_eq!(vms[2].vmid, 1);
    }

    #[test]
    fn test_safety_margin_applied() {
        assert!(SAFETY_MIB > 0);
    }

    #[test]
    fn test_pci_class_gpu_detection() {
        // Classe 0x030200 (VGA 3D) → GPU
        let class: u32 = 0x030200;
        assert_eq!(class >> 16, 0x03, "0x030200 doit être identifié comme GPU");

        // Classe 0x020000 (NIC) → pas un GPU
        let not_gpu: u32 = 0x020000;
        assert_ne!(not_gpu >> 16, 0x03);
    }

    #[test]
    fn test_pci_id_normalization() {
        // Forme courte "02:00.0" → "0000:02:00.0"
        let short = "02:00.0";
        let full = if short.matches(':').count() == 1 {
            format!("0000:{short}")
        } else {
            short.to_string()
        };
        assert_eq!(full, "0000:02:00.0");

        // Forme longue déjà correcte
        let long = "0000:02:00.0";
        let full2 = if long.matches(':').count() == 1 {
            format!("0000:{long}")
        } else {
            long.to_string()
        };
        assert_eq!(full2, "0000:02:00.0");
    }

    #[test]
    fn test_hostpci_value_format() {
        let pci_id = "0000:02:00.0";
        let val = format!("{pci_id},pcie=1,rombar=0");
        assert_eq!(val, "0000:02:00.0,pcie=1,rombar=0");
    }

    #[test]
    fn test_free_gpu_found_when_none_assigned() {
        let all_gpus: Vec<String> = vec!["0000:02:00.0".into(), "0000:03:00.0".into()];
        let assigned: Vec<String> = vec![];
        let free = all_gpus.iter().find(|g| !assigned.iter().any(|a| a == *g));
        assert_eq!(free.map(|s| s.as_str()), Some("0000:02:00.0"));
    }

    #[test]
    fn test_free_gpu_found_when_one_assigned() {
        let all_gpus: Vec<String> = vec!["0000:02:00.0".into(), "0000:03:00.0".into()];
        let assigned: Vec<String> = vec!["0000:02:00.0".into()];
        let free = all_gpus.iter().find(|g| !assigned.iter().any(|a| a == *g));
        assert_eq!(free.map(|s| s.as_str()), Some("0000:03:00.0"));
    }

    #[test]
    fn test_no_free_gpu_when_all_assigned() {
        let all_gpus: Vec<String> = vec!["0000:02:00.0".into()];
        let assigned: Vec<String> = vec!["0000:02:00.0".into()];
        let free = all_gpus.iter().find(|g| !assigned.iter().any(|a| a == *g));
        assert!(free.is_none(), "aucun GPU libre attendu");
    }

    #[test]
    fn test_non_gpu_vms_are_displacement_candidates() {
        // Seules les VMs running sans besoin GPU sont déplaçables pour libérer de la RAM
        let vms = vec![
            VmInfo {
                vmid: 101,
                name: "gpu-vm".into(),
                status: "running".into(),
                memory: 4096,
                needs_gpu: true,
            },
            VmInfo {
                vmid: 102,
                name: "web".into(),
                status: "running".into(),
                memory: 1024,
                needs_gpu: false,
            },
            VmInfo {
                vmid: 103,
                name: "db".into(),
                status: "stopped".into(),
                memory: 2048,
                needs_gpu: false,
            },
        ];
        let my_vm_id = 999u32;
        let candidates: Vec<_> = vms
            .iter()
            .filter(|v| v.status == "running" && !v.needs_gpu && v.vmid != my_vm_id)
            .collect();
        // Seule la VM web (102) est candidate — gpu-vm garde son GPU, db est arrêtée
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].vmid, 102);
    }
}
