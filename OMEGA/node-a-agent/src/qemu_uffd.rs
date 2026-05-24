//! Récepteur du fd userfaultfd envoyé par le bridge LD_PRELOAD depuis QEMU.
//!
//! # Flux
//!
//! 1. L'agent crée `{run_dir}/vm-{vmid}/uffd.sock` et y écoute.
//! 2. Quand QEMU démarre avec LD_PRELOAD=omega-uffd-bridge.so :
//!    – bridge.c intercepte le mmap() de `/dev/shm/omega-vm-{vmid}`
//!      ou du memfd `omega-vm-{vmid}-...`
//!    – bridge.c crée un uffd, enregistre la plage QEMU (MISSING mode)
//!    – bridge.c se connecte au socket et envoie le fd via SCM_RIGHTS
//!      avec payload `[base: u64, len: u64]`
//! 3. Ce module spawne un thread de gestion des fautes QEMU :
//!    – lit les événements uffd (bloquant)
//!    – pour chaque PAGEFAULT : GET_PAGE depuis le store → UFFDIO_COPY
//!
//! # Éviction
//!
//! L'agent mappe aussi le même backend partagé (`memfd` ou shm) en MAP_SHARED.
//! `evict_page()` lit via cette vue, envoie au store, puis appelle
//! MADV_DONTNEED sur sa propre vue. Le mapping QEMU est enregistré auprès
//! de userfaultfd par le bridge, donc une page absente déclenche ce handler,
//! qui injecte la page depuis le store.

use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use libc;
use tokio::runtime::Handle;
use tracing::{debug, error, info, warn};

use crate::memory::PAGE_SIZE;
use crate::metrics::AgentMetrics;
use crate::remote::RemoteStorePool;

// ── constantes userfaultfd ────────────────────────────────────────────────────

const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 1;
const UFFDIO_COPY: libc::c_ulong = 0xC028_AA03;

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

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

// ── réception du fd uffd via SCM_RIGHTS ──────────────────────────────────────

fn receive_uffd(conn_fd: RawFd) -> Result<(RawFd, u64, u64)> {
    // Payload : [base: u64, len: u64]
    let mut payload = [0u64; 2];

    // Buffer cmsg large (CMSG_SPACE(sizeof(int)) ≈ 16 octets sur x86-64)
    let mut cmsg_buf = [0u8; 64];

    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr() as *mut libc::c_void,
        iov_len: std::mem::size_of_val(&payload),
    };

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_buf.len() as _;

    let n = unsafe { libc::recvmsg(conn_fd, &mut msg, 0) };
    if n < 0 {
        bail!("recvmsg: {}", std::io::Error::last_os_error());
    }
    if (n as usize) != std::mem::size_of_val(&payload) {
        bail!("recvmsg: payload partiel ({n} octets, attendu 16)");
    }

    // Extraire le fd uffd depuis le premier cmsg SCM_RIGHTS
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        bail!("recvmsg: pas de cmsg — fd uffd manquant");
    }

    let cmsg_ref = unsafe { &*cmsg };
    if cmsg_ref.cmsg_level != libc::SOL_SOCKET || cmsg_ref.cmsg_type != libc::SCM_RIGHTS {
        bail!(
            "recvmsg: cmsg inattendu level={} type={}",
            cmsg_ref.cmsg_level,
            cmsg_ref.cmsg_type
        );
    }

    let mut uffd_fd: RawFd = -1;
    unsafe {
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg),
            &mut uffd_fd as *mut RawFd as *mut u8,
            std::mem::size_of::<RawFd>(),
        );
    }

    if uffd_fd < 0 {
        bail!("fd uffd reçu invalide ({uffd_fd})");
    }

    let base = payload[0];
    let len = payload[1];

    info!(
        uffd_fd,
        base = format!("0x{base:x}"),
        len,
        "fd uffd reçu depuis bridge.c"
    );
    Ok((uffd_fd, base, len))
}

// ── thread de gestion des fautes QEMU ────────────────────────────────────────

