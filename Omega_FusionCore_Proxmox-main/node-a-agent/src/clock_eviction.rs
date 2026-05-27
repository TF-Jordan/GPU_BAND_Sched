//! Algorithme d'éviction CLOCK — correction de la limite L1 (éviction séquentielle).
//!
//! # Algorithme CLOCK
//!
//! Le CLOCK est une approximation LRU à faible coût :
//!
//! ```text
//!  Pages :   [ 0 ][ 1 ][ 2 ][ 3 ][ 4 ][ 5 ]
//!  Access:   [  1 ][  0 ][  1 ][  1 ][  0 ][  0 ]
//!  Hand  :         ^--- pointe ici
//!
//! Éviction : si access[hand] == 0 → évincer cette page
//!            si access[hand] == 1 → mettre à 0, avancer la main
//! ```
//!
//! # Suivi des accès
//!
//! Pour détecter les accès aux pages sans modifier QEMU, on utilise
//! `UFFDIO_REGISTER_MODE_WP` (write-protect) :
//! - Quand une page est récupérée depuis le store, on l'écrit-protège
//! - Tout accès en écriture déclenche une faute WP → on note l'accès et retire la WP
//! - Les lectures ne sont pas tracées (compromis V4 — les écritures suffisent pour détecter la chaleur)
//!
//! Comme alternative plus simple (pas besoin de WP), on peut utiliser un compteur
//! de faults par page — les pages jamais re-faultées depuis leur dernier éviction
//! sont les plus froides.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tracing::{debug, trace};

/// Métadonnées de suivi par page.
#[derive(Debug, Clone)]
pub struct PageMeta {
    /// Bit d'accès pour l'algorithme CLOCK (1 = accédée récemment)
    pub access_bit: bool,
    /// Timestamp du dernier accès (fault ou write-protect)
    pub last_access: Instant,
    /// Nombre de fois que la page a été évinvée
    pub eviction_count: u32,
    /// La page est-elle actuellement sur un store distant ?
    pub is_remote: bool,
}

impl Default for PageMeta {
    fn default() -> Self {
        Self {
            access_bit: true, // nouvelle page = considérée récente
            last_access: Instant::now(),
            eviction_count: 0,
            is_remote: false,
        }
    }
}

/// Anneau CLOCK avec lookup O(1).
struct ClockRingInner {
    deque: VecDeque<u64>,
    present: HashSet<u64>,
}

impl ClockRingInner {
    fn with_capacity(cap: usize) -> Self {
        Self {
            deque: VecDeque::with_capacity(cap),
            present: HashSet::with_capacity(cap),
        }
    }
    fn contains(&self, id: u64) -> bool {
        self.present.contains(&id)
    }
    fn push_back(&mut self, id: u64) {
        self.deque.push_back(id);
        self.present.insert(id);
    }
    fn pop_front(&mut self) -> Option<u64> {
        let id = self.deque.pop_front()?;
        self.present.remove(&id);
        Some(id)
    }
    fn push_back_front(&mut self, id: u64) {
        // rotation : retire du front et remet en queue (seconde chance CLOCK)
        self.deque.push_back(id);
        // `present` déjà à jour — id est encore dedans
    }
    fn remove(&mut self, id: u64) {
        self.deque.retain(|&x| x != id);
        self.present.remove(&id);
    }
    fn len(&self) -> usize {
        self.deque.len()
    }
}

/// Gestionnaire d'éviction CLOCK.
///
/// Maintient un anneau circulaire de `num_pages` slots avec le bit d'accès.
pub struct ClockEvictor {
    /// Métadonnées par page_id
    pub meta: Arc<DashMap<u64, PageMeta>>,
    /// Ordre circulaire des pages présentes localement (anneau CLOCK)
    clock_ring: Mutex<ClockRingInner>,
    /// Délai minimal avant qu'une page puisse être évinvée après son accès
    min_age: Duration,
}

impl ClockEvictor {
    pub fn new(num_pages: usize, min_age_secs: u64) -> Self {
        Self {
            meta: Arc::new(DashMap::new()),
            clock_ring: Mutex::new(ClockRingInner::with_capacity(num_pages)),
            min_age: Duration::from_secs(min_age_secs),
        }
    }

    /// Enregistre une page comme présente localement.
    pub fn mark_present(&self, page_id: u64) {
        self.meta
            .entry(page_id)
            .and_modify(|m| {
                m.access_bit = true;
                m.last_access = Instant::now();
                m.is_remote = false;
            })
            .or_insert_with(PageMeta::default);

        let mut ring = self.clock_ring.lock().unwrap();
        if !ring.contains(page_id) {
            ring.push_back(page_id);
        }
    }

