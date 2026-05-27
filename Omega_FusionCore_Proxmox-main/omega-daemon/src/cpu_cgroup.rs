//! Contrôleur CPU via cgroups v2 — la méthode standard en production.
//!
//! # Pourquoi cgroups v2 et pas le slot tracking manuel ?
//!
//! Notre `VcpuScheduler` précédent gérait des slots dans une HashMap interne,
//! mais QEMU ignorait totalement ces décisions. Avec **cgroups v2** :
//!
//! - `cpu.weight`  → priorité proportionnelle entre VMs (1–10000, défaut 100)
//! - `cpu.max`     → quota dur : "max N µs de CPU par période de M µs"
//! - `cpu.stat`    → métriques réelles par VM (usage, throttling)
//! - `cpuset.cpus` → épinglage sur des cœurs physiques précis
//!
//! C'est le mécanisme utilisé par Proxmox lui-même, OpenStack, k8s, AWS.
//!
//! # Chemin cgroup par VM sur Proxmox VE 7+/8
//!
//! ```text
//! /sys/fs/cgroup/machine.slice/
//!   machine-qemu\x2d<vmid>-pve.scope/
//!     cpu.weight          ← priorité (1-10000)
//!     cpu.max             ← quota "500000 1000000" = 50% d'un cœur
//!     cpu.stat            ← usage_usec, throttled_usec, ...
//!     cpuset.cpus         ← cœurs alloués "0-3" ou "0,2,4"
//!     cpuset.cpus.effective ← cœurs réellement assignés
//! ```
//!
//! # Relation avec QMP (hotplug vCPU)
//!
//! Le hotplug vCPU réel se fait via `qmp_vcpu.rs` : on envoie
//! `device_add / device_del` à QEMU via la socket QMP. Ce module-ci
//! s'occupe uniquement du **contrôle de la bande passante CPU**.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

// ─── Structures ───────────────────────────────────────────────────────────────

/// Statistiques CPU lues depuis `cpu.stat` du cgroup de la VM.
#[derive(Debug, Clone, Default)]
pub struct VmCpuStat {
    pub vm_id: u32,
    /// Temps CPU total consommé (µs)
    pub usage_usec: u64,
    /// Temps CPU en espace utilisateur (µs)
    pub user_usec: u64,
    /// Temps CPU en espace système (µs)
    pub system_usec: u64,
    /// Nombre de périodes de quota
    pub nr_periods: u64,
    /// Nombre de périodes où le quota a été atteint (throttling)
    pub nr_throttled: u64,
    /// Temps total throttlé (µs) — indicateur clé de saturation CPU
    pub throttled_usec: u64,
    /// Utilisation CPU calculée depuis le dernier appel (0.0–100.0 × nb_cpus)
    pub usage_pct: f64,
}

impl VmCpuStat {
    /// La VM est-elle throttlée (quota atteint) ?
    pub fn is_throttled(&self) -> bool {
        self.nr_periods > 0 && self.nr_throttled > 0
    }

    /// Taux de throttling (0.0–1.0)
    pub fn throttle_ratio(&self) -> f64 {
        if self.nr_periods == 0 {
            return 0.0;
        }
        self.nr_throttled as f64 / self.nr_periods as f64
    }
}

/// Configuration CPU appliquée sur le cgroup d'une VM.
#[derive(Debug, Clone)]
pub struct VmCpuConfig {
    pub vm_id: u32,
    /// Poids de priorité CPU (1–10000 ; défaut Proxmox = 100)
    /// Une VM avec weight=200 reçoit 2× plus de CPU qu'une VM avec weight=100
    /// quand les deux sont en compétition.
    pub weight: u32,
    /// Quota CPU : Some((quota_usec, period_usec)) ou None = illimité
    /// Exemple : Some((500_000, 1_000_000)) = 50% d'un cœur max
    /// Exemple : Some((4_000_000, 1_000_000)) = 4 cœurs max
    pub quota: Option<(u64, u64)>,
    /// Cœurs physiques alloués : None = tous, Some("0-3") = cœurs 0 à 3
    pub cpuset: Option<String>,
}

impl VmCpuConfig {
    pub fn new(vm_id: u32) -> Self {
        Self {
            vm_id,
            weight: 100,
            quota: None,
            cpuset: None,
        }
    }

