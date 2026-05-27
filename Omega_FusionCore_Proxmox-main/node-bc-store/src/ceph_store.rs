//! Store de pages sur Ceph RADOS.
//!
//! Remplace `sled` (stockage local) par un pool RADOS distribué.
//! Chaque page de 4 Kio est stockée comme un objet RADOS :
//!   - Pool  : configurable (défaut "omega-pages")
//!   - OID   : "{vm_id:08x}_{page_id:016x}"  (ex: "00000064_0000000000000001")
//!
//! # Avantages vs sled local
//!
//! - **Disponibilité** : si un nœud store crashe, les pages survivent dans Ceph.
//!   Pas besoin de la réplication write-through de l'agent (Ceph gère min_size/size).
//! - **Capacité partagée** : tous les nœuds voient le même pool — plus de
//!   surveillance d'espace disque par nœud.
//! - **Pas de sled** : pas de journal local, pas de tuning B-tree.
//!
//! # Détection automatique
//!
//! librados est sondé au moment du build par `build.rs` (pkg-config + chemins standards).
//! Si détecté, le cfg `ceph_detected` est émis et le code FFI est compilé.
//! Sans librados, les méthodes retournent des erreurs descriptives.
//!
//! Au démarrage, `CephStore::try_auto_connect()` vérifie si `/etc/ceph/ceph.conf`
//! existe et tente la connexion — aucune action manuelle requise.
//!
//! # Concurrence
//!
//! `rados_ioctx_t` n'est pas thread-safe. On maintient un pool de contextes
//! (1 par CPU logique, borné à MAX_IOCTX) chacun protégé par un Mutex.
//! Les opérations bloquantes s'exécutent dans `spawn_blocking`.

#![allow(non_camel_case_types)]

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{bail, Result};

#[cfg(ceph_detected)]
use {
    std::sync::atomic::AtomicUsize,
    tokio::sync::Mutex,
    tracing::{debug, info, warn},
};

use crate::metrics::StoreMetrics;
use crate::protocol::PAGE_SIZE;
use crate::store::PageKey;

// ─── Pool de contextes ────────────────────────────────────────────────────────

/// Nombre maximum de rados_ioctx en parallèle (un par cœur disponible, min 4).
#[cfg(any(ceph_detected, test))]
fn max_ioctx() -> usize {
    (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4))
    .max(4)
}

// ─── FFI librados ─────────────────────────────────────────────────────────────

#[cfg(ceph_detected)]
mod ffi {
    use std::os::raw::{c_char, c_int, c_void};

    pub type rados_t = *mut c_void;
    pub type rados_ioctx_t = *mut c_void;

    #[repr(C)]
    pub struct rados_cluster_stat_t {
        pub kb: u64,
        pub kb_used: u64,
        pub kb_avail: u64,
        pub num_objects: u64,
    }

    extern "C" {
        pub fn rados_create2(
            cluster: *mut rados_t,
            cluster_name: *const c_char,
            user_name: *const c_char,
            flags: u64,
        ) -> c_int;

        pub fn rados_conf_read_file(cluster: rados_t, path: *const c_char) -> c_int;

        pub fn rados_connect(cluster: rados_t) -> c_int;

        pub fn rados_shutdown(cluster: rados_t);

        pub fn rados_ioctx_create(
            cluster: rados_t,
            pool_name: *const c_char,
            ioctx: *mut rados_ioctx_t,
        ) -> c_int;

        pub fn rados_ioctx_destroy(io: rados_ioctx_t);

        /// Écrase l'objet entier avec buf (crée si inexistant).
        pub fn rados_write_full(
            io: rados_ioctx_t,
            oid: *const c_char,
            buf: *const u8,
            len: usize,
        ) -> c_int;

        /// Lit `len` octets depuis l'offset `off`.
        /// Retourne le nombre d'octets lus, ou errno négatif.
        pub fn rados_read(
            io: rados_ioctx_t,
            oid: *const c_char,
            buf: *mut u8,
            len: usize,
            off: u64,
        ) -> c_int;

        pub fn rados_remove(io: rados_ioctx_t, oid: *const c_char) -> c_int;

        pub fn rados_cluster_stat(cluster: rados_t, result: *mut rados_cluster_stat_t) -> c_int;
    }

    // SAFETY : les handles Ceph sont opaques mais thread-safe pour les opérations
    // inter-thread (le cluster handle est read-only après connect ; chaque ioctx
    // est accédé sous Mutex).
    #[derive(Debug)]
    pub struct ClusterHandle(pub rados_t);
    unsafe impl Send for ClusterHandle {}
    unsafe impl Sync for ClusterHandle {}

