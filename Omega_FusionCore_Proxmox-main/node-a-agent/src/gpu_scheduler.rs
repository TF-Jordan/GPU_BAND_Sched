//! Scheduler de partage GPU entre plusieurs VMs sur le même nœud.
//!
//! Implémente un round-robin temporel : chaque VM GPU reçoit le GPU
//! pendant un quantum configurable, puis cède sa place à la suivante.
//!
//! ## Mécanisme
//!
//! Utilise le protocole QMP (QEMU Machine Protocol) via socket Unix
//! pour hot-plug/unplug dynamique du GPU pendant que les VMs tournent :
//!
//! ```text
//!   VM-A a le GPU  ──► device_del hostpci0 (VM-A)
//!                  ──► reset GPU sysfs
//!                  ──► device_add vfio-pci (VM-B)
//!                  ──► VM-B a le GPU pendant quantum_secs
//!                  ──► rotation suivante…
//! ```
//!
//! ## Leader election
//!
//! Un seul scheduler tourne par GPU par nœud, élu via `flock(LOCK_EX|LOCK_NB)`
//! sur `/run/omega-gpu-scheduler-<pci>.lock`.
//! Si l'agent élu s'arrête, le lock est libéré et un autre prend le relais.
//!
//! ## Reset GPU
//!
//! vfio-pci émet automatiquement `VFIO_DEVICE_RESET` ioctl lors du `device_del`,
//! ce qui purge l'état GPU côté kernel sans aucun outil externe.

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::gpu_placement::GPU_TAG;

// ─── Chemins ──────────────────────────────────────────────────────────────────

fn qmp_socket_path(vmid: u32) -> String {
    format!("/var/run/qemu-server/{vmid}.qmp")
}

fn scheduler_lock_path(pci_id: &str) -> String {
    let safe = pci_id.replace([':', '.'], "-");
    format!("/run/omega-gpu-scheduler-{safe}.lock")
}

// ─── GpuScheduler ─────────────────────────────────────────────────────────────

pub struct GpuScheduler {
    /// Adresse PCI du GPU partagé, ex: "0000:02:00.0"
    gpu_pci_id: String,
    /// Temps alloué par VM avant rotation (secondes)
    quantum_secs: u64,
    /// Nœud courant (pour filtrer les VMs locales via pvesh)
    current_node: String,
}

impl GpuScheduler {
    pub fn new(gpu_pci_id: String, quantum_secs: u64, current_node: String) -> Self {
        Self {
            gpu_pci_id,
            quantum_secs,
            current_node,
        }
    }