    /// Construit une config avec N vCPUs réels comme plafond.
    ///
    /// Exemple : `capped_at_vcpus(4)` → quota = "4000000 1000000"
    pub fn capped_at_vcpus(mut self, num_vcpus: usize) -> Self {
        let period_usec: u64 = 1_000_000; // 1 seconde
        let quota_usec: u64 = num_vcpus as u64 * period_usec;
        self.quota = Some((quota_usec, period_usec));
        self
    }

    /// Fixe le poids de priorité.
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.clamp(1, 10000);
        self
    }

    /// Épingle la VM sur des cœurs spécifiques.
    /// Format : "0-3", "0,2,4,6", "0-1,4-5"
    pub fn pinned_to(mut self, cpuset: impl Into<String>) -> Self {
        self.cpuset = Some(cpuset.into());
        self
    }
}

// ─── Contrôleur cgroup ────────────────────────────────────────────────────────

/// Contrôleur CPU cgroups v2 pour les VMs Proxmox.
pub struct CgroupCpuController {
    /// Base du cgroup v2 (normalement /sys/fs/cgroup)
    cgroup_root: PathBuf,
}

impl CgroupCpuController {
    pub fn new() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
        }
    }

    /// Pour les tests — permet d'injecter un répertoire temporaire.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            cgroup_root: root.into(),
        }
    }

    // ── Découverte du cgroup d'une VM ─────────────────────────────────────

    /// Trouve le chemin cgroup d'une VM Proxmox.
    ///
    /// Proxmox VE 7+ place les VMs QEMU dans :
    ///   `/sys/fs/cgroup/machine.slice/machine-qemu\x2d<vmid>-pve.scope/`
    ///
    /// Le `\x2d` est l'encodage systemd du caractère `-`.
    /// On cherche par glob pour être robuste aux variations de nommage.
    pub fn find_vm_cgroup(&self, vm_id: u32) -> Option<PathBuf> {
        let machine_slice = self.cgroup_root.join("machine.slice");

        // Chercher le scope QEMU pour ce vmid
        if let Ok(entries) = fs::read_dir(&machine_slice) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Format Proxmox VE : machine-qemu\x2d{vmid}-pve.scope
                // ou machine-qemu-{vmid}.scope selon la version
                if (name.contains("qemu") && name.contains(&vm_id.to_string()))
                    && name.ends_with(".scope")
                {
                    return Some(entry.path());
                }
            }
        }

        // Certaines versions/configurations Proxmox placent les VMs sous:
        //   /sys/fs/cgroup/qemu.slice/<vmid>.scope
        let qemu_slice = self
            .cgroup_root
            .join("qemu.slice")
            .join(format!("{}.scope", vm_id));
        if qemu_slice.exists() {
            return Some(qemu_slice);
        }

        // Fallback : chemin direct Proxmox qemu-server
        let direct = self
            .cgroup_root
            .join("system.slice")
            .join(format!("qemu-server@{}.service", vm_id));
        if direct.exists() {
            return Some(direct);
        }

        None
    }

    /// Liste tous les vmids pour lesquels un cgroup est actif.
    pub fn list_active_vms(&self) -> Vec<u32> {
        let machine_slice = self.cgroup_root.join("machine.slice");
        let mut vmids = Vec::new();
        if let Ok(entries) = fs::read_dir(&machine_slice) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("qemu") && name.ends_with(".scope") {
                    // Extraire le vmid depuis le nom
                    for part in name.split('-') {
                        if let Ok(id) = part.parse::<u32>() {
                            if id > 100 {
                                // vmids Proxmox commencent à 100
                                vmids.push(id);
                                break;
                            }
                        }
                    }
                }
            }
        }

        let qemu_slice = self.cgroup_root.join("qemu.slice");
        if let Ok(entries) = fs::read_dir(&qemu_slice) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(id) = name.strip_suffix(".scope") else {
                    continue;
                };
                if let Ok(id) = id.parse::<u32>() {
                    if id > 100 {
                        vmids.push(id);
                    }
                }
            }
        }

        vmids.sort_unstable();
        vmids.dedup();
        vmids
    }

    // ── Lecture des métriques ─────────────────────────────────────────────

    /// Lit `cpu.stat` du cgroup d'une VM.
    ///
    /// Retourne None si la VM n'est pas dans un cgroup connu.
    pub fn read_cpu_stat(&self, vm_id: u32) -> Option<VmCpuStat> {
        let cgroup = self.find_vm_cgroup(vm_id)?;
        let content = fs::read_to_string(cgroup.join("cpu.stat")).ok()?;

        let mut stat = VmCpuStat {
            vm_id,
            ..Default::default()
        };

        for line in content.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or("");
            let val: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            match key {
                "usage_usec" => stat.usage_usec = val,
                "user_usec" => stat.user_usec = val,
                "system_usec" => stat.system_usec = val,
                "nr_periods" => stat.nr_periods = val,
                "nr_throttled" => stat.nr_throttled = val,
                "throttled_usec" => stat.throttled_usec = val,
                _ => {}
            }
        }

        debug!(
            vm_id,
            usage_usec = stat.usage_usec,
            nr_throttled = stat.nr_throttled,
            throttled_usec = stat.throttled_usec,
            "cpu.stat lu"
        );

        Some(stat)
    }

    /// Lit le cpu.weight courant d'une VM.
    pub fn read_weight(&self, vm_id: u32) -> Option<u32> {
        let cgroup = self.find_vm_cgroup(vm_id)?;
        let content = fs::read_to_string(cgroup.join("cpu.weight")).ok()?;
        content.trim().parse().ok()
    }

    /// Lit le cpu.max courant d'une VM.
    /// Retourne (quota_usec, period_usec) ou None si "max" (illimité).
    pub fn read_max(&self, vm_id: u32) -> Option<(u64, u64)> {
        let cgroup = self.find_vm_cgroup(vm_id)?;
        let content = fs::read_to_string(cgroup.join("cpu.max")).ok()?;
        let content = content.trim();
        if content.starts_with("max") {
            return None; // illimité
        }
        let mut parts = content.split_whitespace();
        let quota: u64 = parts.next()?.parse().ok()?;
        let period: u64 = parts.next()?.parse().ok()?;
        Some((quota, period))
    }

    // ── Application de la configuration ──────────────────────────────────

    /// Applique une configuration CPU sur le cgroup d'une VM.
    ///
    /// Cette fonction est la pièce centrale :
    /// elle écrit dans les fichiers cgroup pour que le kernel applique
    /// les contraintes directement sur le processus QEMU.
    pub fn apply(&self, config: &VmCpuConfig) -> Result<()> {
        let cgroup = self.find_vm_cgroup(config.vm_id).with_context(|| {
            format!(
                "cgroup introuvable pour la VM {} — la VM est-elle démarrée ?",
                config.vm_id
            )
        })?;

        // ── 1. cpu.weight ─────────────────────────────────────────────────
        let weight_path = cgroup.join("cpu.weight");
        fs::write(&weight_path, config.weight.to_string())
            .with_context(|| format!("écriture cpu.weight VM {}", config.vm_id))?;

        info!(
            vm_id = config.vm_id,
            weight = config.weight,
            "cpu.weight appliqué"
        );

        // ── 2. cpu.max ────────────────────────────────────────────────────
        let max_path = cgroup.join("cpu.max");
        let max_content = match config.quota {
            Some((quota, period)) => format!("{} {}", quota, period),
            None => "max 1000000".to_string(),
        };
        fs::write(&max_path, &max_content)
            .with_context(|| format!("écriture cpu.max VM {}", config.vm_id))?;

        info!(
            vm_id      = config.vm_id,
            cpu_max    = %max_content,
            "cpu.max appliqué"
        );

        // ── 3. cpuset.cpus (optionnel) ────────────────────────────────────
        if let Some(ref cpuset) = config.cpuset {
            let cpuset_path = cgroup.join("cpuset.cpus");
            if cpuset_path.exists() {
                fs::write(&cpuset_path, cpuset)
                    .with_context(|| format!("écriture cpuset.cpus VM {}", config.vm_id))?;
                info!(
                    vm_id  = config.vm_id,
                    cpuset = %cpuset,
                    "cpuset.cpus appliqué"
                );
            } else {
                warn!(
                    vm_id = config.vm_id,
                    "cpuset.cpus non disponible sur ce nœud (contrôleur cpuset non monté?)"
                );
            }
        }

        Ok(())
    }

    /// Supprime les limites CPU d'une VM (après migration ou arrêt).
    pub fn reset(&self, vm_id: u32) -> Result<()> {
        let config = VmCpuConfig::new(vm_id); // weight=100, quota=None
        self.apply(&config)
    }

    // ── Calcul d'usage CPU ────────────────────────────────────────────────

    /// Calcule le pourcentage d'usage CPU d'une VM entre deux lectures.
    ///
    /// `stat_before` et `stat_after` doivent être lus avec un intervalle
    /// de temps connu (`elapsed_usec`).
    pub fn compute_usage_pct(
        stat_before: &VmCpuStat,
        stat_after: &VmCpuStat,
        elapsed_usec: u64,
    ) -> f64 {
        if elapsed_usec == 0 {
            return 0.0;
        }
        let delta_usec = stat_after.usage_usec.saturating_sub(stat_before.usage_usec);
        (delta_usec as f64 / elapsed_usec as f64) * 100.0
    }

    /// Lit le steal time global du nœud depuis `/proc/stat`.
    ///
    /// Le steal time est le temps CPU que l'hyperviseur a volé à toutes
    /// les VMs de ce nœud (utile seulement si ce nœud est lui-même une VM).
    /// Sur une machine physique, le steal time vient des processus hôte.
    pub fn read_node_steal_pct() -> f64 {
        let Ok(content) = fs::read_to_string("/proc/stat") else {
            return 0.0;
        };
        let Some(line) = content.lines().find(|l| l.starts_with("cpu ")) else {
            return 0.0;
        };

        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|s| s.parse().ok())
            .collect();

        if fields.len() < 8 {
            return 0.0;
        }
        let total = fields.iter().sum::<u64>();
        let steal = fields[7];
        if total == 0 {
            return 0.0;
        }
        (steal as f64 / total as f64) * 100.0
    }
}

