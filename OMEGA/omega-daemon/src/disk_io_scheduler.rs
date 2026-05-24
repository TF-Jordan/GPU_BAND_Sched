//! Scheduler disque local — partage automatique via `io.weight`.
//!
//! # Politique
//!
//! - toutes les VMs démarrent avec un poids disque nominal
//! - si le nœud subit de la pression I/O et qu'une VM est la plus active,
//!   on la favorise temporairement
//! - les VMs durablement idle perdent temporairement de la priorité
//! - quand la pression retombe, on restaure les poids nominaux

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub const DEFAULT_IO_WEIGHT: u32 = 100;
pub const BOOSTED_IO_WEIGHT: u32 = 200;
pub const DONOR_IO_WEIGHT: u32 = 50;
pub const DISK_PRESSURE_TRIGGER_PCT: f64 = 5.0;
pub const DISK_BORROWER_TRIGGER_BPS: f64 = 8.0 * 1024.0 * 1024.0;
pub const DISK_IDLE_BPS: f64 = 512.0 * 1024.0;
pub const DISK_IDLE_STABLE_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize)]
pub struct VmDiskState {
    pub vm_id: u32,
    pub read_bps: f64,
    pub write_bps: f64,
    pub io_weight: u32,
    pub local_share_active: bool,
    pub io_control_supported: bool,
    pub io_control_reason: Option<String>,
    pub idle_since: Option<u64>,
    pub updated_at: u64,
}

impl VmDiskState {
    pub fn new(vm_id: u32) -> Self {
        Self {
            vm_id,
            read_bps: 0.0,
            write_bps: 0.0,
            io_weight: DEFAULT_IO_WEIGHT,
            local_share_active: false,
            io_control_supported: true,
            io_control_reason: None,
            idle_since: Some(now_secs()),
            updated_at: now_secs(),
        }
    }