fn spawn_qemu_fault_handler(
    uffd_fd: RawFd,
    base: u64,
    len: u64,
    store: Arc<RemoteStorePool>,
    vm_id: u32,
    metrics: Arc<AgentMetrics>,
    tokio_handle: Handle,
    shutdown: Arc<AtomicBool>,
    uffd_active: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<()> {
    // Repasser le fd en mode bloquant (bridge.c l'ouvre avec O_NONBLOCK)
    unsafe {
        let flags = libc::fcntl(uffd_fd, libc::F_GETFL);
        libc::fcntl(uffd_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }

    std::thread::Builder::new()
        .name(format!("qemu-uffd-{vm_id}"))
        .spawn(move || {
            uffd_active.fetch_add(1, Ordering::Relaxed);
            info!(
                vm_id,
                base = format!("0x{base:x}"),
                len,
                "thread qemu-uffd-handler démarré"
            );

            let page_size = PAGE_SIZE as u64;
            let mut msg: UffdMsg = unsafe { std::mem::zeroed() };

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let n = unsafe {
                    libc::read(
                        uffd_fd,
                        &mut msg as *mut UffdMsg as *mut libc::c_void,
                        UFFD_MSG_SIZE,
                    )
                };

                if n == 0 {
                    // QEMU a quitté → EOF sur l'uffd → on signale l'arrêt proprement.
                    // Pas de kill() : run_daemon surveille uffd_active et s'arrêtera
                    // dès que ce compteur atteint zéro.
                    info!(vm_id, "uffd fd QEMU fermé (QEMU arrêté) — arrêt agent");
                    shutdown.store(true, Ordering::Relaxed);
                    break;
                }
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    error!(error = %err, "read uffd QEMU échoué");
                    break;
                }
                if (n as usize) != UFFD_MSG_SIZE {
                    warn!(
                        n,
                        expected = UFFD_MSG_SIZE,
                        "lecture uffd partielle ignorée"
                    );
                    continue;
                }
                if msg.event != UFFD_EVENT_PAGEFAULT {
                    debug!(event = msg.event, "événement uffd non-pagefault ignoré");
                    continue;
                }

                let fault_addr = msg.address;
                let _is_write = (msg.flags & UFFD_PAGEFAULT_FLAG_WRITE) != 0;
                let page_id = (fault_addr.saturating_sub(base)) / page_size;
                let page_addr = fault_addr & !(page_size - 1);

                metrics.fault_count.fetch_add(1, Ordering::Relaxed);
                debug!(
                    page_id,
                    fault_addr = format!("0x{fault_addr:x}"),
                    "QEMU page fault"
                );

                // Récupérer la page depuis le store distant.
                // catch_unwind protège contre le panic "Tokio runtime is being shutdown"
                // qui survient si le runtime est en cours d'arrêt (SIGTERM reçu) pendant
                // qu'un block_on est en cours.
                let data = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tokio_handle.block_on(store.get_page(vm_id, page_id))
                })) {
                    Ok(result) => result,
                    Err(_) => {
                        info!(
                            vm_id,
                            "runtime tokio arrêté pendant block_on — sortie du handler"
                        );
                        break;
                    }
                };

                let page_data: [u8; 4096] = match data {
                    Ok(Some(bytes)) if bytes.len() == 4096 => {
                        let mut arr = [0u8; 4096];
                        arr.copy_from_slice(&bytes);
                        metrics.pages_fetched.fetch_add(1, Ordering::Relaxed);
                        arr
                    }
                    Ok(_) => {
                        // Page absente du store : première allocation QEMU → zéro
                        metrics.fetch_zeros.fetch_add(1, Ordering::Relaxed);
                        [0u8; 4096]
                    }
                    Err(e) => {
                        error!(page_id, error = %e, "GET_PAGE échoué — page zéro de secours");
                        metrics.fault_errors.fetch_add(1, Ordering::Relaxed);
                        [0u8; 4096]
                    }
                };

                // Injecter la page dans l'espace virtuel de QEMU via UFFDIO_COPY
                // src = pointeur dans l'espace agent (valide ici)
                // dst = adresse dans l'espace QEMU (kernel fait la copie cross-process)
                let mut copy = UffdioCopy {
                    dst: page_addr,
                    src: page_data.as_ptr() as u64,
                    len: 4096,
                    mode: 0,
                    copy: 0,
                };

                let ret = unsafe { libc::ioctl(uffd_fd, UFFDIO_COPY, &mut copy as *mut _) };
                if ret < 0 {
                    let err = std::io::Error::last_os_error();
                    error!(page_id, error = %err, "UFFDIO_COPY échoué");
                    metrics.fault_errors.fetch_add(1, Ordering::Relaxed);
                } else if copy.copy < 0 {
                    warn!(page_id, copy.copy, "UFFDIO_COPY errno interne");
                    metrics.fault_errors.fetch_add(1, Ordering::Relaxed);
                } else {
                    metrics.fault_served.fetch_add(1, Ordering::Relaxed);
                    debug!(page_id, "page injectée dans QEMU");
                }
            }

            unsafe {
                libc::close(uffd_fd);
            }
            uffd_active.fetch_sub(1, Ordering::Relaxed);
            info!(vm_id, "thread qemu-uffd-handler terminé");
        })
        .expect("impossible de créer le thread qemu-uffd-handler")
}