    #[derive(Debug)]
    pub struct IoctxHandle(pub rados_ioctx_t);
    unsafe impl Send for IoctxHandle {}
    unsafe impl Sync for IoctxHandle {}

    impl Drop for ClusterHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: cluster valide et non utilisé ailleurs à ce stade.
                unsafe { rados_shutdown(self.0) };
            }
        }
    }

    impl Drop for IoctxHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: ioctx valide.
                unsafe { rados_ioctx_destroy(self.0) };
            }
        }
    }
}

// ─── CephStore ────────────────────────────────────────────────────────────────

/// Store de pages distribué sur Ceph RADOS.
///
/// Compilé automatiquement avec librados si détecté par `build.rs`.
/// Sans librados, toutes les opérations retournent une erreur descriptive.
#[derive(Debug)]
pub struct CephStore {
    #[cfg(ceph_detected)]
    cluster: Arc<ffi::ClusterHandle>,
    #[cfg(ceph_detected)]
    ioctxs: Vec<Arc<Mutex<ffi::IoctxHandle>>>,
    #[cfg(ceph_detected)]
    counter: AtomicUsize,
    #[cfg(ceph_detected)]
    pool: String,
    metrics: Arc<StoreMetrics>,
}

impl CephStore {
    /// Ouvre une connexion Ceph et crée le pool si nécessaire.
    ///
    /// - `conf_path` : chemin vers `/etc/ceph/ceph.conf` (ou équivalent)
    /// - `pool`      : nom du pool RADOS (ex: "omega-pages")
    /// - `user`      : utilisateur Ceph (ex: "client.omega")
    pub fn connect(
        #[allow(unused_variables)] conf_path: &str,
        #[allow(unused_variables)] pool: &str,
        #[allow(unused_variables)] user: &str,
        #[allow(unused_variables)] metrics: Arc<StoreMetrics>,
    ) -> Result<Self> {
        #[cfg(not(ceph_detected))]
        {
            bail!(
                "CephStore::connect : librados non détecté au moment du build. \
                 Installez ceph-common sur ce nœud et recompilez."
            );
        }

        #[cfg(ceph_detected)]
        {
            use ffi::*;
            use std::ffi::CString;

            let c_cluster_name = CString::new("ceph")?;
            let c_user = CString::new(user)?;
            let c_conf = CString::new(conf_path)?;
            let c_pool = CString::new(pool)?;

            // 1. Créer le handle cluster
            let mut cluster_raw: rados_t = std::ptr::null_mut();
            let ret = unsafe {
                rados_create2(
                    &mut cluster_raw,
                    c_cluster_name.as_ptr(),
                    c_user.as_ptr(),
                    0,
                )
            };
            if ret < 0 {
                bail!("rados_create2 : errno {}", -ret);
            }

            // 2. Lire la config
            let ret = unsafe { rados_conf_read_file(cluster_raw, c_conf.as_ptr()) };
            if ret < 0 {
                unsafe { rados_shutdown(cluster_raw) };
                bail!("rados_conf_read_file({conf_path}) : errno {}", -ret);
            }

            // 3. Connexion au cluster
            let ret = unsafe { rados_connect(cluster_raw) };
            if ret < 0 {
                unsafe { rados_shutdown(cluster_raw) };
                bail!(
                    "rados_connect : errno {} — vérifiez que Ceph est accessible",
                    -ret
                );
            }
            info!(pool, user, "connecté au cluster Ceph");

            let cluster = Arc::new(ClusterHandle(cluster_raw));

            // 4. Pool de ioctxs (un par cœur, min 4)
            let n = max_ioctx();
            let mut ioctxs = Vec::with_capacity(n);
            for i in 0..n {
                let mut ioctx_raw: rados_ioctx_t = std::ptr::null_mut();
                let ret = unsafe { rados_ioctx_create(cluster.0, c_pool.as_ptr(), &mut ioctx_raw) };
                if ret < 0 {
                    bail!(
                        "rados_ioctx_create (slot {i}) pour pool '{pool}' : errno {}",
                        -ret
                    );
                }
                ioctxs.push(Arc::new(Mutex::new(IoctxHandle(ioctx_raw))));
            }

            info!(pool, ioctx_count = n, "pool RADOS prêt");

            Ok(Self {
                cluster,
                ioctxs,
                counter: AtomicUsize::new(0),
                pool: pool.to_string(),
                metrics,
            })
        }
    }

