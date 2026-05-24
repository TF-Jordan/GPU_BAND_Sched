//! Point d'entrée du store de pages distantes.
//!
//! Ceph est activé automatiquement si librados est compilé ET si
//! `/etc/ceph/ceph.conf` (ou STORE_CEPH_CONF) existe au démarrage.
//! Aucune variable d'environnement manuelle n'est requise.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use node_bc_store::ceph_store::CephStore;
use node_bc_store::config::Config;
use node_bc_store::metrics::StoreMetrics;
use node_bc_store::orphan_cleaner::OrphanCleaner;
use node_bc_store::server;
use node_bc_store::status_server;

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 ne choisit pas de provider automatiquement — ring doit être installé explicitement.
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap_or(()); // idempotent : déjà installé dans les tests = OK

    let cfg = Config::parse();

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    match cfg.log_format.as_str() {
        "json" => fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(false)
            .init(),
        _ => fmt().with_env_filter(filter).with_target(false).init(),
    }

    // Métriques partagées entre le serveur TCP et le serveur HTTP status.
    let metrics = Arc::new(StoreMetrics::default());

    // Auto-connexion Ceph : active si librados détecté au build ET ceph.conf présent.
    let ceph_store: Option<Arc<CephStore>> = {
        CephStore::try_auto_connect(
            &cfg.ceph_conf,
            &cfg.ceph_pool,
            &cfg.ceph_user,
            metrics.clone(),
        )
        .map(Arc::new)
    };

    // Serveur HTTP status cluster (non-bloquant)
    let status_addr = cfg.status_listen.clone();
    let node_id = cfg.node_id.clone();
    let data_path = cfg.store_data_path.clone();
    let ceph_status = ceph_store.clone();
    let status_metrics = metrics.clone();
    tokio::spawn(async move {
        if let Err(e) =
            status_server::run(status_addr, node_id, data_path, ceph_status, status_metrics).await
        {
            tracing::error!(error = %e, "serveur status HTTP terminé avec erreur");
        }
    });

    // Démon de nettoyage des pages orphelines (désactivé si interval = 0)
    let shutdown_store = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if cfg.orphan_check_interval_secs > 0 {
        // Le store est construit dans server::run — on démarre le cleaner
        // avec un store RAM de référence via le même Arc ; server::run reconstruit
        // son propre Arc donc on passe cfg et ceph_store clonés.
        // Pour éviter de dupliquer la construction, on crée un store dédié au cleaner.
        use node_bc_store::server::AnyStore;
        use node_bc_store::store::PageStore;
        let m_clean = std::sync::Arc::new(StoreMetrics::default());
        let clean_store: std::sync::Arc<AnyStore> = if let Some(ref ceph) = ceph_store {
            std::sync::Arc::new(AnyStore::Ceph(ceph.clone()))
        } else {
            std::sync::Arc::new(AnyStore::Ram(std::sync::Arc::new(PageStore::new(m_clean))))
        };
        let cleaner = std::sync::Arc::new(OrphanCleaner::new(
            clean_store,
            cfg.orphan_check_interval_secs,
            cfg.orphan_grace_secs,
        ));
        let sd = shutdown_store.clone();
        tokio::spawn(async move { cleaner.run(sd).await });
        tracing::info!(
            interval_s = cfg.orphan_check_interval_secs,
            grace_s = cfg.orphan_grace_secs,
            "démon nettoyage orphelins activé"
        );
    }

    server::run(cfg, ceph_store, metrics).await
}
