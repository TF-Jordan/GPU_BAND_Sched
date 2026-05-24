//! Détection du matériel local : GPU et espace disque.
//!
//! Utilisé par le serveur de statut HTTP pour enrichir la réponse `/status`
//! avec des informations GPU et disque utilisées par le démon de placement GPU
//! côté agent.
//!
//! # Détection GPU générique
//!
//! On scanne `/sys/bus/pci/devices/*/class` et on cherche les classes PCI
//! de la famille 0x03xxxx (Display controller) :
//!
//!   0x030000  VGA Compatible Controller   — GPU classique (NVIDIA, AMD, Intel)
//!   0x030100  XGA Compatible Controller   — rare, historique
//!   0x030200  3D Controller               — GPU compute sans sortie vidéo (H100, A100…)
//!   0x038000  Display Controller          — Intel Xe, Raspberry Pi, etc.
//!
//! Cette approche est indépendante du fabricant et du driver.  Elle fonctionne
//! quelle que soit la pile GPU installée (NVIDIA propriétaire, AMDGPU, i915,
//! Nouveau, virtio-gpu, etc.) et même sans aucun driver chargé.
//!
//! Fallback : si `/sys/bus/pci` n'est pas disponible (conteneur léger sans
//! namespace PCI), on vérifie `/dev/dri/card*` (DRI présent ↔ driver GPU chargé).

use tracing::warn;

// Préfixe de classe PCI des contrôleurs d'affichage (Display, 3D, VGA).
const PCI_CLASS_DISPLAY_PREFIX: u32 = 0x03;

// ─── Détection GPU ────────────────────────────────────────────────────────────

/// Informations sur le GPU local.
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    /// Au moins un GPU PCI détecté.
    pub present: bool,
    /// Nombre de GPU PCI distincts.
    pub count: u32,
    /// Résumé textuel (vendor + device IDs et descriptions disponibles).
    pub summary: String,
}

/// Détecte les GPUs présents sur ce nœud.
///
/// Méthode : scan PCI via `/sys/bus/pci/devices` — fonctionne pour NVIDIA,
/// AMD, Intel, Qualcomm, Broadcom, virtio-gpu, etc.
pub fn detect_gpus() -> GpuInfo {
    // 1. Tentative via le sysfs PCI (méthode préférée)
    if let Some(info) = detect_via_sysfs_pci() {
        if info.present {
            return info;
        }
    }

    // 2. Fallback DRI (driver chargé)
    detect_via_dri()
}

/// Raccourci booléen pour l'usage dans les requêtes de routage.
pub fn has_gpu() -> bool {
    detect_gpus().present
}

fn detect_via_sysfs_pci() -> Option<GpuInfo> {
    let dir = std::fs::read_dir("/sys/bus/pci/devices").ok()?;
    let mut count = 0u32;
    let mut entries = Vec::new();

    for entry in dir.filter_map(|e| e.ok()) {
        let class_path = entry.path().join("class");
        let class_str = std::fs::read_to_string(&class_path).ok()?;
        // Format : "0x030200\n"
        let class_val: u32 =
            u32::from_str_radix(class_str.trim().trim_start_matches("0x"), 16).ok()?;

        // La classe est sur 24 bits : les 8 bits de poids fort = classe PCI
        let class_top = class_val >> 16;
        if class_top != PCI_CLASS_DISPLAY_PREFIX {
            continue;
        }

        count += 1;

        // Lire vendor/device pour le résumé (best-effort)
        let vendor = read_sysfs_id(entry.path().join("vendor"));
        let device = read_sysfs_id(entry.path().join("device"));
        let label = resolve_pci_class(class_val);
        entries.push(format!("{label} [{vendor}:{device}]"));
    }

    Some(GpuInfo {
        present: count > 0,
        count,
        summary: entries.join(", "),
    })
}

fn detect_via_dri() -> GpuInfo {
    let count = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("card"))
                .count() as u32
        })
        .unwrap_or(0);

    GpuInfo {
        present: count > 0,
        count,
        summary: if count > 0 {
            format!("{count} DRI card(s) détectée(s) (driver chargé)")
        } else {
            String::new()
        },
    }
}

fn read_sysfs_id(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "????".to_string())
}

fn resolve_pci_class(class: u32) -> &'static str {
    match class >> 8 {
        0x0300 => "VGA Compatible Controller",
        0x0301 => "XGA Compatible Controller",
        0x0302 => "3D Controller",
        0x0380 => "Display Controller",
        _ => "GPU/Display Device",
    }
}

// ─── Espace disque ────────────────────────────────────────────────────────────

/// Retourne l'espace disque disponible et total sur `path` (en Mio).
///
/// Utilise `statvfs(2)`. Retourne `(0, 0)` si le chemin n'existe pas ou si
/// l'appel système échoue.
pub fn disk_space_mib(path: &str) -> (u64, u64) {
    let Ok(cpath) = std::ffi::CString::new(path) else {
        return (0, 0);
    };

    // SAFETY: stat sera entièrement initialisé par statvfs si ret == 0.
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let ret = unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        warn!(path = %path, error = %err, "statvfs échoué");
        return (0, 0);
    }

    // SAFETY: ret == 0 garantit que stat est initialisé.
    let s = unsafe { stat.assume_init() };
    let frsize = s.f_frsize as u64;
    if frsize == 0 {
        return (0, 0);
    }

    let avail = (s.f_bavail as u64).saturating_mul(frsize) / (1024 * 1024);
    let total = (s.f_blocks as u64).saturating_mul(frsize) / (1024 * 1024);
    (avail, total)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_space_tmp_non_zero() {
        let (avail, total) = disk_space_mib("/tmp");
        assert!(total > 0, "espace total /tmp doit être > 0");
        assert!(avail <= total, "espace disponible ≤ total");
    }

    #[test]
    fn test_disk_space_nonexistent_returns_zero() {
        let (avail, total) = disk_space_mib("/nonexistent_omega_path_xyz");
        assert_eq!(avail, 0);
        assert_eq!(total, 0);
    }

    #[test]
    fn test_detect_gpus_no_panic() {
        // On ne peut pas affirmer la présence d'un GPU en CI,
        // mais la fonction ne doit jamais paniquer.
        let info = detect_gpus();
        assert!(info.count < 256, "count GPU implausible (> 256)");
    }

    #[test]
    fn test_has_gpu_consistent_with_detect() {
        assert_eq!(has_gpu(), detect_gpus().present);
    }

    #[test]
    fn test_resolve_pci_class_vga() {
        assert_eq!(resolve_pci_class(0x030000), "VGA Compatible Controller");
    }

    #[test]
    fn test_resolve_pci_class_3d() {
        assert_eq!(resolve_pci_class(0x030200), "3D Controller");
    }

    #[test]
    fn test_resolve_pci_class_display() {
        assert_eq!(resolve_pci_class(0x038000), "Display Controller");
    }
}
