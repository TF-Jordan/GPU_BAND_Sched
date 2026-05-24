//! Métriques atomiques du store.
//!
//! Toutes les opérations utilisent `Ordering::Relaxed` : les compteurs sont
//! informatifs, pas des primitives de synchronisation.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default, Debug)]
pub struct StoreMetrics {
    pub pages_stored: AtomicU64,
    pub put_count: AtomicU64,
    pub get_count: AtomicU64,
    pub delete_count: AtomicU64,
    pub hit_count: AtomicU64,
    pub miss_count: AtomicU64,
    pub connections: AtomicU64,
}

impl StoreMetrics {
    /// Capture un snapshot sérialisable à un instant donné.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let pages = self.pages_stored.load(Ordering::Relaxed);
        let gets = self.get_count.load(Ordering::Relaxed);
        let puts = self.put_count.load(Ordering::Relaxed);
        let hits = self.hit_count.load(Ordering::Relaxed);
        let misses = self.miss_count.load(Ordering::Relaxed);
        let deletes = self.delete_count.load(Ordering::Relaxed);
        let conns = self.connections.load(Ordering::Relaxed);

        let hit_rate = if gets > 0 {
            (hits as f64 / gets as f64) * 100.0
        } else {
            0.0
        };

        MetricsSnapshot {
            pages_stored: pages,
            estimated_bytes: pages * 4096,
            put_count: puts,
            get_count: gets,
            delete_count: deletes,
            hit_count: hits,
            miss_count: misses,
            hit_rate_pct: (hit_rate * 10.0).round() / 10.0,
            active_connections: conns,
        }
    }
}

/// Snapshot des métriques à un instant t (sérialisable JSON).
#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub pages_stored: u64,
    pub estimated_bytes: u64,
    pub put_count: u64,
    pub get_count: u64,
    pub delete_count: u64,
    pub hit_count: u64,
    pub miss_count: u64,
    pub hit_rate_pct: f64,
    pub active_connections: u64,
}
