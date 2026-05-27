//! Store de pages persistant — correction de la limite L2.
//!
//! # Problème corrigé
//!
//! Le `PageStore` original stockait tout en RAM (DashMap).
//! Au redémarrage ou crash du daemon, toutes les pages distantes étaient perdues.
//! Les VMs dont les pages étaient sur ce store pouvaient recevoir des données
//! corrompues (zéros) au prochain accès.
//!
//! # Solution
//!
//! `PersistentPageStore` combine deux couches :
//!
//! 1. **Cache chaud (DashMap en RAM)** — les pages fréquemment accédées restent
//!    en mémoire pour une latence minimale (< 1 µs vs ~50 µs pour sled).
//!
//! 2. **Journal sled (sur disque)** — toute écriture (`put`) est journalisée
//!    de façon durable sur disque via `sled` (B-tree log-structured).
//!    Au redémarrage, le cache chaud est reconstruit depuis sled.
//!
//! # Format de clé sled
//!
//! Les clés sont encodées en binaire big-endian : `[vm_id: u32][page_id: u64]` = 12 octets.
//! Ce format permet des scans prefixés efficaces (`scan_prefix(vm_id_bytes)`)
//! pour trouver toutes les pages d'une VM.
//!
//! # Politique d'éviction du cache chaud
//!
//! Quand le cache atteint `max_hot_pages`, les pages les moins récemment accédées
//! sont retirées du cache (mais restent sur disque). Un `get` ultérieur les
//! rechargera depuis sled (accès froid, ~50 µs).
//!
//! # Configuration
//!
//! ```rust,no_run
//! use node_bc_store::persistent_store::{PersistentStoreConfig, SyncMode};
//! let cfg = PersistentStoreConfig {
//!     db_path:       "/var/lib/omega-store".into(),
//!     max_hot_pages: 65536,   // 256 Mio en RAM pour le cache chaud
//!     sync_mode:     SyncMode::PerBatch(64),
//! };
//! ```

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use sled::{Db, IVec};
use tracing::{debug, error, info};

use crate::metrics::StoreMetrics;
use crate::protocol::PAGE_SIZE;
use crate::store::PageKey;

// ─── Configuration ────────────────────────────────────────────────────────────

/// Mode de synchronisation disque.
#[derive(Debug, Clone, Copy)]
pub enum SyncMode {
    /// Synchronisation après chaque écriture (le plus sûr, le plus lent)
    Always,
    /// Synchronisation après N écritures (compromis)
    PerBatch(u64),
    /// Synchronisation déléguée au système d'exploitation (le plus rapide, moins sûr)
    Never,
}

/// Configuration du store persistant.
#[derive(Debug, Clone)]
pub struct PersistentStoreConfig {
    /// Répertoire de la base sled (créé si absent)
    pub db_path: PathBuf,
    /// Nombre maximum de pages maintenues en cache chaud (RAM)
    pub max_hot_pages: usize,
    /// Politique de sync disque
    pub sync_mode: SyncMode,
}

impl Default for PersistentStoreConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("/var/lib/omega-store"),
            max_hot_pages: 65_536, // 256 Mio
            sync_mode: SyncMode::PerBatch(64),
        }
    }
}

// ─── Encodage de clé ──────────────────────────────────────────────────────────

fn encode_key(key: &PageKey) -> [u8; 12] {
    let mut buf = [0u8; 12];
    buf[0..4].copy_from_slice(&key.vm_id.to_be_bytes());
    buf[4..12].copy_from_slice(&key.page_id.to_be_bytes());
    buf
}

fn decode_key(raw: &[u8]) -> Option<PageKey> {
    if raw.len() != 12 {
        return None;
    }
    let vm_id = u32::from_be_bytes(raw[0..4].try_into().ok()?);
    let page_id = u64::from_be_bytes(raw[4..12].try_into().ok()?);
    Some(PageKey::new(vm_id, page_id))
}

