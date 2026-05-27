//! Gestionnaire de balloon RAM — thin-provisioning côté guest.
//!
//! ## Principe
//!
//! La VM est créée avec `memory=<max_mib>` dans Proxmox mais le balloon QEMU
//! lui masque une partie de cette RAM au démarrage (`min_mib`).
//! Ce module surveille le taux de page-faults (proxy de la pression mémoire
//! dans le guest) et ajuste le balloon progressivement :
//!
//! - `fault_rate > grow_faults_per_sec` → dégonfler le balloon (guest voit plus de RAM)
//! - `fault_rate < shrink_faults_per_sec` → gonfler le balloon (récupérer de la RAM)
//!
//! Les bornes dures sont [`min_mib`, `max_mib`] ; on ne sort jamais de cette plage.
//!
//! ## Commande Proxmox
//!
//! Proxmox n'expose pas `qm balloon` sur toutes les versions. On passe donc par
//! le moniteur QEMU/HMP: `qm monitor <vmid>`, puis commande `balloon <mib>`.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::metrics::AgentMetrics;

pub struct BalloonManager {
    vm_id: u32,
    min_mib: u64,
    max_mib: u64,
    step_mib: u64,
    interval: Duration,
    grow_faults_per_sec: u64,
    shrink_faults_per_sec: u64,
    current_mib: Arc<AtomicU64>,
    metrics: Arc<AgentMetrics>,
}

impl BalloonManager {
    pub fn new(
        vm_id: u32,
        min_mib: u64,
        max_mib: u64,
        step_mib: u64,
        interval_secs: u64,
        grow_faults_per_sec: u64,
        shrink_faults_per_sec: u64,
        metrics: Arc<AgentMetrics>,
    ) -> Self {
        let min_mib = min_mib.max(64); // plancher absolu de sécurité
        let max_mib = max_mib.max(min_mib);
        let step_mib = step_mib.max(64);

        Self {
            vm_id,
            min_mib,
            max_mib,
            step_mib,
            interval: Duration::from_secs(interval_secs.max(5)),
            grow_faults_per_sec,
            shrink_faults_per_sec,
            current_mib: Arc::new(AtomicU64::new(min_mib)),
            metrics,
        }
    }

    pub fn current_mib_handle(&self) -> Arc<AtomicU64> {
        self.current_mib.clone()
    }

    pub async fn run(self: Arc<Self>, shutdown: Arc<AtomicBool>) {
        // Au démarrage : positionner le balloon à min_mib
        if let Err(e) = set_balloon(self.vm_id, self.min_mib).await {
            warn!(vm_id = self.vm_id, error = %e, "balloon init échoué — guest voit la RAM complète");
        } else {
            info!(
                vm_id = self.vm_id,
                mib = self.min_mib,
                max_mib = self.max_mib,
                "balloon initialisé — guest démarre avec RAM réduite"
            );
        }

        let mut prev_faults = self.metrics.fault_count.load(Ordering::Relaxed);
        let interval_secs = self.interval.as_secs().max(1);

        loop {
            sleep(self.interval).await;

            if shutdown.load(Ordering::Relaxed) {
                // Libérer toute la RAM au guest avant l'arrêt propre
                let _ = set_balloon(self.vm_id, self.max_mib).await;
                break;
            }

            let cur_faults = self.metrics.fault_count.load(Ordering::Relaxed);
            let delta = cur_faults.saturating_sub(prev_faults);
            let fault_rate = delta / interval_secs;
            prev_faults = cur_faults;

            let current = self.current_mib.load(Ordering::Relaxed);

            let target = if fault_rate >= self.grow_faults_per_sec {
                // Pression mémoire : donner plus de RAM au guest
                (current + self.step_mib).min(self.max_mib)
            } else if fault_rate <= self.shrink_faults_per_sec && current > self.min_mib {
                // Inactivité : récupérer de la RAM
                current.saturating_sub(self.step_mib).max(self.min_mib)
            } else {
                current
            };

            if target != current {
                match set_balloon(self.vm_id, target).await {
                    Ok(_) => {
                        self.current_mib.store(target, Ordering::Relaxed);
                        info!(
                            vm_id = self.vm_id,
                            prev_mib = current,
                            new_mib = target,
                            fault_rate,
                            "balloon ajusté"
                        );
                    }
                    Err(e) => {
                        warn!(vm_id = self.vm_id, error = %e, "commande balloon QMP/HMP échouée")
                    }
                }
            }
        }
    }
}

async fn set_balloon(vm_id: u32, mib: u64) -> anyhow::Result<()> {
    let mut child = Command::new("qm")
        .args(["monitor", &vm_id.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("balloon {mib}\nquit\n").as_bytes())
            .await?;
    }

    let out = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if !out.status.success() {
        anyhow::bail!("qm monitor {vm_id} balloon {mib}: {stderr}");
    }

    let combined = format!("{stdout}\n{stderr}");
    let combined_lower = combined.to_lowercase();
    if combined_lower.contains("unknown command")
        || combined_lower.contains("error:")
        || combined_lower.contains("failed")
    {
        anyhow::bail!("qm monitor {vm_id} balloon {mib}: {}", combined.trim());
    }

    Ok(())
}
