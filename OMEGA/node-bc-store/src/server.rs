//! Serveur TCP asynchrone.
//!
//! Une tâche Tokio par connexion client. Le `AnyStore` et les `StoreMetrics`
//! sont partagés via `Arc` — aucune copie, pas de verrou global.
//!
//! # Backend store
//!
//! `AnyStore` encapsule soit le store RAM (`PageStore`), soit le store Ceph
//! (`CephStore`). Le choix est fait au démarrage selon la config.
//!
//! # TLS
//!
//! Si `STORE_TLS_ENABLED=true`, chaque connexion TCP est enveloppée dans un
//! `TlsStream` (tokio-rustls). Le certificat auto-signé est généré au premier
//! démarrage dans `STORE_TLS_DIR`. L'agent doit activer TLS côté client avec
//! la même empreinte (TOFU).

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::ceph_store::CephStore;
use crate::config::Config;
use crate::metrics::StoreMetrics;
use crate::protocol::{Message, Opcode, PAGE_SIZE};
use crate::store::{PageKey, PageStore};
use crate::tls::{TlsContext, TlsPaths};

// ─── AnyStore ─────────────────────────────────────────────────────────────────

/// Backend store — RAM ou Ceph.
pub enum AnyStore {
    Ram(Arc<PageStore>),
    Ceph(Arc<CephStore>),
}

impl AnyStore {
    pub async fn put(&self, key: PageKey, data: Vec<u8>) -> Result<(), String> {
        match self {
            AnyStore::Ram(s) => s.put(key, data).map(|_| ()).map_err(|e| e.to_string()),
            AnyStore::Ceph(s) => s.put(key, data).await.map_err(|e| e.to_string()),
        }
    }

    pub async fn get(&self, key: &PageKey) -> Option<Vec<u8>> {
        match self {
            AnyStore::Ram(s) => s.get(key),
            AnyStore::Ceph(s) => s.get(key).await.ok().flatten(),
        }
    }

    pub async fn delete(&self, key: &PageKey) -> bool {
        match self {
            AnyStore::Ram(s) => s.delete(key),
            AnyStore::Ceph(s) => s.delete(key).await.unwrap_or(false),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            AnyStore::Ram(s) => s.len(),
            AnyStore::Ceph(s) => s.len(),
        }
    }

    pub fn estimated_bytes(&self) -> u64 {
        (self.len() as u64).saturating_mul(PAGE_SIZE as u64)
    }

    /// Retourne les vm_ids qui ont des pages dans ce store.
    /// Pour Ceph : retourne vide (Ceph gère sa propre capacité ; pas de listing trivial).
    pub fn list_vm_ids(&self) -> Vec<u32> {
        match self {
            AnyStore::Ram(s) => s.list_vm_ids(),
            AnyStore::Ceph(_) => Vec::new(),
        }
    }

    /// Supprime toutes les pages d'une VM.
    pub async fn delete_vm(&self, vm_id: u32) -> usize {
        match self {
            AnyStore::Ram(s) => s.delete_vm(vm_id),
            AnyStore::Ceph(s) => {
                warn!(vm_id, "delete_vm Ceph non supporté sans listing RADOS");
                let _ = s;
                0
            }
        }
    }
}

// ─── Point d'entrée ───────────────────────────────────────────────────────────

/// Lance le listener TCP et dispatche chaque connexion dans sa propre tâche Tokio.
///
/// `prebuilt_ceph` : CephStore déjà construit par main (partagé avec le status server).
/// Si `None` et que `cfg.ceph_enabled`, on le reconstruit ici (usage legacy).
pub async fn run(
    cfg: Config,
    prebuilt_ceph: Option<Arc<CephStore>>,
    metrics: Arc<StoreMetrics>,
) -> Result<()> {
    let listener = TcpListener::bind(&cfg.listen).await?;

    // ── TLS optionnel ─────────────────────────────────────────────────────────
    let tls_acceptor: Option<tokio_rustls::TlsAcceptor> = if cfg.tls_enabled {
        let paths = TlsPaths::new(&cfg.tls_dir);
        let ctx = TlsContext::generate_or_load(paths, &cfg.node_id)?;
        info!(
            fingerprint = %ctx.fingerprint,
            tls_dir     = %cfg.tls_dir,
            "TLS activé — empreinte à distribuer aux agents"
        );
        Some(crate::tls::build_tls_acceptor(&ctx)?)
    } else {
        info!("TLS désactivé — canal de paging en clair");
        None
    };

    info!(node_id = %cfg.node_id, listen = %cfg.listen, tls = cfg.tls_enabled, "store démarré");

    // Construire le store selon la configuration
    let store: Arc<AnyStore> = if let Some(ceph) = prebuilt_ceph {
        info!(pool = %cfg.ceph_pool, "store Ceph auto-connecté depuis main");
        Arc::new(AnyStore::Ceph(ceph))
    } else {
        Arc::new(AnyStore::Ram(Arc::new(PageStore::new(metrics.clone()))))
    };

    // Tâche d'affichage périodique des métriques
    {
        let metrics = metrics.clone();
        let interval = cfg.stats_interval;
        let node_id = cfg.node_id.clone();
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(interval));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let snap = metrics.snapshot();
                info!(
                    node_id       = %node_id,
                    pages         = snap.pages_stored,
                    bytes         = snap.estimated_bytes,
                    puts          = snap.put_count,
                    gets          = snap.get_count,
                    hits          = snap.hit_count,
                    misses        = snap.miss_count,
                    hit_rate_pct  = snap.hit_rate_pct,
                    connections   = snap.active_connections,
                    "stats périodiques"
                );
            }
        });
    }

    loop {
        let (stream, peer) = listener.accept().await?;
        let store = store.clone();
        let metrics = metrics.clone();
        let max_pages = cfg.max_pages;
        let node_id = cfg.node_id.clone();

        metrics.connections.fetch_add(1, Ordering::Relaxed);
        info!(peer = %peer, tls = cfg.tls_enabled, "nouvelle connexion");

        match &tls_acceptor {
            Some(acceptor) => {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            if let Err(e) = handle_connection(
                                tls_stream,
                                peer,
                                store,
                                metrics.clone(),
                                max_pages,
                                &node_id,
                            )
                            .await
                            {
                                if !is_connection_reset(&e) {
                                    warn!(peer = %peer, error = %e, "erreur connexion TLS");
                                }
                            }
                        }
                        Err(e) => warn!(peer = %peer, error = %e, "handshake TLS échoué"),
                    }
                    metrics.connections.fetch_sub(1, Ordering::Relaxed);
                    debug!(peer = %peer, "connexion TLS terminée");
                });
            }
            None => {
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_connection(stream, peer, store, metrics.clone(), max_pages, &node_id)
                            .await
                    {
                        if is_connection_reset(&e) {
                            debug!(peer = %peer, "connexion fermée par le client");
                        } else {
                            warn!(peer = %peer, error = %e, "erreur connexion");
                        }
                    }
                    metrics.connections.fetch_sub(1, Ordering::Relaxed);
                    debug!(peer = %peer, "connexion terminée");
                });
            }
        }
    }
}

