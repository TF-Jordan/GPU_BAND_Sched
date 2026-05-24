//! Intégration virtio-balloon via QEMU Monitor Protocol (QMP) — feature V4.
//!
//! # Principe
//!
//! QEMU expose une socket de contrôle QMP pour chaque VM Proxmox :
//! `/var/run/qemu-server/{vmid}.qmp`
//!
//! On interroge périodiquement cette socket pour lire les statistiques
//! mémoire remontées par le driver virtio-balloon dans le guest.
//!
//! # Statistiques balloon disponibles
//!
//! | Champ                  | Description                               |
//! |------------------------|-------------------------------------------|
//! | stat-free-memory       | RAM libre dans le guest (octets)          |
//! | stat-available-memory  | RAM disponible (avec cache) dans le guest |
//! | stat-total-memory      | RAM totale visible par le guest           |
//! | stat-minor-faults      | Page faults mineurs (depuis démarrage)    |
//! | stat-major-faults      | Page faults majeurs = accès disque/swap   |
//!
//! # Protocole QMP
//!
//! 1. Connexion sur la socket Unix → le serveur envoie le greeting JSON
//! 2. `{"execute": "qmp_capabilities"}` → active les commandes
//! 3. `{"execute": "query-balloon"}` → retourne l'état du balloon
//! 4. Pour les stats : il faut d'abord configurer la fréquence de rapport :
//!    `{"execute": "balloon", "arguments": {"value": <target_bytes>}}`
//! 5. Puis `{"execute": "qom-get", "arguments": {"path": "/machine/peripheral/balloon0", "property": "guest-stats"}}`
//!
//! # Décision d'éviction basée sur balloon
//!
//! Si `stat-free-memory < free_threshold_pct × stat-total-memory` :
//! → augmenter le taux d'éviction de l'agent sur ce nœud
//! → signaler l'urgence au controller pour accélérer la décision de migration

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tracing::{debug, info, trace, warn};

/// Statistiques mémoire rapportées par le balloon driver du guest.
#[derive(Debug, Clone, Default)]
pub struct BalloonStats {
    /// RAM libre dans le guest (octets)
    pub free_bytes: u64,
    /// RAM disponible dans le guest (octets, avec reclaimable)
    pub available_bytes: u64,
    /// RAM totale visible par le guest (octets)
    pub total_bytes: u64,
    /// Page faults majeurs depuis démarrage (indicateur de swap)
    pub major_faults: u64,
    /// Taille actuelle du balloon (octets retirés au guest)
    pub actual_bytes: u64,
}

impl BalloonStats {
    /// Pourcentage de RAM libre dans le guest.
    pub fn free_pct(&self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        (self.free_bytes as f64 / self.total_bytes as f64) * 100.0
    }

    /// Pourcentage de RAM disponible dans le guest.
    pub fn available_pct(&self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        (self.available_bytes as f64 / self.total_bytes as f64) * 100.0
    }

    /// Le guest est-il sous pression mémoire ?
    pub fn is_under_pressure(&self, free_threshold_pct: f64) -> bool {
        self.free_pct() < free_threshold_pct
    }
}

/// Client QMP pour un VMID Proxmox donné.
pub struct QmpClient {
    vmid: u32,
    sock_path: PathBuf,
}

impl QmpClient {
    /// Construit un client QMP pour la socket Proxmox standard.
    pub fn for_vm(vmid: u32, qmp_dir: &str) -> Self {
        Self {
            vmid,
            sock_path: PathBuf::from(format!("{}/{}.qmp", qmp_dir, vmid)),
        }
    }

