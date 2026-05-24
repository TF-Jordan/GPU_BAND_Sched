//! Client TCP vers les stores distants (nœuds B et C).
//!
//! # TLS optionnel
//!
//! Si `tls_fingerprints` est non vide à la construction, chaque connexion TCP
//! est enveloppée dans un `TlsStream` avec vérification par empreinte (TOFU).
//! Compatible avec le `TlsAcceptor` de `node-bc-store`.
//!
//! # Stratégie de routage
//!
//! La sélection du store cible est déterministe : `store_index = page_id % num_stores`.
//!
//! # Pool de connexions
//!
//! Chaque store dispose de `CONN_POOL_SIZE` connexions persistantes.
//! Les requêtes sont distribuées en round-robin atomique pour permettre
//! aux workers uffd de faire des GET_PAGE en parallèle.

use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use tokio::io::{AsyncRead, AsyncWrite, BufStream, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

use node_bc_store::protocol::{BatchPutRequest, BatchPutResponse, Message, Opcode, PAGE_SIZE};

/// Nombre de connexions TCP maintenues par store.
const CONN_POOL_SIZE: usize = 4;

// ─── Stream abstrait TCP/TLS ──────────────────────────────────────────────────

/// Enveloppe le stream de transport : TCP clair ou TCP+TLS.
/// Les deux variants sont `Unpin` donc on peut utiliser `Pin::new()` directement.
enum AnyBufStream {
    Plain(BufStream<TcpStream>),
    Tls(BufStream<TlsStream<TcpStream>>),
}

impl AsyncRead for AnyBufStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for AnyBufStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

// ─── Connexion individuelle ───────────────────────────────────────────────────

struct StoreConn {
    addr: String,
    stream: Option<AnyBufStream>,
    timeout: Duration,
    connector: Option<Arc<TlsConnector>>,
    server_name: Option<ServerName<'static>>,
}

impl StoreConn {
    fn new(
        addr: String,
        timeout: Duration,
        connector: Option<Arc<TlsConnector>>,
        server_name: Option<ServerName<'static>>,
    ) -> Self {
        Self {
            addr,
            stream: None,
            timeout,
            connector,
            server_name,
        }
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        debug!(addr = %self.addr, "connexion au store");

        let tcp = timeout(self.timeout, TcpStream::connect(&self.addr))
            .await
            .context("timeout connexion store")?
            .with_context(|| format!("connexion TCP vers {} échouée", self.addr))?;
        tcp.set_nodelay(true)?;

        self.stream = Some(match &self.connector {
            Some(connector) => {
                let sn = self
                    .server_name
                    .clone()
                    .context("TLS activé mais server_name absent")?;
                let tls = timeout(self.timeout, connector.connect(sn, tcp))
                    .await
                    .context("timeout handshake TLS")?
                    .context("handshake TLS échoué")?;
                info!(addr = %self.addr, "connecté au store (TLS)");
                AnyBufStream::Tls(BufStream::new(tls))
            }
            None => {
                info!(addr = %self.addr, "connecté au store (TCP clair)");
                AnyBufStream::Plain(BufStream::new(tcp))
            }
        });
        Ok(())
    }

    fn disconnect(&mut self) {
        if self.stream.is_some() {
            warn!(addr = %self.addr, "connexion invalidée — reconnexion au prochain appel");
            self.stream = None;
        }
    }

    async fn send_recv(&mut self, req: Message) -> Result<Message> {
        self.ensure_connected().await?;
        let stream = self.stream.as_mut().unwrap();

        match timeout(self.timeout, req.write_to(stream)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.disconnect();
                return Err(e.into());
            }
            Err(_) => {
                self.disconnect();
                bail!("timeout écriture vers store {}", self.addr);
            }
        }

        match timeout(self.timeout, Message::read_from(stream)).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => {
                self.disconnect();
                Err(e.into())
            }
            Err(_) => {
                self.disconnect();
                bail!("timeout lecture depuis store {}", self.addr);
            }
        }
    }
}

// ─── Pool de connexions pour un store ────────────────────────────────────────

struct ConnPool {
    conns: Vec<Mutex<StoreConn>>,
    next: AtomicUsize,
}

