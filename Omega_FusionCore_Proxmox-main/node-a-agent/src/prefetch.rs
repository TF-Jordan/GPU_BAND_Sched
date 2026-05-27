//! Moteur de préfetch — correction de la limite L7.
//!
//! # Stratégie
//!
//! Détecte les accès séquentiels aux pages et déclenche des GET_PAGE en avance
//! de phase pour masquer la latence réseau au prochain page fault.
//!
//! ## Détection de séquentialité
//!
//! On maintient un `AccessHistory` : les N derniers page_ids faultés.
//! Si les K derniers accès forment une progression arithmétique de raison r,
//! on préfetch les pages suivantes avec la même raison.
//!
//! ## Stockage local des pages préfetchées
//!
//! Les pages préfetchées sont stockées dans un `DashMap<u64, [u8; 4096]>`
//! (le "prefetch cache"). Quand un page fault arrive pour une page déjà préfetchée,
//! on l'injecte directement via UFFDIO_COPY sans aller sur le réseau.
//!
//! ## Impact mémoire
//!
//! Le cache de préfetch est borné : au-delà de `max_cached` pages, les plus
//! anciennes sont évincées pour éviter de surcharger la RAM locale.

use std::collections::VecDeque;
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, trace};

use node_bc_store::protocol::PAGE_SIZE;

// ─── Historique d'accès ───────────────────────────────────────────────────────

/// Fenêtre d'observation des page_ids récents pour détecter la séquentialité.
struct AccessHistory {
    window: VecDeque<u64>,
    capacity: usize,
}

impl AccessHistory {
    fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, page_id: u64) {
        if self.window.len() >= self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(page_id);
    }

    /// Détecte si les derniers accès forment une progression arithmétique.
    ///
    /// Retourne `Some(stride)` si la progression est détectée, `None` sinon.
    fn detect_stride(&self) -> Option<i64> {
        if self.window.len() < 3 {
            return None;
        }

        let pages: Vec<u64> = self.window.iter().copied().collect();
        let stride = pages[1] as i64 - pages[0] as i64;

        // Vérifier que tous les écarts sont égaux
        let all_equal = pages
            .windows(2)
            .all(|w| (w[1] as i64 - w[0] as i64) == stride);

        if all_equal && stride != 0 {
            Some(stride)
        } else {
            None
        }
    }
}

// ─── Cache de préfetch ────────────────────────────────────────────────────────

/// Cache local des pages préfetchées en avance de phase.
pub struct PrefetchCache {
    /// page_id → données de la page
    cache: Arc<DashMap<u64, Box<[u8; PAGE_SIZE]>>>,
    /// File d'insertion (pour évincer les plus anciennes en FIFO)
    insertion: std::sync::Mutex<VecDeque<u64>>,
    max_cached: usize,
}

impl PrefetchCache {
    pub fn new(max_cached: usize) -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
            insertion: std::sync::Mutex::new(VecDeque::new()),
            max_cached,
        }
    }

    /// Insère une page dans le cache.
    pub fn insert(&self, page_id: u64, data: [u8; PAGE_SIZE]) {
        let mut ins = self.insertion.lock().unwrap();

        // Éviction FIFO si cache plein
        while ins.len() >= self.max_cached {
            if let Some(old_id) = ins.pop_front() {
                self.cache.remove(&old_id);
                trace!(old_id, "préfetch cache : éviction FIFO");
            }
        }

        self.cache.insert(page_id, Box::new(data));
        ins.push_back(page_id);
        trace!(page_id, "préfetch cache : page insérée");
    }

    /// Récupère et retire une page du cache (consommation one-shot).
    pub fn take(&self, page_id: u64) -> Option<[u8; PAGE_SIZE]> {
        let (_, data) = self.cache.remove(&page_id)?;
        // On retire aussi de la file d'insertion
        let mut ins = self.insertion.lock().unwrap();
        ins.retain(|&id| id != page_id);
        debug!(page_id, "préfetch hit");
        Some(*data)
    }

    pub fn contains(&self, page_id: u64) -> bool {
        self.cache.contains_key(&page_id)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }
}

