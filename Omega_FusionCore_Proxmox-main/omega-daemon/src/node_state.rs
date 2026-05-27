//! État courant du nœud — partagé entre le store, l'API HTTP et le vm_tracker.
//!
//! `NodeState` est le point central de synchronisation entre tous les composants
//! du daemon. Il est partagé via `Arc<NodeState>` sans verrou global grâce aux
//! atomiques et à `DashMap`.

use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;

use crate::disk_io_scheduler::DiskIoScheduler;
use crate::gpu_runtime::GpuRuntime;
use crate::quota::QuotaRegistry;
use crate::vcpu_scheduler::VcpuScheduler;
use crate::vm_tracker::VmTracker;
use node_bc_store::metrics::StoreMetrics;
use node_bc_store::store::PageStore;

/// Snapshot sérialisable de l'état du nœud — exposé via HTTP `/api/status`.
#[derive(Debug, Serialize, Clone)]
pub struct NodeStatus {
    pub node_id: String,
    pub store_addr: String,
    pub api_addr: String,
    /// RAM totale du nœud physique (Ko)
    pub mem_total_kb: u64,
    /// RAM disponible (Ko)
    pub mem_available_kb: u64,
    /// Pourcentage d'usage RAM
    pub mem_usage_pct: f64,
    /// Pages stockées dans le store de ce nœud
    pub pages_stored: u64,
    /// Mémoire utilisée par le store (Ko)
    pub store_used_kb: u64,
    /// Slots vCPU totaux sur ce nœud (num_pcpus × VCPU_PER_PCPU)
    pub vcpu_total: usize,
    /// Slots vCPU encore disponibles (peuvent accepter au moins 1 VM)
    pub vcpu_free: usize,
    /// Taux d'occupation vCPU (0.0 – 100.0)
    pub vcpu_occupancy_pct: f64,
    /// Pression I/O du nœud (PSI avg10)
    pub disk_pressure_pct: f64,
    /// VMs locales et leurs compteurs de pages distantes
    pub local_vms: Vec<VmStatusEntry>,
    /// État GPU local si le multiplexeur est actif
    pub gpu: Option<GpuNodeStatus>,
    /// Timestamp Unix (secondes)
    pub timestamp_secs: u64,
}

/// Résumé d'une VM locale pour l'API cluster.
#[derive(Debug, Serialize, Clone)]
pub struct VmStatusEntry {
    pub vmid: u32,
    pub max_mem_mib: u64,
    pub rss_kb: u64,
    pub remote_pages: u64,
    pub remote_mem_mib: u64,
    pub status: String,
    /// Utilisation CPU moyenne (%) — depuis le scheduler vCPU, 0.0 si VM non enregistrée
    pub avg_cpu_pct: f64,
    /// Ratio de throttle cgroup v2 (0.0 – 1.0) — proxy steal time
    pub throttle_ratio: f64,
    /// Budget VRAM réservé pour cette VM (Mio)
    pub gpu_vram_budget_mib: u64,
    /// Débit lecture disque VM (octets/s)
    pub disk_read_bps: f64,
    /// Débit écriture disque VM (octets/s)
    pub disk_write_bps: f64,
    /// Poids I/O cgroup courant
    pub disk_io_weight: u32,
    /// La VM participe-t-elle à un partage I/O local temporaire ?
    pub disk_local_share_active: bool,
    /// Le backend local expose-t-il `io.weight` pour cette VM ?
    pub disk_io_control_supported: bool,
    /// Raison du fallback si `io.weight` n'est pas disponible.
    pub disk_io_control_reason: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GpuNodeStatus {
    pub enabled: bool,
    pub backend_name: String,
    pub render_node: Option<String>,
    pub socket_path: String,
    pub total_vram_mib: u64,
    pub reserved_vram_mib: u64,
    pub free_vram_mib: u64,
    pub budgeted_vms: usize,
}

/// État central du nœud.
pub struct NodeState {
    pub node_id: String,
    pub store_addr: String,
    pub api_addr: String,

    pub store: Arc<PageStore>,
    pub metrics: Arc<StoreMetrics>,
    pub vm_tracker: Arc<VmTracker>,
    /// Registre des quotas mémoire par VM (non-dépassement garanti)
    pub quota_registry: Arc<QuotaRegistry>,
    /// Planificateur vCPU élastique local
    pub vcpu_scheduler: Arc<VcpuScheduler>,
    /// Répertoire des sockets QMP Proxmox
    pub qmp_dir: String,
    /// Runtime GPU optionnel
    pub gpu_runtime: Option<Arc<GpuRuntime>>,
    /// Planificateur disque local
    pub disk_io_scheduler: Arc<DiskIoScheduler>,
}

impl NodeState {
    pub fn new(
        node_id: String,
        store_addr: String,
        api_addr: String,
        store: Arc<PageStore>,
        metrics: Arc<StoreMetrics>,
        vm_tracker: Arc<VmTracker>,
        num_pcpus: usize,
        qmp_dir: String,
        gpu_runtime: Option<Arc<GpuRuntime>>,
    ) -> Self {
        Self {
            node_id,
            store_addr,
            api_addr,
            store,
            metrics,
            vm_tracker,
            quota_registry: QuotaRegistry::new(),
            vcpu_scheduler: VcpuScheduler::new(num_pcpus),
            qmp_dir,
            gpu_runtime,
            disk_io_scheduler: DiskIoScheduler::new(),
        }
    }