/// Retourne le préfixe de clé sled pour un vm_id (pour scan_prefix).
fn vm_prefix(vm_id: u32) -> [u8; 4] {
    vm_id.to_be_bytes()
}

// ─── Store ────────────────────────────────────────────────────────────────────

/// Store de pages persistant : cache RAM (LRU) + journal sled.
///
/// Le cache chaud stocke `(données, timestamp_dernier_accès)`.
/// L'éviction retire les pages les moins récemment accédées (LRU réel).
pub struct PersistentPageStore {
    /// Cache chaud — (page, timestamp dernier accès) pour LRU
    hot_cache: DashMap<PageKey, (Arc<[u8]>, Instant)>,
    /// Base sled — journal durable sur disque
    db: Db,
    /// Configuration
    cfg: PersistentStoreConfig,
    /// Métriques partagées
    metrics: Arc<StoreMetrics>,
    /// Compteur d'écritures (pour SyncMode::PerBatch)
    write_counter: std::sync::atomic::AtomicU64,
}

impl PersistentPageStore {
    /// Ouvre (ou crée) le store persistant.
    ///
    /// Si la base sled existe déjà, reconstruit le cache chaud depuis le disque.
    pub fn open(cfg: PersistentStoreConfig, metrics: Arc<StoreMetrics>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.db_path)?;

        let db = sled::Config::new()
            .path(&cfg.db_path)
            .cache_capacity(256 * 1024 * 1024) // 256 Mio cache sled interne
            .flush_every_ms(Some(1000)) // flush asynchrone toutes les 1s
            .open()?;

        let store = Self {
            hot_cache: DashMap::new(),
            db,
            cfg,
            metrics,
            write_counter: std::sync::atomic::AtomicU64::new(0),
        };

        // Reconstruction du cache chaud depuis sled
        let restored = store.restore_hot_cache();
        info!(
            db_path  = %store.cfg.db_path.display(),
            restored,
            "PersistentPageStore ouvert"
        );

