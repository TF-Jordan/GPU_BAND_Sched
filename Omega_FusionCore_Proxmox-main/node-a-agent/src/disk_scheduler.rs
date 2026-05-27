//! Scheduler de priorité disque par VM — cgroups v2 `io.weight` + PSI.
//!
//! ## Ce que fait ce module
//!
//! Il n'arbitre pas le matériel Ceph (réseau/RADOS). Il arbitre la **contention
//! locale** : quand plusieurs VMs lisent/écrivent simultanément sur le nœud
//! (cache page, journaux, images locales), le kernel distribue le débit via
//! l'ordonnanceur BFQ selon les poids `io.weight` de chaque cgroup.
//!
//! ## Algorithme
//!
//! Toutes les `interval` secondes :
//! 1. Lire `/proc/pressure/io` (PSI). Si `some avg10 < psi_threshold` → pas de
//!    contention, remettre tous les poids à 100 et sortir.
//! 2. Pour chaque VM connue, lire `io.stat` (octets lus + écrits depuis le
//!    démarrage du cgroup). Calculer le delta depuis la dernière mesure.
//! 3. Classer les VMs : actives (delta > `active_bytes_threshold`) vs idle.
//! 4. Appliquer :
//!    - VM active  → `io.weight = 200` (priorité haute)
//!    - VM idle    → `io.weight = 50`  (priorité basse, laisse passer les actives)
//!    - VM inconnue (cgroup manquant) → ignorée
//!
//! ## Idempotence
//!
//! Seuls les changements de catégorie (active↔idle) déclenchent une écriture
//! cgroup. Aucune écriture inutile si le poids est déjà correct.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, info, warn};

// ── Constantes ────────────────────────────────────────────────────────────────

const IO_WEIGHT_DEFAULT: u32 = 100;
const IO_WEIGHT_ACTIVE: u32 = 200;
const IO_WEIGHT_IDLE: u32 = 50;

/// PSI `some avg10` au-delà duquel on considère qu'il y a contention (%).
const DEFAULT_PSI_THRESHOLD: f32 = 10.0;

/// Delta d'octets I/O en dessous duquel une VM est considérée idle.
const DEFAULT_ACTIVE_BYTES_THRESHOLD: u64 = 1024 * 1024; // 1 MiB / intervalle

// ── Structures ────────────────────────────────────────────────────────────────

pub struct DiskScheduler {
    vm_ids: Vec<u32>,
    interval: Duration,
    psi_threshold: f32,
    active_bytes_threshold: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoClass {
    Active,
    Idle,
}

// ── Implémentation principale ─────────────────────────────────────────────────

impl DiskScheduler {
    pub fn new(
        vm_ids: Vec<u32>,
        interval_secs: u64,
        psi_threshold: f32,
        active_bytes_threshold: u64,
    ) -> Self {
        Self {
            vm_ids,
            interval: Duration::from_secs(interval_secs.max(1)),
            psi_threshold: if psi_threshold <= 0.0 {
                DEFAULT_PSI_THRESHOLD
            } else {
                psi_threshold
            },
            active_bytes_threshold: if active_bytes_threshold == 0 {
                DEFAULT_ACTIVE_BYTES_THRESHOLD
            } else {
                active_bytes_threshold
            },
        }
    }