// ── listener Unix socket ──────────────────────────────────────────────────────

/// Démarre le thread listener qui attend la connexion de bridge.c.
///
/// Ce thread :
/// 1. Crée le socket Unix à `socket_path` et écoute.
/// 2. Pour chaque connexion (normalement une seule par VM/démarrage) :
///    – reçoit le fd uffd + (base, len)
///    – spawne un thread `qemu-uffd-{vm_id}` de gestion des fautes
pub fn spawn_qemu_uffd_listener(
    socket_path: PathBuf,
    store: Arc<RemoteStorePool>,
    vm_id: u32,
    metrics: Arc<AgentMetrics>,
    tokio_handle: Handle,
    shutdown: Arc<AtomicBool>,
    uffd_active: Arc<AtomicUsize>,
) -> Result<std::thread::JoinHandle<()>> {
    // Supprimer un éventuel socket résiduel du démarrage précédent
    let _ = std::fs::remove_file(&socket_path);

    // Créer le socket Unix non-bloquant (pour pouvoir checker shutdown)
    let sock = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if sock < 0 {
        bail!("socket(AF_UNIX): {}", std::io::Error::last_os_error());
    }

    unsafe {
        let flags = libc::fcntl(sock, libc::F_GETFL);
        libc::fcntl(sock, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let path_str = socket_path.to_str().context("chemin socket non-UTF8")?;

    if path_str.len() >= 108 {
        unsafe {
            libc::close(sock);
        }
        bail!("chemin socket trop long (max 107 octets) : {path_str}");
    }

    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path_bytes = path_str.as_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            sa.sun_path.as_mut_ptr() as *mut u8,
            path_bytes.len(),
        );
    }

    let bind_ret = unsafe {
        libc::bind(
            sock,
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if bind_ret < 0 {
        unsafe {
            libc::close(sock);
        }
        bail!(
            "bind({}): {}",
            socket_path.display(),
            std::io::Error::last_os_error()
        );
    }

    if unsafe { libc::listen(sock, 4) } < 0 {
        unsafe {
            libc::close(sock);
        }
        bail!("listen: {}", std::io::Error::last_os_error());
    }

    info!(path = %socket_path.display(), "socket uffd QEMU en écoute");

    let sock_path_clone = socket_path.clone();
    let handle = std::thread::Builder::new()
        .name(format!("uffd-listener-{vm_id}"))
        .spawn(move || {
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let conn =
                    unsafe { libc::accept(sock, std::ptr::null_mut(), std::ptr::null_mut()) };

                if conn < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.raw_os_error() == Some(libc::EAGAIN)
                    {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    error!(error = %err, "accept() échoué");
                    break;
                }

                match receive_uffd(conn) {
                    Ok((uffd_fd, base, len)) => {
                        unsafe {
                            libc::close(conn);
                        }
                        spawn_qemu_fault_handler(
                            uffd_fd,
                            base,
                            len,
                            store.clone(),
                            vm_id,
                            metrics.clone(),
                            tokio_handle.clone(),
                            shutdown.clone(),
                            uffd_active.clone(),
                        );
                    }
                    Err(e) => {
                        error!(error = %e, "receive_uffd échoué");
                        unsafe {
                            libc::close(conn);
                        }
                    }
                }
            }

            unsafe {
                libc::close(sock);
            }
            let _ = std::fs::remove_file(&sock_path_clone);
            info!(vm_id, "listener uffd QEMU arrêté");
        })
        .context("impossible de créer le thread uffd-listener")?;

    Ok(handle)
}