// ─── Moteur de préfetch ───────────────────────────────────────────────────────

/// Moteur de préfetch — détecte la séquentialité et déclenche des GET_PAGE anticipés.
pub struct PrefetchEngine {
    history: std::sync::Mutex<AccessHistory>,
    pub cache: Arc<PrefetchCache>,
    /// Nombre de pages à préfetcher en avance
    lookahead: u64,
    /// Seuil minimum de confiance (nombre d'accès séquentiels avant de préfetcher)
    min_history: usize,
    num_pages: u64,
}

impl PrefetchEngine {
    pub fn new(num_pages: u64, lookahead: u64, max_cached: usize) -> Self {
        Self {
            history: std::sync::Mutex::new(AccessHistory::new(8)),
            cache: Arc::new(PrefetchCache::new(max_cached)),
            lookahead,
            min_history: 3,
            num_pages,
        }
    }

    /// Enregistre un accès à `page_id` et retourne les pages à préfetcher.
    ///
    /// Appelé par le handler uffd après chaque fault résolu.
    /// Retourne une liste de page_ids à charger en avance de phase.
    pub fn record_access(&self, page_id: u64) -> Vec<u64> {
        let mut history = self.history.lock().unwrap();
        history.push(page_id);

        if history.window.len() < self.min_history {
            return vec![];
        }

        let Some(stride) = history.detect_stride() else {
            return vec![];
        };

        // Générer les page_ids à préfetcher
        let candidates: Vec<u64> = (1..=self.lookahead)
            .filter_map(|k| {
                let next = page_id as i64 + stride * k as i64;
                if next >= 0 && (next as u64) < self.num_pages {
                    let next_u64 = next as u64;
                    // Ne pas préfetcher si déjà en cache
                    if !self.cache.contains(next_u64) {
                        Some(next_u64)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        if !candidates.is_empty() {
            debug!(
                page_id,
                stride,
                prefetch_count = candidates.len(),
                "préfetch séquentiel détecté"
            );
        }

        candidates
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stride_detection_sequential() {
        let mut h = AccessHistory::new(8);
        for i in [10u64, 11, 12, 13] {
            h.push(i);
        }
        assert_eq!(h.detect_stride(), Some(1));
    }

    #[test]
    fn test_stride_detection_reverse() {
        let mut h = AccessHistory::new(8);
        for i in [20u64, 18, 16, 14] {
            h.push(i);
        }
        assert_eq!(h.detect_stride(), Some(-2));
    }

    #[test]
    fn test_no_stride_random() {
        let mut h = AccessHistory::new(8);
        for i in [5u64, 3, 9, 1] {
            h.push(i);
        }
        assert_eq!(h.detect_stride(), None);
    }

    #[test]
    fn test_prefetch_engine_detects_and_caches() {
        let engine = PrefetchEngine::new(100, 3, 32);
        // Simuler 3 accès séquentiels
        engine.record_access(0);
        engine.record_access(1);
        let to_prefetch = engine.record_access(2);
        // Doit suggérer 3, 4, 5
        assert_eq!(to_prefetch, vec![3, 4, 5]);
    }

    #[test]
    fn test_cache_take() {
        let cache = PrefetchCache::new(10);
        let data = [0xABu8; PAGE_SIZE];
        cache.insert(42, data);
        assert!(cache.contains(42));
        let got = cache.take(42).unwrap();
        assert_eq!(got[0], 0xAB);
        assert!(!cache.contains(42));
    }

    #[test]
    fn test_cache_eviction_fifo() {
        let cache = PrefetchCache::new(3);
        for i in 0..5u64 {
            cache.insert(i, [i as u8; PAGE_SIZE]);
        }
        // Les 2 premières doivent avoir été évincées
        assert!(!cache.contains(0));
        assert!(!cache.contains(1));
        assert!(cache.contains(2));
        assert!(cache.contains(3));
        assert!(cache.contains(4));
    }
}
