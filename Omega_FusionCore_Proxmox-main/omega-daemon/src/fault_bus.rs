//! Canal IPC entre le handler uffd (node-a-agent) et le moteur d'éviction — correction L1.
//!
//! # Problème corrigé
//!
//! En V4, l'agent uffd (`node-a-agent`) et le daemon d'éviction (`omega-daemon`)
//! étaient deux binaires indépendants qui ne se parlaient pas en temps réel.
//! Résultat : le moteur d'éviction ne savait pas qu'une page fault venait d'être
//! servie depuis le store distant, et ne pouvait pas ajuster sa politique en
//! conséquence (ex : accélérer l'éviction si le taux de fault augmente).
//!
//! # Solution
//!
//! Un `FaultBus` est un canal tokio mpsc publié dans `NodeState`.
//! - L'agent uffd (ou son thread reader) poste un `FaultEvent` à chaque faute résolue.
//! - Le `EvictionEngine` écoute ce bus et ajuste son rythme d'éviction en temps réel.
//!
//! # Événements
//!
//! ```text
//! FaultEvent::PageServed  → page servie depuis le store distant (bon)
//! FaultEvent::PageMissing → page absente du store (bad — store incohérent)
//! FaultEvent::PageLocal   → page servie depuis la RAM locale (très bon)
//! ```
//!
//! # Politique d'accélération
//!
//! Si le taux de `PageServed` dépasse `accel_threshold_rps` (requêtes/sec),
//! le moteur réduit son intervalle d'éviction pour anticiper la prochaine pression.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ─── Types publics ────────────────────────────────────────────────────────────

/// Événement émis par le handler uffd à chaque faute résolue.
#[derive(Debug, Clone)]
pub enum FaultEvent {
    /// Page récupérée depuis le store distant.
    PageServed {
        vm_id: u32,
        page_id: u64,
        latency_us: u64,
    },
    /// Page absente du store → injections de zéros.
    PageMissing { vm_id: u32, page_id: u64 },
    /// Page résolue localement (RAM locale disponible).
    PageLocal { vm_id: u32, page_id: u64 },
}

impl FaultEvent {
    pub fn vm_id(&self) -> u32 {
        match self {
            Self::PageServed { vm_id, .. } => *vm_id,
            Self::PageMissing { vm_id, .. } => *vm_id,
            Self::PageLocal { vm_id, .. } => *vm_id,
        }
    }
}

/// Statistiques de fenêtre glissante alimentées par le FaultBus.
#[derive(Debug, Default, Clone)]
pub struct FaultStats {
    /// Fautes servies depuis le store distant (fenêtre dernière minute)
    pub remote_served: u64,
    /// Fautes sans données dans le store (incohérence)
    pub store_misses: u64,
    /// Fautes locales (RAM dispo)
    pub local_served: u64,
    /// Latence moyenne de récupération distante (µs)
    pub avg_latency_us: u64,
    /// Débit de fautes distantes (fautes/sec, fenêtre 10s)
    pub remote_rps: f64,
}

impl FaultStats {
    /// Le nœud est-il sous pression uffd (trop de fautes distantes) ?
    pub fn is_under_fault_pressure(&self, threshold_rps: f64) -> bool {
        self.remote_rps > threshold_rps
    }
}

// ─── FaultBus ─────────────────────────────────────────────────────────────────

/// Canal de communication entre le handler uffd et le moteur d'éviction.
pub struct FaultBus {
    /// Émetteur — clonable, utilisé par le(s) thread(s) uffd.
    sender: mpsc::Sender<FaultEvent>,
    /// Récepteur — consommé par le EvictionEngine dans sa boucle.
    receiver: Option<mpsc::Receiver<FaultEvent>>,
}

impl FaultBus {
    /// Crée un nouveau bus avec une capacité de file de `capacity` événements.
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Some(receiver),
        }
    }

    /// Retourne un émetteur clonable pour le handler uffd.
    pub fn sender(&self) -> FaultBusSender {
        FaultBusSender {
            inner: self.sender.clone(),
        }
    }

    /// Extrait le récepteur (ne peut être appelé qu'une fois).
    pub fn take_receiver(&mut self) -> Option<mpsc::Receiver<FaultEvent>> {
        self.receiver.take()
    }
}

/// Emetteur clonable — côté uffd.
#[derive(Clone)]
pub struct FaultBusSender {
    inner: mpsc::Sender<FaultEvent>,
}

impl FaultBusSender {
    /// Envoie un événement de faute au bus. Non-bloquant.
    /// Si le canal est plein, l'événement est silencieusement abandonné
    /// (mieux vaut perdre des stats que bloquer le handler uffd).
    pub fn send(&self, event: FaultEvent) {
        let _ = self.inner.try_send(event);
    }
}

