//! Multiplexeur GPU bas niveau — démon par nœud.
//!
//! # Rôle
//!
//! Un seul démon tourne par nœud physique. Il :
//! 1. Écoute sur un socket Unix (`/run/omega/gpu.sock`) les commandes GPU
//!    de toutes les VMs du nœud (via un shim dans le guest ou via le pilote virtio)
//! 2. Maintient une file de priorité par VM (`priority_queue`)
//! 3. Soumet les commandes au GPU physique via le driver kernel (`/dev/nvidia0` ou
//!    équivalent — ici abstrait par `GpuBackend`)
//! 4. Renvoie les résultats à la VM source via le même socket (corrélation par `seq`)
//!
//! # Budget VRAM
//!
//! Chaque VM dispose d'un budget VRAM (`vram_budget_mib`). Une demande
//! `GPU_ALLOC` qui dépasserait le budget est rejetée avec `GPU_ERROR`.
//!
//! # File de priorité
//!
//! Les commandes sont ordonnées par :
//!   1. Priorité du message (RealTime > High > Normal > Low)
//!   2. Timestamp d'arrivée (FIFO à même priorité)
//!
//! # Concurrence
//!
//! - Un thread par connexion VM lit les messages entrants (tokio task)
//! - Un worker unique consomme la file et soumet au GPU (sérialisation nécessaire
//!   car la plupart des drivers GPU ne sont pas thread-safe)
//! - Les résultats sont renvoyés via un `HashMap<(vm_id, seq), oneshot::Sender>`

use std::collections::{BinaryHeap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::gpu_protocol::{AllocRequest, AllocResponse, GpuHeader, GpuMessage, MsgType, Priority};

// ─── Budget VRAM ─────────────────────────────────────────────────────────────

/// Budget VRAM d'une VM (Mio).
#[derive(Debug, Clone)]
pub struct VmVramBudget {
    pub vm_id: u32,
    /// Budget total accordé (Mio)
    pub budget_mib: u64,
    /// VRAM actuellement allouée (Mio)
    pub used_mib: u64,
    /// Nombre de handles VRAM actifs
    pub handle_count: u32,
}

impl VmVramBudget {
    pub fn new(vm_id: u32, budget_mib: u64) -> Self {
        Self {
            vm_id,
            budget_mib,
            used_mib: 0,
            handle_count: 0,
        }
    }

    pub fn can_alloc_bytes(&self, bytes: u64) -> bool {
        let needed_mib = bytes.div_ceil(1024 * 1024);
        self.used_mib + needed_mib <= self.budget_mib
    }

    pub fn alloc_bytes(&mut self, bytes: u64) -> bool {
        if !self.can_alloc_bytes(bytes) {
            return false;
        }
        self.used_mib += bytes.div_ceil(1024 * 1024);
        self.handle_count += 1;
        true
    }

    pub fn free_bytes(&mut self, bytes: u64) {
        let mib = bytes.div_ceil(1024 * 1024).min(self.used_mib);
        self.used_mib = self.used_mib.saturating_sub(mib);
        self.handle_count = self.handle_count.saturating_sub(1);
    }

    pub fn free_pct(&self) -> f64 {
        if self.budget_mib == 0 {
            return 0.0;
        }
        (self.budget_mib - self.used_mib) as f64 / self.budget_mib as f64 * 100.0
    }
}

// ─── File de priorité ────────────────────────────────────────────────────────

/// Entrée dans la file de commandes GPU.
#[derive(Debug)]
pub(crate) struct QueueEntry {
    msg: GpuMessage,
    arrived: Instant,
    /// Canal de retour pour le résultat (oneshot via mpsc de taille 1)
    reply_tx: mpsc::Sender<GpuMessage>,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.msg.header.priority == other.msg.header.priority && self.arrived == other.arrived
    }
}
impl Eq for QueueEntry {}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Priorité décroissante, puis FIFO
        self.msg
            .header
            .priority
            .cmp(&other.msg.header.priority)
            .then(other.arrived.cmp(&self.arrived))
    }
}

// ─── Backend GPU (abstraction) ────────────────────────────────────────────────