impl ConnPool {
    fn new(
        addr: String,
        timeout: Duration,
        size: usize,
        connector: Option<Arc<TlsConnector>>,
        server_name: Option<ServerName<'static>>,
    ) -> Self {
        let conns = (0..size)
            .map(|_| {
                Mutex::new(StoreConn::new(
                    addr.clone(),
                    timeout,
                    connector.clone(),
                    server_name.clone(),
                ))
            })
            .collect();
        Self {
            conns,
            next: AtomicUsize::new(0),
        }
    }

    async fn send_recv(&self, req: Message) -> Result<Message> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        self.conns[idx].lock().await.send_recv(req).await
    }

    async fn ping(&self) -> bool {
        self.conns[0]
            .lock()
            .await
            .send_recv(Message::ping())
            .await
            .map(|r| matches!(r.opcode, Opcode::Pong))
            .unwrap_or(false)
    }
}

// ─── RemoteStorePool ──────────────────────────────────────────────────────────

/// Pool de connexions vers les N stores (B et C).
///
/// Partagé via `Arc<RemoteStorePool>` entre les workers uffd et le thread principal.
pub struct RemoteStorePool {
    stores: Vec<ConnPool>,
}

impl RemoteStorePool {
    /// Construit le pool.
    ///
    /// `tls_fingerprints` : empreintes SHA-256 (hex) des stores de confiance.
    /// Si vide, le canal est en TCP clair. Si non vide, TLS TOFU est activé.
    pub fn new(addrs: Vec<String>, timeout_ms: u64, tls_fingerprints: Vec<String>) -> Self {
        let t = Duration::from_millis(timeout_ms);

        let connector: Option<Arc<TlsConnector>> = if tls_fingerprints.is_empty() {
            None
        } else {
            let client_cfg = node_bc_store::tls::TlsContext::client_config(tls_fingerprints)
                .expect("construction TlsConnector échouée");
            Some(Arc::new(TlsConnector::from(client_cfg)))
        };

        let stores = addrs
            .into_iter()
            .map(|addr| {
                // Dériver le ServerName depuis l'adresse IP (ex: "10.10.0.12:9100")
                let server_name = connector.as_ref().and_then(|_| {
                    let host = addr.split(':').next().unwrap_or(&addr);
                    IpAddr::from_str(host)
                        .ok()
                        .map(|ip| ServerName::IpAddress(ip.into()))
                });
                ConnPool::new(addr, t, CONN_POOL_SIZE, connector.clone(), server_name)
            })
            .collect();

        Self { stores }
    }

    fn store_index(&self, page_id: u64) -> usize {
        (page_id as usize) % self.stores.len()
    }

    fn replica_index(&self, page_id: u64) -> usize {
        (self.store_index(page_id) + 1) % self.stores.len()
    }