impl Default for CgroupCpuController {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_vm_cgroup(root: &Path, vm_id: u32) -> PathBuf {
        // Simuler la structure cgroup Proxmox
        let scope = root
            .join("machine.slice")
            .join(format!("machine-qemu-{}-pve.scope", vm_id));
        fs::create_dir_all(&scope).unwrap();

        // Créer les fichiers cgroup avec des valeurs par défaut
        fs::write(scope.join("cpu.weight"), "100\n").unwrap();
        fs::write(scope.join("cpu.max"), "max 1000000\n").unwrap();
        fs::write(
            scope.join("cpu.stat"),
            "\
usage_usec 5000000\n\
user_usec 3000000\n\
system_usec 2000000\n\
nr_periods 50\n\
nr_throttled 3\n\
throttled_usec 150000\n\
nr_burst_periods 0\n\
burst_usec 0\n",
        )
        .unwrap();
        fs::write(scope.join("cpuset.cpus"), "0-3\n").unwrap();

        scope
    }

    fn make_vm_cgroup_qemu_slice(root: &Path, vm_id: u32) -> PathBuf {
        let scope = root.join("qemu.slice").join(format!("{}.scope", vm_id));
        fs::create_dir_all(&scope).unwrap();
        fs::write(scope.join("cpu.weight"), "100\n").unwrap();
        fs::write(scope.join("cpu.max"), "max 1000000\n").unwrap();
        fs::write(
            scope.join("cpu.stat"),
            "\
usage_usec 5000000\n\
user_usec 3000000\n\
system_usec 2000000\n\
nr_periods 50\n\
nr_throttled 3\n\
throttled_usec 150000\n\
nr_burst_periods 0\n\
burst_usec 0\n",
        )
        .unwrap();
        fs::write(scope.join("cpuset.cpus"), "0-3\n").unwrap();
        scope
    }