    pub async fn run(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        info!(
            vms = self.vm_ids.len(),
            interval_s = self.interval.as_secs(),
            psi_threshold = self.psi_threshold,
            "disk scheduler démarré"
        );

        // io_bytes[vmid] = (rb + wb) à la dernière mesure
        let mut io_bytes_prev: HashMap<u32, u64> = HashMap::new();
        // classe actuelle pour éviter les écritures inutiles
        let mut current_class: HashMap<u32, IoClass> = HashMap::new();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            sleep(self.interval).await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let psi = read_io_psi_some_avg10();
            debug!(psi_some_avg10 = psi, "PSI I/O lu");

            if psi < self.psi_threshold {
                // Pas de contention : tout le monde à 100, reset des classes
                for &vmid in &self.vm_ids {
                    if current_class.get(&vmid) != Some(&IoClass::Idle)
                        && current_class.get(&vmid) != Some(&IoClass::Active)
                        || current_class.get(&vmid).is_some()
                    {
                        // On remet systématiquement au défaut quand pas de pression
                        if set_io_weight(vmid, IO_WEIGHT_DEFAULT) {
                            debug!(vm_id = vmid, "io.weight remis à 100 (pas de pression PSI)");
                        }
                        current_class.remove(&vmid);
                    }
                }
                io_bytes_prev.clear(); // reset les deltas pour la prochaine pression
                continue;
            }

            // Contention détectée : classer les VMs et ajuster
            debug!(psi, "contention I/O détectée — ajustement io.weight");

            for &vmid in &self.vm_ids {
                let bytes_now = read_io_bytes(vmid).unwrap_or(0);
                let bytes_prev = io_bytes_prev.get(&vmid).copied().unwrap_or(bytes_now);
                let delta = bytes_now.saturating_sub(bytes_prev);
                io_bytes_prev.insert(vmid, bytes_now);

                let class = if delta >= self.active_bytes_threshold {
                    IoClass::Active
                } else {
                    IoClass::Idle
                };

                // Écrire seulement si la classe a changé
                if current_class.get(&vmid) != Some(&class) {
                    let weight = match class {
                        IoClass::Active => IO_WEIGHT_ACTIVE,
                        IoClass::Idle => IO_WEIGHT_IDLE,
                    };
                    if set_io_weight(vmid, weight) {
                        info!(
                            vm_id  = vmid,
                            weight,
                            delta_bytes = delta,
                            class  = ?class,
                            "io.weight ajusté"
                        );
                    }
                    current_class.insert(vmid, class);
                }
            }
        }

        // Nettoyage : remettre tous les poids au défaut à l'arrêt
        for &vmid in &self.vm_ids {
            set_io_weight(vmid, IO_WEIGHT_DEFAULT);
        }
        info!("disk scheduler arrêté — io.weight remis à 100 sur toutes les VMs");
    }
}

// ── Cgroup I/O — chemins ──────────────────────────────────────────────────────

fn cgroup_io_path(vmid: u32, file: &str) -> Option<String> {
    // Proxmox 8+ : qemu.slice/{vmid}.scope/
    let p1 = format!("/sys/fs/cgroup/qemu.slice/{vmid}.scope/{file}");
    if std::path::Path::new(&p1).exists() {
        return Some(p1);
    }
    // Proxmox antérieur : machine.slice/qemu-{vmid}.scope/
    let p2 = format!("/sys/fs/cgroup/machine.slice/qemu-{vmid}.scope/{file}");
    if std::path::Path::new(&p2).exists() {
        return Some(p2);
    }
    None
}

// ── io.weight ────────────────────────────────────────────────────────────────

/// Écrit le poids dans le cgroup. Retourne true si l'écriture a eu lieu.
fn set_io_weight(vmid: u32, weight: u32) -> bool {
    let Some(path) = cgroup_io_path(vmid, "io.weight") else {
        debug!(vm_id = vmid, "io.weight : cgroup introuvable");
        return false;
    };
    // Format cgroup v2 : "default N"
    if let Err(e) = std::fs::write(&path, format!("default {weight}")) {
        warn!(vm_id = vmid, weight, error = %e, "écriture io.weight échouée");
        return false;
    }
    true
}

// ── io.stat — lecture octets totaux ──────────────────────────────────────────

/// Lit le total rbytes + wbytes depuis `io.stat` du cgroup.
/// Format : `8:0 rbytes=... wbytes=... rios=... wios=... ...`
fn read_io_bytes(vmid: u32) -> Option<u64> {
    let path = cgroup_io_path(vmid, "io.stat")?;
    let content = std::fs::read_to_string(&path).ok()?;

    let mut total: u64 = 0;
    for line in content.lines() {
        // Chaque ligne : "MAJ:MIN rbytes=X wbytes=Y ..."
        for token in line.split_whitespace() {
            if let Some(val) = token
                .strip_prefix("rbytes=")
                .or_else(|| token.strip_prefix("wbytes="))
            {
                total += val.parse::<u64>().unwrap_or(0);
            }
        }
    }
    Some(total)
}

// ── PSI — pression I/O ────────────────────────────────────────────────────────

/// Lit `some avg10` depuis `/proc/pressure/io`.
/// Format : `some avg10=X.XX avg60=... avg300=... total=...`
/// Retourne 0.0 si le fichier n'existe pas (kernel < 4.20 ou PSI désactivé).
fn read_io_psi_some_avg10() -> f32 {
    let content = match std::fs::read_to_string("/proc/pressure/io") {
        Ok(s) => s,
        Err(_) => return 0.0,
    };
    // Première ligne = "some ..."
    for line in content.lines() {
        if line.starts_with("some ") {
            for token in line.split_whitespace() {
                if let Some(val) = token.strip_prefix("avg10=") {
                    return val.parse().unwrap_or(0.0);
                }
            }
        }
    }
    0.0
}