// ─── FaultBusConsumer ─────────────────────────────────────────────────────────

/// Consommateur du bus de fautes — intégré dans l'EvictionEngine.
///
/// Accumule les statistiques sur une fenêtre glissante et calcule le débit
/// de fautes distantes pour permettre à l'EvictionEngine d'accélérer/ralentir.
pub struct FaultBusConsumer {
    receiver: mpsc::Receiver<FaultEvent>,
    /// Fenêtre temporelle pour le calcul du RPS
    window: Duration,
    /// Événements de la fenêtre courante
    window_events: Vec<(Instant, FaultEvent)>,
    /// Stats courantes (recalculées à chaque `poll`)
    pub stats: FaultStats,
    /// Seuil RPS au-delà duquel on accélère l'éviction
    pub accel_threshold: f64,
}

impl FaultBusConsumer {
    pub fn new(
        receiver: mpsc::Receiver<FaultEvent>,
        window_secs: u64,
        accel_threshold: f64,
    ) -> Self {
        Self {
            receiver,
            window: Duration::from_secs(window_secs),
            window_events: Vec::new(),
            stats: FaultStats::default(),
            accel_threshold,
        }
    }

    /// Draine tous les événements disponibles et met à jour les statistiques.
    ///
    /// Retourne `true` si l'éviction doit être accélérée.
    pub fn poll(&mut self) -> bool {
        // Drainer le canal
        while let Ok(event) = self.receiver.try_recv() {
            self.window_events.push((Instant::now(), event));
        }

        // Purger les événements hors fenêtre
        let cutoff = Instant::now() - self.window;
        self.window_events.retain(|(ts, _)| *ts >= cutoff);

        // Recalculer les stats
        let mut remote_served = 0u64;
        let mut store_misses = 0u64;
        let mut local_served = 0u64;
        let mut total_lat_us = 0u64;

        for (_, event) in &self.window_events {
            match event {
                FaultEvent::PageServed { latency_us, .. } => {
                    remote_served += 1;
                    total_lat_us += latency_us;
                }
                FaultEvent::PageMissing { .. } => store_misses += 1,
                FaultEvent::PageLocal { .. } => local_served += 1,
            }
        }

        let window_secs = self.window.as_secs_f64().max(1.0);
        let remote_rps = remote_served as f64 / window_secs;
        let avg_lat = if remote_served > 0 {
            total_lat_us / remote_served
        } else {
            0
        };

        self.stats = FaultStats {
            remote_served,
            store_misses,
            local_served,
            avg_latency_us: avg_lat,
            remote_rps,
        };

        if self.stats.is_under_fault_pressure(self.accel_threshold) {
            debug!(
                remote_rps = format!("{:.1}", remote_rps),
                threshold = self.accel_threshold,
                avg_latency_us = avg_lat,
                "FaultBus : pression détectée → accélération éviction"
            );
            return true;
        }

        if store_misses > 0 {
            warn!(
                store_misses,
                "FaultBus : pages absentes du store — incohérence possible"
            );
        }

        false
    }
}

// ─── Intégration dans EvictionEngine ─────────────────────────────────────────

/// Wrapper qui donne un intervalle d'éviction dynamique basé sur le FaultBus.
///
/// Usage dans EvictionEngine :
/// ```rust,no_run
/// use omega_daemon::fault_bus::AdaptiveInterval;
/// let mut interval = AdaptiveInterval::new(5, 1);
/// interval.update(true);  // pression détectée → mode accéléré
/// let _sleep_dur = interval.current();
/// ```
pub struct AdaptiveInterval {
    base: Duration,
    minimum: Duration,
    accelerated: bool,
}

impl AdaptiveInterval {
    pub fn new(base_secs: u64, min_secs: u64) -> Self {
        Self {
            base: Duration::from_secs(base_secs),
            minimum: Duration::from_secs(min_secs),
            accelerated: false,
        }
    }

    /// Met à jour l'état d'accélération depuis le bus.
    pub fn update(&mut self, under_pressure: bool) {
        if under_pressure && !self.accelerated {
            info!(
                base_secs = self.base.as_secs(),
                min_secs = self.minimum.as_secs(),
                "AdaptiveInterval : passage en mode accéléré"
            );
        } else if !under_pressure && self.accelerated {
            info!("AdaptiveInterval : retour au rythme normal");
        }
        self.accelerated = under_pressure;
    }

    /// Retourne l'intervalle courant.
    pub fn current(&self) -> Duration {
        if self.accelerated {
            self.minimum
        } else {
            self.base
        }
    }
}
