//! Suivi des VMs QEMU locales et de leurs pages distantes.
//!
//! # Découverte des VMs
//!
//! Sur un nœud Proxmox, les VMs QEMU sont identifiables via :
//! - `/var/run/qemu-server/{vmid}.pid`  → PID du processus QEMU
//! - `/etc/pve/qemu-server/{vmid}.conf` → config de la VM (RAM, CPU, etc.)
//! - `/proc/{pid}/status`               → mémoire réellement utilisée
//!
//! # Pages distantes
//!
//! Le `VmTracker` est notifié par le store (via `record_page_stored`) quand une
//! page est PUT pour un vm_id. Il maintient un compteur de pages distantes par VM.
//! Quand ce compteur dépasse un seuil, la VM est candidate à la migration.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::Serialize;
use tracing::{debug, warn};

/// Informations sur une VM QEMU locale.
#[derive(Debug, Clone, Serialize)]
pub struct LocalVm {
    pub vmid: u32,
    pub pid: Option<u32>,
    /// RAM maximale configurée (Mio)
    pub max_mem_mib: u64,
    /// RAM actuellement utilisée par le process QEMU (Ko)
    pub rss_kb: u64,
    /// Nombre de pages de cette VM stockées dans ce nœud (comme store)
    pub local_stored_pages: u64,
    /// Nombre de pages de cette VM stockées sur des nœuds DISTANTS
    pub remote_pages: u64,
    pub status: VmStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum VmStatus {
    Running,
    Stopped,
    Unknown,
}

impl LocalVm {
    /// Mémoire totale de la VM en Ko
    pub fn max_mem_kb(&self) -> u64 {
        self.max_mem_mib * 1024
    }

    /// Pourcentage de la RAM de la VM stocké à distance
    pub fn remote_pct(&self) -> f64 {
        let total_pages = self.max_mem_mib * 256; // pages de 4Ko dans max_mem_mib
        if total_pages == 0 {
            return 0.0;
        }
        (self.remote_pages as f64 / total_pages as f64) * 100.0
    }

    /// Mémoire distante en Mio
    pub fn remote_mem_mib(&self) -> u64 {
        self.remote_pages * 4 / 1024
    }
}

/// État global de suivi des VMs.
pub struct VmTracker {
    /// VMs locales en cours d'exécution
    local_vms: Mutex<HashMap<u32, LocalVm>>,
    /// Pages stockées localement pour chaque vm_id (compteur par store)
    pages_stored_per_vm: Arc<dashmap::DashMap<u32, u64>>,
    /// Répertoires de configuration Proxmox
    pid_dir: String,
    conf_dir: String,
}

impl VmTracker {
    pub fn new(pid_dir: String, conf_dir: String) -> Self {
        Self {
            local_vms: Mutex::new(HashMap::new()),
            pages_stored_per_vm: Arc::new(dashmap::DashMap::new()),
            pid_dir,
            conf_dir,
        }
    }

    /// Enregistre qu'une page d'une VM est stockée sur CE nœud (callback du store).
    pub fn record_page_stored(&self, vm_id: u32, delta: i64) {
        let mut entry = self.pages_stored_per_vm.entry(vm_id).or_insert(0);
        if delta >= 0 {
            *entry = entry.saturating_add(delta as u64);
        } else {
            *entry = entry.saturating_sub((-delta) as u64);
        }
    }

    /// Retourne le nombre de pages de ce vm_id stockées sur ce nœud.
    pub fn pages_stored_for(&self, vm_id: u32) -> u64 {
        self.pages_stored_per_vm
            .get(&vm_id)
            .map(|v| *v)
            .unwrap_or(0)
    }

    /// Scanne le système de fichiers Proxmox pour découvrir les VMs locales.
    ///
    /// Appelé périodiquement par le daemon.
    pub fn refresh_local_vms(&self) -> Result<()> {
        let mut vms = self.local_vms.lock().unwrap();
        vms.clear();

        // Énumération par les PIDs QEMU
        let pid_path = Path::new(&self.pid_dir);
        if !pid_path.exists() {
            debug!(path = %self.pid_dir, "répertoire PID QEMU absent (pas de Proxmox ici ?)");
            return Ok(());
        }

        for entry in std::fs::read_dir(pid_path)? {
            let entry = entry?;
            let fname = entry.file_name();
            let name = fname.to_string_lossy();

            // Format attendu : "{vmid}.pid"
            if !name.ends_with(".pid") {
                continue;
            }

            let vmid_str = name.trim_end_matches(".pid");
            let vmid: u32 = match vmid_str.parse() {
                Ok(v) => v,
                Err(_) => {
                    warn!(file = %name, "nom de fichier PID inattendu");
                    continue;
                }
            };

            let pid = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());

            let max_mem_mib = self.read_vm_max_mem(vmid).unwrap_or(0);
            let rss_kb = pid.and_then(read_proc_rss).unwrap_or(0);
            let status = if pid.is_some() {
                VmStatus::Running
            } else {
                VmStatus::Stopped
            };

            let vm = LocalVm {
                vmid,
                pid,
                max_mem_mib,
                rss_kb,
                local_stored_pages: self.pages_stored_for(vmid),
                remote_pages: 0, // mis à jour par le cluster state
                status,
            };

            debug!(vmid, pid = ?vm.pid, max_mem_mib, rss_kb, "VM locale découverte");
            vms.insert(vmid, vm);
        }