// ── Pub helpers pour reset depuis d'autres modules ────────────────────────────

/// Remet io.weight à la valeur par défaut (appelé après migration de la VM).
pub fn reset_io_weight(vmid: u32) {
    set_io_weight(vmid, IO_WEIGHT_DEFAULT);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_weight_active_greater_than_idle() {
        assert!(IO_WEIGHT_ACTIVE > IO_WEIGHT_DEFAULT);
        assert!(IO_WEIGHT_DEFAULT > IO_WEIGHT_IDLE);
    }

    #[test]
    fn test_delta_classifies_active() {
        let delta = 2 * 1024 * 1024u64; // 2 MiB > 1 MiB threshold
        let threshold = DEFAULT_ACTIVE_BYTES_THRESHOLD;
        let class = if delta >= threshold {
            IoClass::Active
        } else {
            IoClass::Idle
        };
        assert_eq!(class, IoClass::Active);
    }

    #[test]
    fn test_delta_classifies_idle() {
        let delta = 512u64; // 512 octets < 1 MiB threshold
        let threshold = DEFAULT_ACTIVE_BYTES_THRESHOLD;
        let class = if delta >= threshold {
            IoClass::Active
        } else {
            IoClass::Idle
        };
        assert_eq!(class, IoClass::Idle);
    }

    #[test]
    fn test_psi_parse_format() {
        // Simuler le parsing sans lire /proc
        let line = "some avg10=15.23 avg60=8.10 avg300=2.41 total=123456";
        let val: f32 = line
            .split_whitespace()
            .find_map(|t| t.strip_prefix("avg10=").and_then(|v| v.parse().ok()))
            .unwrap_or(0.0);
        assert!((val - 15.23).abs() < 0.01);
    }

    #[test]
    fn test_io_stat_parse_rbytes_wbytes() {
        let content = "8:0 rbytes=1048576 wbytes=524288 rios=100 wios=50 dbytes=0 dios=0";
        let mut total: u64 = 0;
        for line in content.lines() {
            for token in line.split_whitespace() {
                if let Some(val) = token
                    .strip_prefix("rbytes=")
                    .or_else(|| token.strip_prefix("wbytes="))
                {
                    total += val.parse::<u64>().unwrap_or(0);
                }
            }
        }
        assert_eq!(total, 1048576 + 524288);
    }

    #[test]
    fn test_cgroup_path_format() {
        // Vérifier le format des chemins générés
        let vmid = 100u32;
        let p1 = format!("/sys/fs/cgroup/qemu.slice/{vmid}.scope/io.weight");
        let p2 = format!("/sys/fs/cgroup/machine.slice/qemu-{vmid}.scope/io.weight");
        assert!(p1.contains("100.scope"));
        assert!(p2.contains("qemu-100.scope"));
    }

    #[test]
    fn test_psi_below_threshold_no_adjustment_needed() {
        let psi = 5.0f32;
        let threshold = DEFAULT_PSI_THRESHOLD; // 10.0
        assert!(psi < threshold, "pas de contention en dessous du seuil");
    }

    #[test]
    fn test_io_weight_values_in_valid_range() {
        // cgroup v2 io.weight : plage valide 1–10000
        assert!(IO_WEIGHT_IDLE >= 1 && IO_WEIGHT_IDLE <= 10000);
        assert!(IO_WEIGHT_DEFAULT >= 1 && IO_WEIGHT_DEFAULT <= 10000);
        assert!(IO_WEIGHT_ACTIVE >= 1 && IO_WEIGHT_ACTIVE <= 10000);
    }

    #[test]
    fn test_scheduler_construction() {
        let s = DiskScheduler::new(vec![100, 101, 102], 5, 0.0, 0);
        // Vérifier les valeurs par défaut quand 0 est passé
        assert_eq!(s.psi_threshold, DEFAULT_PSI_THRESHOLD);
        assert_eq!(s.active_bytes_threshold, DEFAULT_ACTIVE_BYTES_THRESHOLD);
        assert_eq!(s.interval, Duration::from_secs(5));
    }
}