    /// Interroge les statistiques balloon du guest.
    ///
    /// Retourne `None` si la socket n'existe pas ou si le balloon driver
    /// n'est pas actif dans le guest.
    pub fn query_stats(&self) -> Result<Option<BalloonStats>> {
        if !self.sock_path.exists() {
            trace!(
                vmid = self.vmid,
                "socket QMP absente — balloon non disponible"
            );
            return Ok(None);
        }

        let mut stream = UnixStream::connect(&self.sock_path)
            .with_context(|| format!("connexion QMP vmid={}", self.vmid))?;

        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;

        let mut reader = BufReader::new(stream.try_clone()?);

        // ── 1. Lire le greeting QMP ────────────────────────────────────────
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        trace!(vmid = self.vmid, greeting = %greeting.trim(), "QMP greeting");

        // ── 2. Activer les capacités QMP ──────────────────────────────────
        self.send_json(&mut stream, &json!({"execute": "qmp_capabilities"}))?;
        let caps_resp = self.read_response(&mut reader)?;
        if caps_resp.get("error").is_some() {
            bail!("qmp_capabilities échoué : {:?}", caps_resp);
        }

        // ── 3. Interroger l'état du balloon ───────────────────────────────
        self.send_json(&mut stream, &json!({"execute": "query-balloon"}))?;
        let balloon_resp = self.read_response(&mut reader)?;

        // Si error → balloon non disponible (driver non chargé dans le guest)
        if balloon_resp.get("error").is_some() {
            debug!(vmid = self.vmid, "balloon driver absent dans ce guest");
            return Ok(None);
        }

        let actual_bytes = balloon_resp["return"]["actual"].as_u64().unwrap_or(0);

        // ── 4. Interroger les stats guest via QOM ─────────────────────────
        self.send_json(
            &mut stream,
            &json!({
                "execute": "qom-get",
                "arguments": {
                    "path": "/machine/peripheral/balloon0",
                    "property": "guest-stats"
                }
            }),
        )?;
        let stats_resp = self.read_response(&mut reader)?;

        if stats_resp.get("error").is_some() {
            // Stats non disponibles (poll interval pas encore configuré)
            // On retourne ce qu'on a
            return Ok(Some(BalloonStats {
                actual_bytes,
                ..Default::default()
            }));
        }

        let gs = &stats_resp["return"]["stats"];

        let stats = BalloonStats {
            free_bytes: self.parse_stat(gs, "stat-free-memory"),
            available_bytes: self.parse_stat(gs, "stat-available-memory"),
            total_bytes: self.parse_stat(gs, "stat-total-memory"),
            major_faults: self.parse_stat(gs, "stat-major-faults"),
            actual_bytes,
        };

        debug!(
            vmid = self.vmid,
            free_pct = format!("{:.1}%", stats.free_pct()),
            total_mib = stats.total_bytes / 1024 / 1024,
            actual_mib = stats.actual_bytes / 1024 / 1024,
            major_faults = stats.major_faults,
            "balloon stats lues"
        );

        Ok(Some(stats))
    }