        Ok(store)
    }

    /// Insère ou remplace une page (cache + disque).
    pub fn put(&self, key: PageKey, data: Vec<u8>) -> Result<bool, String> {
        if data.len() != PAGE_SIZE {
            return Err(format!(
                "taille incorrecte : {} octets (attendu {})",
                data.len(),
                PAGE_SIZE
            ));
        }

        let raw_key = encode_key(&key);
        let existed = self.db.contains_key(raw_key).unwrap_or(false);

        // Écriture disque (sled)
        if let Err(e) = self.db.insert(raw_key, data.as_slice()) {
            error!(error = %e, "sled insert échoué");
            return Err(e.to_string());
        }

        // Mise à jour cache chaud avec timestamp courant
        let arc_data: Arc<[u8]> = data.into();
        self.hot_cache.insert(key, (arc_data, Instant::now()));

        // Éviction cache si dépassement
        if self.hot_cache.len() > self.cfg.max_hot_pages {
            self.evict_cold_from_cache();
        }

        // Sync disque selon la politique
        let writes = self.write_counter.fetch_add(1, Ordering::Relaxed);
        match self.cfg.sync_mode {
            SyncMode::Always => {
                let _ = self.db.flush();
            }
            SyncMode::PerBatch(n) if writes.is_multiple_of(n) => {
                let _ = self.db.flush();
            }
            _ => {}
        }

        if !existed {
            self.metrics.pages_stored.fetch_add(1, Ordering::Relaxed);
        }
        self.metrics.put_count.fetch_add(1, Ordering::Relaxed);

        Ok(existed)
    }

    /// Récupère une page (cache d'abord, puis disque).
    pub fn get(&self, key: &PageKey) -> Option<Vec<u8>> {
        self.metrics.get_count.fetch_add(1, Ordering::Relaxed);

        // 1. Cache chaud — mise à jour du timestamp LRU à chaque accès
        if let Some(mut entry) = self.hot_cache.get_mut(key) {
            entry.1 = Instant::now();
            self.metrics.hit_count.fetch_add(1, Ordering::Relaxed);
            debug!(vm_id = key.vm_id, page_id = key.page_id, "hit cache chaud");
            return Some(entry.0.to_vec());
        }

        // 2. Lecture depuis sled (accès froid)
        let raw_key = encode_key(key);
        match self.db.get(raw_key) {
            Ok(Some(ivec)) => {
                let data = ivec.to_vec();
                // Remettre en cache chaud avec timestamp courant
                let arc_data: Arc<[u8]> = data.clone().into();
                self.hot_cache
                    .insert(key.clone(), (arc_data, Instant::now()));
                self.metrics.hit_count.fetch_add(1, Ordering::Relaxed);
                debug!(
                    vm_id = key.vm_id,
                    page_id = key.page_id,
                    "hit sled (accès froid)"
                );
                Some(data)
            }
            Ok(None) => {
                self.metrics.miss_count.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(e) => {
                error!(error = %e, "sled get échoué");
                self.metrics.miss_count.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Supprime une page (cache + disque).
    pub fn delete(&self, key: &PageKey) -> bool {
        let raw_key = encode_key(key);
        let existed = self.db.remove(raw_key).ok().flatten().is_some();
        self.hot_cache.remove(key);

        if existed {
            self.metrics.pages_stored.fetch_sub(1, Ordering::Relaxed);
            self.metrics.delete_count.fetch_add(1, Ordering::Relaxed);
        }
        existed
    }

    /// Supprime toutes les pages d'un vm_id (post-migration).
    ///
    /// Utilise `scan_prefix` pour trouver toutes les pages sans scanner toute la base.
    pub fn delete_vm(&self, vm_id: u32) -> usize {
        let prefix = vm_prefix(vm_id);
        let keys: Vec<IVec> = self
            .db
            .scan_prefix(prefix)
            .filter_map(|r| r.ok().map(|(k, _)| k))
            .collect();

        let count = keys.len();
        for raw_key in &keys {
            let _ = self.db.remove(raw_key);
            // Nettoyer le cache chaud si présent
            if let Some(pk) = decode_key(raw_key) {
                self.hot_cache.remove(&pk);
            }
        }

        if count > 0 {
            self.metrics
                .pages_stored
                .fetch_sub(count as u64, Ordering::Relaxed);
        }

        info!(
            vm_id,
            deleted = count,
            "pages VM supprimées du store persistant"
        );
        count
    }

    /// Nombre de pages sur disque (authoritative).
    pub fn len_on_disk(&self) -> usize {
        self.db.len()
    }

    /// Nombre de pages dans le cache chaud.
    pub fn len_hot(&self) -> usize {
        self.hot_cache.len()
    }

    // ─── Helpers internes ──────────────────────────────────────────────────

    /// Reconstruit le cache chaud depuis sled au démarrage.
    ///
    /// Charge jusqu'à `max_hot_pages` pages (les plus récentes selon l'ordre de clé).
    fn restore_hot_cache(&self) -> usize {
        let mut count = 0usize;
        let total_on_disk = self.db.len();

        // Si la base est vide, rien à faire
        if total_on_disk == 0 {
            return 0;
        }

        info!(total_on_disk, "restauration cache chaud depuis sled...");

        // Charger jusqu'à max_hot_pages pages dans le cache
        for result in self.db.iter() {
            if count >= self.cfg.max_hot_pages {
                break;
            }
            if let Ok((raw_key, raw_val)) = result {
                if let Some(key) = decode_key(&raw_key) {
                    let arc_data: Arc<[u8]> = raw_val.to_vec().into();
                    self.hot_cache.insert(key, (arc_data, Instant::now()));
                    count += 1;
                }
            }
        }

        // Mettre à jour le compteur de pages stockées
        self.metrics
            .pages_stored
            .store(total_on_disk as u64, Ordering::Relaxed);

        info!(
            restored_to_cache = count,
            remaining_cold = total_on_disk.saturating_sub(count),
            "cache chaud restauré"
        );
        count
    }

    /// Retire les pages LRU du cache chaud quand il déborde.
    ///
    /// Collecte tous les timestamps, trie par ancienneté, évince les 10% les plus vieux.
    /// C'est un vrai LRU : on évince toujours les pages les moins récemment accédées.
    fn evict_cold_from_cache(&self) {
        let target_evict = (self.cfg.max_hot_pages / 10).max(1);

        // Collecter les clés avec leurs timestamps (snapshot instantané)
        let mut entries: Vec<(PageKey, Instant)> = self
            .hot_cache
            .iter()
            .map(|e| (e.key().clone(), e.value().1))
            .collect();

        // Trier par accès le plus ancien en premier
        entries.sort_unstable_by_key(|(_, t)| *t);

        for (key, _) in entries.into_iter().take(target_evict) {
            self.hot_cache.remove(&key);
        }

        debug!(evicted = target_evict, "LRU éviction cache chaud");
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_store() -> (PersistentPageStore, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PersistentStoreConfig {
            db_path: dir.path().to_path_buf(),
            max_hot_pages: 16,
            sync_mode: SyncMode::Always,
        };
        let metrics = Arc::new(StoreMetrics::default());
        let store = PersistentPageStore::open(cfg, metrics).unwrap();
        (store, dir)
    }

    #[test]
    fn test_put_get_roundtrip() {
        let (store, _dir) = make_store();
        let key = PageKey::new(1, 42);
        let data = vec![0xABu8; PAGE_SIZE];

        assert_eq!(store.put(key.clone(), data.clone()), Ok(false));
        let got = store.get(&key).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let key = PageKey::new(7, 999);
        let data = vec![0xCDu8; PAGE_SIZE];

        {
            let cfg = PersistentStoreConfig {
                db_path: dir.path().to_path_buf(),
                ..Default::default()
            };
            let store = PersistentPageStore::open(cfg, Arc::new(StoreMetrics::default())).unwrap();
            store.put(key.clone(), data.clone()).unwrap();
        }

        // Réouverture — les données doivent survivre
        {
            let cfg = PersistentStoreConfig {
                db_path: dir.path().to_path_buf(),
                ..Default::default()
            };
            let store = PersistentPageStore::open(cfg, Arc::new(StoreMetrics::default())).unwrap();
            let got = store.get(&key);
            assert!(got.is_some(), "la page doit survivre au redémarrage");
            assert_eq!(got.unwrap(), data);
        }
    }

    #[test]
    fn test_delete_vm_removes_all_pages() {
        let (store, _dir) = make_store();
        let vm_id = 42u32;

        // Insérer 5 pages pour vm 42 et 2 pour vm 99
        for i in 0..5 {
            store
                .put(PageKey::new(vm_id, i), vec![0u8; PAGE_SIZE])
                .unwrap();
        }
        for i in 0..2 {
            store
                .put(PageKey::new(99, i), vec![1u8; PAGE_SIZE])
                .unwrap();
        }

        assert_eq!(store.delete_vm(vm_id), 5);

        // Les pages de vm 42 sont effacées
        for i in 0..5 {
            assert!(store.get(&PageKey::new(vm_id, i)).is_none());
        }
        // Mais pas celles de vm 99
        for i in 0..2 {
            assert!(store.get(&PageKey::new(99, i)).is_some());
        }
    }

    #[test]
    fn test_cache_cold_reload() {
        let (store, _dir) = make_store();
        let key = PageKey::new(3, 100);

        store.put(key.clone(), vec![0x55u8; PAGE_SIZE]).unwrap();
        // Retirer du cache chaud manuellement
        store.hot_cache.remove(&key);

        // La lecture doit passer par sled et remettre en cache
        let got = store.get(&key);
        assert!(got.is_some());
        assert!(
            store.hot_cache.contains_key(&key),
            "doit être rechargé dans le cache"
        );
    }

    #[test]
    fn test_wrong_size_rejected() {
        let (store, _dir) = make_store();
        let key = PageKey::new(1, 1);
        assert!(store.put(key, vec![0u8; 100]).is_err());
    }
}
