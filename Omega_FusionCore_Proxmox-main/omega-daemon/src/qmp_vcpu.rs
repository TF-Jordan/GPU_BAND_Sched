//! Hotplug vCPU réel via QMP (QEMU Monitor Protocol).
//!
//! # Pourquoi QMP et pas juste changer un compteur ?
//!
//! Notre ancien VcpuScheduler gérait des slots dans une HashMap —
//! QEMU ne savait rien de nos décisions. Avec QMP, on envoie des commandes
//! directement au processus QEMU de la VM :
//!
//! - `query-cpus-fast`  → lire le nombre de vCPUs actuels
//! - `device_add cpu`   → brancher un vCPU à chaud (hotplug)
//! - `device_del`       → débrancher un vCPU (hot-unplug)
//!
//! Ce mécanisme est exactement ce qu'utilisent OpenStack Nova et les
//! outils de gestion cloud pour le redimensionnement à chaud des VMs.
//!
//! # Prérequis côté VM
//!
//! Pour que le hotplug fonctionne, la VM doit être démarrée avec :
//! ```text
//! -smp <min_vcpus>,maxcpus=<max_vcpus>
//! ```
//! et le guest doit avoir le driver cpu-hotplug chargé (automatique sur
//! les kernels Linux ≥ 3.16 et Windows avec les drivers VirtIO).
//!
//! Proxmox configure cela automatiquement si `hotplug: cpu` est activé
//! dans la configuration de la VM (`/etc/pve/qemu-server/{vmid}.conf`).
//!
//! # Socket QMP Proxmox
//!
//! `/var/run/qemu-server/{vmid}.qmp`  ← même socket que le balloon monitor

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

// ─── Structures ───────────────────────────────────────────────────────────────

/// État vCPU d'une VM tel que rapporté par QEMU.
#[derive(Debug, Clone)]
pub struct QmpVcpuInfo {
    pub vm_id: u32,
    /// Nombre de vCPUs actuellement actifs
    pub online_count: usize,
    /// Nombre total de vCPUs configurés (y compris hors-ligne)
    pub total_count: usize,
    /// Détails par vCPU
    pub cpus: Vec<VcpuEntry>,
}

/// Informations sur un vCPU individuel.
#[derive(Debug, Clone)]
pub struct VcpuEntry {
    pub index: usize,
    pub online: bool,
    pub thread_id: Option<u64>,
    pub qom_path: String,
}

#[derive(Debug, Clone)]
struct HotpluggableCpuSlot {
    qom_path: Option<String>,
    cpu_type: String,
    socket_id: i64,
    core_id: i64,
    thread_id: i64,
}

/// Résultat d'une opération hotplug.
#[derive(Debug, Clone)]
pub enum HotplugResult {
    /// vCPU ajouté avec succès — nouveau total
    Added { new_count: usize },
    /// vCPU retiré avec succès — nouveau total
    Removed { new_count: usize },
    /// Impossible : la VM n'a pas de slots hotplug disponibles
    NoSlots { current: usize, max: usize },
    /// Impossible : déjà au minimum
    AtMin { current: usize, min: usize },
    /// VM inaccessible (arrêtée ou QMP indisponible)
    Unavailable { reason: String },
}

// ─── Client QMP vCPU ─────────────────────────────────────────────────────────

/// Client QMP dédié au contrôle des vCPUs.
pub struct QmpVcpuClient {
    vm_id: u32,
    sock_path: PathBuf,
    timeout: Duration,
}