    /// Configure la fréquence de rapport des stats balloon.
    ///
    /// Doit être appelé au moins une fois avant de lire les stats.
    /// `poll_interval_secs` = 0 désactive le polling.
    pub fn set_stats_polling(&self, poll_interval_secs: u32) -> Result<()> {
        if !self.sock_path.exists() {
            return Ok(());
        }

        let mut stream = UnixStream::connect(&self.sock_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut reader = BufReader::new(stream.try_clone()?);

        // Greeting
        let mut line = String::new();
        reader.read_line(&mut line)?;

        // Capabilities
        self.send_json(&mut stream, &json!({"execute": "qmp_capabilities"}))?;
        let _ = self.read_response(&mut reader)?;

        // Configurer l'intervalle de polling
        self.send_json(
            &mut stream,
            &json!({
                "execute": "qom-set",
                "arguments": {
                    "path": "/machine/peripheral/balloon0",
                    "property": "guest-stats-polling-interval",
                    "value": poll_interval_secs
                }
            }),
        )?;
        let resp = self.read_response(&mut reader)?;

        if resp.get("error").is_some() {
            warn!(
                vmid = self.vmid,
                "impossible de configurer le polling balloon : {:?}", resp
            );
        } else {
            info!(
                vmid = self.vmid,
                poll_interval_secs, "polling balloon configuré"
            );
        }

        Ok(())
    }

    /// Demande à QEMU de modifier la taille visible du balloon.
    ///
    /// `target_bytes` = RAM que le guest doit voir après l'opération.
    /// - Inflation : `target_bytes < actual_bytes` → le balloon retire des pages au guest
    /// - Déflation : `target_bytes > actual_bytes` → le balloon rend des pages au guest
    pub fn set_balloon_target(&self, target_bytes: u64) -> Result<()> {
        if !self.sock_path.exists() {
            return Ok(());
        }

        let mut stream = UnixStream::connect(&self.sock_path)
            .with_context(|| format!("connexion QMP vmid={}", self.vmid))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut reader = BufReader::new(stream.try_clone()?);

        let mut line = String::new();
        reader.read_line(&mut line)?;

        self.send_json(&mut stream, &json!({"execute": "qmp_capabilities"}))?;
        let _ = self.read_response(&mut reader)?;

        self.send_json(
            &mut stream,
            &json!({
                "execute": "balloon",
                "arguments": {"value": target_bytes}
            }),
        )?;
        let resp = self.read_response(&mut reader)?;

        if let Some(err) = resp.get("error") {
            bail!("set_balloon_target vmid={}: {:?}", self.vmid, err);
        }

        info!(
            vmid = self.vmid,
            target_mib = target_bytes / 1024 / 1024,
            "balloon target mis à jour"
        );
        Ok(())
    }

    // ─── Helpers ──────────────────────────────────────────────────────────

    fn send_json(&self, stream: &mut UnixStream, cmd: &Value) -> Result<()> {
        let mut payload = cmd.to_string();
        payload.push('\n');
        stream.write_all(payload.as_bytes())?;
        Ok(())
    }

    fn read_response(&self, reader: &mut BufReader<UnixStream>) -> Result<Value> {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        // Ignorer les événements async (lignes commençant par {"event":...})
        let parsed: Value = serde_json::from_str(line.trim())?;
        if parsed.get("event").is_some() {
            // Relire la prochaine ligne
            line.clear();
            reader.read_line(&mut line)?;
            return Ok(serde_json::from_str(line.trim())?);
        }
        Ok(parsed)
    }

    fn parse_stat(&self, gs: &Value, key: &str) -> u64 {
        gs[key].as_u64().unwrap_or(0)
    }
}

// ─── Monitor balloon (tâche de fond) ─────────────────────────────────────────

/// Moniteur balloon — tourne en tâche Tokio, interroge les stats périodiquement.
pub struct BalloonMonitor {
    qmp_dir: String,
    poll_interval: Duration,
    /// Seuil de RAM libre en-dessous duquel on considère le guest sous pression (%)
    free_threshold_pct: f64,
    /// Callback déclenché quand un guest est sous pression
    /// Arguments : vmid, BalloonStats
    on_pressure: Arc<dyn Fn(u32, BalloonStats) + Send + Sync>,
    /// Callback déclenché à chaque échantillon valide
    on_sample: Option<Arc<dyn Fn(u32, BalloonStats) + Send + Sync>>,
}

use std::sync::Arc;

impl BalloonMonitor {
    pub fn new(
        qmp_dir: String,
        poll_interval_secs: u64,
        free_threshold_pct: f64,
        on_pressure: impl Fn(u32, BalloonStats) + Send + Sync + 'static,
    ) -> Self {
        Self {
            qmp_dir,
            poll_interval: Duration::from_secs(poll_interval_secs),
            free_threshold_pct,
            on_pressure: Arc::new(on_pressure),
            on_sample: None,
        }
    }

    pub fn with_sample_hook(
        mut self,
        on_sample: impl Fn(u32, BalloonStats) + Send + Sync + 'static,
    ) -> Self {
        self.on_sample = Some(Arc::new(on_sample));
        self
    }

