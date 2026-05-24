//! Interface avec Linux userfaultfd (uffd).
//!
//! # Principe
//!
//! 1. On crée un fd uffd via `SYS_userfaultfd`.
//! 2. On négocie l'API avec `UFFDIO_API`.
//! 3. On enregistre la région mémoire avec `UFFDIO_REGISTER` (mode MISSING).
//! 4. Un thread dédié lit les événements de faute de page sur le fd.
//! 5. Pour chaque faute, on appelle le callback `fault_handler` qui :
//!    a. récupère la page depuis le store distant ;
//!    b. la copie dans la région via `UFFDIO_COPY`.
//!
//! # Sécurité
//!
//! Ce module utilise `unsafe` pour les appels système Linux bas-niveau.
//! Chaque bloc unsafe est justifié par un commentaire inline.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use tracing::{debug, error, info, warn};

use crate::metrics::AgentMetrics;

// ---------------------------------------------------------------------------------
// Constantes Linux userfaultfd (x86-64, kernel ≥ 4.11)
// ---------------------------------------------------------------------------------

/// Numéro de syscall SYS_userfaultfd sur x86-64.
const SYS_USERFAULTFD: libc::c_long = 323;

/// Version de l'API userfaultfd qu'on cible.
const UFFD_API: u64 = 0xAA;

// Valeur de `features` qu'on accepte en V1 (on ne demande pas de feature avancée).
const UFFD_FEATURES_NONE: u64 = 0;

// Modes d'enregistrement
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;

// Numéros ioctl (calculés selon _IOWR(_UFFDIO=0xAA, nr, sizeof(struct)))
// Vérifiables via `strace` ou <linux/userfaultfd.h>.
const UFFDIO_API: libc::c_ulong = 0xC018AA3F;
const UFFDIO_REGISTER: libc::c_ulong = 0xC020AA00;
const UFFDIO_COPY: libc::c_ulong = 0xC028AA03;

// Événement page fault
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

// Flag : la faute est une écriture
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 1;

// ---------------------------------------------------------------------------------
// Structures C correspondant aux ioctl userfaultfd
// ---------------------------------------------------------------------------------

#[repr(C)]
struct UffdioApi {
    api: u64,      // version demandée
    features: u64, // features demandées (entrée) / disponibles (sortie)
    ioctls: u64,   // ioctls disponibles (sortie)
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64, // sortie : ioctls disponibles sur la région
}

#[repr(C)]
struct UffdioCopy {
    dst: u64,  // destination page-alignée dans la région fautée
    src: u64,  // source (notre buffer de page)
    len: u64,  // PAGE_SIZE
    mode: u64, // 0 = wake le thread fauteur
    copy: i64, // sortie : octets copiés (négatif = errno)
}

/// Format du message lu depuis le fd uffd (32 octets sur x86-64).
#[repr(C, packed)]
struct UffdMsg {
    event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    // union — on lit les champs pagefault directement
    flags: u64,
    address: u64,
    ptid: u32,
    _pad: u32,
}

const UFFD_MSG_SIZE: usize = std::mem::size_of::<UffdMsg>();

// ---------------------------------------------------------------------------------
// Handle userfaultfd
// ---------------------------------------------------------------------------------

/// Représente un fd userfaultfd ouvert et l'API négociée.
pub struct UffdHandle {
    fd: RawFd,
}

impl UffdHandle {
    /// Ouvre un fd userfaultfd et négocie l'API avec le kernel.
    pub fn open() -> Result<Self> {
        // SAFETY: syscall standard, flags validés.
        let fd = unsafe {
            libc::syscall(SYS_USERFAULTFD, libc::O_CLOEXEC | libc::O_NONBLOCK) as libc::c_int
        };

        if fd < 0 {
            let err = std::io::Error::last_os_error();
            bail!("SYS_userfaultfd échoué : {err} — vérifiez que le kernel ≥ 4.11 et les droits CAP_SYS_PTRACE ou /proc/sys/vm/unprivileged_userfaultfd=1");
        }

        info!(fd = fd, "userfaultfd ouvert");

        // Négociation API
        let mut api = UffdioApi {
            api: UFFD_API,
            features: UFFD_FEATURES_NONE,
            ioctls: 0,
        };

        // SAFETY: fd valide, structure correctement alignée.
        let ret = unsafe { libc::ioctl(fd, UFFDIO_API, &mut api as *mut _) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: fd valide.
            unsafe { libc::close(fd) };
            bail!("UFFDIO_API échoué : {err}");
        }

        if api.api != UFFD_API {
            // SAFETY: fd valide.
            unsafe { libc::close(fd) };
            bail!(
                "version API incompatible : kernel répond 0x{:02x}, attendu 0x{:02x}",
                api.api,
                UFFD_API
            );
        }

        info!(
            features = api.features,
            ioctls = api.ioctls,
            "API userfaultfd négociée"
        );

        Ok(Self { fd })
    }