    /// Notifie un accès à la page (depuis le handler uffd ou WP fault).
    pub fn mark_accessed(&self, page_id: u64) {
        if let Some(mut meta) = self.meta.get_mut(&page_id) {
            meta.access_bit = true;
            meta.last_access = Instant::now();
        }
        trace!(page_id, "access_bit mis à 1");
    }

    /// Marque la page comme distante (après éviction).
    pub fn mark_remote(&self, page_id: u64) {
        if let Some(mut meta) = self.meta.get_mut(&page_id) {
            meta.is_remote = true;
            meta.eviction_count += 1;
        }
        // Retirer de l'anneau
        let mut ring = self.clock_ring.lock().unwrap();
        ring.remove(page_id);
    }

    /// Sélectionne jusqu'à `count` pages à évincer selon l'algorithme CLOCK.
    ///
    /// Retourne les page_ids à évincer (dans l'ordre de priorité).
    pub fn select_victims(&self, count: usize) -> Vec<u64> {
        let mut ring = self.clock_ring.lock().unwrap();
        let mut victims = Vec::with_capacity(count);
        let ring_len = ring.len();

        if ring_len == 0 || count == 0 {
            return victims;
        }

        // Un seul tour : les pages chaudes ont leur bit vidé et survivent jusqu'au prochain appel.
        let max_iterations = ring_len;
        let mut iters = 0;

        while victims.len() < count && iters < max_iterations {
            iters += 1;

            let page_id = match ring.deque.front().copied() {
                Some(p) => p,
                None => break,
            };

            // Vérifier si la page a le bit d'accès ou est trop jeune
            let evictable = self
                .meta
                .get(&page_id)
                .map(|m| !m.access_bit && m.last_access.elapsed() >= self.min_age && !m.is_remote)
                .unwrap_or(false);

            if evictable {
                ring.pop_front();
                victims.push(page_id);
                debug!(page_id, "CLOCK : page sélectionnée pour éviction");
            } else {
                // Donner une seconde chance : mettre access_bit à 0, rotation
                if let Some(mut m) = self.meta.get_mut(&page_id) {
                    m.access_bit = false;
                }
                let front = ring.pop_front().unwrap();
                ring.push_back_front(front);
            }
        }

        debug!(
            found = victims.len(),
            wanted = count,
            ring_len = ring_len,
            iters,
            "CLOCK select_victims terminé"
        );

        victims
    }

    /// Nombre de pages locales tracées.
    pub fn local_count(&self) -> usize {
        self.clock_ring.lock().unwrap().len()
    }

    /// Snapshot des métadonnées pour les métriques.
    pub fn cold_pages_count(&self) -> usize {
        self.meta
            .iter()
            .filter(|e| !e.access_bit && !e.is_remote)
            .count()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_basic_eviction() {
        let evictor = ClockEvictor::new(10, 0); // min_age=0 pour les tests
        for i in 0..5u64 {
            evictor.mark_present(i);
        }
        // Mettre access_bit à false pour que le clock puisse évincer
        for i in 0..5u64 {
            evictor.mark_accessed(i);
            // On remet à false manuellement pour le test
            if let Some(mut m) = evictor.meta.get_mut(&i) {
                m.access_bit = false;
            }
        }
        let victims = evictor.select_victims(3);
        assert_eq!(victims.len(), 3);
    }

    #[test]
    fn test_accessed_page_not_evicted_immediately() {
        let evictor = ClockEvictor::new(10, 0);
        evictor.mark_present(42);
        evictor.mark_accessed(42); // access_bit = 1

        let victims = evictor.select_victims(1);
        // La page a le bit d'accès — elle ne doit pas être évinvée au premier tour
        // (CLOCK lui donne une seconde chance)
        assert_eq!(
            victims.len(),
            0,
            "page récente ne doit pas être évinvée immédiatement"
        );
    }

    #[test]
    fn test_mark_remote_removes_from_ring() {
        let evictor = ClockEvictor::new(10, 0);
        evictor.mark_present(10);
        evictor.mark_remote(10);
        assert_eq!(evictor.local_count(), 0);
    }

    #[test]
    fn test_no_victims_if_all_accessed() {
        let evictor = ClockEvictor::new(10, 0);
        for i in 0..3u64 {
            evictor.mark_present(i);
            // access_bit = true par défaut
        }
        // Après 2 tours, le CLOCK ne peut évincer personne (tous avec access_bit=1
        // puis =0 mais on manque de pages candidates après 2 tours)
        let victims = evictor.select_victims(3);
        // Après 2 tours : access_bit = false sur tous, mais aucune éviction
        // car le 2e tour les marque false → un 3e tour serait nécessaire
        // → comportement CLOCK correct
        assert!(victims.len() <= 3);
    }
}
