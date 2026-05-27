use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClusterStateSnapshot {
    pub nodes: Vec<NodeInfoSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfoSnapshot {
    pub node_id: String,
    pub mem_total_kb: i64,
    pub mem_available_kb: i64,
    #[serde(default)]
    pub reachable: bool,
    #[serde(default)]
    pub local_vms: Vec<VmEntrySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmEntrySnapshot {
    pub vmid: i64,
    pub max_mem_mib: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionVmSpec {
    pub vmid: i64,
    pub max_mem_mib: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_vcpus")]
    pub vcpus: i64,
    #[serde(default)]
    pub preferred_node: Option<String>,
    #[serde(default)]
    pub forbidden_nodes: Vec<String>,
}

fn default_vcpus() -> i64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionConfig {
    #[serde(default = "default_safety_margin")]
    pub safety_margin: f64,
    #[serde(default = "default_true")]
    pub prefer_local: bool,
    #[serde(default = "default_min_remote_node_free")]
    pub min_remote_node_free: i64,
}

fn default_safety_margin() -> f64 {
    0.10
}

fn default_true() -> bool {
    true
}

fn default_min_remote_node_free() -> i64 {
    512
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            safety_margin: default_safety_margin(),
            prefer_local: true,
            min_remote_node_free: default_min_remote_node_free(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionDecisionPayload {
    pub admitted: bool,
    pub vmid: i64,
    pub max_mem_mib: i64,
    #[serde(default)]
    pub placement_node: String,
    #[serde(default)]
    pub local_budget_mib: i64,
    #[serde(default)]
    pub remote_budget_mib: i64,
    #[serde(default)]
    pub remote_nodes: Vec<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub cluster_free_mib: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionBatchRequest {
    pub config: AdmissionConfig,
    pub cluster: ClusterStateSnapshot,
    pub vms: Vec<AdmissionVmSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionRequest {
    pub config: AdmissionConfig,
    pub cluster: ClusterStateSnapshot,
    pub vm: AdmissionVmSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationThresholdsPayload {
    #[serde(default = "default_ram_high_pct")]
    pub ram_high_pct: f64,
    #[serde(default = "default_ram_critical_pct")]
    pub ram_critical_pct: f64,
    #[serde(default = "default_vcpu_throttle_trigger")]
    pub vcpu_throttle_trigger: f64,
    #[serde(default = "default_vcpu_saturation_pct")]
    pub vcpu_saturation_pct: f64,
    #[serde(default = "default_remote_paging_pct")]
    pub remote_paging_pct: f64,
    #[serde(default = "default_gpu_high_pct")]
    pub gpu_high_pct: f64,
    #[serde(default = "default_disk_high_pct")]
    pub disk_high_pct: f64,
    #[serde(default = "default_disk_hot_vm_bps")]
    pub disk_hot_vm_bps: f64,
    #[serde(default = "default_idle_cpu_pct")]
    pub idle_cpu_pct: f64,
    #[serde(default = "default_idle_duration_secs")]
    pub idle_duration_secs: f64,
    #[serde(default = "default_target_max_ram_pct")]
    pub target_max_ram_pct: f64,
    #[serde(default = "default_target_max_vcpu_pct")]
    pub target_max_vcpu_pct: f64,
}

fn default_ram_high_pct() -> f64 {
    85.0
}
fn default_ram_critical_pct() -> f64 {
    95.0
}
fn default_vcpu_throttle_trigger() -> f64 {
    0.30
}
fn default_vcpu_saturation_pct() -> f64 {
    90.0
}
fn default_remote_paging_pct() -> f64 {
    60.0
}
fn default_gpu_high_pct() -> f64 {
    90.0
}
fn default_disk_high_pct() -> f64 {
    10.0
}
fn default_disk_hot_vm_bps() -> f64 {
    8.0 * 1024.0 * 1024.0
}
fn default_idle_cpu_pct() -> f64 {
    5.0
}
fn default_idle_duration_secs() -> f64 {
    60.0
}
fn default_target_max_ram_pct() -> f64 {
    80.0
}
fn default_target_max_vcpu_pct() -> f64 {
    80.0
}

impl Default for MigrationThresholdsPayload {
    fn default() -> Self {
        Self {
            ram_high_pct: default_ram_high_pct(),
            ram_critical_pct: default_ram_critical_pct(),
            vcpu_throttle_trigger: default_vcpu_throttle_trigger(),
            vcpu_saturation_pct: default_vcpu_saturation_pct(),
            remote_paging_pct: default_remote_paging_pct(),
            gpu_high_pct: default_gpu_high_pct(),
            disk_high_pct: default_disk_high_pct(),
            disk_hot_vm_bps: default_disk_hot_vm_bps(),
            idle_cpu_pct: default_idle_cpu_pct(),
            idle_duration_secs: default_idle_duration_secs(),
            target_max_ram_pct: default_target_max_ram_pct(),
            target_max_vcpu_pct: default_target_max_vcpu_pct(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationVmStatePayload {
    pub vm_id: i64,
    pub status: String,
    pub max_mem_mib: i64,
    #[serde(default)]
    pub rss_kb: i64,
    #[serde(default)]
    pub remote_pages: i64,
    #[serde(default)]
    pub avg_cpu_pct: f64,
    #[serde(default)]
    pub throttle_ratio: f64,
    #[serde(default)]
    pub gpu_vram_budget_mib: i64,
    #[serde(default)]
    pub disk_read_bps: f64,
    #[serde(default)]
    pub disk_write_bps: f64,
    #[serde(default = "default_io_weight")]
    pub disk_io_weight: i64,
    #[serde(default)]
    pub disk_local_share_active: bool,
    #[serde(default)]
    pub idle_duration_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationNodeStatePayload {
    pub node_id: String,
    pub mem_total_kb: i64,
    pub mem_available_kb: i64,
    #[serde(default = "default_vcpu_total")]
    pub vcpu_total: i64,
    #[serde(default = "default_vcpu_total")]
    pub vcpu_free: i64,
    #[serde(default)]
    pub gpu_total_vram_mib: i64,
    #[serde(default)]
    pub gpu_free_vram_mib: i64,
    #[serde(default)]
    pub disk_pressure_pct: f64,
    #[serde(default)]
    pub local_vms: Vec<MigrationVmStatePayload>,
}

fn default_vcpu_total() -> i64 {
    24
}

fn default_io_weight() -> i64 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationEvaluateRequest {
    #[serde(default)]
    pub thresholds: MigrationThresholdsPayload,
    pub nodes: Vec<MigrationNodeStatePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickMigrationTypeRequest {
    #[serde(default)]
    pub thresholds: MigrationThresholdsPayload,
    pub vm: MigrationVmStatePayload,
    pub node_ram_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationCandidatePayload {
    pub vm: MigrationVmStatePayload,
    pub source: String,
    pub target: String,
    pub mtype: String,
    pub reason: String,
    pub urgency: i64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuRebalanceConfigPayload {
    #[serde(default = "default_gpu_migration_cooldown_secs")]
    pub migration_cooldown_secs: f64,
    #[serde(default = "default_gpu_load_improvement_pct")]
    pub load_improvement_pct: f64,
}

fn default_gpu_migration_cooldown_secs() -> f64 {
    120.0
}

fn default_gpu_load_improvement_pct() -> f64 {
    10.0
}

impl Default for GpuRebalanceConfigPayload {
    fn default() -> Self {
        Self {
            migration_cooldown_secs: default_gpu_migration_cooldown_secs(),
            load_improvement_pct: default_gpu_load_improvement_pct(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuRebalanceRequest {
    #[serde(default)]
    pub config: GpuRebalanceConfigPayload,
    pub source_node: String,
    pub required_vcpus: i64,
    pub gpu_budget_mib: i64,
    pub now: f64,
    #[serde(default)]
    pub last_gpu_migrations: HashMap<i64, f64>,
    pub vm: MigrationVmStatePayload,
    pub nodes: Vec<MigrationNodeStatePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuEvictionPayload {
    pub vm: MigrationVmStatePayload,
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuRebalancePlanPayload {
    pub action: String,
    #[serde(default)]
    pub target_node: String,
    #[serde(default)]
    pub evictions: Vec<GpuEvictionPayload>,
    #[serde(default)]
    pub detail: String,
}

pub fn admit_vm(
    config: &AdmissionConfig,
    cluster: &ClusterStateSnapshot,
    vm: &AdmissionVmSpec,
) -> AdmissionDecisionPayload {
    let reachable: Vec<&NodeInfoSnapshot> = cluster.nodes.iter().filter(|n| n.reachable).collect();
    if reachable.is_empty() {
        return AdmissionDecisionPayload {
            admitted: false,
            vmid: vm.vmid,
            max_mem_mib: vm.max_mem_mib,
            placement_node: String::new(),
            local_budget_mib: 0,
            remote_budget_mib: 0,
            remote_nodes: Vec::new(),
            reason: "aucun nœud joignable dans le cluster".to_string(),
            cluster_free_mib: 0,
        };
    }

    let candidates: Vec<&NodeInfoSnapshot> = reachable
        .into_iter()
        .filter(|n| !vm.forbidden_nodes.iter().any(|f| f == &n.node_id))
        .collect();
    if candidates.is_empty() {
        return AdmissionDecisionPayload {
            admitted: false,
            vmid: vm.vmid,
            max_mem_mib: vm.max_mem_mib,
            placement_node: String::new(),
            local_budget_mib: 0,
            remote_budget_mib: 0,
            remote_nodes: Vec::new(),
            reason: "tous les nœuds sont dans la liste forbidden_nodes".to_string(),
            cluster_free_mib: 0,
        };
    }

    let cluster_free: i64 = candidates.iter().map(|n| effective_free(config, n)).sum();
    if cluster_free < vm.max_mem_mib {
        return AdmissionDecisionPayload {
            admitted: false,
            vmid: vm.vmid,
            max_mem_mib: vm.max_mem_mib,
            placement_node: String::new(),
            local_budget_mib: 0,
            remote_budget_mib: 0,
            remote_nodes: Vec::new(),
            reason: format!(
                "capacité cluster insuffisante : {} Mio dispo < {} Mio demandé — ajouter de la RAM ou éteindre des VMs",
                cluster_free, vm.max_mem_mib
            ),
            cluster_free_mib: cluster_free,
        };
    }

    let placement = select_placement_node(config, &candidates, vm);
    let local_budget = effective_free(config, placement).min(vm.max_mem_mib);
    let remote_budget = vm.max_mem_mib - local_budget;

    let remote_nodes = if remote_budget > 0 {
        match check_remote_availability(config, &candidates, &placement.node_id, remote_budget) {
            Some(nodes) => nodes,
            None => {
                return AdmissionDecisionPayload {
                    admitted: false,
                    vmid: vm.vmid,
                    max_mem_mib: vm.max_mem_mib,
                    placement_node: String::new(),
                    local_budget_mib: 0,
                    remote_budget_mib: 0,
                    remote_nodes: Vec::new(),
                    reason: format!(
                        "nœud {} a seulement {} Mio (besoin de {} Mio), et pas assez de RAM distante disponible sur les autres nœuds pour les {} Mio restants",
                        placement.node_id, local_budget, vm.max_mem_mib, remote_budget
                    ),
                    cluster_free_mib: cluster_free,
                };
            }
        }
    } else {
        Vec::new()
    };

    let reason = if remote_budget > 0 {
        format!(
            "admis sur {} ({} Mio local + {} Mio remote sur {:?})",
            placement.node_id, local_budget, remote_budget, remote_nodes
        )
    } else {
        format!(
            "admis sur {} ({} Mio local)",
            placement.node_id, local_budget
        )
    };

    AdmissionDecisionPayload {
        admitted: true,
        vmid: vm.vmid,
        max_mem_mib: vm.max_mem_mib,
        placement_node: placement.node_id.clone(),
        local_budget_mib: local_budget,
        remote_budget_mib: remote_budget,
        remote_nodes,
        reason,
        cluster_free_mib: cluster_free,
    }
}

pub fn admit_batch(
    config: &AdmissionConfig,
    cluster: &ClusterStateSnapshot,
    vms: &[AdmissionVmSpec],
) -> Vec<AdmissionDecisionPayload> {
    let mut reservations: Vec<(String, i64)> = Vec::new();
    let mut decisions = Vec::new();
    for vm in vms {
        let virtual_cluster = apply_reservations(cluster, &reservations);
        let decision = admit_vm(config, &virtual_cluster, vm);
        if decision.admitted {
            reservations.push((decision.placement_node.clone(), decision.local_budget_mib));
        }
        decisions.push(decision);
    }
    decisions
}

fn apply_reservations(
    cluster: &ClusterStateSnapshot,
    reservations: &[(String, i64)],
) -> ClusterStateSnapshot {
    let mut nodes = cluster.nodes.clone();
    for node in &mut nodes {
        let reserved_mib: i64 = reservations
            .iter()
            .filter(|(node_id, _)| node_id == &node.node_id)
            .map(|(_, mib)| *mib)
            .sum();
        if reserved_mib > 0 {
            node.mem_available_kb = (node.mem_available_kb - reserved_mib * 1024).max(0);
        }
    }
    ClusterStateSnapshot { nodes }
}

fn effective_free(config: &AdmissionConfig, node: &NodeInfoSnapshot) -> i64 {
    let free_mib = node.mem_available_kb / 1024;
    ((free_mib as f64) * (1.0 - config.safety_margin)).floor() as i64
}

fn select_placement_node<'a>(
    config: &AdmissionConfig,
    candidates: &[&'a NodeInfoSnapshot],
    vm: &AdmissionVmSpec,
) -> &'a NodeInfoSnapshot {
    if let Some(preferred) = &vm.preferred_node {
        if let Some(node) = candidates.iter().copied().find(|n| {
            n.node_id == *preferred
                && effective_free(config, n) >= (vm.max_mem_mib as f64 * 0.5) as i64
        }) {
            return node;
        }
    }

    let full_capacity: Vec<&NodeInfoSnapshot> = candidates
        .iter()
        .copied()
        .filter(|n| effective_free(config, n) >= vm.max_mem_mib)
        .collect();

    if config.prefer_local && !full_capacity.is_empty() {
        return full_capacity
            .into_iter()
            .min_by_key(|n| effective_free(config, n))
            .unwrap();
    }

    candidates
        .iter()
        .copied()
        .max_by_key(|n| effective_free(config, n))
        .unwrap()
}

fn check_remote_availability(
    config: &AdmissionConfig,
    candidates: &[&NodeInfoSnapshot],
    placement_id: &str,
    remote_budget: i64,
) -> Option<Vec<String>> {
    let mut others: Vec<&NodeInfoSnapshot> = candidates
        .iter()
        .copied()
        .filter(|n| {
            n.node_id != placement_id && effective_free(config, n) >= config.min_remote_node_free
        })
        .collect();
    if others.is_empty() {
        return None;
    }
    let others_free: i64 = others.iter().map(|n| effective_free(config, n)).sum();
    if others_free < remote_budget {
        return None;
    }
    others.sort_by_key(|n| -effective_free(config, n));
    Some(others.into_iter().map(|n| n.node_id.clone()).collect())
}

pub fn pick_migration_type(
    thresholds: &MigrationThresholdsPayload,
    vm: &MigrationVmStatePayload,
    node_ram_pct: f64,
) -> String {
    let critical = node_ram_pct >= thresholds.ram_critical_pct;
    pick_type(thresholds, vm, critical).to_string()
}

pub fn evaluate_migrations(
    thresholds: &MigrationThresholdsPayload,
    nodes: &[MigrationNodeStatePayload],
) -> Vec<MigrationCandidatePayload> {
    let mut candidates = Vec::new();
    for node in nodes {
        for vm in &node.local_vms {
            if remote_pct(vm) > thresholds.remote_paging_pct {
                if let Some(target) = best_target(thresholds, &node.node_id, vm, nodes) {
                    candidates.push(MigrationCandidatePayload {
                        vm: vm.clone(),
                        source: node.node_id.clone(),
                        target,
                        mtype: pick_type(thresholds, vm, false).to_string(),
                        reason: "excessive_remote_paging".to_string(),
                        urgency: 1,
                        detail: format!("{:.0}% RAM stockée à distance", remote_pct(vm)),
                    });
                }
            }

            let ram_used_pct = node_ram_used_pct(node);
            if ram_used_pct >= thresholds.ram_critical_pct {
                if let Some(target) = best_target(thresholds, &node.node_id, vm, nodes) {
                    candidates.push(MigrationCandidatePayload {
                        vm: vm.clone(),
                        source: node.node_id.clone(),
                        target,
                        mtype: "cold".to_string(),
                        reason: "memory_pressure".to_string(),
                        urgency: 2,
                        detail: format!(
                            "RAM nœud {:.0}% (critique ≥ {:.0}%)",
                            ram_used_pct, thresholds.ram_critical_pct
                        ),
                    });
                }
            } else if ram_used_pct >= thresholds.ram_high_pct {
                if let Some(target) = best_target(thresholds, &node.node_id, vm, nodes) {
                    candidates.push(MigrationCandidatePayload {
                        vm: vm.clone(),
                        source: node.node_id.clone(),
                        target,
                        mtype: pick_type(thresholds, vm, false).to_string(),
                        reason: "memory_pressure".to_string(),
                        urgency: 1,
                        detail: format!(
                            "RAM nœud {:.0}% (haute ≥ {:.0}%)",
                            ram_used_pct, thresholds.ram_high_pct
                        ),
                    });
                }
            }

            if vm.throttle_ratio > thresholds.vcpu_throttle_trigger
                && node_vcpu_used_pct(node) > thresholds.vcpu_saturation_pct
            {
                if let Some(target) = best_target_vcpu(thresholds, &node.node_id, vm, nodes) {
                    candidates.push(MigrationCandidatePayload {
                        vm: vm.clone(),
                        source: node.node_id.clone(),
                        target,
                        mtype: "live".to_string(),
                        reason: "cpu_saturation".to_string(),
                        urgency: 1,
                        detail: format!(
                            "throttle {:.0}%, nœud {:.0}% vCPU",
                            vm.throttle_ratio * 100.0,
                            node_vcpu_used_pct(node)
                        ),
                    });
                }
            }

            let gpu_used_pct = node_gpu_used_pct(node);
            if vm.gpu_vram_budget_mib > 0
                && node.gpu_total_vram_mib > 0
                && gpu_used_pct >= thresholds.gpu_high_pct
            {
                if let Some(target) = best_target(thresholds, &node.node_id, vm, nodes) {
                    candidates.push(MigrationCandidatePayload {
                        vm: vm.clone(),
                        source: node.node_id.clone(),
                        target,
                        mtype: pick_type(thresholds, vm, false).to_string(),
                        reason: "gpu_saturation".to_string(),
                        urgency: 1,
                        detail: format!(
                            "GPU réservé {:.0}% ({} Mio pour la VM)",
                            gpu_used_pct, vm.gpu_vram_budget_mib
                        ),
                    });
                }
            }

            if node.disk_pressure_pct >= thresholds.disk_high_pct
                && vm_disk_total_bps(vm) >= thresholds.disk_hot_vm_bps
            {
                if let Some(target) = best_target(thresholds, &node.node_id, vm, nodes) {
                    candidates.push(MigrationCandidatePayload {
                        vm: vm.clone(),
                        source: node.node_id.clone(),
                        target,
                        mtype: "live".to_string(),
                        reason: "disk_saturation".to_string(),
                        urgency: 1,
                        detail: format!(
                            "PSI disque {:.1}% VM {:.1} Mio/s",
                            node.disk_pressure_pct,
                            vm_disk_total_bps(vm) / 1024.0 / 1024.0
                        ),
                    });
                }
            }
        }
    }

    candidates.sort_by(|a, b| b.urgency.cmp(&a.urgency));
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| seen.insert(c.vm.vm_id))
        .collect()
}

pub fn evaluate_gpu_rebalance(req: &GpuRebalanceRequest) -> GpuRebalancePlanPayload {
    if req.gpu_budget_mib <= 0 {
        return GpuRebalancePlanPayload {
            action: "none".to_string(),
            target_node: String::new(),
            evictions: Vec::new(),
            detail: String::new(),
        };
    }

    if let Some(last) = req.last_gpu_migrations.get(&req.vm.vm_id) {
        if req.now - last < req.config.migration_cooldown_secs {
            return GpuRebalancePlanPayload {
                action: "none".to_string(),
                target_node: String::new(),
                evictions: Vec::new(),
                detail: "cooldown".to_string(),
            };
        }
    }

    if let Some(target) = gpu_candidate_target(
        &req.source_node,
        &req.vm,
        &req.nodes,
        req.required_vcpus,
        req.gpu_budget_mib,
        req.config.load_improvement_pct,
    ) {
        return GpuRebalancePlanPayload {
            action: "migrate".to_string(),
            target_node: target,
            evictions: Vec::new(),
            detail: "direct_gpu_rebalance".to_string(),
        };
    }

    if let Some((target, evictions)) = find_gpu_space_creation_plan(
        &req.source_node,
        &req.vm,
        &req.nodes,
        req.required_vcpus,
        req.gpu_budget_mib,
        &req.last_gpu_migrations,
        req.now,
        req.config.load_improvement_pct,
        req.config.migration_cooldown_secs,
    ) {
        let has_evictions = !evictions.is_empty();
        return GpuRebalancePlanPayload {
            action: if has_evictions {
                "evict_then_migrate".to_string()
            } else {
                "migrate".to_string()
            },
            target_node: target,
            evictions,
            detail: if has_evictions {
                "gpu_space_creation".to_string()
            } else {
                "gpu_rebalance".to_string()
            },
        };
    }

    GpuRebalancePlanPayload {
        action: "none".to_string(),
        target_node: String::new(),
        evictions: Vec::new(),
        detail: String::new(),
    }
}

fn pick_type(
    thresholds: &MigrationThresholdsPayload,
    vm: &MigrationVmStatePayload,
    critical: bool,
) -> &'static str {
    if vm.status == "stopped" {
        return "cold";
    }
    if critical {
        return "cold";
    }
    if is_idle(thresholds, vm) {
        return "cold";
    }
    "live"
}

fn is_idle(thresholds: &MigrationThresholdsPayload, vm: &MigrationVmStatePayload) -> bool {
    vm.avg_cpu_pct < thresholds.idle_cpu_pct
        && vm.idle_duration_secs.unwrap_or(0.0) >= thresholds.idle_duration_secs
}

fn remote_pct(vm: &MigrationVmStatePayload) -> f64 {
    let total_pages = vm.max_mem_mib * 256;
    if total_pages <= 0 {
        return 0.0;
    }
    (vm.remote_pages as f64) / (total_pages as f64) * 100.0
}

fn node_ram_used_pct(node: &MigrationNodeStatePayload) -> f64 {
    if node.mem_total_kb <= 0 {
        return 0.0;
    }
    ((node.mem_total_kb - node.mem_available_kb) as f64) / (node.mem_total_kb as f64) * 100.0
}

fn node_vcpu_used_pct(node: &MigrationNodeStatePayload) -> f64 {
    if node.vcpu_total <= 0 {
        return 0.0;
    }
    ((node.vcpu_total - node.vcpu_free) as f64) / (node.vcpu_total as f64) * 100.0
}

fn node_gpu_used_pct(node: &MigrationNodeStatePayload) -> f64 {
    if node.gpu_total_vram_mib <= 0 {
        return 0.0;
    }
    ((node.gpu_total_vram_mib - node.gpu_free_vram_mib) as f64) / (node.gpu_total_vram_mib as f64)
        * 100.0
}

fn node_can_accept_vm(
    thresholds: &MigrationThresholdsPayload,
    node: &MigrationNodeStatePayload,
    vm: &MigrationVmStatePayload,
) -> bool {
    let vm_ram_kb = vm.max_mem_mib * 1024;
    let new_avail = node.mem_available_kb - vm_ram_kb;
    let new_avail_pct = if node.mem_total_kb > 0 {
        (new_avail as f64) / (node.mem_total_kb as f64) * 100.0
    } else {
        0.0
    };
    let ram_ok = new_avail_pct >= (100.0 - thresholds.target_max_ram_pct);
    let vcpu_ok = node_vcpu_used_pct(node) < thresholds.target_max_vcpu_pct;
    let disk_ok = node.disk_pressure_pct < 15.0;
    let gpu_ok = vm.gpu_vram_budget_mib == 0
        || (node.gpu_total_vram_mib > 0 && node.gpu_free_vram_mib >= vm.gpu_vram_budget_mib);
    ram_ok && vcpu_ok && disk_ok && gpu_ok
}

fn placement_score_for_vm(node: &MigrationNodeStatePayload, vm: &MigrationVmStatePayload) -> f64 {
    let ram_free_ratio = 1.0 - node_ram_used_pct(node) / 100.0;
    let vcpu_free_ratio = 1.0 - node_vcpu_used_pct(node) / 100.0;
    let disk_free_ratio = 1.0 - node.disk_pressure_pct / 100.0;
    if vm.gpu_vram_budget_mib <= 0 {
        return ram_free_ratio * 0.5 + vcpu_free_ratio * 0.3 + disk_free_ratio * 0.2;
    }
    let gpu_free_ratio = if node.gpu_total_vram_mib > 0 {
        node.gpu_free_vram_mib as f64 / node.gpu_total_vram_mib as f64
    } else {
        0.0
    };
    ram_free_ratio * 0.35 + vcpu_free_ratio * 0.25 + disk_free_ratio * 0.20 + gpu_free_ratio * 0.20
}

fn vm_disk_total_bps(vm: &MigrationVmStatePayload) -> f64 {
    vm.disk_read_bps + vm.disk_write_bps
}

fn best_target(
    thresholds: &MigrationThresholdsPayload,
    source_id: &str,
    vm: &MigrationVmStatePayload,
    nodes: &[MigrationNodeStatePayload],
) -> Option<String> {
    nodes
        .iter()
        .filter(|n| n.node_id != source_id && node_can_accept_vm(thresholds, n, vm))
        .max_by(|a, b| {
            placement_score_for_vm(a, vm)
                .partial_cmp(&placement_score_for_vm(b, vm))
                .unwrap_or(Ordering::Equal)
        })
        .map(|n| n.node_id.clone())
}

fn best_target_vcpu(
    thresholds: &MigrationThresholdsPayload,
    source_id: &str,
    vm: &MigrationVmStatePayload,
    nodes: &[MigrationNodeStatePayload],
) -> Option<String> {
    nodes
        .iter()
        .filter(|n| {
            n.node_id != source_id
                && node_vcpu_used_pct(n) < thresholds.target_max_vcpu_pct
                && node_can_accept_vm(thresholds, n, vm)
        })
        .max_by(|a, b| {
            (a.vcpu_free, (placement_score_for_vm(a, vm) * 1000.0) as i64)
                .cmp(&(b.vcpu_free, (placement_score_for_vm(b, vm) * 1000.0) as i64))
        })
        .map(|n| n.node_id.clone())
}

fn target_can_host_vm_with_requirements(
    node: &MigrationNodeStatePayload,
    vm: &MigrationVmStatePayload,
    required_vcpus: i64,
    gpu_budget_mib: i64,
) -> bool {
    if node.vcpu_free < required_vcpus {
        return false;
    }
    if vm.max_mem_mib > 0 && node.mem_available_kb < vm.max_mem_mib * 1024 {
        return false;
    }
    if gpu_budget_mib > 0
        && (node.gpu_total_vram_mib <= 0 || node.gpu_free_vram_mib < gpu_budget_mib)
    {
        return false;
    }
    true
}

fn gpu_candidate_target(
    source_id: &str,
    vm: &MigrationVmStatePayload,
    nodes: &[MigrationNodeStatePayload],
    required_vcpus: i64,
    gpu_budget_mib: i64,
    load_improvement_pct: f64,
) -> Option<String> {
    let source = nodes.iter().find(|n| n.node_id == source_id)?;
    let mut candidates: Vec<&MigrationNodeStatePayload> = nodes
        .iter()
        .filter(|n| {
            n.node_id != source_id
                && n.gpu_total_vram_mib > 0
                && target_can_host_vm_with_requirements(n, vm, required_vcpus, gpu_budget_mib)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|n| {
        (
            (node_gpu_used_pct(n) * 1000.0) as i64,
            -n.gpu_free_vram_mib,
            (node_vcpu_used_pct(n) * 1000.0) as i64,
            (node_ram_used_pct(n) * 1000.0) as i64,
        )
    });
    let best = candidates[0];
    if source.gpu_total_vram_mib <= 0
        || source.gpu_free_vram_mib < gpu_budget_mib
        || node_gpu_used_pct(source) - node_gpu_used_pct(best) >= load_improvement_pct
    {
        return Some(best.node_id.clone());
    }
    None
}

fn best_gpu_eviction_target(
    source_id: &str,
    vm: &MigrationVmStatePayload,
    nodes: &[MigrationNodeStatePayload],
    blocked_nodes: &HashSet<String>,
) -> Option<String> {
    let mut candidates: Vec<&MigrationNodeStatePayload> = nodes
        .iter()
        .filter(|n| {
            n.node_id != source_id
                && !blocked_nodes.contains(&n.node_id)
                && n.gpu_total_vram_mib > 0
                && target_can_host_vm_with_requirements(n, vm, 1, vm.gpu_vram_budget_mib)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|n| {
        (
            (node_gpu_used_pct(n) * 1000.0) as i64,
            -n.gpu_free_vram_mib,
            (node_vcpu_used_pct(n) * 1000.0) as i64,
            (node_ram_used_pct(n) * 1000.0) as i64,
        )
    });
    Some(candidates[0].node_id.clone())
}

fn find_gpu_space_creation_plan(
    source_id: &str,
    vm: &MigrationVmStatePayload,
    nodes: &[MigrationNodeStatePayload],
    required_vcpus: i64,
    gpu_budget_mib: i64,
    last_gpu_migrations: &HashMap<i64, f64>,
    now: f64,
    load_improvement_pct: f64,
    migration_cooldown_secs: f64,
) -> Option<(String, Vec<GpuEvictionPayload>)> {
    let source = nodes.iter().find(|n| n.node_id == source_id)?;
    let mut candidates: Vec<&MigrationNodeStatePayload> = nodes
        .iter()
        .filter(|n| {
            n.node_id != source_id
                && n.gpu_total_vram_mib >= gpu_budget_mib
                && n.mem_available_kb >= vm.max_mem_mib * 1024
                && n.vcpu_free >= required_vcpus
        })
        .collect();
    candidates.sort_by_key(|n| {
        (
            (node_gpu_used_pct(n) * 1000.0) as i64,
            -n.gpu_free_vram_mib,
            (node_vcpu_used_pct(n) * 1000.0) as i64,
            (node_ram_used_pct(n) * 1000.0) as i64,
        )
    });

    for target in candidates {
        let improvement = node_gpu_used_pct(source) - node_gpu_used_pct(target);
        if source.gpu_total_vram_mib > 0
            && source.gpu_free_vram_mib >= gpu_budget_mib
            && improvement < load_improvement_pct
        {
            continue;
        }

        let needed_free = gpu_budget_mib - target.gpu_free_vram_mib;
        if needed_free <= 0 {
            return Some((target.node_id.clone(), Vec::new()));
        }

        let mut movable: Vec<(i64, MigrationVmStatePayload, String)> = Vec::new();
        for resident in &target.local_vms {
            if resident.vm_id == vm.vm_id || resident.gpu_vram_budget_mib <= 0 {
                continue;
            }
            if let Some(last) = last_gpu_migrations.get(&resident.vm_id) {
                if now - last < migration_cooldown_secs {
                    continue;
                }
            }
            let mut blocked = HashSet::new();
            blocked.insert(target.node_id.clone());
            if let Some(eviction_target) =
                best_gpu_eviction_target(&target.node_id, resident, nodes, &blocked)
            {
                movable.push((
                    resident.gpu_vram_budget_mib,
                    resident.clone(),
                    eviction_target,
                ));
            }
        }

        movable.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.vm_id.cmp(&b.1.vm_id)));
        let mut selected = Vec::new();
        let mut freed = 0;
        for (budget, resident, eviction_target) in movable {
            selected.push(GpuEvictionPayload {
                vm: resident,
                source: target.node_id.clone(),
                target: eviction_target,
            });
            freed += budget;
            if freed >= needed_free {
                return Some((target.node_id.clone(), selected));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(node_id: &str, mem_total_mib: i64, mem_free_mib: i64) -> NodeInfoSnapshot {
        NodeInfoSnapshot {
            node_id: node_id.to_string(),
            mem_total_kb: mem_total_mib * 1024,
            mem_available_kb: mem_free_mib * 1024,
            reachable: true,
            local_vms: Vec::new(),
        }
    }

    #[test]
    fn admit_respects_quota_invariant() {
        let config = AdmissionConfig {
            safety_margin: 0.0,
            prefer_local: true,
            min_remote_node_free: 256,
        };
        let cluster = ClusterStateSnapshot {
            nodes: vec![node("a", 16384, 8192), node("b", 8192, 4096)],
        };
        let vm = AdmissionVmSpec {
            vmid: 1,
            max_mem_mib: 10240,
            name: String::new(),
            vcpus: 1,
            preferred_node: None,
            forbidden_nodes: Vec::new(),
        };
        let decision = admit_vm(&config, &cluster, &vm);
        assert!(decision.admitted);
        assert_eq!(
            decision.local_budget_mib + decision.remote_budget_mib,
            vm.max_mem_mib
        );
    }

    #[test]
    fn evaluate_returns_cpu_candidate() {
        let thresholds = MigrationThresholdsPayload::default();
        let nodes = vec![
            MigrationNodeStatePayload {
                node_id: "node-a".into(),
                mem_total_kb: 32 * 1024 * 1024,
                mem_available_kb: 16 * 1024 * 1024,
                vcpu_total: 24,
                vcpu_free: 2,
                gpu_total_vram_mib: 0,
                gpu_free_vram_mib: 0,
                disk_pressure_pct: 0.0,
                local_vms: vec![MigrationVmStatePayload {
                    vm_id: 1,
                    status: "running".into(),
                    max_mem_mib: 4096,
                    rss_kb: 4096 * 1024,
                    remote_pages: 0,
                    avg_cpu_pct: 90.0,
                    throttle_ratio: 0.40,
                    gpu_vram_budget_mib: 0,
                    disk_read_bps: 0.0,
                    disk_write_bps: 0.0,
                    disk_io_weight: 100,
                    disk_local_share_active: false,
                    idle_duration_secs: None,
                }],
            },
            MigrationNodeStatePayload {
                node_id: "node-b".into(),
                mem_total_kb: 32 * 1024 * 1024,
                mem_available_kb: 24 * 1024 * 1024,
                vcpu_total: 24,
                vcpu_free: 18,
                gpu_total_vram_mib: 0,
                gpu_free_vram_mib: 0,
                disk_pressure_pct: 0.0,
                local_vms: vec![],
            },
        ];
        let candidates = evaluate_migrations(&thresholds, &nodes);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].reason, "cpu_saturation");
        assert_eq!(candidates[0].target, "node-b");
    }

    #[test]
    fn gpu_rebalance_returns_direct_target() {
        let req = GpuRebalanceRequest {
            config: GpuRebalanceConfigPayload::default(),
            source_node: "node-a".into(),
            required_vcpus: 2,
            gpu_budget_mib: 2048,
            now: 0.0,
            last_gpu_migrations: HashMap::new(),
            vm: MigrationVmStatePayload {
                vm_id: 10,
                status: "running".into(),
                max_mem_mib: 2048,
                rss_kb: 0,
                remote_pages: 0,
                avg_cpu_pct: 10.0,
                throttle_ratio: 0.0,
                gpu_vram_budget_mib: 512,
                disk_read_bps: 0.0,
                disk_write_bps: 0.0,
                disk_io_weight: 100,
                disk_local_share_active: false,
                idle_duration_secs: None,
            },
            nodes: vec![
                MigrationNodeStatePayload {
                    node_id: "node-a".into(),
                    mem_total_kb: 16 * 1024 * 1024,
                    mem_available_kb: 12 * 1024 * 1024,
                    vcpu_total: 12,
                    vcpu_free: 4,
                    gpu_total_vram_mib: 8192,
                    gpu_free_vram_mib: 512,
                    disk_pressure_pct: 0.0,
                    local_vms: vec![],
                },
                MigrationNodeStatePayload {
                    node_id: "node-b".into(),
                    mem_total_kb: 16 * 1024 * 1024,
                    mem_available_kb: 12 * 1024 * 1024,
                    vcpu_total: 12,
                    vcpu_free: 6,
                    gpu_total_vram_mib: 8192,
                    gpu_free_vram_mib: 4096,
                    disk_pressure_pct: 0.0,
                    local_vms: vec![],
                },
            ],
        };

        let plan = evaluate_gpu_rebalance(&req);
        assert_eq!(plan.action, "migrate");
        assert_eq!(plan.target_node, "node-b");
    }

    #[test]
    fn gpu_rebalance_returns_eviction_plan() {
        let req = GpuRebalanceRequest {
            config: GpuRebalanceConfigPayload::default(),
            source_node: "node-a".into(),
            required_vcpus: 2,
            gpu_budget_mib: 4096,
            now: 0.0,
            last_gpu_migrations: HashMap::new(),
            vm: MigrationVmStatePayload {
                vm_id: 10,
                status: "running".into(),
                max_mem_mib: 2048,
                rss_kb: 0,
                remote_pages: 0,
                avg_cpu_pct: 10.0,
                throttle_ratio: 0.0,
                gpu_vram_budget_mib: 512,
                disk_read_bps: 0.0,
                disk_write_bps: 0.0,
                disk_io_weight: 100,
                disk_local_share_active: false,
                idle_duration_secs: None,
            },
            nodes: vec![
                MigrationNodeStatePayload {
                    node_id: "node-a".into(),
                    mem_total_kb: 16 * 1024 * 1024,
                    mem_available_kb: 12 * 1024 * 1024,
                    vcpu_total: 12,
                    vcpu_free: 4,
                    gpu_total_vram_mib: 8192,
                    gpu_free_vram_mib: 512,
                    disk_pressure_pct: 0.0,
                    local_vms: vec![],
                },
                MigrationNodeStatePayload {
                    node_id: "node-b".into(),
                    mem_total_kb: 16 * 1024 * 1024,
                    mem_available_kb: 12 * 1024 * 1024,
                    vcpu_total: 12,
                    vcpu_free: 6,
                    gpu_total_vram_mib: 8192,
                    gpu_free_vram_mib: 2048,
                    disk_pressure_pct: 0.0,
                    local_vms: vec![MigrationVmStatePayload {
                        vm_id: 20,
                        status: "running".into(),
                        max_mem_mib: 2048,
                        rss_kb: 0,
                        remote_pages: 0,
                        avg_cpu_pct: 10.0,
                        throttle_ratio: 0.0,
                        gpu_vram_budget_mib: 2048,
                        disk_read_bps: 0.0,
                        disk_write_bps: 0.0,
                        disk_io_weight: 100,
                        disk_local_share_active: false,
                        idle_duration_secs: None,
                    }],
                },
                MigrationNodeStatePayload {
                    node_id: "node-c".into(),
                    mem_total_kb: 16 * 1024 * 1024,
                    mem_available_kb: 12 * 1024 * 1024,
                    vcpu_total: 12,
                    vcpu_free: 6,
                    gpu_total_vram_mib: 8192,
                    gpu_free_vram_mib: 2048,
                    disk_pressure_pct: 0.0,
                    local_vms: vec![],
                },
            ],
        };

        let plan = evaluate_gpu_rebalance(&req);
        assert_eq!(plan.action, "evict_then_migrate");
        assert_eq!(plan.target_node, "node-b");
        assert_eq!(plan.evictions.len(), 1);
        assert_eq!(plan.evictions[0].vm.vm_id, 20);
        assert_eq!(plan.evictions[0].source, "node-b");
        assert_eq!(plan.evictions[0].target, "node-c");
    }
}