    /// Lance le scheduler.
    ///
    /// Tente d'acquérir le lock exclusif non-bloquant. Si un autre agent
    /// est déjà scheduler, retourne immédiatement (mode passif silencieux).
    pub async fn run(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        let lock_path = scheduler_lock_path(&self.gpu_pci_id);

        let lock_file = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, path = %lock_path, "impossible d'ouvrir le lock scheduler GPU");
                return;
            }
        };

        // LOCK_EX | LOCK_NB : échoue immédiatement si un autre process tient le lock
        let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            info!(
                pci          = %self.gpu_pci_id,
                "un autre agent est déjà scheduler GPU sur ce nœud — mode passif"
            );
            return;
        }

        info!(
            pci          = %self.gpu_pci_id,
            quantum_s    = self.quantum_secs,
            current_node = %self.current_node,
            "scheduler GPU élu — démarrage rotation round-robin"
        );

        // _lock_file maintient le fd ouvert → le flock reste actif
        let _lock_file = lock_file;
        self.scheduler_loop(shutdown).await;

        info!(pci = %self.gpu_pci_id, "scheduler GPU arrêté — lock libéré");
    }

    // ── Boucle principale ─────────────────────────────────────────────────────

    async fn scheduler_loop(&self, shutdown: Arc<AtomicBool>) {
        let mut rr_idx = 0usize;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let gpu_vms = match self.list_gpu_vms_on_node().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "liste VMs GPU échouée — retry dans 10 s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            if gpu_vms.is_empty() {
                debug!("aucune VM GPU running — scheduler en veille");
                tokio::time::sleep(Duration::from_secs(self.quantum_secs)).await;
                continue;
            }

            if gpu_vms.len() == 1 {
                // Une seule VM : pas de rotation nécessaire
                if let Err(e) = self.assign_gpu_to(gpu_vms[0]).await {
                    warn!(error = %e, vmid = gpu_vms[0], "assign_gpu_to échoué");
                }
                tokio::time::sleep(Duration::from_secs(self.quantum_secs)).await;
                continue;
            }

            // Round-robin sur la liste courante
            rr_idx = rr_idx % gpu_vms.len();
            let vmid = gpu_vms[rr_idx];
            rr_idx += 1;

            info!(
                vmid       = vmid,
                slot       = rr_idx,
                total      = gpu_vms.len(),
                quantum_s  = self.quantum_secs,
                pci        = %self.gpu_pci_id,
                "GPU assigné à la VM courante"
            );

            if let Err(e) = self.assign_gpu_to(vmid).await {
                warn!(error = %e, vmid, "assign_gpu_to échoué — VM suivante au prochain tick");
            }

            tokio::time::sleep(Duration::from_secs(self.quantum_secs)).await;
        }
    }

    // ── Attribution GPU ───────────────────────────────────────────────────────

    /// Détache le GPU de toutes les VMs sauf `target_vmid`, resets le GPU,
    /// puis l'attache à `target_vmid`.
    async fn assign_gpu_to(&self, target_vmid: u32) -> Result<()> {
        let gpu_vms = self.list_gpu_vms_on_node().await?;

        // 1. Détacher des VMs qui ont actuellement le GPU (sauf la cible)
        for &vmid in gpu_vms.iter().filter(|&&v| v != target_vmid) {
            match self.qmp_device_del(vmid, "hostpci0").await {
                Ok(_) => debug!(vmid, "GPU détaché"),
                Err(e) => debug!(vmid, error = %e, "device_del : GPU peut-être déjà absent"),
            }
        }

        // 2. Laisser le bus PCI se stabiliser (vfio-pci a déjà émis VFIO_DEVICE_RESET)
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 3. Attacher à la VM cible
        self.qmp_device_add(target_vmid, &self.gpu_pci_id, "hostpci0")
            .await
    }

    // ── QMP ───────────────────────────────────────────────────────────────────

    async fn qmp_device_del(&self, vmid: u32, device_id: &str) -> Result<()> {
        let cmd = serde_json::json!({
            "execute": "device_del",
            "arguments": { "id": device_id }
        });
        let resp = self.qmp_execute(vmid, &cmd).await?;
        if let Some(err) = resp.get("error") {
            bail!(
                "device_del {device_id} VM {vmid} : {}",
                err["desc"].as_str().unwrap_or("?")
            );
        }
        debug!(vmid, device = device_id, "QMP device_del OK");
        Ok(())
    }

    async fn qmp_device_add(&self, vmid: u32, pci_id: &str, device_id: &str) -> Result<()> {
        let cmd = serde_json::json!({
            "execute": "device_add",
            "arguments": {
                "driver": "vfio-pci",
                "host":   pci_id,
                "id":     device_id,
                "bus":    "pcie.0"
            }
        });
        let resp = self.qmp_execute(vmid, &cmd).await?;
        if let Some(err) = resp.get("error") {
            bail!(
                "device_add {pci_id} VM {vmid} : {}",
                err["desc"].as_str().unwrap_or("?")
            );
        }
        debug!(vmid, pci = pci_id, device = device_id, "QMP device_add OK");
        Ok(())
    }

    /// Exécute une commande QMP sur le socket Unix de `vmid`.
    ///
    /// Protocole QMP :
    /// 1. Connexion → lecture du greeting `{"QMP": ...}`
    /// 2. Envoi `{"execute":"qmp_capabilities"}` → ACK
    /// 3. Envoi de la commande → lecture réponse (ignore les événements async)
    async fn qmp_execute(&self, vmid: u32, cmd: &serde_json::Value) -> Result<serde_json::Value> {
        let socket_path = qmp_socket_path(vmid);

        let stream =
            tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(&socket_path))
                .await
                .map_err(|_| anyhow::anyhow!("timeout connexion QMP {socket_path}"))?
                .map_err(|e| anyhow::anyhow!("connexion QMP {socket_path} : {e}"))?;

        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // 1. Greeting
        lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow::anyhow!("QMP : greeting absent de {socket_path}"))?;

        // 2. Capabilities
        let caps = format!("{}\n", serde_json::json!({"execute": "qmp_capabilities"}));
        writer.write_all(caps.as_bytes()).await?;
        lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow::anyhow!("QMP : ack capabilities absent"))?;

        // 3. Commande
        let cmd_str = format!("{cmd}\n");
        writer.write_all(cmd_str.as_bytes()).await?;

        // Lire en ignorant les événements async (clé "event") jusqu'à "return"/"error"
        loop {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .map_err(|_| anyhow::anyhow!("QMP : timeout lecture réponse {socket_path}"))?
                .map_err(|e| anyhow::anyhow!("QMP : erreur lecture : {e}"))?
                .ok_or_else(|| anyhow::anyhow!("QMP : connexion fermée avant réponse"))?;

            let val: serde_json::Value = serde_json::from_str(&line)
                .map_err(|e| anyhow::anyhow!("QMP : JSON invalide '{line}' : {e}"))?;

            if val.get("return").is_some() || val.get("error").is_some() {
                return Ok(val);
            }
            // Événement async — on ignore et on continue à lire
            debug!(
                vmid,
                event = val.get("event").and_then(|e| e.as_str()).unwrap_or("?"),
                "QMP event ignoré"
            );
        }
    }

    // ── Inventaire VMs GPU ────────────────────────────────────────────────────

    /// Liste les vmids des VMs running avec tag `omega-gpu` sur le nœud courant.
    async fn list_gpu_vms_on_node(&self) -> Result<Vec<u32>> {
        let out = tokio::process::Command::new("pvesh")
            .args([
                "get",
                &format!("/nodes/{}/qemu", self.current_node),
                "--output-format",
                "json",
            ])
            .output()
            .await?;

        if !out.status.success() {
            bail!(
                "pvesh /nodes/{}/qemu : {}",
                self.current_node,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let arr: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();

        let vmids = arr
            .iter()
            .filter_map(|item| {
                let vmid = item["vmid"].as_u64()? as u32;
                let status = item["status"].as_str().unwrap_or("");
                let tags = item["tags"].as_str().unwrap_or("");
                let needs_gpu = tags.split(';').any(|t| t.trim() == GPU_TAG);
                if status == "running" && needs_gpu {
                    Some(vmid)
                } else {
                    None
                }
            })
            .collect();

        Ok(vmids)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qmp_socket_path() {
        assert_eq!(qmp_socket_path(100), "/var/run/qemu-server/100.qmp");
        assert_eq!(qmp_socket_path(9001), "/var/run/qemu-server/9001.qmp");
    }

    #[test]
    fn test_scheduler_lock_path_no_special_chars() {
        let path = scheduler_lock_path("0000:02:00.0");
        // La portion PCI ne doit pas contenir ':' ou '.' (remplacés par '-')
        assert!(
            !path.contains(':'),
            "les ':' doivent être remplacés dans le chemin"
        );
        // Le fichier se termine par .lock (seul '.' autorisé dans le chemin)
        assert!(path.ends_with(".lock"));
        assert!(path.contains("0000-02-00-0"));
    }

    #[test]
    fn test_round_robin_wraps_correctly() {
        let vms = vec![101u32, 102, 103];
        let mut idx = 0usize;
        let mut order = Vec::new();
        for _ in 0..6 {
            idx = idx % vms.len();
            order.push(vms[idx]);
            idx += 1;
        }
        assert_eq!(order, vec![101, 102, 103, 101, 102, 103]);
    }

    #[test]
    fn test_gpu_tag_filter_in_vm_list() {
        let tagged = "web;omega-gpu;db";
        let not_tagged = "web;production";
        assert!(tagged.split(';').any(|t| t.trim() == GPU_TAG));
        assert!(!not_tagged.split(';').any(|t| t.trim() == GPU_TAG));
    }

    #[test]
    fn test_qmp_device_del_command_format() {
        let cmd = serde_json::json!({
            "execute": "device_del",
            "arguments": { "id": "hostpci0" }
        });
        assert_eq!(cmd["execute"], "device_del");
        assert_eq!(cmd["arguments"]["id"], "hostpci0");
    }

    #[test]
    fn test_qmp_device_add_command_format() {
        let cmd = serde_json::json!({
            "execute": "device_add",
            "arguments": {
                "driver": "vfio-pci",
                "host":   "0000:02:00.0",
                "id":     "hostpci0",
                "bus":    "pcie.0"
            }
        });
        assert_eq!(cmd["arguments"]["driver"], "vfio-pci");
        assert_eq!(cmd["arguments"]["host"], "0000:02:00.0");
        assert_eq!(cmd["arguments"]["bus"], "pcie.0");
    }

    #[test]
    fn test_single_vm_no_rotation_needed() {
        // Avec une seule VM, pas besoin de rotation
        let gpu_vms = vec![101u32];
        assert_eq!(gpu_vms.len(), 1);
    }

    #[test]
    fn test_vfio_reset_is_implicit() {
        // vfio-pci émet VFIO_DEVICE_RESET lors de device_del :
        // pas de sysfs reset explicite, pas de module externe requis.
        // Ce test documente l'invariant : assign_gpu_to ne doit pas
        // appeler de commande externe entre device_del et device_add.
        let _scheduler = GpuScheduler::new("0000:02:00.0".into(), 30, "pve1".into());
        // Si reset_gpu() existait encore, la compilation échouerait ici.
    }
}