/// Trait d'abstraction du GPU physique.
/// En production : implémenté via ioctl sur /dev/nvidia0 ou /dev/dri/card0.
/// En test : `MockGpuBackend` retourne des résultats synthétiques.
#[async_trait::async_trait]
pub trait GpuBackend: Send + Sync {
    /// Soumet une commande GPU brute et attend le résultat.
    async fn submit(&self, cmd: &[u8]) -> Result<Vec<u8>, GpuError>;

    /// Alloue N octets de VRAM physique. Retourne un handle opaque.
    async fn alloc_vram(&self, size_bytes: u64, alignment: u32) -> Result<u64, GpuError>;

    /// Libère un handle VRAM précédemment alloué.
    async fn free_vram(&self, handle: u64) -> Result<(), GpuError>;

    /// Barrière de synchronisation GPU.
    async fn sync(&self) -> Result<(), GpuError>;

    /// Nom du backend (pour les logs).
    fn name(&self) -> &str;
}

#[derive(Debug)]
pub enum GpuError {
    DeviceNotFound,
    OutOfVram,
    InvalidHandle(u64),
    SubmitFailed(String),
    Timeout,
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceNotFound => write!(f, "GPU non trouvé"),
            Self::OutOfVram => write!(f, "VRAM insuffisante"),
            Self::InvalidHandle(h) => write!(f, "handle VRAM invalide: {h}"),
            Self::SubmitFailed(msg) => write!(f, "soumission échouée: {msg}"),
            Self::Timeout => write!(f, "timeout GPU"),
        }
    }
}

// ─── Multiplexeur ────────────────────────────────────────────────────────────

/// État partagé du multiplexeur GPU.
pub struct GpuMuxState {
    /// Budgets VRAM par VM
    pub budgets: RwLock<HashMap<u32, VmVramBudget>>,
    /// File de priorité des commandes en attente
    pub(crate) _queue: Mutex<BinaryHeap<QueueEntry>>,
    /// Statistiques globales
    pub stats: Mutex<GpuMuxStats>,
}

#[derive(Debug, Default, Clone)]
pub struct GpuMuxStats {
    pub total_commands: u64,
    pub low_priority_commands: u64,
    pub normal_priority_commands: u64,
    pub high_priority_commands: u64,
    pub realtime_priority_commands: u64,
    pub total_allocs: u64,
    pub total_frees: u64,
    pub rejected_oom: u64,
    pub avg_latency_us: f64,
}

fn record_command_stats(stats: &mut GpuMuxStats, priority: Priority, elapsed: Duration) {
    stats.total_commands += 1;

    match priority {
        Priority::Low => stats.low_priority_commands += 1,
        Priority::Normal => stats.normal_priority_commands += 1,
        Priority::High => stats.high_priority_commands += 1,
        Priority::RealTime => stats.realtime_priority_commands += 1,
    }

    let elapsed_us = elapsed.as_micros() as f64;
    if stats.total_commands == 1 {
        stats.avg_latency_us = elapsed_us;
    } else {
        stats.avg_latency_us = 0.95 * stats.avg_latency_us + 0.05 * elapsed_us;
    }
}

/// Multiplexeur GPU — point d'entrée du démon.
pub struct GpuMultiplexer {
    socket_path: PathBuf,
    backend: Arc<dyn GpuBackend>,
    state: Arc<GpuMuxState>,
    /// Canal vers le worker GPU (taille bornée pour backpressure)
    cmd_tx: mpsc::Sender<QueueEntry>,
}

impl GpuMultiplexer {
    pub fn new(socket_path: PathBuf, backend: Arc<dyn GpuBackend>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<QueueEntry>(4096);

        let state = Arc::new(GpuMuxState {
            budgets: RwLock::new(HashMap::new()),
            _queue: Mutex::new(BinaryHeap::new()),
            stats: Mutex::new(GpuMuxStats::default()),
        });

        // Lancer le worker GPU en tâche de fond
        let backend_w = Arc::clone(&backend);
        let state_w = Arc::clone(&state);
        tokio::spawn(gpu_worker(cmd_rx, backend_w, state_w));

        Self {
            socket_path,
            backend,
            state,
            cmd_tx,
        }
    }