// ─── Gestion d'une connexion (générique TLS/TCP) ──────────────────────────────

async fn handle_connection<S>(
    stream: S,
    peer: SocketAddr,
    store: Arc<AnyStore>,
    _metrics: Arc<StoreMetrics>,
    max_pages: u64,
    node_id: &str,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    loop {
        let msg = match Message::read_from(&mut reader).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        debug!(
            peer    = %peer,
            opcode  = ?msg.opcode,
            vm_id   = msg.vm_id,
            page_id = msg.page_id,
            "message reçu"
        );

        let response = dispatch(&msg, &store, max_pages, node_id).await;

        if let Err(e) = response.write_to(&mut writer).await {
            error!(peer = %peer, error = %e, "erreur écriture réponse");
            return Err(e.into());
        }
    }
}

// ─── Dispatch (async) ─────────────────────────────────────────────────────────

async fn dispatch(msg: &Message, store: &AnyStore, max_pages: u64, node_id: &str) -> Message {
    match msg.opcode {
        Opcode::Ping => Message::pong(),

        Opcode::PutPage => {
            if max_pages > 0 && store.len() as u64 >= max_pages {
                return Message::error_msg("store plein : limite max_pages atteinte");
            }
            let key = PageKey::new(msg.vm_id, msg.page_id);
            match store.put(key, msg.payload.clone()).await {
                Ok(_) => Message::ok(msg.vm_id, msg.page_id),
                Err(err) => Message::error_msg(&err),
            }
        }

        Opcode::GetPage => {
            let key = PageKey::new(msg.vm_id, msg.page_id);
            match store.get(&key).await {
                Some(data) => Message::new(Opcode::Ok, msg.vm_id, msg.page_id, data),
                None => Message::not_found(msg.vm_id, msg.page_id),
            }
        }

        Opcode::DeletePage => {
            let key = PageKey::new(msg.vm_id, msg.page_id);
            if store.delete(&key).await {
                Message::ok(msg.vm_id, msg.page_id)
            } else {
                Message::not_found(msg.vm_id, msg.page_id)
            }
        }

        Opcode::StatsRequest => {
            let json = serde_json::json!({
                "node_id":         node_id,
                "pages_stored":    store.len(),
                "estimated_bytes": store.estimated_bytes(),
            });
            Message::stats_response(json.to_string())
        }

        Opcode::BatchPutPage => {
            let count = msg.page_id as usize;
            let entry_size = 8 + PAGE_SIZE;
            if msg.payload.len() != count * entry_size {
                return Message::error_msg("BATCH_PUT payload corrompu");
            }
            let vm_id = msg.vm_id;
            let mut stored = 0u32;
            let mut failed = 0u32;
            for i in 0..count {
                let off = i * entry_size;
                let page_id = u64::from_be_bytes(msg.payload[off..off + 8].try_into().unwrap());
                let data = msg.payload[off + 8..off + entry_size].to_vec();
                let key = PageKey::new(vm_id, page_id);
                match store.put(key, data).await {
                    Ok(_) => stored += 1,
                    Err(_) => failed += 1,
                }
            }
            crate::protocol::BatchPutResponse::ok_message(vm_id, stored, failed)
        }

        op => {
            warn!(opcode = ?op, "opcode réponse reçu côté serveur — protocole invalide");
            Message::error_msg("opcode inattendu côté serveur")
        }
    }
}

fn is_connection_reset(e: &anyhow::Error) -> bool {
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        )
    } else {
        false
    }
}