    pub async fn put_page(&self, vm_id: u32, page_id: u64, data: Vec<u8>) -> Result<()> {
        if data.len() != PAGE_SIZE {
            bail!(
                "put_page : taille incorrecte {} (attendu {})",
                data.len(),
                PAGE_SIZE
            );
        }
        let idx = self.store_index(page_id);
        let base = Message::put_page(vm_id, page_id, data);
        let req = base.try_compress().unwrap_or(base);
        let resp = self.stores[idx]
            .send_recv(req)
            .await
            .with_context(|| format!("PUT_PAGE vm={vm_id} page={page_id} vers store[{idx}]"))?;

        match resp.opcode {
            Opcode::Ok => {
                debug!(vm_id, page_id, store_idx = idx, "PUT_PAGE ok");
                Ok(())
            }
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("PUT_PAGE refusé : {m}")
            }
            op => bail!("PUT_PAGE réponse inattendue : {op:?}"),
        }
    }

    pub async fn get_page(&self, vm_id: u32, page_id: u64) -> Result<Option<Vec<u8>>> {
        let idx = self.store_index(page_id);
        let req = Message::get_page(vm_id, page_id);
        let resp = self.stores[idx]
            .send_recv(req)
            .await
            .with_context(|| format!("GET_PAGE vm={vm_id} page={page_id} depuis store[{idx}]"))?;

        match resp.opcode {
            Opcode::Ok => {
                if resp.payload.len() != PAGE_SIZE {
                    bail!(
                        "GET_PAGE : réponse taille incorrecte {}",
                        resp.payload.len()
                    );
                }
                debug!(vm_id, page_id, store_idx = idx, "GET_PAGE hit");
                Ok(Some(resp.payload))
            }
            Opcode::NotFound => {
                debug!(vm_id, page_id, store_idx = idx, "GET_PAGE miss");
                Ok(None)
            }
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("GET_PAGE erreur store : {m}")
            }
            op => bail!("GET_PAGE réponse inattendue : {op:?}"),
        }
    }

    pub async fn delete_page(&self, vm_id: u32, page_id: u64) -> Result<bool> {
        let idx = self.store_index(page_id);
        let req = Message::delete_page(vm_id, page_id);
        let resp = self.stores[idx]
            .send_recv(req)
            .await
            .with_context(|| format!("DELETE_PAGE vm={vm_id} page={page_id}"))?;

        match resp.opcode {
            Opcode::Ok => Ok(true),
            Opcode::NotFound => Ok(false),
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("DELETE_PAGE erreur : {m}")
            }
            op => bail!("DELETE_PAGE réponse inattendue : {op:?}"),
        }
    }

    /// Envoie N pages en une seule trame BATCH_PUT, groupées par store.
    pub async fn batch_put_pages(&self, vm_id: u32, pages: Vec<(u64, Vec<u8>)>) -> Result<u32> {
        if pages.is_empty() {
            return Ok(0);
        }

        let mut by_store: Vec<Vec<(u64, Vec<u8>)>> = vec![Vec::new(); self.stores.len()];
        for (pid, data) in pages {
            by_store[self.store_index(pid)].push((pid, data));
        }

        let mut total_stored = 0u32;
        for (idx, store_pages) in by_store.into_iter().enumerate() {
            if store_pages.is_empty() {
                continue;
            }

            let mut req = BatchPutRequest::new(vm_id);
            for (pid, data) in store_pages {
                req.push(pid, data);
            }

            let slot_idx = self.stores[idx].next.fetch_add(1, Ordering::Relaxed)
                % self.stores[idx].conns.len();
            let mut conn = self.stores[idx].conns[slot_idx].lock().await;
            conn.ensure_connected().await?;

            let write_result = {
                let stream = conn.stream.as_mut().unwrap();
                req.write_to(stream).await
            };
            if let Err(e) = write_result {
                conn.stream = None;
                return Err(e.into());
            }

            let read_result = {
                let stream = conn.stream.as_mut().unwrap();
                BatchPutResponse::read_from(stream).await
            };
            let resp = match read_result {
                Ok(r) => r,
                Err(e) => {
                    conn.stream = None;
                    return Err(e.into());
                }
            };

            total_stored += resp.stored;
            debug!(
                vm_id,
                store_idx = idx,
                stored = resp.stored,
                failed = resp.failed,
                "BATCH_PUT ok"
            );
        }

        Ok(total_stored)
    }

    /// Envoie une page vers un store spécifique (routage dynamique).
    pub async fn put_page_to(
        &self,
        vm_id: u32,
        page_id: u64,
        data: Vec<u8>,
        store_idx: usize,
    ) -> Result<()> {
        if data.len() != PAGE_SIZE {
            bail!("put_page_to : taille incorrecte {}", data.len());
        }
        if store_idx >= self.stores.len() {
            bail!(
                "put_page_to : store_idx={store_idx} hors limites ({})",
                self.stores.len()
            );
        }
        let base = Message::put_page(vm_id, page_id, data);
        let req = base.try_compress().unwrap_or(base);
        let resp = self.stores[store_idx]
            .send_recv(req)
            .await
            .with_context(|| format!("PUT_PAGE_TO vm={vm_id} page={page_id} store[{store_idx}]"))?;

        match resp.opcode {
            Opcode::Ok => Ok(()),
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("PUT_PAGE_TO refusé : {m}")
            }
            op => bail!("PUT_PAGE_TO réponse inattendue : {op:?}"),
        }
    }

    /// Récupère une page depuis un store spécifique (routage dynamique).
    pub async fn get_page_from(
        &self,
        vm_id: u32,
        page_id: u64,
        store_idx: usize,
    ) -> Result<Option<Vec<u8>>> {
        if store_idx >= self.stores.len() {
            bail!("get_page_from : store_idx={store_idx} hors limites");
        }
        let req = Message::get_page(vm_id, page_id);
        let resp = self.stores[store_idx]
            .send_recv(req)
            .await
            .with_context(|| {
                format!("GET_PAGE_FROM vm={vm_id} page={page_id} store[{store_idx}]")
            })?;

        match resp.opcode {
            Opcode::Ok => {
                if resp.payload.len() != PAGE_SIZE {
                    bail!("GET_PAGE_FROM taille incorrecte {}", resp.payload.len());
                }
                Ok(Some(resp.payload))
            }
            Opcode::NotFound => Ok(None),
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("GET_PAGE_FROM erreur : {m}")
            }
            op => bail!("GET_PAGE_FROM réponse inattendue : {op:?}"),
        }
    }

    pub async fn put_page_replica(&self, vm_id: u32, page_id: u64, data: Vec<u8>) -> Result<()> {
        let idx = self.replica_index(page_id);
        let base = Message::put_page(vm_id, page_id, data);
        let req = base.try_compress().unwrap_or(base);
        let resp = self.stores[idx].send_recv(req).await.with_context(|| {
            format!("PUT_PAGE_REPLICA vm={vm_id} page={page_id} vers store[{idx}]")
        })?;

        match resp.opcode {
            Opcode::Ok => Ok(()),
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("PUT réplica refusé : {m}")
            }
            op => bail!("PUT réplica réponse inattendue : {op:?}"),
        }
    }

    /// Supprime une page d'un store spécifique (routage dynamique).
    pub async fn delete_page_from(
        &self,
        vm_id: u32,
        page_id: u64,
        store_idx: usize,
    ) -> Result<bool> {
        if store_idx >= self.stores.len() {
            bail!("delete_page_from : store_idx={store_idx} hors limites");
        }
        let req = Message::delete_page(vm_id, page_id);
        let resp = self.stores[store_idx]
            .send_recv(req)
            .await
            .with_context(|| {
                format!("DELETE_PAGE_FROM vm={vm_id} page={page_id} store[{store_idx}]")
            })?;

        match resp.opcode {
            Opcode::Ok => Ok(true),
            Opcode::NotFound => Ok(false),
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("DELETE_PAGE_FROM erreur : {m}")
            }
            op => bail!("DELETE_PAGE_FROM réponse inattendue : {op:?}"),
        }
    }

    pub async fn get_page_replica(&self, vm_id: u32, page_id: u64) -> Result<Option<Vec<u8>>> {
        let idx = self.replica_index(page_id);
        let req = Message::get_page(vm_id, page_id);
        let resp = self.stores[idx].send_recv(req).await.with_context(|| {
            format!("GET_PAGE_REPLICA vm={vm_id} page={page_id} depuis store[{idx}]")
        })?;

        match resp.opcode {
            Opcode::Ok => Ok(Some(resp.payload)),
            Opcode::NotFound => Ok(None),
            Opcode::Error => {
                let m = String::from_utf8_lossy(&resp.payload);
                bail!("GET réplica erreur : {m}")
            }
            op => bail!("GET réplica réponse inattendue : {op:?}"),
        }
    }

    pub async fn delete_page_replica(&self, vm_id: u32, page_id: u64) -> Result<bool> {
        let idx = self.replica_index(page_id);
        let req = Message::delete_page(vm_id, page_id);
        let resp = self.stores[idx]
            .send_recv(req)
            .await
            .with_context(|| format!("DELETE_PAGE_REPLICA vm={vm_id} page={page_id}"))?;

        match resp.opcode {
            Opcode::Ok => Ok(true),
            Opcode::NotFound => Ok(false),
            op => bail!("DELETE réplica réponse inattendue : {op:?}"),
        }
    }

    pub async fn ping_all(&self) -> Vec<(usize, bool)> {
        let mut results = Vec::new();
        for (idx, pool) in self.stores.iter().enumerate() {
            results.push((idx, pool.ping().await));
        }
        results
    }

    pub fn num_stores(&self) -> usize {
        self.stores.len()
    }
}
