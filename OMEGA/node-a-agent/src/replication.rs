//! Réplication des pages distantes.
//!
//! # Stratégie de réplication V4 (améliorée)
//!
//! Chaque page est écrite sur **deux stores** :
//! - **Primaire** : `page_id % num_stores`
//! - **Réplica**  : `(page_id + 1) % num_stores`
//!
//! Lors d'un PUT_PAGE, le primaire et le réplica sont écrits **en parallèle**
//! via `tokio::join!` — la latence d'éviction est celle du store le plus lent,
//! non la somme des deux.
//!
//! Lors d'un GET_PAGE :
//! - On contacte d'abord le primaire
//! - Si le primaire échoue → fallback vers le réplica
//!
//! # Sémantique d'erreur
//!
//! - PUT : ok si au moins un des deux stores a accepté la page.
//! - GET : ok si au moins un des deux stores retourne la page.
//! - DELETE : opération sur primaire + réplica (erreur réplica non fatale).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use tracing::{debug, warn};

use crate::remote::RemoteStorePool;
use node_bc_store::protocol::PAGE_SIZE;

/// Métriques de réplication.
#[derive(Default, Debug)]
pub struct ReplicationMetrics {
    pub primary_puts: AtomicU64,
    pub replica_puts: AtomicU64,
    pub primary_gets: AtomicU64,
    pub replica_gets: AtomicU64,
    pub primary_fails: AtomicU64,
    pub replica_fails: AtomicU64,
}

/// Client avec réplication.
pub struct ReplicatedStoreClient {
    pool: Arc<RemoteStorePool>,
    metrics: Arc<ReplicationMetrics>,
    factor: usize,
}

impl ReplicatedStoreClient {
    pub fn new(pool: Arc<RemoteStorePool>, factor: usize) -> Self {
        let factor = factor.min(pool.num_stores()).max(1);
        Self {
            pool,
            metrics: Arc::new(ReplicationMetrics::default()),
            factor,
        }
    }

    pub fn metrics(&self) -> Arc<ReplicationMetrics> {
        self.metrics.clone()
    }

    /// PUT_PAGE avec réplication **parallèle**.
    ///
    /// Primaire et réplica sont écrits simultanément via `tokio::join!`.
    /// La latence est celle du store le plus lent, non la somme des deux.
    /// Succès si au moins un des deux stores a accepté la page.
    pub async fn put_page(&self, vm_id: u32, page_id: u64, data: Vec<u8>) -> Result<()> {
        if data.len() != PAGE_SIZE {
            bail!("put_page répliqué : taille incorrecte {}", data.len());
        }

        if self.factor >= 2 && self.pool.num_stores() >= 2 {
            let (primary_result, replica_result) = tokio::join!(
                self.pool.put_page(vm_id, page_id, data.clone()),
                self.pool.put_page_replica(vm_id, page_id, data),
            );

            match &primary_result {
                Ok(_) => {
                    self.metrics.primary_puts.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    self.metrics.primary_fails.fetch_add(1, Ordering::Relaxed);
                    warn!(vm_id, page_id, error = %e, "PUT primaire échoué");
                }
            }
            match &replica_result {
                Ok(_) => {
                    self.metrics.replica_puts.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    self.metrics.replica_fails.fetch_add(1, Ordering::Relaxed);
                    warn!(vm_id, page_id, error = %e, "PUT réplica échoué (non fatal)");
                }
            }

            // Succès si au moins un store a accepté
            if primary_result.is_err() && replica_result.is_err() {
                return primary_result;
            }
            return Ok(());
        }

        // Pas de réplication (factor == 1 ou un seul store)
        let result = self.pool.put_page(vm_id, page_id, data).await;
        match &result {
            Ok(_) => {
                self.metrics.primary_puts.fetch_add(1, Ordering::Relaxed);
                debug!(vm_id, page_id, "PUT primaire ok");
            }
            Err(e) => {
                self.metrics.primary_fails.fetch_add(1, Ordering::Relaxed);
                warn!(vm_id, page_id, error = %e, "PUT primaire échoué");
            }
        }
        result
    }

    /// GET_PAGE avec fallback vers le réplica.
    pub async fn get_page(&self, vm_id: u32, page_id: u64) -> Result<Option<Vec<u8>>> {
        self.metrics.primary_gets.fetch_add(1, Ordering::Relaxed);

        match self.pool.get_page(vm_id, page_id).await {
            Ok(Some(data)) => return Ok(Some(data)),
            Ok(None) => {}
            Err(e) => {
                warn!(vm_id, page_id, error = %e, "GET primaire échoué — tentative réplica");
                self.metrics.primary_fails.fetch_add(1, Ordering::Relaxed);
            }
        }

        if self.factor >= 2 && self.pool.num_stores() >= 2 {
            self.metrics.replica_gets.fetch_add(1, Ordering::Relaxed);
            match self.pool.get_page_replica(vm_id, page_id).await {
                Ok(Some(data)) => {
                    warn!(
                        vm_id,
                        page_id, "GET servi depuis le réplica — store primaire dégradé ?"
                    );
                    return Ok(Some(data));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(vm_id, page_id, error = %e, "GET réplica aussi échoué");
                }
            }
        }

        Ok(None)
    }

    /// DELETE_PAGE sur primaire + réplica.
    pub async fn delete_page(&self, vm_id: u32, page_id: u64) -> Result<bool> {
        let primary = self.pool.delete_page(vm_id, page_id).await?;
        if self.factor >= 2 && self.pool.num_stores() >= 2 {
            let _ = self.pool.delete_page_replica(vm_id, page_id).await;
        }
        Ok(primary)
    }
}