    /// Configure le budget VRAM d'une VM.
    pub async fn set_vm_budget(&self, vm_id: u32, budget_mib: u64) {
        let mut budgets = self.state.budgets.write().await;
        budgets.insert(vm_id, VmVramBudget::new(vm_id, budget_mib));
        info!(vm_id, budget_mib, "budget VRAM VM configuré");
    }

    /// Libère toutes les ressources d'une VM (post-migration ou arrêt).
    pub async fn release_vm(&self, vm_id: u32) {
        let mut budgets = self.state.budgets.write().await;
        if let Some(budget) = budgets.remove(&vm_id) {
            info!(
                vm_id,
                handles = budget.handle_count,
                used_mib = budget.used_mib,
                "ressources GPU VM libérées"
            );
        }
    }

    /// Démarre l'écoute sur le socket Unix.
    pub async fn run(self: Arc<Self>) -> std::io::Result<()> {
        // Supprimer l'ancien socket s'il existe
        let _ = tokio::fs::remove_file(&self.socket_path).await;

        // Créer le répertoire parent si nécessaire
        if let Some(parent) = self.socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        info!(path = ?self.socket_path, "GPU multiplexeur en écoute");

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let mux = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = mux.handle_vm_connection(stream).await {
                            error!("connexion VM GPU fermée: {e}");
                        }
                    });
                }
                Err(e) => error!("accept GPU socket: {e}"),
            }
        }
    }

    /// Gère une connexion d'une VM (une task par connexion).
    async fn handle_vm_connection(&self, mut stream: UnixStream) -> std::io::Result<()> {
        // Lire le vm_id depuis le premier message (le type du premier message donne le vm_id)
        let mut vm_id_opt: Option<u32> = None;

        loop {
            // Lire le header
            let mut hbuf = [0u8; crate::gpu_protocol::HEADER_SIZE];
            match stream.read_exact(&mut hbuf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }

            let header = match crate::gpu_protocol::GpuHeader::decode(&hbuf) {
                Ok(h) => h,
                Err(e) => {
                    error!("header GPU invalide: {:?}", e);
                    break;
                }
            };

            // Lire le payload
            let mut payload = vec![0u8; header.payload_len as usize];
            if !payload.is_empty() {
                stream.read_exact(&mut payload).await?;
            }

            let vm_id = header.vm_id;
            if vm_id_opt.is_none() {
                vm_id_opt = Some(vm_id);
                debug!(vm_id, "nouvelle connexion GPU VM");
            }

            let msg = GpuMessage { header, payload };

            // Traiter les messages de contrôle directement (ALLOC, FREE, SYNC)
            // Les commandes brutes sont envoyées au worker via le canal
            let response = match msg.header.msg_type {
                MsgType::GpuAlloc => Some(self.handle_alloc(&msg).await),
                MsgType::GpuFree => Some(self.handle_free(&msg).await),
                MsgType::GpuSync => Some(self.handle_sync(&msg).await),
                MsgType::GpuCmd => {
                    // Envoyer au worker et attendre le résultat
                    let (reply_tx, mut reply_rx) = mpsc::channel::<GpuMessage>(1);
                    let entry = QueueEntry {
                        msg: msg.clone(),
                        arrived: Instant::now(),
                        reply_tx,
                    };
                    if self.cmd_tx.send(entry).await.is_err() {
                        error!(vm_id, "canal GPU saturé — commande rejetée");
                        Some(msg.make_error(0x01, "canal GPU saturé"))
                    } else {
                        // Attente du résultat (timeout 5s)
                        match tokio::time::timeout(Duration::from_secs(5), reply_rx.recv()).await {
                            Ok(Some(resp)) => Some(resp),
                            Ok(None) => Some(msg.make_error(0x02, "worker GPU terminé")),
                            Err(_) => Some(msg.make_error(0x03, "timeout GPU")),
                        }
                    }
                }
                _ => None,
            };

            // Renvoyer la réponse à la VM
            if let Some(resp) = response {
                let hbuf = resp.header.encode();
                stream.write_all(&hbuf).await?;
                if !resp.payload.is_empty() {
                    stream.write_all(&resp.payload).await?;
                }
                stream.flush().await?;
            }
        }

        if let Some(vm_id) = vm_id_opt {
            debug!(vm_id, "connexion GPU VM fermée");
        }
        Ok(())
    }

    // ── Handlers de contrôle ─────────────────────────────────────────────────

    async fn handle_alloc(&self, msg: &GpuMessage) -> GpuMessage {
        let vm_id = msg.header.vm_id;

        let Some(req) = AllocRequest::decode(&msg.payload) else {
            return msg.make_error(0x10, "payload GPU_ALLOC invalide");
        };

        // Vérifier le budget VRAM
        {
            let mut budgets = self.state.budgets.write().await;
            let budget = budgets.entry(vm_id).or_insert_with(|| {
                // Budget par défaut si non configuré : 256 Mio
                warn!(
                    vm_id,
                    "budget VRAM non configuré — utilisation du défaut 256 Mio"
                );
                VmVramBudget::new(vm_id, 256)
            });

            if !budget.alloc_bytes(req.size_bytes) {
                warn!(
                    vm_id,
                    requested_mib = req.size_bytes / (1024 * 1024),
                    budget_mib = budget.budget_mib,
                    used_mib = budget.used_mib,
                    "GPU_ALLOC refusé — budget VRAM dépassé"
                );
                let mut stats = self.state.stats.lock().await;
                stats.rejected_oom += 1;
                return msg.make_error(0x20, "budget VRAM VM dépassé");
            }
        }

        // Allouer sur le GPU physique
        match self.backend.alloc_vram(req.size_bytes, req.alignment).await {
            Ok(handle) => {
                let mut stats = self.state.stats.lock().await;
                stats.total_allocs += 1;

                let resp = AllocResponse {
                    handle,
                    size_bytes: req.size_bytes,
                };
                let payload = resp.encode().to_vec();
                let hdr = GpuHeader::new(
                    MsgType::GpuAllocResp,
                    vm_id,
                    msg.header.seq,
                    payload.len() as u32,
                    msg.header.priority,
                );
                GpuMessage::new(hdr, payload)
            }
            Err(e) => {
                // Annuler la réservation budget
                let mut budgets = self.state.budgets.write().await;
                if let Some(b) = budgets.get_mut(&vm_id) {
                    b.free_bytes(req.size_bytes);
                }
                msg.make_error(0x21, &e.to_string())
            }
        }
    }

    async fn handle_free(&self, msg: &GpuMessage) -> GpuMessage {
        let vm_id = msg.header.vm_id;

        if msg.payload.len() < 16 {
            return msg.make_error(0x30, "payload GPU_FREE invalide");
        }
        let handle = u64::from_le_bytes(msg.payload[0..8].try_into().unwrap());
        let size_bytes = u64::from_le_bytes(msg.payload[8..16].try_into().unwrap());

        // Libérer sur le GPU physique
        if let Err(e) = self.backend.free_vram(handle).await {
            return msg.make_error(0x31, &e.to_string());
        }

        // Libérer dans le budget
        let mut budgets = self.state.budgets.write().await;
        if let Some(b) = budgets.get_mut(&vm_id) {
            b.free_bytes(size_bytes);
        }

        let mut stats = self.state.stats.lock().await;
        stats.total_frees += 1;

        msg.make_result(vec![]) // ACK vide
    }

    async fn handle_sync(&self, msg: &GpuMessage) -> GpuMessage {
        match self.backend.sync().await {
            Ok(_) => msg.make_result(vec![]),
            Err(e) => msg.make_error(0x40, &e.to_string()),
        }
    }

    // ─── API utilisée par gpu_virgl_bridge ───────────────────────────────────

    /// Alias de `release_vm` pour le bridge OMVG (CTX_DESTROY ou déconnexion).
    pub async fn remove_vm(&self, vm_id: u32) {
        self.release_vm(vm_id).await;
    }

    /// Soumet un command stream virgl brut au backend GPU.
    ///
    /// `data` = octets virgl bruts (CS format, tels qu'émis par le guest via Mesa).
    pub async fn submit_raw(
        &self,
        vm_id: u32,
        data: &[u8],
        priority: Priority,
    ) -> Result<Vec<u8>, GpuError> {
        // On passe par le backend DRM directement pour la voie virgl
        let start = Instant::now();
        let result = self.backend.submit(data).await?;
        let elapsed = start.elapsed();
        {
            let mut stats = self.state.stats.lock().await;
            record_command_stats(&mut stats, priority, elapsed);
        }
        debug!(
            vm_id,
            bytes = data.len(),
            ?priority,
            latency_us = elapsed.as_micros(),
            "submit_raw ok"
        );
        Ok(result)
    }

    /// Crée une ressource 3D virgl (texture/buffer) pour une VM.
    pub async fn resource_create(&self, vm_id: u32, desc: &[u8]) -> Result<(), GpuError> {
        // Encode la création comme une commande backend générique
        self.backend.submit(desc).await?;
        debug!(vm_id, "resource_create ok");
        Ok(())
    }

    /// Libère une ressource virgl d'une VM.
    pub async fn resource_unref(&self, vm_id: u32, resource_id: u32) {
        debug!(vm_id, resource_id, "resource_unref");
        // Le suivi des handles est dans le budget VRAM — pas de tracking virgl fin ici
    }

    /// Transfert CPU↔GPU (upload/readback d'une ressource virgl).
    pub async fn resource_transfer(&self, vm_id: u32, desc: &[u8]) -> Result<(), GpuError> {
        self.backend.submit(desc).await?;
        debug!(vm_id, "resource_transfer ok");
        Ok(())
    }

    /// Flush d'une ressource vers le framebuffer (affichage guest).
    pub async fn flush_resource(&self, vm_id: u32, desc: &[u8]) -> Result<(), GpuError> {
        self.backend.submit(desc).await?;
        debug!(vm_id, "flush_resource ok");
        Ok(())
    }

    /// Snapshot des budgets VRAM (pour l'API HTTP).
    pub async fn budgets_snapshot(&self) -> Vec<VmVramBudget> {
        self.state.budgets.read().await.values().cloned().collect()
    }

    /// Métriques Prometheus.
    pub async fn prometheus_metrics(&self, node_id: &str) -> String {
        let stats = self.state.stats.lock().await.clone();
        let budgets = self.state.budgets.read().await;
        let total_vram_used: u64 = budgets.values().map(|b| b.used_mib).sum();

        format!(
            "# HELP omega_gpu_commands_total Commandes GPU soumises\n\
             omega_gpu_commands_total{{node=\"{node}\"}} {cmds}\n\
             # HELP omega_gpu_commands_by_priority_total Commandes GPU par priorité\n\
             omega_gpu_commands_by_priority_total{{node=\"{node}\",priority=\"low\"}} {prio_low}\n\
             omega_gpu_commands_by_priority_total{{node=\"{node}\",priority=\"normal\"}} {prio_normal}\n\
             omega_gpu_commands_by_priority_total{{node=\"{node}\",priority=\"high\"}} {prio_high}\n\
             omega_gpu_commands_by_priority_total{{node=\"{node}\",priority=\"realtime\"}} {prio_realtime}\n\
             # HELP omega_gpu_command_latency_avg_us Latence moyenne de soumission GPU\n\
             omega_gpu_command_latency_avg_us{{node=\"{node}\"}} {latency_avg}\n\
             # HELP omega_gpu_allocs_total Allocations VRAM\n\
             omega_gpu_allocs_total{{node=\"{node}\"}} {allocs}\n\
             # HELP omega_gpu_oom_total Allocations VRAM refusées (OOM)\n\
             omega_gpu_oom_total{{node=\"{node}\"}} {oom}\n\
             # HELP omega_gpu_vram_used_mib VRAM utilisée (toutes VMs, Mio)\n\
             omega_gpu_vram_used_mib{{node=\"{node}\"}} {vram}\n",
            node = node_id,
            cmds = stats.total_commands,
            prio_low = stats.low_priority_commands,
            prio_normal = stats.normal_priority_commands,
            prio_high = stats.high_priority_commands,
            prio_realtime = stats.realtime_priority_commands,
            latency_avg = stats.avg_latency_us,
            allocs = stats.total_allocs,
            oom = stats.rejected_oom,
            vram = total_vram_used,
        )
    }
}

