//! Contrôleur disque via cgroups v2 — poids I/O et télémétrie réels.
//!
//! # Principe
//!
//! Sur Proxmox/QEMU, le scheduler I/O réaliste côté hôte passe par les cgroups v2:
//! - `io.weight` pour prioriser une VM face aux autres
//! - `io.stat` pour lire les compteurs réels de lecture/écriture
//! - `/proc/pressure/io` pour savoir si le nœud entier souffre de contention disque
//!
//! On ne "prête" donc pas un disque d'une VM à une autre. On rééquilibre la
//! priorité d'accès au stockage partagé/local et on laisse le noyau arbitrer.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

#[derive(Debug, Clone, Default)]
pub struct VmDiskStat {
    pub vm_id: u32,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
    pub read_bps: f64,
    pub write_bps: f64,
}

impl VmDiskStat {
    pub fn total_bps(&self) -> f64 {
        self.read_bps + self.write_bps
    }
}

#[derive(Debug, Clone)]
pub struct VmDiskConfig {
    pub vm_id: u32,
    pub weight: u32,
}

impl VmDiskConfig {
    pub fn new(vm_id: u32) -> Self {
        Self { vm_id, weight: 100 }
    }

    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.clamp(1, 10000);
        self
    }
}

pub struct CgroupIoController {
    cgroup_root: PathBuf,
}

impl CgroupIoController {
    pub fn new() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            cgroup_root: root.into(),
        }
    }

    pub fn find_vm_cgroup(&self, vm_id: u32) -> Option<PathBuf> {
        find_vm_cgroup_under(&self.cgroup_root, vm_id)
    }

    pub fn read_io_stat(&self, vm_id: u32) -> Option<VmDiskStat> {
        let cgroup = self.find_vm_cgroup(vm_id)?;
        let content = fs::read_to_string(cgroup.join("io.stat")).ok()?;
        let mut stat = VmDiskStat {
            vm_id,
            ..Default::default()
        };

        for line in content.lines() {
            for part in line.split_whitespace().skip(1) {
                let Some((key, value)) = part.split_once('=') else {
                    continue;
                };
                let parsed = value.parse::<u64>().unwrap_or(0);
                match key {
                    "rbytes" => stat.read_bytes = stat.read_bytes.saturating_add(parsed),
                    "wbytes" => stat.write_bytes = stat.write_bytes.saturating_add(parsed),
                    "rios" => stat.read_ios = stat.read_ios.saturating_add(parsed),
                    "wios" => stat.write_ios = stat.write_ios.saturating_add(parsed),
                    _ => {}
                }
            }
        }

        Some(stat)
    }

    pub fn read_weight(&self, vm_id: u32) -> Option<u32> {
        let cgroup = self.find_vm_cgroup(vm_id)?;
        let content = fs::read_to_string(cgroup.join("io.weight")).ok()?;
        parse_io_weight(&content)
    }

    pub fn apply(&self, config: &VmDiskConfig) -> Result<()> {
        let cgroup = self.find_vm_cgroup(config.vm_id).with_context(|| {
            format!(
                "cgroup I/O introuvable pour la VM {} — la VM est-elle démarrée ?",
                config.vm_id
            )
        })?;

        let io_weight_path = cgroup.join("io.weight");
        if !io_weight_path.exists() {
            anyhow::bail!(
                "io.weight non disponible pour la VM {} (backend/cgroup non supporté)",
                config.vm_id
            );
        }

        fs::write(
            io_weight_path,
            format!("default {}", config.weight.clamp(1, 10000)),
        )
        .with_context(|| format!("écriture io.weight VM {}", config.vm_id))?;

        info!(
            vm_id = config.vm_id,
            weight = config.weight,
            "io.weight appliqué"
        );
        Ok(())
    }

    pub fn io_weight_supported(&self, vm_id: u32) -> Option<bool> {
        let cgroup = self.find_vm_cgroup(vm_id)?;
        Some(cgroup.join("io.weight").exists())
    }

    pub fn read_node_pressure_pct(&self) -> f64 {
        read_node_io_pressure_pct(&self.cgroup_root)
    }

    pub fn compute_bps(before: &VmDiskStat, after: &VmDiskStat, elapsed_micros: u64) -> (f64, f64) {
        if elapsed_micros == 0 {
            return (0.0, 0.0);
        }
        let secs = elapsed_micros as f64 / 1_000_000.0;
        let read_delta = after.read_bytes.saturating_sub(before.read_bytes) as f64;
        let write_delta = after.write_bytes.saturating_sub(before.write_bytes) as f64;
        (read_delta / secs, write_delta / secs)
    }
}

