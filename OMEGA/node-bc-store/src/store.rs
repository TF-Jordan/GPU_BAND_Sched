//! Stockage en RAM des pages distantes.
//!
//! La clé est `(vm_id: u32, page_id: u64)`.
//! Les valeurs sont des `Box<[u8; PAGE_SIZE]>` pour éviter de cloner inutilement
//! les 4 Kio lors des lectures (on clone une seule fois pour la réponse réseau).
//!
//! `DashMap` offre un sharding interne qui permet la concurrence sans verrou global.

use crate::metrics::StoreMetrics;
use crate::protocol::PAGE_SIZE;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Instant;

/// Clé unique d'une page dans le store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub vm_id: u32,
    pub page_id: u64,
}

impl PageKey {
    pub fn new(vm_id: u32, page_id: u64) -> Self {
        Self { vm_id, page_id }
    }
}

/// Le store de pages.
///
/// Conçu pour être partagé via `Arc<PageStore>` entre toutes les connexions.
pub struct PageStore {
    pages: DashMap<PageKey, (Box<[u8]>, Instant)>,
    metrics: Arc<StoreMetrics>,
}

impl PageStore {
    pub fn new(metrics: Arc<StoreMetrics>) -> Self {
        Self {
            pages: DashMap::new(),
            metrics,
        }
    }

    /// Insère ou remplace une page.
    ///
    /// Retourne `true` si la page existait déjà (mise à jour), `false` sinon (nouvelle entrée).
    pub fn put(&self, key: PageKey, data: Vec<u8>) -> Result<bool, String> {
        if data.len() != PAGE_SIZE {
            return Err(format!(
                "taille incorrecte : {} octets (attendu {})",
                data.len(),
                PAGE_SIZE
            ));
        }

        let existed = self.pages.contains_key(&key);
        self.pages
            .insert(key, (data.into_boxed_slice(), Instant::now()));

        if existed {
            // Mise à jour : ne compte pas comme une nouvelle page
        } else {
            self.metrics
                .pages_stored
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.metrics
            .put_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(existed)
    }

    /// Récupère une copie de la page.
    ///
    /// Retourne `None` si la page n'existe pas.
    pub fn get(&self, key: &PageKey) -> Option<Vec<u8>> {
        self.metrics
            .get_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        match self.pages.get_mut(key) {
            Some(mut entry) => {
                self.metrics
                    .hit_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                entry.value_mut().1 = Instant::now();
                Some(entry.value().0.to_vec())
            }
            None => {
                self.metrics
                    .miss_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
        }
    }

    /// Supprime une page.
    ///
    /// Retourne `true` si la page existait.
    pub fn delete(&self, key: &PageKey) -> bool {
        let removed = self.pages.remove(key).is_some();
        if removed {
            self.metrics
                .pages_stored
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            self.metrics
                .delete_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        removed
    }

    /// Nombre de pages actuellement stockées.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Mémoire brute estimée en octets (sans compter l'overhead DashMap/Box).
    pub fn estimated_bytes(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }

    /// Retourne toutes les clés appartenant à un vm_id donné.
    ///
    /// Utilisé par l'API HTTP pour supprimer toutes les pages d'une VM post-migration.
    pub fn keys_for_vm(&self, vm_id: u32) -> Vec<PageKey> {
        self.pages
            .iter()
            .filter(|entry| entry.key().vm_id == vm_id)
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Retourne tous les vm_ids qui ont au moins une page dans ce store.
    pub fn list_vm_ids(&self) -> Vec<u32> {
        let mut seen = std::collections::HashSet::new();
        for entry in self.pages.iter() {
            seen.insert(entry.key().vm_id);
        }
        seen.into_iter().collect()
    }

    /// Supprime toutes les pages d'une VM (nettoyage post-crash ou migration).
    pub fn delete_vm(&self, vm_id: u32) -> usize {
        let keys = self.keys_for_vm(vm_id);
        let count = keys.len();
        for key in &keys {
            self.pages.remove(key);
        }
        if count > 0 {
            self.metrics
                .pages_stored
                .fetch_sub(count as u64, std::sync::atomic::Ordering::Relaxed);
            self.metrics
                .delete_count
                .fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
        }
        count
    }

    /// Retourne un map vm_id → nombre de pages (pour l'API /api/pages).
    pub fn page_counts_by_vm(&self) -> std::collections::HashMap<u32, u64> {
        let mut counts = std::collections::HashMap::new();
        for entry in self.pages.iter() {
            *counts.entry(entry.key().vm_id).or_insert(0) += 1;
        }
        counts
    }

    /// Éviction LRU vraie : retire les `count` pages les moins récemment accédées.
    pub fn evict_lru(&self, count: usize) -> usize {
        let mut entries: Vec<(PageKey, Instant)> = self
            .pages
            .iter()
            .map(|e| (e.key().clone(), e.value().1))
            .collect();
        entries.sort_unstable_by_key(|(_, ts)| *ts);
        let to_evict = entries.into_iter().take(count).collect::<Vec<_>>();
        let n = to_evict.len();
        for (key, _) in to_evict {
            self.pages.remove(&key);
        }
        n
    }
}

// ---------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::StoreMetrics;

    fn make_store() -> PageStore {
        PageStore::new(Arc::new(StoreMetrics::default()))
    }

    #[test]
    fn test_put_get_delete() {
        let store = make_store();
        let key = PageKey::new(1, 100);
        let data = vec![0x42u8; PAGE_SIZE];

        // Insertion
        assert_eq!(store.put(key.clone(), data.clone()), Ok(false));
        assert_eq!(store.len(), 1);

        // Récupération
        let got = store.get(&key).unwrap();
        assert_eq!(got, data);

        // Mise à jour
        let new_data = vec![0xFFu8; PAGE_SIZE];
        assert_eq!(store.put(key.clone(), new_data.clone()), Ok(true));
        assert_eq!(store.len(), 1); // toujours 1 page

        // Vérification de la mise à jour
        let got2 = store.get(&key).unwrap();
        assert_eq!(got2, new_data);

        // Suppression
        assert!(store.delete(&key));
        assert!(store.is_empty());
        assert!(store.get(&key).is_none());
    }

    #[test]
    fn test_wrong_size_rejected() {
        let store = make_store();
        let key = PageKey::new(1, 1);
        let bad = vec![0u8; 1024]; // pas 4096
        assert!(store.put(key, bad).is_err());
    }

    #[test]
    fn test_miss_increments_counter() {
        let metrics = Arc::new(StoreMetrics::default());
        let store = PageStore::new(metrics.clone());
        let key = PageKey::new(99, 999);

        assert!(store.get(&key).is_none());
        assert_eq!(
            metrics
                .miss_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
}