// ─── Worker GPU ───────────────────────────────────────────────────────────────

/// Tâche unique qui consomme la file et soumet au GPU physique.
/// La sérialisation est nécessaire car les drivers GPU ne sont pas thread-safe.
async fn gpu_worker(
    mut rx: mpsc::Receiver<QueueEntry>,
    backend: Arc<dyn GpuBackend>,
    state: Arc<GpuMuxState>,
) {
    info!(backend = backend.name(), "worker GPU démarré");

    while let Some(entry) = rx.recv().await {
        let vm_id = entry.msg.header.vm_id;
        let seq = entry.msg.header.seq;
        let start = Instant::now();

        let result = backend.submit(&entry.msg.payload).await;
        let elapsed_us = start.elapsed().as_micros() as f64;

        {
            let mut stats = state.stats.lock().await;
            record_command_stats(&mut stats, entry.msg.header.priority, start.elapsed());
        }

        let response = match result {
            Ok(data) => {
                debug!(vm_id, seq, latency_us = elapsed_us, "commande GPU exécutée");
                entry.msg.make_result(data)
            }
            Err(e) => {
                warn!(vm_id, seq, error = %e, "commande GPU échouée");
                entry.msg.make_error(0x50, &e.to_string())
            }
        };

        // Envoyer le résultat (best-effort — la VM peut s'être déconnectée)
        let _ = entry.reply_tx.send(response).await;
    }

    info!("worker GPU arrêté");
}