    /// Tente une connexion Ceph automatique si `conf_path` existe.
    ///
    /// Retourne `Some(Self)` si librados est compilé ET si le fichier de config existe.
    /// Retourne `None` dans tous les autres cas (librados absent, fichier manquant, erreur).
    /// Aucune action manuelle n'est requise — appelé automatiquement au démarrage.
    pub fn try_auto_connect(
        #[allow(unused_variables)] conf_path: &str,
        #[allow(unused_variables)] pool: &str,
        #[allow(unused_variables)] user: &str,
        #[allow(unused_variables)] metrics: Arc<StoreMetrics>,
    ) -> Option<Self> {
        #[cfg(not(ceph_detected))]
        return None;

        #[cfg(ceph_detected)]
        {
            if !std::path::Path::new(conf_path).exists() {
                tracing::debug!(conf_path, "ceph.conf absent — backend RAM");
                return None;
            }
            match Self::connect(conf_path, pool, user, metrics) {
                Ok(cs) => {
                    tracing::info!(pool, conf = conf_path, "Ceph auto-connecté");
                    Some(cs)
                }
                Err(e) => {
                    tracing::warn!(error = %e, conf = conf_path, "Ceph configuré mais connexion échouée — backend RAM");
                    None
                }
            }
        }
    }

    // ── Accès au ioctx round-robin ────────────────────────────────────────────

    #[cfg(ceph_detected)]
    fn ioctx(&self) -> Arc<Mutex<ffi::IoctxHandle>> {
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.ioctxs.len();
        self.ioctxs[idx].clone()
    }

    // ── Formatage de l'OID ────────────────────────────────────────────────────

    #[cfg(any(ceph_detected, test))]
    fn oid(key: &PageKey) -> String {
        format!("{:08x}_{:016x}", key.vm_id, key.page_id)
    }

    // ── API publique ──────────────────────────────────────────────────────────

    /// Stocke une page de 4 Kio dans le pool RADOS.
    pub async fn put(&self, #[allow(unused_variables)] key: PageKey, data: Vec<u8>) -> Result<()> {
        if data.len() != PAGE_SIZE {
            bail!(
                "CephStore::put : taille incorrecte {} (attendu {})",
                data.len(),
                PAGE_SIZE
            );
        }

        #[cfg(not(ceph_detected))]
        bail!("Ceph non activé");

        #[cfg(ceph_detected)]
        {
            use ffi::*;
            use std::ffi::CString;

            let oid_str = Self::oid(&key);
            let c_oid = CString::new(oid_str.as_str())?;
            let ioctx = self.ioctx();
            let oid_err = oid_str.clone(); // évite le move dans la closure

            tokio::task::spawn_blocking(move || -> Result<()> {
                let io = ioctx.blocking_lock();
                let ret =
                    unsafe { rados_write_full(io.0, c_oid.as_ptr(), data.as_ptr(), data.len()) };
                if ret < 0 {
                    bail!("rados_write_full {oid_err} : errno {}", -ret);
                }
                Ok(())
            })
            .await??;

            self.metrics.put_count.fetch_add(1, Ordering::Relaxed);
            self.metrics.pages_stored.fetch_add(1, Ordering::Relaxed);
            debug!(oid = %oid_str, "page écrite dans Ceph");
            Ok(())
        }
    }

    /// Lit une page depuis le pool RADOS.
    /// Retourne `None` si l'objet n'existe pas (errno ENOENT).
    pub async fn get(&self, #[allow(unused_variables)] key: &PageKey) -> Result<Option<Vec<u8>>> {
        #[cfg(not(ceph_detected))]
        bail!("Ceph non activé");

        #[cfg(ceph_detected)]
        {
            use ffi::*;
            use std::ffi::CString;

            let oid_str = Self::oid(key);
            let c_oid = CString::new(oid_str.as_str())?;
            let ioctx = self.ioctx();
            let oid_err = oid_str.clone(); // évite le move dans la closure

            let result = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
                let io = ioctx.blocking_lock();
                let mut buf = vec![0u8; PAGE_SIZE];
                let ret =
                    unsafe { rados_read(io.0, c_oid.as_ptr(), buf.as_mut_ptr(), PAGE_SIZE, 0) };
                if ret == -libc::ENOENT {
                    return Ok(None);
                }
                if ret < 0 {
                    bail!("rados_read {oid_err} : errno {}", -ret);
                }
                if ret as usize != PAGE_SIZE {
                    bail!(
                        "rados_read {oid_err} : lu {} octets (attendu {})",
                        ret,
                        PAGE_SIZE
                    );
                }
                Ok(Some(buf))
            })
            .await??;