fn find_vm_cgroup_under(root: &Path, vm_id: u32) -> Option<PathBuf> {
    let machine_slice = root.join("machine.slice");
    if let Ok(entries) = fs::read_dir(&machine_slice) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if (name.contains("qemu") && name.contains(&vm_id.to_string()))
                && name.ends_with(".scope")
            {
                return Some(entry.path());
            }
        }
    }

    let qemu_slice = root.join("qemu.slice").join(format!("{}.scope", vm_id));
    if qemu_slice.exists() {
        return Some(qemu_slice);
    }

    let direct = root
        .join("system.slice")
        .join(format!("qemu-server@{}.service", vm_id));
    if direct.exists() {
        return Some(direct);
    }

    None
}

fn parse_io_weight(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if let Some(rest) = trimmed.strip_prefix("default ") {
        return rest.trim().parse::<u32>().ok();
    }
    trimmed.parse::<u32>().ok()
}

fn parse_pressure_avg10(content: &str) -> f64 {
    for line in content.lines() {
        if !line.starts_with("some ") {
            continue;
        }
        for field in line.split_whitespace().skip(1) {
            if let Some(value) = field.strip_prefix("avg10=") {
                if let Ok(pct) = value.parse::<f64>() {
                    return pct;
                }
            }
        }
    }
    0.0
}

fn read_node_io_pressure_pct(root: &Path) -> f64 {
    let cgroup_pressure = root.join("io.pressure");
    if let Ok(content) = fs::read_to_string(&cgroup_pressure) {
        return parse_pressure_avg10(&content);
    }
    if let Ok(content) = fs::read_to_string("/proc/pressure/io") {
        return parse_pressure_avg10(&content);
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("omega-io-cgroup-{unique}"));
        fs::create_dir_all(root.join("machine.slice")).unwrap();
        root
    }

    #[test]
    fn test_parse_io_weight_default_format() {
        assert_eq!(parse_io_weight("default 150\n"), Some(150));
        assert_eq!(parse_io_weight("200\n"), Some(200));
    }

    #[test]
    fn test_read_io_stat_aggregates_devices() {
        let root = temp_root();
        let vm = root.join("machine.slice").join("machine-qemu-9004.scope");
        fs::create_dir_all(&vm).unwrap();
        fs::write(
            vm.join("io.stat"),
            "8:16 rbytes=100 wbytes=200 rios=3 wios=4\n8:32 rbytes=50 wbytes=25 rios=1 wios=2\n",
        )
        .unwrap();

        let ctrl = CgroupIoController::with_root(&root);
        let stat = ctrl.read_io_stat(9004).unwrap();
        assert_eq!(stat.read_bytes, 150);
        assert_eq!(stat.write_bytes, 225);
        assert_eq!(stat.read_ios, 4);
        assert_eq!(stat.write_ios, 6);
    }

    #[test]
    fn test_compute_bps_uses_deltas() {
        let before = VmDiskStat {
            read_bytes: 100,
            write_bytes: 200,
            ..Default::default()
        };
        let after = VmDiskStat {
            read_bytes: 1100,
            write_bytes: 2200,
            ..Default::default()
        };
        let (read_bps, write_bps) = CgroupIoController::compute_bps(&before, &after, 1_000_000);
        assert_eq!(read_bps, 1000.0);
        assert_eq!(write_bps, 2000.0);
    }

    #[test]
    fn test_io_weight_supported_detects_missing_file() {
        let root = temp_root();
        let vm = root.join("qemu.slice").join("9004.scope");
        fs::create_dir_all(&vm).unwrap();

        let ctrl = CgroupIoController::with_root(&root);
        assert_eq!(ctrl.io_weight_supported(9004), Some(false));
    }

    #[test]
    fn test_parse_pressure_avg10_reads_some_line() {
        let value = parse_pressure_avg10(
            "some avg10=12.34 avg60=5.00 avg300=1.00 total=123\nfull avg10=0.50 avg60=0.20 avg300=0.10 total=12\n",
        );
        assert!((value - 12.34).abs() < 0.001);
    }
}