    /// Boucle de surveillance — à lancer dans une tâche Tokio via `tokio::task::spawn_blocking`.
    pub fn run_blocking(self, vmids: Vec<u32>) {
        info!(
            vms = vmids.len(),
            threshold_pct = self.free_threshold_pct,
            interval_secs = self.poll_interval.as_secs(),
            "BalloonMonitor démarré"
        );

        // Configuration initiale du polling QMP sur chaque VM
        for &vmid in &vmids {
            let client = QmpClient::for_vm(vmid, &self.qmp_dir);
            if let Err(e) = client.set_stats_polling(self.poll_interval.as_secs() as u32) {
                debug!(vmid, error = %e, "configuration polling balloon ignorée");
            }
        }

        loop {
            std::thread::sleep(self.poll_interval);

            for &vmid in &vmids {
                let client = QmpClient::for_vm(vmid, &self.qmp_dir);
                match client.query_stats() {
                    Ok(Some(stats)) => {
                        if let Some(ref hook) = self.on_sample {
                            hook(vmid, stats.clone());
                        }
                        if stats.is_under_pressure(self.free_threshold_pct) {
                            info!(
                                vmid,
                                free_pct = format!("{:.1}%", stats.free_pct()),
                                threshold = format!("{:.1}%", self.free_threshold_pct),
                                "pression balloon détectée → augmentation taux d'éviction"
                            );
                            (self.on_pressure)(vmid, stats);
                        } else {
                            trace!(
                                vmid,
                                free_pct = format!("{:.1}%", stats.free_pct()),
                                "balloon ok"
                            );
                        }
                    }
                    Ok(None) => trace!(vmid, "balloon non disponible"),
                    Err(e) => debug!(vmid, error = %e, "erreur lecture balloon"),
                }
            }
        }
    }
}

// ─── Gestionnaire proactif de balloon (Scenario S5) ───────────────────────────

/// Gestionnaire proactif balloon — gonfle pour récupérer la RAM inutilisée des guests
/// et dégonfle quand un guest est sous pression.
///
/// Contrairement à `BalloonMonitor` (observateur passif), `BalloonManager` envoie
/// des commandes QMP `balloon` pour ajuster la RAM visible des guests.
pub struct BalloonManager {
    qmp_dir: String,
    interval_secs: u64,
    /// Gonfler si RAM disponible guest > X% de sa RAM actuelle
    inflate_threshold_pct: f64,
    /// Dégonfler si RAM disponible guest < X% de sa RAM actuelle
    deflate_threshold_pct: f64,
    /// Ne jamais donner moins de X% de max_mem au guest (plancher absolu)
    min_guest_pct: f64,
    /// Bande morte : ignorer les transitions inflate↔deflate inférieures à cette durée (secondes).
    /// Évite le ping-pong quand un guest oscille autour du seuil.
    hysteresis_secs: u64,
}

impl BalloonManager {
    pub fn new(qmp_dir: String, interval_secs: u64) -> Self {
        Self {
            qmp_dir,
            interval_secs,
            inflate_threshold_pct: 20.0,
            deflate_threshold_pct: 10.0,
            min_guest_pct: 50.0,
            hysteresis_secs: 60,
        }
    }