    /// Enregistre une région mémoire pour les fautes MISSING.
    pub fn register_region(&self, addr: *mut libc::c_void, size: usize) -> Result<()> {
        let mut reg = UffdioRegister {
            range: UffdioRange {
                start: addr as u64,
                len: size as u64,
            },
            mode: UFFDIO_REGISTER_MODE_MISSING,
            ioctls: 0,
        };

        // SAFETY: fd et reg valides, région mmap valide.
        let ret = unsafe { libc::ioctl(self.fd, UFFDIO_REGISTER, &mut reg as *mut _) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            bail!("UFFDIO_REGISTER échoué : {err}");
        }

        info!(
            addr = format!("{:p}", addr),
            size = size,
            ioctls = reg.ioctls,
            "région enregistrée avec userfaultfd"
        );
        Ok(())
    }

    /// Copie `page_data` vers `fault_addr` (doit être aligné PAGE_SIZE).
    ///
    /// Débloque le thread qui a déclenché la faute de page.
    pub fn copy_page(&self, fault_addr: u64, page_data: &[u8; 4096]) -> Result<()> {
        let mut copy = UffdioCopy {
            dst: fault_addr,
            src: page_data.as_ptr() as u64,
            len: 4096,
            mode: 0, // 0 = wake le thread fauteur immédiatement
            copy: 0,
        };

        // SAFETY: fd valide, src/dst valides et alignés.
        let ret = unsafe { libc::ioctl(self.fd, UFFDIO_COPY, &mut copy as *mut _) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            bail!("UFFDIO_COPY échoué @ 0x{:x} : {err}", fault_addr);
        }

        if copy.copy != 4096 {
            bail!("UFFDIO_COPY partiel : {} octets copiés", copy.copy);
        }

        Ok(())
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for UffdHandle {
    fn drop(&mut self) {
        // SAFETY: fd valide, fermeture propre.
        unsafe { libc::close(self.fd) };
    }
}

// ---------------------------------------------------------------------------------
// Thread de gestion des fautes
// ---------------------------------------------------------------------------------

/// Type du callback appelé pour chaque faute de page.
/// Arguments : (vm_id, page_id, fault_addr, is_write)
/// Retourne : les 4096 octets à injecter dans la région fautée.
pub type FaultHandlerFn = Box<dyn Fn(u32, u64, u64, bool) -> Result<[u8; 4096]> + Send + Sync>;

/// Lance le thread bloquant qui lit les événements uffd.
///
/// Ce thread utilise `read()` bloquant (fd en mode non-blocking uniquement pour
/// l'ouverture — on le repasse en blocking pour la boucle de lecture).
pub fn spawn_fault_handler_thread(
    uffd: UffdHandle,
    region_start: u64,
    page_size: u64,
    vm_id: u32,
    handler: FaultHandlerFn,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<AgentMetrics>,
) -> std::thread::JoinHandle<()> {
    // On passe le fd en bloquant pour la boucle de lecture
    let fd = uffd.fd();

    // SAFETY: fd valide, on retire O_NONBLOCK pour les reads bloquants.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }

    std::thread::Builder::new()
        .name("uffd-handler".to_string())
        .spawn(move || {
            // On prend ownership du UffdHandle ici pour que le fd reste valide
            // pendant toute la durée du thread.
            let _uffd_owner = uffd;
            let mut msg_buf = UffdMsg {
                event: 0,
                reserved1: 0,
                reserved2: 0,
                reserved3: 0,
                flags: 0,
                address: 0,
                ptid: 0,
                _pad: 0,
            };

            info!(tid = ?std::thread::current().id(), "thread uffd-handler démarré");

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    info!("uffd-handler : arrêt demandé");
                    break;
                }

                // Lecture bloquante d'un événement uffd (32 octets)
                // SAFETY: buf de taille correcte, fd valide.
                let n = unsafe {
                    libc::read(
                        fd,
                        &mut msg_buf as *mut UffdMsg as *mut libc::c_void,
                        UFFD_MSG_SIZE,
                    )
                };

                if n == 0 {
                    info!("uffd fd fermé, thread handler s'arrête");
                    break;
                }

                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue; // EINTR — reprendre
                    }
                    error!(error = %err, "read uffd échoué");
                    break;
                }

                if n as usize != UFFD_MSG_SIZE {
                    warn!(
                        n = n,
                        expected = UFFD_MSG_SIZE,
                        "lecture uffd partielle — ignorée"
                    );
                    continue;
                }

                // On ne gère que UFFD_EVENT_PAGEFAULT en V1
                if msg_buf.event != UFFD_EVENT_PAGEFAULT {
                    debug!(event = msg_buf.event, "événement uffd non-pagefault ignoré");
                    continue;
                }

                let fault_addr = msg_buf.address;
                let is_write = (msg_buf.flags & UFFD_PAGEFAULT_FLAG_WRITE) != 0;

                // Calcul du page_id à partir de l'adresse fautée et du début de la région
                let page_id = (fault_addr - region_start) / page_size;
                // Alignement page de l'adresse fautée
                let page_aligned_addr = fault_addr & !(page_size - 1);

                metrics
                    .fault_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                debug!(
                    fault_addr = format!("0x{:x}", fault_addr),
                    page_id = page_id,
                    is_write = is_write,
                    "page fault interceptée"
                );

                // Appel du handler (bloque le réseau — acceptable en V1 car le thread
                // est dédié ; en V2 on passera à un pool de threads)
                match handler(vm_id, page_id, page_aligned_addr, is_write) {
                    Ok(page_data) => {
                        // SAFETY: fd est valide (fd vit aussi longtemps que ce thread),
                        // page_data est un tableau de 4096 octets sur la pile, aligné.
                        if let Err(e) =
                            unsafe { uffdio_copy_raw(fd, page_aligned_addr, &page_data) }
                        {
                            error!(page_id = page_id, error = %e, "UFFDIO_COPY échoué");
                            metrics
                                .fault_errors
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            metrics
                                .fault_served
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            debug!(page_id = page_id, "page injectée avec succès");
                        }
                    }
                    Err(e) => {
                        error!(page_id = page_id, error = %e, "handler de faute échoué");
                        metrics
                            .fault_errors
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // En cas d'échec : injecter une page zéro pour ne pas bloquer
                        // la VM indéfiniment (comportement défensif V1)
                        let zero_page = [0u8; 4096];
                        if let Err(e2) =
                            unsafe { uffdio_copy_raw(fd, page_aligned_addr, &zero_page) }
                        {
                            error!(error = %e2, "injection page zéro de secours également échouée");
                        }
                    }
                }
            }