        Ok(())
    }

    /// Lit la RAM maximale configurée pour une VM depuis sa conf Proxmox.
    fn read_vm_max_mem(&self, vmid: u32) -> Option<u64> {
        let conf_file = format!("{}/{}.conf", self.conf_dir, vmid);
        let content = std::fs::read_to_string(&conf_file).ok()?;

        // Ligne format : "memory: 4096" (en Mio dans les confs Proxmox)
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("memory:") {
                if let Ok(mib) = rest.trim().parse::<u64>() {
                    return Some(mib);
                }
            }
        }
        None
    }

    /// Met à jour le compteur de pages distantes pour une VM.
    /// Appelé par le controller quand il reçoit les stats de cluster.
    pub fn update_remote_pages(&self, vmid: u32, count: u64) {
        if let Ok(mut vms) = self.local_vms.lock() {
            if let Some(vm) = vms.get_mut(&vmid) {
                vm.remote_pages = count;
            }
        }
    }

    /// Retourne la liste des VMs locales (snapshot).
    pub fn local_vms_snapshot(&self) -> Vec<LocalVm> {
        self.local_vms.lock().unwrap().values().cloned().collect()
    }

    /// Retourne uniquement les VMs locales réellement en cours d'exécution.
    pub fn local_running_vms_snapshot(&self) -> Vec<LocalVm> {
        self.local_vms
            .lock()
            .unwrap()
            .values()
            .filter(|vm| vm.status == VmStatus::Running)
            .cloned()
            .collect()
    }

    /// Retourne les VMs locales candidates à la migration.
    ///
    /// Une VM est candidate si ses pages distantes dépassent `threshold_pages`.
    pub fn migration_candidates(&self, threshold_pages: u64) -> Vec<LocalVm> {
        self.local_vms
            .lock()
            .unwrap()
            .values()
            .filter(|vm| vm.status == VmStatus::Running && vm.remote_pages >= threshold_pages)
            .cloned()
            .collect()
    }
}

/// Lit le RSS (Resident Set Size) d'un processus depuis /proc/{pid}/status.
fn read_proc_rss(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format : "VmRSS:    123456 kB"
            return rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    None
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_tracker() -> VmTracker {
        VmTracker::new(
            "/tmp/nonexistent-pid".into(),
            "/tmp/nonexistent-conf".into(),
        )
    }

    #[test]
    fn test_pages_stored_counter() {
        let t = make_tracker();
        t.record_page_stored(100, 5);
        assert_eq!(t.pages_stored_for(100), 5);
        t.record_page_stored(100, 3);
        assert_eq!(t.pages_stored_for(100), 8);
        t.record_page_stored(100, -3);
        assert_eq!(t.pages_stored_for(100), 5);
    }

    #[test]
    fn test_remote_pages_zero_initially() {
        let t = make_tracker();
        assert_eq!(t.pages_stored_for(999), 0);
    }

    #[test]
    fn test_no_crash_on_missing_proc_dirs() {
        let t = make_tracker();
        // Ne doit pas paniquer même si les dossiers n'existent pas
        let _ = t.refresh_local_vms();
    }

    #[test]
    fn test_remote_pct() {
        let vm = LocalVm {
            vmid: 100,
            pid: None,
            max_mem_mib: 1024, // 1 Gio = 262144 pages de 4Ko
            rss_kb: 0,
            local_stored_pages: 0,
            remote_pages: 26214, // 10% de 262144
            status: VmStatus::Running,
        };
        assert!((vm.remote_pct() - 10.0).abs() < 0.1);
    }

    #[test]
    fn test_running_snapshot_filters_stopped_vms() {
        let t = make_tracker();
        let mut vms = HashMap::new();
        vms.insert(
            100,
            LocalVm {
                vmid: 100,
                pid: Some(1234),
                max_mem_mib: 1024,
                rss_kb: 0,
                local_stored_pages: 0,
                remote_pages: 0,
                status: VmStatus::Running,
            },
        );
        vms.insert(
            200,
            LocalVm {
                vmid: 200,
                pid: None,
                max_mem_mib: 1024,
                rss_kb: 0,
                local_stored_pages: 0,
                remote_pages: 0,
                status: VmStatus::Stopped,
            },
        );
        *t.local_vms.lock().unwrap() = vms;

        let running = t.local_running_vms_snapshot();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].vmid, 100);
    }
}
