use std::sync::Arc;

use dashmap::DashMap;
use serde::Serialize;

use crate::gpu_multiplexer::GpuMultiplexer;

#[derive(Debug, Clone, Serialize)]
pub struct GpuVmBudgetSnapshot {
    pub vm_id: u32,
    pub budget_mib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuStatusSnapshot {
    pub enabled: bool,
    pub backend_name: String,
    pub render_node: Option<String>,
    pub socket_path: String,
    pub total_vram_mib: u64,
    pub reserved_vram_mib: u64,
    pub free_vram_mib: u64,
    pub budgeted_vms: usize,
    pub budgets: Vec<GpuVmBudgetSnapshot>,
}

pub struct GpuRuntime {
    mux: Arc<GpuMultiplexer>,
    backend_name: String,
    render_node: Option<String>,
    socket_path: String,
    total_vram_mib: u64,
    budgets: DashMap<u32, u64>,
}

impl GpuRuntime {
    pub fn new(
        mux: Arc<GpuMultiplexer>,
        backend_name: String,
        render_node: Option<String>,
        socket_path: String,
        total_vram_mib: u64,
    ) -> Self {
        Self {
            mux,
            backend_name,
            render_node,
            socket_path,
            total_vram_mib,
            budgets: DashMap::new(),
        }
    }

    pub fn mux(&self) -> Arc<GpuMultiplexer> {
        Arc::clone(&self.mux)
    }

    pub async fn set_vm_budget(&self, vm_id: u32, budget_mib: u64) {
        self.mux.set_vm_budget(vm_id, budget_mib).await;
        if budget_mib == 0 {
            self.budgets.remove(&vm_id);
        } else {
            self.budgets.insert(vm_id, budget_mib);
        }
    }

    pub async fn release_vm(&self, vm_id: u32) {
        self.mux.release_vm(vm_id).await;
        self.budgets.remove(&vm_id);
    }

    pub fn vm_budget_mib(&self, vm_id: u32) -> u64 {
        self.budgets.get(&vm_id).map(|v| *v).unwrap_or(0)
    }

    pub fn snapshot(&self) -> GpuStatusSnapshot {
        let mut budgets: Vec<GpuVmBudgetSnapshot> = self
            .budgets
            .iter()
            .map(|entry| GpuVmBudgetSnapshot {
                vm_id: *entry.key(),
                budget_mib: *entry.value(),
            })
            .collect();
        budgets.sort_by_key(|entry| entry.vm_id);

        let reserved_vram_mib = budgets.iter().map(|entry| entry.budget_mib).sum::<u64>();
        let free_vram_mib = self.total_vram_mib.saturating_sub(reserved_vram_mib);

        GpuStatusSnapshot {
            enabled: true,
            backend_name: self.backend_name.clone(),
            render_node: self.render_node.clone(),
            socket_path: self.socket_path.clone(),
            total_vram_mib: self.total_vram_mib,
            reserved_vram_mib,
            free_vram_mib,
            budgeted_vms: budgets.len(),
            budgets,
        }
    }

    pub async fn prometheus_metrics(&self, node_id: &str) -> String {
        let mut metrics = self.mux.prometheus_metrics(node_id).await;
        let snap = self.snapshot();
        metrics.push_str(&format!(
            "# HELP omega_gpu_vram_total_mib VRAM totale gérée par le daemon (Mio)\n\
             omega_gpu_vram_total_mib{{node=\"{node}\"}} {total}\n\
             # HELP omega_gpu_vram_reserved_mib VRAM réservée par budgets VM (Mio)\n\
             omega_gpu_vram_reserved_mib{{node=\"{node}\"}} {reserved}\n\
             # HELP omega_gpu_vram_free_mib VRAM libre pour de nouveaux budgets (Mio)\n\
             omega_gpu_vram_free_mib{{node=\"{node}\"}} {free}\n",
            node = node_id,
            total = snap.total_vram_mib,
            reserved = snap.reserved_vram_mib,
            free = snap.free_vram_mib,
        ));
        metrics
    }
}