impl QmpVcpuClient {
    pub fn new(vm_id: u32, qmp_dir: impl Into<PathBuf>) -> Self {
        let dir = qmp_dir.into();
        Self {
            vm_id,
            sock_path: dir.join(format!("{}.qmp", vm_id)),
            timeout: Duration::from_secs(5),
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    pub fn is_available(&self) -> bool {
        self.sock_path.exists()
    }

    // ── Connexion et initialisation ───────────────────────────────────────

    fn connect(&self) -> Result<(UnixStream, BufReader<UnixStream>)> {
        let stream = UnixStream::connect(&self.sock_path)
            .with_context(|| format!("connexion QMP vmid={}", self.vm_id))?;

        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let reader = BufReader::new(stream.try_clone()?);
        Ok((stream, reader))
    }

    fn handshake(&self, stream: &mut UnixStream, reader: &mut BufReader<UnixStream>) -> Result<()> {
        // Lire le greeting
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        debug!(vm_id = self.vm_id, "QMP greeting reçu");

        // Activer les capacités
        self.send(stream, &json!({"execute": "qmp_capabilities"}))?;
        let resp = self.recv_command_response(reader)?;
        if resp.get("error").is_some() {
            bail!("qmp_capabilities échoué : {:?}", resp);
        }
        Ok(())
    }

    fn send(&self, stream: &mut UnixStream, msg: &Value) -> Result<()> {
        let mut buf = serde_json::to_string(msg)?;
        buf.push('\n');
        stream.write_all(buf.as_bytes())?;
        Ok(())
    }

    fn recv(&self, reader: &mut BufReader<UnixStream>) -> Result<Value> {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        serde_json::from_str(&line).with_context(|| format!("JSON invalide: {}", line))
    }

    fn recv_command_response(&self, reader: &mut BufReader<UnixStream>) -> Result<Value> {
        loop {
            let resp = self.recv(reader)?;
            if resp.get("return").is_some() || resp.get("error").is_some() {
                return Ok(resp);
            }
            debug!(
                vm_id = self.vm_id,
                response = ?resp,
                "événement QMP ignoré en attente de la réponse commande"
            );
        }
    }

    // ── API publique ──────────────────────────────────────────────────────

    /// Interroge l'état des vCPUs de la VM.
    pub fn query_cpus(&self) -> Result<QmpVcpuInfo> {
        if !self.is_available() {
            bail!("socket QMP absente pour vmid={}", self.vm_id);
        }

        let (mut stream, mut reader) = self.connect()?;
        self.handshake(&mut stream, &mut reader)?;

        self.send(&mut stream, &json!({"execute": "query-cpus-fast"}))?;
        let resp = self.recv_command_response(&mut reader)?;

        if let Some(err) = resp.get("error") {
            bail!("query-cpus-fast échoué : {:?}", err);
        }

        let cpus_json = resp["return"].as_array().cloned().unwrap_or_default();

        let mut cpus = Vec::new();
        for (idx, cpu) in cpus_json.iter().enumerate() {
            let thread_id = cpu["thread-id"].as_u64();
            let qom_path = cpu["qom-path"].as_str().unwrap_or("").to_string();
            cpus.push(VcpuEntry {
                index: idx,
                online: cpu["online"]
                    .as_bool()
                    .unwrap_or(thread_id.is_some() || !qom_path.is_empty()),
                thread_id,
                qom_path,
            });
        }

        let online_count = cpus.iter().filter(|c| c.online).count();
        let total_count = self
            .query_hotpluggable_cpus()
            .map(|slots| slots.len().max(cpus.len()))
            .unwrap_or(cpus.len());

        debug!(
            vm_id = self.vm_id,
            online = online_count,
            total = total_count,
            "vCPUs queryés"
        );

        Ok(QmpVcpuInfo {
            vm_id: self.vm_id,
            online_count,
            total_count,
            cpus,
        })
    }

    fn query_hotpluggable_cpus(&self) -> Result<Vec<HotpluggableCpuSlot>> {
        if !self.is_available() {
            bail!("socket QMP absente pour vmid={}", self.vm_id);
        }

        let (mut stream, mut reader) = self.connect()?;
        self.handshake(&mut stream, &mut reader)?;

        self.send(&mut stream, &json!({"execute": "query-hotpluggable-cpus"}))?;
        let resp = self.recv_command_response(&mut reader)?;

        if let Some(err) = resp.get("error") {
            bail!("query-hotpluggable-cpus échoué : {:?}", err);
        }

        let mut slots = Vec::new();
        for entry in resp["return"].as_array().cloned().unwrap_or_default() {
            let props = entry.get("props").cloned().unwrap_or_else(|| json!({}));
            slots.push(HotpluggableCpuSlot {
                qom_path: entry
                    .get("qom-path")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                cpu_type: entry
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("host-x86_64-cpu")
                    .to_string(),
                socket_id: props.get("socket-id").and_then(|v| v.as_i64()).unwrap_or(0),
                core_id: props.get("core-id").and_then(|v| v.as_i64()).unwrap_or(0),
                thread_id: props.get("thread-id").and_then(|v| v.as_i64()).unwrap_or(0),
            });
        }

        Ok(slots)
    }

    /// Ajoute un vCPU à chaud (hotplug +1).
    ///
    /// QEMU doit avoir été démarré avec `-smp maxcpus=N` où N > vCPUs actuels.
    /// Proxmox active cela automatiquement si `hotplug: cpu` est dans la conf VM.
    ///
    /// Retourne le nouveau nombre de vCPUs en ligne.
    pub fn hotplug_add(&self, _min_vcpus: usize, max_vcpus: usize) -> Result<HotplugResult> {
        if !self.is_available() {
            return Ok(HotplugResult::Unavailable {
                reason: format!("socket QMP absente pour vmid={}", self.vm_id),
            });
        }

        // Lire l'état courant
        let info = match self.query_cpus() {
            Ok(i) => i,
            Err(e) => {
                return Ok(HotplugResult::Unavailable {
                    reason: e.to_string(),
                })
            }
        };

        if info.online_count >= max_vcpus {
            return Ok(HotplugResult::NoSlots {
                current: info.online_count,
                max: max_vcpus,
            });
        }

        // Trouver le prochain slot CPU hors-ligne réellement hotpluggable.
        let hotpluggable = match self.query_hotpluggable_cpus() {
            Ok(slots) => slots,
            Err(e) => {
                return Ok(HotplugResult::Unavailable {
                    reason: e.to_string(),
                })
            }
        };

        let Some(cpu_slot) = hotpluggable.iter().find(|slot| slot.qom_path.is_none()) else {
            warn!(
                vm_id = self.vm_id,
                online = info.online_count,
                max_vcpus = max_vcpus,
                "aucun slot CPU hotpluggable hors-ligne — démarrer la VM avec -smp maxcpus={}",
                max_vcpus
            );
            return Ok(HotplugResult::NoSlots {
                current: info.online_count,
                max: max_vcpus,
            });
        };

        // Envoyer device_add pour brancher le vCPU
        let (mut stream, mut reader) = self.connect()?;
        self.handshake(&mut stream, &mut reader)?;

        self.send(
            &mut stream,
            &json!({
                "execute": "device_add",
                "arguments": {
                    "driver": cpu_slot.cpu_type,
                    "id":     format!("cpu-{}-{}-{}", cpu_slot.socket_id, cpu_slot.core_id, cpu_slot.thread_id),
                    "socket-id": cpu_slot.socket_id,
                    "core-id":   cpu_slot.core_id,
                    "thread-id": cpu_slot.thread_id
                }
            }),
        )?;

        let resp = self.recv_command_response(&mut reader)?;

        if let Some(err) = resp.get("error") {
            // Certains guests ne supportent pas le hot-unplug → fallback gracieux
            let msg = err["desc"].as_str().unwrap_or("inconnu").to_string();
            warn!(vm_id = self.vm_id, error = %msg, "device_add cpu échoué");
            return Ok(HotplugResult::Unavailable { reason: msg });
        }

        let expected_count = info.online_count + 1;
        let verified_count = match self.query_cpus() {
            Ok(after) => after.online_count,
            Err(e) => {
                let msg = format!("device_add envoyé mais vérification QMP impossible: {e}");
                warn!(vm_id = self.vm_id, error = %msg, "hotplug vCPU non validé");
                return Ok(HotplugResult::Unavailable { reason: msg });
            }
        };
        if verified_count < expected_count {
            let msg = format!(
                "device_add envoyé mais vCPUs online inchangés: avant={} après={} attendu={}",
                info.online_count, verified_count, expected_count
            );
            warn!(vm_id = self.vm_id, error = %msg, "hotplug vCPU non validé");
            return Ok(HotplugResult::Unavailable { reason: msg });
        }

        info!(
            vm_id = self.vm_id,
            socket_id = cpu_slot.socket_id,
            core_id = cpu_slot.core_id,
            thread_id = cpu_slot.thread_id,
            cpu_type = %cpu_slot.cpu_type,
            new_count = verified_count,
            "vCPU hotplugged via QMP"
        );

        Ok(HotplugResult::Added {
            new_count: verified_count,
        })
    }

    /// Retire un vCPU à chaud (hot-unplug -1).
    ///
    /// Ne descend jamais sous `min_vcpus`.
    pub fn hotplug_remove(&self, min_vcpus: usize) -> Result<HotplugResult> {
        if !self.is_available() {
            return Ok(HotplugResult::Unavailable {
                reason: format!("socket QMP absente pour vmid={}", self.vm_id),
            });
        }

        let info = match self.query_cpus() {
            Ok(i) => i,
            Err(e) => {
                return Ok(HotplugResult::Unavailable {
                    reason: e.to_string(),
                })
            }
        };

        if info.online_count <= min_vcpus {
            return Ok(HotplugResult::AtMin {
                current: info.online_count,
                min: min_vcpus,
            });
        }

        // Retirer le vCPU avec l'index le plus élevé (LIFO)
        let last_online = info
            .cpus
            .iter()
            .filter(|c| c.online)
            .max_by_key(|c| c.index);

        let Some(cpu) = last_online else {
            return Ok(HotplugResult::AtMin {
                current: 0,
                min: min_vcpus,
            });
        };

        let cpu_id =
            device_id_from_qom_path(&cpu.qom_path).unwrap_or_else(|| format!("cpu-{}", cpu.index));

        let (mut stream, mut reader) = self.connect()?;
        self.handshake(&mut stream, &mut reader)?;

        self.send(
            &mut stream,
            &json!({
                "execute": "device_del",
                "arguments": { "id": cpu_id }
            }),
        )?;

        let resp = self.recv_command_response(&mut reader)?;

        if let Some(err) = resp.get("error") {
            let msg = err["desc"].as_str().unwrap_or("inconnu").to_string();
            warn!(vm_id = self.vm_id, error = %msg, "device_del cpu échoué");
            return Ok(HotplugResult::Unavailable { reason: msg });
        }

        let expected_count = info.online_count - 1;
        let verified_count = match self.query_cpus() {
            Ok(after) => after.online_count,
            Err(e) => {
                let msg = format!("device_del envoyé mais vérification QMP impossible: {e}");
                warn!(vm_id = self.vm_id, error = %msg, "hot-unplug vCPU non validé");
                return Ok(HotplugResult::Unavailable { reason: msg });
            }
        };
        if verified_count > expected_count {
            let msg = format!(
                "device_del envoyé mais vCPUs online non réduits: avant={} après={} attendu={}",
                info.online_count, verified_count, expected_count
            );
            warn!(vm_id = self.vm_id, error = %msg, "hot-unplug vCPU non validé");
            return Ok(HotplugResult::Unavailable { reason: msg });
        }

        info!(
            vm_id     = self.vm_id,
            cpu_id    = %cpu_id,
            qom_path  = %cpu.qom_path,
            new_count = verified_count,
            "vCPU retiré via QMP"
        );

        Ok(HotplugResult::Removed {
            new_count: verified_count,
        })
    }
}

fn device_id_from_qom_path(qom_path: &str) -> Option<String> {
    let trimmed = qom_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    trimmed
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
}

// ─── Gestionnaire de hotplug cluster ─────────────────────────────────────────

/// Applique les décisions du VcpuScheduler via QMP + cgroups.
///
/// C'est le pont entre la logique de décision (VcpuScheduler) et
/// les mécanismes réels du kernel (cgroups v2) et de QEMU (QMP).
pub struct VcpuHotplugManager {
    qmp_dir: PathBuf,
}

impl VcpuHotplugManager {
    pub fn new(qmp_dir: impl Into<PathBuf>) -> Self {
        Self {
            qmp_dir: qmp_dir.into(),
        }
    }

    /// Applique un hotplug +1 vCPU à une VM via QMP.
    pub fn add_vcpu(&self, vm_id: u32, min_vcpus: usize, max_vcpus: usize) -> HotplugResult {
        let client = QmpVcpuClient::new(vm_id, &self.qmp_dir);
        client
            .hotplug_add(min_vcpus, max_vcpus)
            .unwrap_or_else(|e| HotplugResult::Unavailable {
                reason: e.to_string(),
            })
    }

    /// Applique un hot-unplug -1 vCPU à une VM via QMP.
    pub fn remove_vcpu(&self, vm_id: u32, min_vcpus: usize) -> HotplugResult {
        let client = QmpVcpuClient::new(vm_id, &self.qmp_dir);
        client
            .hotplug_remove(min_vcpus)
            .unwrap_or_else(|e| HotplugResult::Unavailable {
                reason: e.to_string(),
            })
    }

    /// Lit le nombre de vCPUs en ligne d'une VM.
    pub fn online_vcpu_count(&self, vm_id: u32) -> Option<usize> {
        let client = QmpVcpuClient::new(vm_id, &self.qmp_dir);
        client.query_cpus().ok().map(|i| i.online_count)
    }

    pub fn vcpu_info(&self, vm_id: u32) -> Option<QmpVcpuInfo> {
        let client = QmpVcpuClient::new(vm_id, &self.qmp_dir);
        client.query_cpus().ok()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_device_id_from_qom_path_extracts_leaf() {
        assert_eq!(
            device_id_from_qom_path("/machine/unattached/device[3]"),
            Some("device[3]".to_string())
        );
    }

    #[test]
    fn test_device_id_from_qom_path_rejects_empty() {
        assert_eq!(device_id_from_qom_path(""), None);
        assert_eq!(device_id_from_qom_path("   "), None);
    }

    #[test]
    fn test_client_unavailable_when_no_socket() {
        let tmp = TempDir::new().unwrap();
        let client = QmpVcpuClient::new(101, tmp.path());
        assert!(!client.is_available());
    }

    #[test]
    fn test_hotplug_add_unavailable_when_no_socket() {
        let tmp = TempDir::new().unwrap();
        let client = QmpVcpuClient::new(101, tmp.path());
        let result = client.hotplug_add(1, 4).unwrap();
        assert!(matches!(result, HotplugResult::Unavailable { .. }));
    }

    #[test]
    fn test_hotplug_remove_unavailable_when_no_socket() {
        let tmp = TempDir::new().unwrap();
        let client = QmpVcpuClient::new(101, tmp.path());
        let result = client.hotplug_remove(1).unwrap();
        assert!(matches!(result, HotplugResult::Unavailable { .. }));
    }

    #[test]
    fn test_manager_returns_unavailable_without_qmp() {
        let tmp = TempDir::new().unwrap();
        let mgr = VcpuHotplugManager::new(tmp.path());
        let result = mgr.add_vcpu(200, 1, 4);
        assert!(matches!(result, HotplugResult::Unavailable { .. }));
        assert!(mgr.online_vcpu_count(200).is_none());
    }

    #[test]
    fn test_socket_path_construction() {
        let tmp = TempDir::new().unwrap();
        // Créer un faux socket (fichier)
        fs::write(tmp.path().join("300.qmp"), "").unwrap();
        let client = QmpVcpuClient::new(300, tmp.path());
        assert!(client.is_available());
    }

    #[test]
    fn test_hotplug_result_variants() {
        // Vérifier que toutes les variantes sont constructibles et matchables
        let r1 = HotplugResult::Added { new_count: 3 };
        let r2 = HotplugResult::Removed { new_count: 1 };
        let r3 = HotplugResult::NoSlots { current: 4, max: 4 };
        let r4 = HotplugResult::AtMin { current: 1, min: 1 };
        let r5 = HotplugResult::Unavailable {
            reason: "test".into(),
        };

        assert!(matches!(r1, HotplugResult::Added { new_count: 3 }));
        assert!(matches!(r2, HotplugResult::Removed { new_count: 1 }));
        assert!(matches!(r3, HotplugResult::NoSlots { current: 4, max: 4 }));
        assert!(matches!(r4, HotplugResult::AtMin { current: 1, min: 1 }));
        assert!(matches!(r5, HotplugResult::Unavailable { .. }));
    }
}