    #[test]
    fn test_find_vm_cgroup() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 101);
        let ctrl = CgroupCpuController::with_root(tmp.path());
        let found = ctrl.find_vm_cgroup(101);
        assert!(found.is_some(), "cgroup introuvable");
        assert!(found.unwrap().to_string_lossy().contains("101"));
    }

    #[test]
    fn test_read_cpu_stat() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 102);
        let ctrl = CgroupCpuController::with_root(tmp.path());
        let stat = ctrl.read_cpu_stat(102).expect("stat non lue");
        assert_eq!(stat.usage_usec, 5_000_000);
        assert_eq!(stat.user_usec, 3_000_000);
        assert_eq!(stat.system_usec, 2_000_000);
        assert_eq!(stat.nr_periods, 50);
        assert_eq!(stat.nr_throttled, 3);
        assert_eq!(stat.throttled_usec, 150_000);
    }

    #[test]
    fn test_find_vm_cgroup_in_qemu_slice() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup_qemu_slice(tmp.path(), 107);
        let ctrl = CgroupCpuController::with_root(tmp.path());
        let found = ctrl.find_vm_cgroup(107);
        assert!(found.is_some(), "cgroup qemu.slice introuvable");
        assert!(found
            .unwrap()
            .to_string_lossy()
            .contains("qemu.slice/107.scope"));
    }

    #[test]
    fn test_is_throttled() {
        let stat = VmCpuStat {
            nr_periods: 10,
            nr_throttled: 2,
            ..Default::default()
        };
        assert!(stat.is_throttled());
        assert!((stat.throttle_ratio() - 0.2).abs() < 0.001);
    }

    #[test]
    fn test_apply_weight() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 103);
        let ctrl = CgroupCpuController::with_root(tmp.path());

        let config = VmCpuConfig::new(103).with_weight(200);
        ctrl.apply(&config).unwrap();

        let written = ctrl.read_weight(103).unwrap();
        assert_eq!(written, 200);
    }

    #[test]
    fn test_apply_quota_vcpus() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 104);
        let ctrl = CgroupCpuController::with_root(tmp.path());

        // Limiter à 2 vCPUs
        let config = VmCpuConfig::new(104).capped_at_vcpus(2);
        ctrl.apply(&config).unwrap();

        let (quota, period) = ctrl.read_max(104).unwrap();
        assert_eq!(quota, 2_000_000); // 2 × 1_000_000
        assert_eq!(period, 1_000_000);
    }

    #[test]
    fn test_apply_unlimited() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 105);
        let ctrl = CgroupCpuController::with_root(tmp.path());

        let config = VmCpuConfig::new(105); // quota = None
        ctrl.apply(&config).unwrap();

        // "max 1000000" → read_max retourne None
        assert!(ctrl.read_max(105).is_none());
    }

    #[test]
    fn test_apply_cpuset() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 106);
        let ctrl = CgroupCpuController::with_root(tmp.path());

        let config = VmCpuConfig::new(106).pinned_to("0-1");
        ctrl.apply(&config).unwrap();

        let scope = tmp
            .path()
            .join("machine.slice")
            .join("machine-qemu-106-pve.scope");
        let cpuset = fs::read_to_string(scope.join("cpuset.cpus")).unwrap();
        assert_eq!(cpuset.trim(), "0-1");
    }

    #[test]
    fn test_compute_usage_pct() {
        let before = VmCpuStat {
            usage_usec: 0,
            ..Default::default()
        };
        let after = VmCpuStat {
            usage_usec: 2_000_000,
            ..Default::default()
        };
        // Sur 4 secondes = 4_000_000 µs → 50% d'un cœur
        let pct = CgroupCpuController::compute_usage_pct(&before, &after, 4_000_000);
        assert!((pct - 50.0).abs() < 0.1);
    }

    #[test]
    fn test_list_active_vms() {
        let tmp = TempDir::new().unwrap();
        make_vm_cgroup(tmp.path(), 200);
        make_vm_cgroup(tmp.path(), 201);
        make_vm_cgroup(tmp.path(), 202);
        make_vm_cgroup_qemu_slice(tmp.path(), 203);
        let ctrl = CgroupCpuController::with_root(tmp.path());
        let vms = ctrl.list_active_vms();
        assert_eq!(vms.len(), 4);
        assert!(vms.contains(&203));
    }

    #[test]
    fn test_vm_not_found_returns_none() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("machine.slice")).unwrap();
        let ctrl = CgroupCpuController::with_root(tmp.path());
        assert!(ctrl.find_vm_cgroup(9999).is_none());
        assert!(ctrl.read_cpu_stat(9999).is_none());
    }

    #[test]
    fn test_weight_clamp() {
        let config = VmCpuConfig::new(1).with_weight(99999);
        assert_eq!(config.weight, 10000); // clamped
        let config2 = VmCpuConfig::new(1).with_weight(0);
        assert_eq!(config2.weight, 1); // clamped
    }
}
