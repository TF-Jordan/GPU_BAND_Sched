//! Thread pool pour le handler userfaultfd — correction de la limite L6.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Thread reader (1)  — bloquant sur read(uffd_fd, &msg)      │
//! │                                                             │
//! │         ↓  mpsc::channel  (FaultRequest)                    │
//! │                                                             │
//! │  Worker pool (N threads)                                    │
//! │    ├─ Worker 0 : GET_PAGE store → UFFDIO_COPY              │
//! │    ├─ Worker 1 : GET_PAGE store → UFFDIO_COPY              │
//! │    └─ Worker N : GET_PAGE store → UFFDIO_COPY              │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! Le thread reader est le seul à appeler `read(uffd_fd)`.
//! Il dispatche chaque faute vers un worker via un canal.
//! Les workers appellent `GET_PAGE` en parallèle et injectent la page
//! via `UFFDIO_COPY` (plusieurs ioctl simultanés sur le même fd uffd sont
//! autorisés par le kernel — adresses de destination distinctes).
//!
//! # Dégradation gracieuse sous charge
//!
//! Si tous les workers sont occupés et le canal est plein, le thread reader
//! applique une backpressure : il attend qu'un slot se libère (channel borné).
//! Le kernel met en attente le thread fauteur en espace noyau — la VM ne crash pas.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use anyhow::Result;
use tracing::{debug, error, info, warn};

use crate::metrics::AgentMetrics;

// ─── FFI minimale (ré-exposition depuis uffd.rs) ──────────────────────────────

#[repr(C, packed)]
struct UffdMsg {
    event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    flags: u64,
    address: u64,
    ptid: u32,
    _pad: u32,
}

const UFFD_MSG_SIZE: usize = std::mem::size_of::<UffdMsg>();
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 1;
const UFFDIO_COPY: libc::c_ulong = 0xC028AA03;

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

// ─── Types ────────────────────────────────────────────────────────────────────

/// Requête de faute envoyée du reader vers les workers.
pub struct FaultRequest {
    pub page_id: u64,
    pub fault_addr: u64, // adresse page-alignée
    pub is_write: bool,
}

/// Handler appelé par chaque worker pour résoudre une faute.
/// Retourne les 4096 octets à injecter.
pub type FaultHandlerFn = Arc<dyn Fn(u64, bool) -> Result<[u8; 4096]> + Send + Sync>;

/// Configuration du pool.
pub struct PoolConfig {
    pub num_workers: usize,
    pub channel_cap: usize,
    pub region_start: u64,
    pub page_size: u64,
    pub vm_id: u32,
}

// ─── Pool ─────────────────────────────────────────────────────────────────────