    pub fn total_bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }

    pub fn idle_duration_secs(&self) -> u64 {
        self.idle_since
            .map(|started| now_secs().saturating_sub(started))
            .unwrap_or(0)
    }

    pub fn is_idle(&self) -> bool {
        self.total_bps() <= DISK_IDLE_BPS && self.idle_duration_secs() >= DISK_IDLE_STABLE_SECS
    }

    pub fn needs_priority(&self, node_pressure_pct: f64) -> bool {
        node_pressure_pct >= DISK_PRESSURE_TRIGGER_PCT
            && self.total_bps() >= DISK_BORROWER_TRIGGER_BPS
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskSchedulerStatus {
    pub node_pressure_pct: f64,
    pub busy_vms: Vec<u32>,
    pub donor_vms: Vec<u32>,
    pub vm_states: Vec<VmDiskState>,
}

pub struct DiskIoScheduler {
    node_pressure_pct: RwLock<f64>,
    vms: RwLock<HashMap<u32, VmDiskState>>,
}

impl DiskIoScheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            node_pressure_pct: RwLock::new(0.0),
            vms: RwLock::new(HashMap::new()),
        })
    }

    pub fn ensure_vm(&self, vm_id: u32) {
        self.vms
            .write()
            .unwrap()
            .entry(vm_id)
            .or_insert_with(|| VmDiskState::new(vm_id));
    }

    pub fn release_vm(&self, vm_id: u32) {
        self.vms.write().unwrap().remove(&vm_id);
    }

    pub fn update_vm_io(&self, vm_id: u32, read_bps: f64, write_bps: f64) {
        let mut vms = self.vms.write().unwrap();
        let vm = vms.entry(vm_id).or_insert_with(|| VmDiskState::new(vm_id));
        vm.read_bps = read_bps.max(0.0);
        vm.write_bps = write_bps.max(0.0);
        if vm.total_bps() <= DISK_IDLE_BPS {
            if vm.idle_since.is_none() {
                vm.idle_since = Some(now_secs());
            }
        } else {
            vm.idle_since = None;
        }
        vm.updated_at = now_secs();
    }

    pub fn set_node_pressure_pct(&self, pressure_pct: f64) {
        *self.node_pressure_pct.write().unwrap() = pressure_pct.max(0.0);
    }

    pub fn read_node_pressure_pct(&self) -> f64 {
        *self.node_pressure_pct.read().unwrap()
    }

    pub fn set_vm_weight(&self, vm_id: u32, weight: u32, local_share_active: bool) {
        if let Some(vm) = self.vms.write().unwrap().get_mut(&vm_id) {
            vm.io_weight = weight.clamp(1, 10000);
            vm.local_share_active = local_share_active;
            vm.io_control_supported = true;
            vm.io_control_reason = None;
            vm.updated_at = now_secs();
        }
    }

    pub fn mark_io_control_unsupported(&self, vm_id: u32, reason: String) -> bool {
        let mut vms = self.vms.write().unwrap();
        let vm = vms.entry(vm_id).or_insert_with(|| VmDiskState::new(vm_id));
        let changed =
            vm.io_control_supported || vm.io_control_reason.as_deref() != Some(reason.as_str());
        vm.io_control_supported = false;
        vm.io_control_reason = Some(reason);
        vm.local_share_active = false;
        vm.io_weight = DEFAULT_IO_WEIGHT;
        vm.updated_at = now_secs();
        changed
    }

    pub fn get_vm_state(&self, vm_id: u32) -> Option<VmDiskState> {
        self.vms.read().unwrap().get(&vm_id).cloned()
    }

    pub fn vm_snapshot(&self) -> Vec<VmDiskState> {
        let mut out: Vec<_> = self.vms.read().unwrap().values().cloned().collect();
        out.sort_by_key(|vm| vm.vm_id);
        out
    }

    pub fn local_share_borrowers(&self) -> Vec<u32> {
        let pressure = self.read_node_pressure_pct();
        let mut borrowers: Vec<_> = self
            .vms
            .read()
            .unwrap()
            .values()
            .filter(|vm| vm.needs_priority(pressure))
            .cloned()
            .collect();
        borrowers.sort_by(|a, b| {
            b.total_bps()
                .partial_cmp(&a.total_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        borrowers.into_iter().map(|vm| vm.vm_id).collect()
    }

    pub fn idle_peers(&self) -> Vec<u32> {
        let mut peers: Vec<_> = self
            .vms
            .read()
            .unwrap()
            .values()
            .filter(|vm| vm.is_idle())
            .cloned()
            .collect();
        peers.sort_by(|a, b| {
            a.total_bps()
                .partial_cmp(&b.total_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        peers.into_iter().map(|vm| vm.vm_id).collect()
    }

    pub fn status(&self) -> DiskSchedulerStatus {
        let busy_set: HashSet<u32> = self.local_share_borrowers().into_iter().collect();
        let donor_set: HashSet<u32> = self.idle_peers().into_iter().collect();
        DiskSchedulerStatus {
            node_pressure_pct: self.read_node_pressure_pct(),
            busy_vms: busy_set.iter().copied().collect(),
            donor_vms: donor_set.iter().copied().collect(),
            vm_states: self.vm_snapshot(),
        }
    }

    pub fn prometheus_metrics(&self, node_id: &str) -> String {
        let status = self.status();
        let local_share_count = status
            .vm_states
            .iter()
            .filter(|vm| vm.local_share_active)
            .count();
        let unsupported_count = status
            .vm_states
            .iter()
            .filter(|vm| !vm.io_control_supported)
            .count();

        let mut out = format!(
            "# HELP omega_disk_io_pressure_pct Pression I/O du nœud (PSI avg10)\n\
             omega_disk_io_pressure_pct{{node=\"{node}\"}} {pressure:.2}\n\
             # HELP omega_disk_local_share_vms Nombre de VMs en partage I/O local\n\
             omega_disk_local_share_vms{{node=\"{node}\"}} {local_share_count}\n\
             # HELP omega_disk_io_control_unsupported_vms Nombre de VMs sans support io.weight\n\
             omega_disk_io_control_unsupported_vms{{node=\"{node}\"}} {unsupported_count}\n",
            node = node_id,
            pressure = status.node_pressure_pct,
            local_share_count = local_share_count,
            unsupported_count = unsupported_count,
        );

        for vm in &status.vm_states {
            out.push_str(&format!(
                "# HELP omega_vm_disk_read_bps Débit lecture VM (octets/s)\n\
                 omega_vm_disk_read_bps{{node=\"{node}\",vm=\"{vmid}\"}} {read:.0}\n\
                 # HELP omega_vm_disk_write_bps Débit écriture VM (octets/s)\n\
                 omega_vm_disk_write_bps{{node=\"{node}\",vm=\"{vmid}\"}} {write:.0}\n\
                 # HELP omega_vm_disk_io_weight Poids I/O courant VM\n\
                 omega_vm_disk_io_weight{{node=\"{node}\",vm=\"{vmid}\"}} {weight}\n",
                node = node_id,
                vmid = vm.vm_id,
                read = vm.read_bps,
                write = vm.write_bps,
                weight = vm.io_weight,
            ));
        }

        out
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_busy_vm_becomes_borrower_under_pressure() {
        let sched = DiskIoScheduler::new();
        sched.ensure_vm(9001);
        sched.ensure_vm(9002);
        sched.update_vm_io(9001, 12.0 * 1024.0 * 1024.0, 0.0);
        sched.update_vm_io(9002, 100.0, 100.0);
        sched.set_node_pressure_pct(12.0);

        let borrowers = sched.local_share_borrowers();
        assert_eq!(borrowers, vec![9001]);
    }

    #[test]
    fn test_idle_vm_becomes_donor_after_stability() {
        let sched = DiskIoScheduler::new();
        sched.ensure_vm(9001);
        sched.update_vm_io(9001, 10.0, 10.0);
        {
            let mut vms = sched.vms.write().unwrap();
            vms.get_mut(&9001).unwrap().idle_since = Some(now_secs() - DISK_IDLE_STABLE_SECS - 1);
        }
        let donors = sched.idle_peers();
        assert_eq!(donors, vec![9001]);
    }
}