            info!("thread uffd-handler terminé");
        })
        .expect("impossible de créer le thread uffd-handler")
}

/// Injecte une page de façon proactive (sans faute pendante) — utilisé pour le recall LIFO.
///
/// Mode UFFDIO_COPY_MODE_DONTWAKE (bit 0) : ne réveille pas de thread en attente.
/// EEXIST (errno 17) est ignoré silencieusement : la page est déjà présente.
///
/// # Safety
/// `fd` doit être un fd userfaultfd valide. `page_data` doit pointer vers 4096 octets valides.
/// L'adresse `dst_addr` doit être alignée sur PAGE_SIZE et dans la région enregistrée.
pub unsafe fn recall_inject(fd: RawFd, dst_addr: u64, page_data: &[u8; 4096]) -> Result<()> {
    const UFFDIO_COPY_MODE_DONTWAKE: u64 = 1;
    let mut copy = UffdioCopy {
        dst: dst_addr,
        src: page_data.as_ptr() as u64,
        len: 4096,
        mode: UFFDIO_COPY_MODE_DONTWAKE,
        copy: 0,
    };
    let ret = libc::ioctl(fd, UFFDIO_COPY, &mut copy as *mut _);
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(()); // page déjà présente — race bénigne
        }
        bail!("UFFDIO_COPY recall @ 0x{dst_addr:x} : {err}");
    }
    Ok(())
}

/// Exécute UFFDIO_COPY directement via un fd raw (évite d'avoir un UffdHandle owning).
///
/// # Safety
/// `fd` doit être un fd userfaultfd valide. `page_data` doit pointer vers 4096 octets valides.
unsafe fn uffdio_copy_raw(fd: RawFd, dst_addr: u64, page_data: &[u8; 4096]) -> Result<()> {
    let mut copy = UffdioCopy {
        dst: dst_addr,
        src: page_data.as_ptr() as u64,
        len: 4096,
        mode: 0,
        copy: 0,
    };

    let ret = libc::ioctl(fd, UFFDIO_COPY, &mut copy as *mut _);
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        bail!("UFFDIO_COPY échoué @ 0x{:x} : {err}", dst_addr);
    }

    if copy.copy < 0 {
        bail!(
            "UFFDIO_COPY retourne errno {} @ 0x{:x}",
            -copy.copy,
            dst_addr
        );
    }

    Ok(())
}