/// Lance le pool uffd-handler : 1 reader + N workers.
///
/// Retourne les JoinHandle des threads lancés.
pub fn spawn_uffd_pool(
    uffd_fd: RawFd,
    cfg: PoolConfig,
    handler: FaultHandlerFn,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<AgentMetrics>,
) -> Vec<thread::JoinHandle<()>> {
    let mut handles = Vec::new();

    // Canal borné : backpressure si workers surchargés
    let (tx, rx) = mpsc::sync_channel::<FaultRequest>(cfg.channel_cap);
    let rx = Arc::new(std::sync::Mutex::new(rx));

    // ── Lancement des workers ──────────────────────────────────────────────
    for worker_id in 0..cfg.num_workers {
        let rx = rx.clone();
        let handler = handler.clone();
        let metrics = metrics.clone();
        let shutdown = shutdown.clone();

        let handle = thread::Builder::new()
            .name(format!("uffd-worker-{}", worker_id))
            .spawn(move || {
                info!(worker_id, "worker uffd démarré");
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    // Attente d'une requête (timeout 100ms pour vérifier shutdown)
                    let req = {
                        let guard = rx.lock().unwrap();
                        guard
                            .recv_timeout(std::time::Duration::from_millis(100))
                            .ok()
                    };

                    let Some(req) = req else { continue };

                    debug!(worker_id, page_id = req.page_id, "worker traite une faute");

                    match handler(req.page_id, req.is_write) {
                        Ok(page_data) => {
                            // SAFETY: fd valide (même durée de vie que le pool),
                            // page_data et fault_addr valides.
                            if let Err(e) =
                                unsafe { copy_page_raw(uffd_fd, req.fault_addr, &page_data) }
                            {
                                error!(page_id = req.page_id, error = %e, "UFFDIO_COPY échoué");
                                metrics.fault_errors.fetch_add(1, Ordering::Relaxed);
                            } else {
                                metrics.fault_served.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            error!(page_id = req.page_id, error = %e, "handler échoué — page zéro");
                            metrics.fault_errors.fetch_add(1, Ordering::Relaxed);
                            // Injection page zéro pour débloquer la VM
                            let zero = [0u8; 4096];
                            let _ = unsafe { copy_page_raw(uffd_fd, req.fault_addr, &zero) };
                        }
                    }
                }
                info!(worker_id, "worker uffd terminé");
            })
            .expect("impossible de créer le thread worker uffd");

        handles.push(handle);
    }

    // ── Thread reader (unique, bloquant) ──────────────────────────────────
    {
        let shutdown = shutdown.clone();
        let metrics = metrics.clone();
        let region_start = cfg.region_start;
        let page_size = cfg.page_size;

        // Passer le fd en mode bloquant
        unsafe {
            let flags = libc::fcntl(uffd_fd, libc::F_GETFL);
            libc::fcntl(uffd_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }

        let handle = thread::Builder::new()
            .name("uffd-reader".into())
            .spawn(move || {
                info!("thread uffd-reader démarré");
                let mut msg = UffdMsg {
                    event: 0,
                    reserved1: 0,
                    reserved2: 0,
                    reserved3: 0,
                    flags: 0,
                    address: 0,
                    ptid: 0,
                    _pad: 0,
                };

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    // Lecture bloquante d'un événement uffd
                    let n = unsafe {
                        libc::read(
                            uffd_fd,
                            &mut msg as *mut UffdMsg as *mut libc::c_void,
                            UFFD_MSG_SIZE,
                        )
                    };

                    if n == 0 {
                        info!("uffd fd fermé");
                        break;
                    }
                    if n < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        error!(error = %e, "read uffd échoué");
                        break;
                    }
                    if n as usize != UFFD_MSG_SIZE {
                        continue;
                    }
                    if msg.event != UFFD_EVENT_PAGEFAULT {
                        continue;
                    }

                    let fault_addr = msg.address;
                    let is_write = (msg.flags & UFFD_PAGEFAULT_FLAG_WRITE) != 0;
                    let page_id = (fault_addr - region_start) / page_size;
                    let aligned = fault_addr & !(page_size - 1);

                    metrics.fault_count.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        page_id,
                        fault_addr = format!("0x{:x}", fault_addr),
                        is_write,
                        "faute reçue"
                    );

                    let req = FaultRequest {
                        page_id,
                        fault_addr: aligned,
                        is_write,
                    };

                    // Envoi vers le pool (bloquant si canal plein → backpressure)
                    if tx.send(req).is_err() {
                        warn!("canal vers workers fermé — reader s'arrête");
                        break;
                    }
                }
                info!("thread uffd-reader terminé");
            })
            .expect("impossible de créer le thread uffd-reader");

        handles.push(handle);
    }

    handles
}

/// UFFDIO_COPY via fd raw.
///
/// # Safety
/// `fd` valide, `page_data` pointe vers 4096 octets valides et alignés.
unsafe fn copy_page_raw(fd: RawFd, dst_addr: u64, page_data: &[u8; 4096]) -> Result<()> {
    let mut copy = UffdioCopy {
        dst: dst_addr,
        src: page_data.as_ptr() as u64,
        len: 4096,
        mode: 0,
        copy: 0,
    };
    let ret = libc::ioctl(fd, UFFDIO_COPY, &mut copy as *mut _);
    if ret < 0 {
        let e = std::io::Error::last_os_error();
        anyhow::bail!("UFFDIO_COPY échoué @ 0x{:x} : {e}", dst_addr);
    }
    if copy.copy < 0 {
        anyhow::bail!("UFFDIO_COPY errno={} @ 0x{:x}", -copy.copy, dst_addr);
    }
    Ok(())
}