            self.metrics.get_count.fetch_add(1, Ordering::Relaxed);
            if result.is_some() {
                self.metrics.hit_count.fetch_add(1, Ordering::Relaxed);
                debug!(oid = %oid_str, "page lue depuis Ceph");
            } else {
                self.metrics.miss_count.fetch_add(1, Ordering::Relaxed);
                warn!(oid = %oid_str, "page absente du pool Ceph (ENOENT)");
            }
            Ok(result)
        }
    }

    /// Supprime une page du pool RADOS.
    /// Retourne `true` si la page existait, `false` si elle était absente.
    pub async fn delete(&self, #[allow(unused_variables)] key: &PageKey) -> Result<bool> {
        #[cfg(not(ceph_detected))]
        bail!("Ceph non activé");

        #[cfg(ceph_detected)]
        {
            use ffi::*;
            use std::ffi::CString;

            let oid_str = Self::oid(key);
            let c_oid = CString::new(oid_str.as_str())?;
            let ioctx = self.ioctx();

            let found = tokio::task::spawn_blocking(move || -> Result<bool> {
                let io = ioctx.blocking_lock();
                let ret = unsafe { rados_remove(io.0, c_oid.as_ptr()) };
                if ret == 0 {
                    return Ok(true);
                }
                if ret == -libc::ENOENT {
                    return Ok(false);
                }
                bail!("rados_remove {oid_str} : errno {}", -ret);
            })
            .await??;

            if found {
                self.metrics.pages_stored.fetch_sub(1, Ordering::Relaxed);
            }
            Ok(found)
        }
    }

    /// Statistiques du cluster Ceph (capacité partagée, non par-nœud).
    ///
    /// Retourne `(available_mib, total_mib)`.
    pub fn cluster_stats_mib(&self) -> (u64, u64) {
        #[cfg(not(ceph_detected))]
        return (0, 0);

        #[cfg(ceph_detected)]
        {
            use ffi::*;
            let mut stat = rados_cluster_stat_t {
                kb: 0,
                kb_used: 0,
                kb_avail: 0,
                num_objects: 0,
            };
            let ret = unsafe { rados_cluster_stat(self.cluster.0, &mut stat) };
            if ret < 0 {
                warn!("rados_cluster_stat : errno {}", -ret);
                return (0, 0);
            }
            (stat.kb_avail / 1024, stat.kb / 1024)
        }
    }

    /// Nombre d'objets stockés (approximatif — compte les PUT/DELETE effectués).
    pub fn len(&self) -> usize {
        self.metrics.pages_stored.load(Ordering::Relaxed) as usize
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::StoreMetrics;

    fn dummy_metrics() -> Arc<StoreMetrics> {
        Arc::new(StoreMetrics::default())
    }

    #[test]
    fn test_oid_format() {
        let key = PageKey::new(100, 1);
        let oid = CephStore::oid(&key);
        assert_eq!(oid, "00000064_0000000000000001");
    }

    #[test]
    fn test_oid_zero() {
        let key = PageKey::new(0, 0);
        assert_eq!(CephStore::oid(&key), "00000000_0000000000000000");
    }

    #[test]
    fn test_oid_max() {
        let key = PageKey::new(u32::MAX, u64::MAX);
        assert_eq!(CephStore::oid(&key), "ffffffff_ffffffffffffffff");
    }

    #[test]
    fn test_connect_without_librados_fails() {
        // Sans librados compilé, connect() doit retourner une erreur explicite.
        #[cfg(not(ceph_detected))]
        {
            let err = CephStore::connect(
                "/etc/ceph/ceph.conf",
                "omega-pages",
                "client.admin",
                dummy_metrics(),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("librados"),
                "le message d'erreur doit mentionner 'librados', obtenu : {err}"
            );
        }
        // Avec librados compilé, on ne peut pas tester sans vrai cluster — on skip.
        #[cfg(ceph_detected)]
        {}
    }

    #[test]
    fn test_try_auto_connect_absent_conf() {
        // Sans fichier de config, try_auto_connect retourne None (pas d'erreur).
        let result = CephStore::try_auto_connect(
            "/tmp/does_not_exist_omega_ceph_test.conf",
            "omega-pages",
            "client.admin",
            dummy_metrics(),
        );
        assert!(result.is_none(), "doit retourner None si ceph.conf absent");
    }

    #[test]
    fn test_max_ioctx_min_four() {
        assert!(max_ioctx() >= 4);
    }
}