    /// Construit un snapshot de l'état courant.
    pub fn snapshot(&self) -> NodeStatus {
        let mem = read_meminfo();
        let vms = self.vm_tracker.local_vms_snapshot();

        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Index vm_id → (cpu_usage_pct, throttle_ratio) depuis le scheduler vCPU
        let vcpu_stats: std::collections::HashMap<u32, (f64, f64)> = {
            self.vcpu_scheduler
                .vm_snapshot()
                .into_iter()
                .map(|s| {
                    // steal_pct est stocké comme throttle_ratio × 100 (voir update_from_cgroup)
                    let throttle = s.steal_pct / 100.0;
                    (s.vm_id, (s.cpu_usage_pct, throttle))
                })
                .collect()
        };
        let disk_stats: std::collections::HashMap<
            u32,
            (f64, f64, u32, bool, bool, Option<String>),
        > = self
            .disk_io_scheduler
            .vm_snapshot()
            .into_iter()
            .map(|s| {
                (
                    s.vm_id,
                    (
                        s.read_bps,
                        s.write_bps,
                        s.io_weight,
                        s.local_share_active,
                        s.io_control_supported,
                        s.io_control_reason,
                    ),
                )
            })
            .collect();

        let vm_entries: Vec<VmStatusEntry> = vms
            .iter()
            .map(|vm| {
                let (avg_cpu_pct, throttle_ratio) =
                    vcpu_stats.get(&vm.vmid).copied().unwrap_or((0.0, 0.0));
                let (
                    disk_read_bps,
                    disk_write_bps,
                    disk_io_weight,
                    disk_local_share_active,
                    disk_io_control_supported,
                    disk_io_control_reason,
                ) = disk_stats
                    .get(&vm.vmid)
                    .cloned()
                    .unwrap_or((0.0, 0.0, 100, false, true, None));
                let gpu_vram_budget_mib = self
                    .gpu_runtime
                    .as_ref()
                    .map(|gpu| gpu.vm_budget_mib(vm.vmid))
                    .unwrap_or(0);
                VmStatusEntry {
                    vmid: vm.vmid,
                    max_mem_mib: vm.max_mem_mib,
                    rss_kb: vm.rss_kb,
                    remote_pages: vm.remote_pages,
                    remote_mem_mib: vm.remote_mem_mib(),
                    status: format!("{:?}", vm.status),
                    avg_cpu_pct,
                    throttle_ratio,
                    gpu_vram_budget_mib,
                    disk_read_bps,
                    disk_write_bps,
                    disk_io_weight,
                    disk_local_share_active,
                    disk_io_control_supported,
                    disk_io_control_reason,
                }
            })
            .collect();

        let vcpu_total = self.vcpu_scheduler.total_vslots();
        let vcpu_free = self.vcpu_scheduler.free_vslots();
        let vcpu_occupancy_pct = self.vcpu_scheduler.occupancy_ratio() * 100.0;
        let gpu = self.gpu_runtime.as_ref().map(|runtime| {
            let snap = runtime.snapshot();
            GpuNodeStatus {
                enabled: snap.enabled,
                backend_name: snap.backend_name,
                render_node: snap.render_node,
                socket_path: snap.socket_path,
                total_vram_mib: snap.total_vram_mib,
                reserved_vram_mib: snap.reserved_vram_mib,
                free_vram_mib: snap.free_vram_mib,
                budgeted_vms: snap.budgeted_vms,
            }
        });

        NodeStatus {
            node_id: self.node_id.clone(),
            store_addr: self.store_addr.clone(),
            api_addr: self.api_addr.clone(),
            mem_total_kb: mem.0,
            mem_available_kb: mem.1,
            mem_usage_pct: if mem.0 > 0 {
                ((mem.0 - mem.1) as f64 / mem.0 as f64) * 100.0
            } else {
                0.0
            },
            pages_stored: self.store.len() as u64,
            store_used_kb: self.store.estimated_bytes() as u64 / 1024,
            vcpu_total,
            vcpu_free,
            vcpu_occupancy_pct,
            disk_pressure_pct: self.disk_io_scheduler.read_node_pressure_pct(),
            local_vms: vm_entries,
            gpu,
            timestamp_secs: ts,
        }
    }

    /// Pages stockées par vm_id sur ce nœud — pour l'API `/api/pages`.
    pub fn pages_per_vm(&self) -> Vec<(u32, u64)> {
        // On parcourt les VMs connues par le tracker
        let vms = self.vm_tracker.local_vms_snapshot();
        let mut result: Vec<(u32, u64)> = vms
            .iter()
            .map(|vm| (vm.vmid, self.vm_tracker.pages_stored_for(vm.vmid)))
            .filter(|(_, pages)| *pages > 0)
            .collect();

        // On inclut aussi les VMs qui ont des pages ici mais ne sont pas locales
        // (pages provenant d'un nœud A distant)
        // Pour cela on itère sur les pages du store — approximation via DashMap
        // En V4 on ajoute un index vm_id→count directement dans le tracker
        result.sort_by_key(|(vmid, _)| *vmid);
        result
    }
}

/// Lit les valeurs clés de /proc/meminfo.
/// Retourne (total_kb, available_kb).
pub fn read_meminfo() -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };

    let mut total = 0u64;
    let mut available = 0u64;

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }

    (total, available)
}