    /// Évalue et applique une action balloon pour une VM.
    ///
    /// Retourne le nouveau `actual` en MiB si une commande QMP a été envoyée.
    pub fn reconcile_vm(&self, vmid: u32, stats: &BalloonStats, max_mem_mib: u64) -> Option<u64> {
        if stats.actual_bytes == 0 || max_mem_mib == 0 {
            return None;
        }

        let max_mem_bytes = max_mem_mib * 1024 * 1024;
        // RAM minimale garantie au guest (50% de max_mem par défaut)
        let floor_bytes = (max_mem_bytes as f64 * self.min_guest_pct / 100.0) as u64;
        // Seuil minimal de changement : 64 MiB (évite les micro-ajustements)
        let min_delta = 64 * 1024 * 1024_u64;

        let available_pct = stats.available_bytes as f64 / stats.actual_bytes as f64 * 100.0;

        let client = QmpClient::for_vm(vmid, &self.qmp_dir);

        if available_pct > self.inflate_threshold_pct
            && stats.actual_bytes > floor_bytes + min_delta
        {
            // Guest a de la RAM inutilisée → gonfler le balloon pour en récupérer la moitié
            let reclaimable = stats.available_bytes / 2;
            let new_target = stats
                .actual_bytes
                .saturating_sub(reclaimable)
                .max(floor_bytes);

            if stats.actual_bytes.saturating_sub(new_target) >= min_delta {
                info!(
                    vmid,
                    available_pct = format!("{:.1}%", available_pct),
                    current_mib = stats.actual_bytes / 1024 / 1024,
                    new_target_mib = new_target / 1024 / 1024,
                    "BALLOON inflate — récupération RAM inutilisée"
                );
                if let Err(e) = client.set_balloon_target(new_target) {
                    warn!(vmid, error = %e, "inflation balloon échouée");
                    return None;
                }
                return Some(new_target / 1024 / 1024);
            }
        } else if available_pct < self.deflate_threshold_pct {
            // Guest sous pression → dégonfler pour lui rendre de la RAM
            let inflated = max_mem_bytes.saturating_sub(stats.actual_bytes);
            if inflated >= min_delta {
                // Rendre au minimum 64 MiB, au maximum la moitié de ce qu'on a pris
                let give_back = (inflated / 2).max(min_delta);
                let new_target = (stats.actual_bytes + give_back).min(max_mem_bytes);

                info!(
                    vmid,
                    available_pct = format!("{:.1}%", available_pct),
                    current_mib = stats.actual_bytes / 1024 / 1024,
                    new_target_mib = new_target / 1024 / 1024,
                    "BALLOON deflate — restitution RAM au guest sous pression"
                );
                if let Err(e) = client.set_balloon_target(new_target) {
                    warn!(vmid, error = %e, "déflation balloon échouée");
                    return None;
                }
                return Some(new_target / 1024 / 1024);
            }
        }

        None
    }

    /// Boucle proactive — à lancer dans un thread dédié via `tokio::task::spawn_blocking`.
    ///
    /// - `vmids_fn` : closure appelée à chaque cycle, retourne `(vmid, max_mem_mib)` pour chaque VM locale
    /// - `on_adjust` : callback appelé après chaque ajustement réussi avec `(vmid, new_actual_mib)`
    pub fn run_blocking<F, G>(self, vmids_fn: F, on_adjust: G)
    where
        F: Fn() -> Vec<(u32, u64)>,
        G: Fn(u32, u64),
    {
        use std::collections::HashMap;
        use std::time::Instant;

        info!(
            interval_secs = self.interval_secs,
            inflate_threshold_pct = self.inflate_threshold_pct,
            deflate_threshold_pct = self.deflate_threshold_pct,
            min_guest_pct = self.min_guest_pct,
            hysteresis_secs = self.hysteresis_secs,
            "BalloonManager démarré"
        );

        // Horodatage du dernier ajustement par vmid — sert à l'hysteresis
        let mut last_adjust: HashMap<u32, Instant> = HashMap::new();

        loop {
            std::thread::sleep(Duration::from_secs(self.interval_secs));

            for (vmid, max_mem_mib) in vmids_fn() {
                // Respecter l'hysteresis : ne pas ré-ajuster trop vite
                if let Some(&t) = last_adjust.get(&vmid) {
                    if t.elapsed().as_secs() < self.hysteresis_secs {
                        trace!(vmid, "BalloonManager : hysteresis actif, skip");
                        continue;
                    }
                }

                let client = QmpClient::for_vm(vmid, &self.qmp_dir);
                match client.query_stats() {
                    Ok(Some(stats)) => {
                        if let Some(new_actual_mib) = self.reconcile_vm(vmid, &stats, max_mem_mib) {
                            last_adjust.insert(vmid, Instant::now());
                            on_adjust(vmid, new_actual_mib);
                        }
                    }
                    Ok(None) => trace!(vmid, "balloon non disponible (manager)"),
                    Err(e) => debug!(vmid, error = %e, "erreur lecture balloon (manager)"),
                }
            }
        }
    }
}