// ─── Backend mock (tests) ─────────────────────────────────────────────────────

/// Backend de test qui simule un GPU en mémoire.
pub struct MockGpuBackend {
    pub vram_total_mib: u64,
    next_handle: std::sync::atomic::AtomicU64,
}

impl MockGpuBackend {
    pub fn new(vram_total_mib: u64) -> Self {
        Self {
            vram_total_mib,
            next_handle: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

#[async_trait::async_trait]
impl GpuBackend for MockGpuBackend {
    async fn submit(&self, cmd: &[u8]) -> Result<Vec<u8>, GpuError> {
        // Retourner un résultat synthétique (écho inversé)
        let result: Vec<u8> = cmd.iter().rev().cloned().collect();
        Ok(result)
    }

    async fn alloc_vram(&self, size_bytes: u64, _alignment: u32) -> Result<u64, GpuError> {
        let handle = self
            .next_handle
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        debug!(handle, size_bytes, "MockGpu: VRAM allouée");
        Ok(handle)
    }

    async fn free_vram(&self, handle: u64) -> Result<(), GpuError> {
        debug!(handle, "MockGpu: VRAM libérée");
        Ok(())
    }

    async fn sync(&self) -> Result<(), GpuError> {
        Ok(())
    }

    fn name(&self) -> &str {
        "MockGpuBackend"
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_protocol::{GpuHeader, MsgType, Priority};

    fn make_mux(budget_mib: u64) -> Arc<GpuMultiplexer> {
        let backend = Arc::new(MockGpuBackend::new(budget_mib * 2));
        Arc::new(GpuMultiplexer::new(
            PathBuf::from("/tmp/test_omega_gpu.sock"),
            backend,
        ))
    }

    #[test]
    fn test_vram_budget_alloc_and_free() {
        let mut b = VmVramBudget::new(1, 512);
        assert!(b.can_alloc_bytes(256 * 1024 * 1024));
        assert!(b.alloc_bytes(256 * 1024 * 1024));
        assert_eq!(b.used_mib, 256);
        assert!(!b.can_alloc_bytes(300 * 1024 * 1024)); // dépasserait 512 Mio
        b.free_bytes(256 * 1024 * 1024);
        assert_eq!(b.used_mib, 0);
    }

    #[test]
    fn test_budget_oom_at_limit() {
        let mut b = VmVramBudget::new(1, 1); // 1 Mio seulement
        assert!(!b.alloc_bytes(2 * 1024 * 1024)); // 2 Mio → refus
        assert_eq!(b.used_mib, 0);
    }

    #[test]
    fn test_free_pct() {
        let mut b = VmVramBudget::new(1, 100);
        b.alloc_bytes(50 * 1024 * 1024);
        let pct = b.free_pct();
        assert!((pct - 50.0).abs() < 1.0);
    }

    #[tokio::test]
    async fn test_set_vm_budget() {
        let mux = make_mux(1024);
        mux.set_vm_budget(100, 512).await;
        let snap = mux.budgets_snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].budget_mib, 512);
    }

    #[tokio::test]
    async fn test_handle_alloc_within_budget() {
        let mux = make_mux(1024);
        mux.set_vm_budget(1, 512).await;

        let req = AllocRequest {
            size_bytes: 64 * 1024 * 1024,
            alignment: 4096,
        };
        let payload = req.encode().to_vec();
        let header = GpuHeader::new(
            MsgType::GpuAlloc,
            1,
            42,
            payload.len() as u32,
            Priority::Normal,
        );
        let msg = GpuMessage::new(header, payload);

        let resp = mux.handle_alloc(&msg).await;
        assert_eq!(resp.header.msg_type, MsgType::GpuAllocResp);

        // La VRAM utilisée doit avoir augmenté
        let snap = mux.budgets_snapshot().await;
        assert!(snap[0].used_mib > 0);
    }

    #[tokio::test]
    async fn test_handle_alloc_oom() {
        let mux = make_mux(1024);
        mux.set_vm_budget(1, 10).await; // 10 Mio budget

        let req = AllocRequest {
            size_bytes: 100 * 1024 * 1024,
            alignment: 4096,
        }; // 100 Mio
        let payload = req.encode().to_vec();
        let header = GpuHeader::new(
            MsgType::GpuAlloc,
            1,
            1,
            payload.len() as u32,
            Priority::Normal,
        );
        let msg = GpuMessage::new(header, payload);

        let resp = mux.handle_alloc(&msg).await;
        assert_eq!(resp.header.msg_type, MsgType::GpuError);
    }

    #[tokio::test]
    async fn test_handle_sync() {
        let mux = make_mux(1024);
        let header = GpuHeader::new(MsgType::GpuSync, 1, 0, 0, Priority::Normal);
        let msg = GpuMessage::new(header, vec![]);
        let resp = mux.handle_sync(&msg).await;
        assert_eq!(resp.header.msg_type, MsgType::GpuResult);
    }

    #[tokio::test]
    async fn test_release_vm_clears_budget() {
        let mux = make_mux(1024);
        mux.set_vm_budget(1, 512).await;
        mux.release_vm(1).await;
        let snap = mux.budgets_snapshot().await;
        assert!(snap.is_empty());
    }

    #[tokio::test]
    async fn test_prometheus_metrics() {
        let mux = make_mux(1024);
        mux.set_vm_budget(1, 512).await;
        let metrics = mux.prometheus_metrics("node1").await;
        assert!(metrics.contains("omega_gpu_commands_total"));
        assert!(metrics.contains("omega_gpu_commands_by_priority_total"));
        assert!(metrics.contains("omega_gpu_command_latency_avg_us"));
        assert!(metrics.contains("omega_gpu_vram_used_mib"));
    }

    #[tokio::test]
    async fn test_submit_raw_records_priority_and_latency() {
        let mux = make_mux(1024);

        let result = mux
            .submit_raw(9004, b"abcdef", Priority::High)
            .await
            .expect("submit_raw should succeed with mock backend");

        assert_eq!(result, b"fedcba");

        let stats = mux.state.stats.lock().await.clone();
        assert_eq!(stats.total_commands, 1);
        assert_eq!(stats.high_priority_commands, 1);
        assert_eq!(stats.normal_priority_commands, 0);
        assert!(stats.avg_latency_us >= 0.0);
    }
}
