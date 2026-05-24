//! Métriques atomiques de l'agent.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default, Debug)]
pub struct AgentMetrics {
    /// Nombre total de page faults interceptés par userfaultfd
    pub fault_count: AtomicU64,
    /// Nombre de fautes correctement servies (UFFDIO_COPY réussi)
    pub fault_served: AtomicU64,
    /// Nombre de fautes ayant entraîné une erreur (injection zéro de secours)
    pub fault_errors: AtomicU64,
    /// Pages évinvées vers les stores
    pub pages_evicted: AtomicU64,
    /// Pages récupérées depuis les stores (suite à faute)
    pub pages_fetched: AtomicU64,
    /// Pages retournées comme zéro (page absente du store)
    pub fetch_zeros: AtomicU64,
    /// Pages actuellement présentes physiquement sur ce nœud pour cette VM
    pub local_present: AtomicU64,
    /// Pages rapatriées depuis les stores (recall LIFO)
    pub pages_recalled: AtomicU64,
    /// Nombre de fois qu'aucun nœud n'a pu prendre les pages (alerte)
    pub eviction_alerts: AtomicU64,
    /// Nombre de recherches de migration déclenchées
    pub migration_searches: AtomicU64,
}

impl AgentMetrics {
    pub fn snapshot(&self) -> AgentMetricsSnapshot {
        AgentMetricsSnapshot {
            fault_count: self.fault_count.load(Ordering::Relaxed),
            fault_served: self.fault_served.load(Ordering::Relaxed),
            fault_errors: self.fault_errors.load(Ordering::Relaxed),
            pages_evicted: self.pages_evicted.load(Ordering::Relaxed),
            pages_fetched: self.pages_fetched.load(Ordering::Relaxed),
            fetch_zeros: self.fetch_zeros.load(Ordering::Relaxed),
            local_present: self.local_present.load(Ordering::Relaxed),
            pages_recalled: self.pages_recalled.load(Ordering::Relaxed),
            eviction_alerts: self.eviction_alerts.load(Ordering::Relaxed),
            migration_searches: self.migration_searches.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AgentMetricsSnapshot {
    pub fault_count: u64,
    pub fault_served: u64,
    pub fault_errors: u64,
    pub pages_evicted: u64,
    pub pages_fetched: u64,
    pub fetch_zeros: u64,
    pub local_present: u64,
    pub pages_recalled: u64,
    pub eviction_alerts: u64,
    pub migration_searches: u64,
}
