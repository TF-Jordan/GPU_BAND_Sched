//! Serveur TCP store — wrapper autour du serveur V1 avec intégration VmTracker.
//!
//! Réutilise la logique de `node-bc-store` en interceptant les opérations PUT/DELETE
//! pour mettre à jour le `VmTracker` (compteur de pages par vm_id).

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use node_bc_store::metrics::StoreMetrics;
use node_bc_store::protocol::{Message, Opcode};
use node_bc_store::store::{PageKey, PageStore};

use crate::vm_tracker::VmTracker;

/// Lance le serveur TCP du store avec notification au VmTracker.
pub async fn run_store_server(
    listen_addr: String,
    store: Arc<PageStore>,
    metrics: Arc<StoreMetrics>,
    vm_tracker: Arc<VmTracker>,
    max_pages: u64,
    node_id: String,
) -> Result<()> {
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(
        node_id  = %node_id,
        listen   = %listen_addr,
        "store TCP démarré"
    );

    loop {
        let (stream, peer) = listener.accept().await?;
        let store = store.clone();
        let metrics = metrics.clone();
        let vm_tracker = vm_tracker.clone();
        let node_id = node_id.clone();

        metrics.connections.fetch_add(1, Ordering::Relaxed);
        debug!(peer = %peer, "connexion store");

        tokio::spawn(async move {
            if let Err(e) = handle_client(
                stream,
                peer,
                store,
                metrics.clone(),
                vm_tracker,
                max_pages,
                &node_id,
            )
            .await
            {
                if !is_normal_disconnect(&e) {
                    warn!(peer = %peer, error = %e, "erreur connexion store");
                }
            }
            metrics.connections.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn handle_client(
    mut stream: TcpStream,
    _peer: SocketAddr,
    store: Arc<PageStore>,
    _metrics: Arc<StoreMetrics>,
    vm_tracker: Arc<VmTracker>,
    max_pages: u64,
    node_id: &str,
) -> Result<()> {
    let (mut reader, mut writer) = stream.split();

    loop {
        let msg = match Message::read_from(&mut reader).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        let response = dispatch_with_tracking(&msg, &store, &vm_tracker, max_pages, node_id);
        response.write_to(&mut writer).await?;
    }
}

/// Dispatch avec mise à jour du VmTracker sur PUT/DELETE.
fn dispatch_with_tracking(
    msg: &Message,
    store: &Arc<PageStore>,
    vm_tracker: &Arc<VmTracker>,
    max_pages: u64,
    node_id: &str,
) -> Message {
    match msg.opcode {
        Opcode::Ping => Message::pong(),

        Opcode::PutPage => {
            if max_pages > 0 && store.len() as u64 >= max_pages {
                return Message::error_msg("store plein");
            }
            let key = PageKey::new(msg.vm_id, msg.page_id);
            match store.put(key, msg.payload.clone()) {
                Ok(was_update) => {
                    if !was_update {
                        // Nouvelle page : incrémenter le compteur
                        vm_tracker.record_page_stored(msg.vm_id, 1);
                    }
                    Message::ok(msg.vm_id, msg.page_id)
                }
                Err(e) => Message::error_msg(&e),
            }
        }

        Opcode::GetPage => {
            let key = PageKey::new(msg.vm_id, msg.page_id);
            match store.get(&key) {
                Some(data) => Message::new(Opcode::Ok, msg.vm_id, msg.page_id, data),
                None => Message::not_found(msg.vm_id, msg.page_id),
            }
        }

        Opcode::DeletePage => {
            let key = PageKey::new(msg.vm_id, msg.page_id);
            if store.delete(&key) {
                vm_tracker.record_page_stored(msg.vm_id, -1);
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
            })
            .to_string();
            Message::stats_response(json)
        }

        op => {
            warn!(opcode = ?op, "opcode réponse reçu côté serveur");
            Message::error_msg("opcode inattendu")
        }
    }
}

fn is_normal_disconnect(e: &anyhow::Error) -> bool {
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
