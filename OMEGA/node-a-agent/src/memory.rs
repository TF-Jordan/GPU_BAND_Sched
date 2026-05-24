//! Gestion de la région mémoire paginée distante.
//!
//! # Cycle de vie d'une page
//!
//! ```text
//! État LOCALE  ──evict_page_to(store_idx)──►  État DISTANTE (store_idx connu)
//!      ▲                                              │
//!      └──────── fetch_page() ────────────────────────┘   (depuis uffd handler)
//!      └──────── recall_n_pages() ─────────────────────    (proactif LIFO)
//! ```
//!
//! # Routage dynamique
//!
//! Contrairement à l'ancien round-robin fixe (page_id % n),
//! le routage est maintenant décidé à l'éviction selon la disponibilité RAM des stores.
//! La table `page_locations: HashMap<page_id, store_idx>` enregistre où chaque page est stockée
//! pour pouvoir la récupérer au bon endroit lors d'une faute ou d'un recall.
//!
//! # Recall LIFO
//!
//! L'ordre d'éviction est enregistré dans `eviction_order: VecDeque<page_id>`.
//! `recall_n_pages` dépile depuis le back (dernière page évinvée en premier).

use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

use crate::clock_eviction::ClockEvictor;
use crate::cluster::ClusterState;
use crate::metrics::AgentMetrics;
use crate::remote::RemoteStorePool;
use crate::shared_memory::{MemoryBackend, MemoryBackendKind, MemoryBackendOptions};

pub const PAGE_SIZE: usize = 4096;

/// Région mémoire anonyme mmap, enregistrée auprès d'userfaultfd.
pub struct MemoryRegion {
    pub base: *mut u8,
    pub size: usize,
    pub num_pages: usize,
    pub vm_id: u32,
    pub backend_kind: MemoryBackendKind,

    /// Plafond absolu de pages pouvant être externalisées = vm_requested_mib * 1024 / 4.
    /// Évite de stocker sur les nœuds distants plus que ce que la VM a demandé.
    vm_requested_pages: usize,

    /// page_id → store_idx (routage dynamique).
    /// Présent ssi la page est actuellement sur un store distant.
    page_locations: std::sync::Mutex<HashMap<u64, usize>>,

    /// Pile LIFO : la dernière page évinvée est en queue.
    eviction_order: std::sync::Mutex<VecDeque<u64>>,

    /// Algorithme CLOCK pour la sélection des victimes.
    evictor: ClockEvictor,

    /// Mis à `true` avant une migration live : bloque toute nouvelle éviction
    /// pour que QEMU transfère un mmap cohérent et complet.
    eviction_frozen: AtomicBool,

    /// Réplication write-through : chaque page évinvée est aussi envoyée au store suivant.
    /// Peut être désactivée à chaud (quand Ceph est détecté sur tous les stores).
    replication_enabled: Arc<AtomicBool>,

    backend: MemoryBackend,
    store: Arc<RemoteStorePool>,
    cluster: Arc<ClusterState>,
    metrics: Arc<AgentMetrics>,
    tokio_handle: Handle,
}

// SAFETY: base est un pointeur vers une région mmap propre à ce processus.
unsafe impl Send for MemoryRegion {}
unsafe impl Sync for MemoryRegion {}

impl MemoryRegion {
    pub fn allocate(
        size_bytes: usize,
        vm_id: u32,
        vm_requested_mib: u64,
        store: Arc<RemoteStorePool>,
        metrics: Arc<AgentMetrics>,
        tokio_handle: Handle,
        backend_options: MemoryBackendOptions,
        cluster: Arc<ClusterState>,
        replication_enabled: Arc<AtomicBool>,
    ) -> Result<Self> {
        assert!(size_bytes.is_multiple_of(PAGE_SIZE));

        let backend = MemoryBackend::allocate(&backend_options, size_bytes)?;
        let base = backend.map().with_context(|| {
            format!(
                "mappage backend {:?} ({} Mio)",
                backend.kind,
                size_bytes / 1024 / 1024
            )
        })?;
        let num_pages = size_bytes / PAGE_SIZE;

        info!(
            base      = format!("{:p}", base),
            size_bytes,
            num_pages,
            backend   = ?backend.kind,
            "région mémoire allouée"
        );

        let vm_requested_pages =
            ((vm_requested_mib * 1024 * 1024) as usize / PAGE_SIZE).min(num_pages);

        Ok(Self {
            base,
            size: size_bytes,
            num_pages,
            vm_id,
            backend_kind: backend.kind,
            vm_requested_pages,
            page_locations: std::sync::Mutex::new(HashMap::new()),
            eviction_order: std::sync::Mutex::new(VecDeque::new()),
            evictor: ClockEvictor::new(num_pages, 0),
            eviction_frozen: AtomicBool::new(false),
            replication_enabled,
            backend,
            store,
            cluster,
            metrics,
            tokio_handle,
        })
    }

    fn page_ptr(&self, page_id: u64) -> *mut u8 {
        unsafe { self.base.add(page_id as usize * PAGE_SIZE) }
    }

    /// Écrit `data` dans la page locale `page_id` (bypass uffd).
    pub fn write_page_local(&self, page_id: u64, data: &[u8; PAGE_SIZE]) -> Result<()> {
        if page_id >= self.num_pages as u64 {
            bail!(
                "page_id {page_id} hors limites (max {})",
                self.num_pages - 1
            );
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.page_ptr(page_id), PAGE_SIZE);
        }
        self.evictor.mark_present(page_id);
        self.metrics.local_present.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Lit les 4096 octets de la page locale `page_id`.
    pub fn read_page_local(&self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        if page_id >= self.num_pages as u64 {
            bail!("page_id {page_id} hors limites");
        }
        let mut buf = [0u8; PAGE_SIZE];
        unsafe {
            std::ptr::copy_nonoverlapping(self.page_ptr(page_id), buf.as_mut_ptr(), PAGE_SIZE);
        }
        Ok(buf)
    }

    /// Évince `page_id` vers `store_idx` (routage dynamique décidé par l'appelant).
    ///
    /// Séquence : lecture locale → PUT_PAGE_TO(store_idx) → MADV_DONTNEED → marquage.
    pub fn evict_page_to(&self, page_id: u64, store_idx: usize) -> Result<()> {
        if page_id >= self.num_pages as u64 {
            bail!("evict_page_to : page_id {page_id} hors limites");
        }
        if self.eviction_frozen.load(Ordering::Acquire) {
            debug!(page_id, "éviction gelée (migration en cours) — ignorée");
            return Ok(());
        }

        {
            let locs = self.page_locations.lock().unwrap();
            if locs.contains_key(&page_id) {
                debug!(page_id, "page déjà distante — éviction ignorée");
                return Ok(());
            }
            // Vérifier le plafond : total pages distantes ≤ vm_requested_pages
            if locs.len() >= self.vm_requested_pages {
                bail!(
                    "cap vm_requested atteint : {} pages distantes (max {})",
                    locs.len(),
                    self.vm_requested_pages
                );
            }
        }

        // 1. Lecture locale
        let data = self.read_page_local(page_id)?;

        // 2. PUT vers le store cible
        let vm_id = self.vm_id;
        let store = self.store.clone();
        tokio::task::block_in_place(|| {
            self.tokio_handle
                .block_on(store.put_page_to(vm_id, page_id, data.to_vec(), store_idx))
        })
        .with_context(|| format!("evict_page_to : PUT page={page_id} store[{store_idx}]"))?;

        // Mise à jour immédiate de la capacité estimée (item 3)
        self.cluster.track_eviction(store_idx);

        // Réplication write-through vers le store suivant (item 4)
        if self.replication_enabled.load(Ordering::Relaxed) && self.store.num_stores() > 1 {
            let replica_idx = (store_idx + 1) % self.store.num_stores();
            let store2 = self.store.clone();
            if let Err(e) = tokio::task::block_in_place(|| {
                self.tokio_handle.block_on(store2.put_page_to(
                    vm_id,
                    page_id,
                    data.to_vec(),
                    replica_idx,
                ))
            }) {
                warn!(page_id, replica_idx, error = %e, "réplication échouée — données sur primaire uniquement");
            }
        }

        // 3. Libération locale
        let ptr = self.page_ptr(page_id);
        let ret =
            unsafe { libc::madvise(ptr as *mut libc::c_void, PAGE_SIZE, libc::MADV_DONTNEED) };
        if ret != 0 {
            warn!(page_id, error = %std::io::Error::last_os_error(), "MADV_DONTNEED échoué");
        }

        // 4. Marquage
        self.page_locations
            .lock()
            .unwrap()
            .insert(page_id, store_idx);
        self.eviction_order.lock().unwrap().push_back(page_id);
        self.evictor.mark_remote(page_id);
        self.metrics.pages_evicted.fetch_add(1, Ordering::Relaxed);
        self.metrics.local_present.fetch_sub(1, Ordering::Relaxed);

        debug!(page_id, store_idx, "page évinvée");
        Ok(())
    }

    /// Appelé par le handler userfaultfd pour servir une faute de page.
    ///
    /// - Page distante (dans page_locations) → GET depuis son store, retourne les octets.
    /// - Page jamais évinvée (premier accès QEMU) → retourne zéros (allocation initiale normale).
    pub fn fetch_page(&self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let store_idx_opt = self.page_locations.lock().unwrap().get(&page_id).copied();

        match store_idx_opt {
            Some(store_idx) => {
                let vm_id = self.vm_id;
                let store = self.store.clone();
                let data = self
                    .tokio_handle
                    .block_on(store.get_page_from(vm_id, page_id, store_idx))
                    .with_context(|| {
                        format!("fetch_page : GET page={page_id} store[{store_idx}]")
                    })?;

                match data {
                    Some(bytes) => {
                        let mut arr = [0u8; PAGE_SIZE];
                        arr.copy_from_slice(&bytes);
                        // Démarquer comme distant (la page revient via UFFDIO_COPY)
                        self.page_locations.lock().unwrap().remove(&page_id);
                        // Retirer de la pile LIFO (peut ne pas être en queue — O(n) acceptable)
                        self.eviction_order
                            .lock()
                            .unwrap()
                            .retain(|&p| p != page_id);
                        self.evictor.mark_present(page_id);
                        self.metrics.pages_fetched.fetch_add(1, Ordering::Relaxed);
                        self.metrics.local_present.fetch_add(1, Ordering::Relaxed);
                        Ok(arr)
                    }
                    None => {
                        // Store redémarré ou page perdue — retourner zéros
                        warn!(
                            page_id,
                            store_idx, "page absente du store — zéros retournés"
                        );
                        self.page_locations.lock().unwrap().remove(&page_id);
                        self.eviction_order
                            .lock()
                            .unwrap()
                            .retain(|&p| p != page_id);
                        self.metrics.fetch_zeros.fetch_add(1, Ordering::Relaxed);
                        Ok([0u8; PAGE_SIZE])
                    }
                }
            }
            None => {
                // Premier accès QEMU à cette page — normal, on sert des zéros
                self.metrics.fetch_zeros.fetch_add(1, Ordering::Relaxed);
                Ok([0u8; PAGE_SIZE])
            }
        }
    }

    /// Gèle toute nouvelle éviction. Irréversible : à appeler juste avant une migration.
    pub fn freeze_eviction(&self) {
        self.eviction_frozen.store(true, Ordering::Release);
        info!(vm_id = self.vm_id, "éviction gelée — migration imminente");
    }

    /// Rappelle toutes les pages encore distantes dans le mmap local.
    ///
    /// À appeler après `freeze_eviction()` et avant `qm migrate --online`.
    /// Garantit que QEMU transfère un mmap complet et cohérent : pas de zéros
    /// là où des données réelles existent sur les stores.
    pub fn recall_all_pages(&self, uffd_fd: RawFd) -> Result<usize> {
        let count = self.remote_count();
        if count == 0 {
            return Ok(0);
        }
        info!(count, vm_id = self.vm_id, "recall complet avant migration");
        self.recall_n_pages(count, uffd_fd)
    }

    /// Sélectionne jusqu'à `count` pages froides selon CLOCK, sans les évincer.
    ///
    /// Phase 2 : si CLOCK ne trouve pas assez, complète avec des pages locales
    /// jamais tracées (écrites directement par QEMU sans write_page_local).
    pub fn select_cold_pages(&self, count: usize) -> Vec<u64> {
        let mut victims = self.evictor.select_victims(count);

        if victims.len() < count {
            let remaining = count - victims.len();
            let already = victims
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            let locs = self.page_locations.lock().unwrap();
            for page_id in 0..self.num_pages as u64 {
                if victims.len() - (count - remaining) >= remaining {
                    break;
                }
                if !self.evictor.meta.contains_key(&page_id)
                    && !already.contains(&page_id)
                    && !locs.contains_key(&page_id)
                {
                    victims.push(page_id);
                }
                if victims.len() >= count {
                    break;
                }
            }
        }

        victims
    }

    /// Rappel LIFO : rapatrie les `count` dernières pages évinvées vers la région locale.
    ///
    /// Pour chaque page :
    ///  1. GET depuis son store_idx.
    ///  2. Inject via UFFDIO_COPY (DONTWAKE) — la page redevient accessible sans faute.
    ///  3. DELETE du store pour libérer l'espace.
    ///  4. Démarquage comme distante.
    ///
    /// # Safety
    /// `uffd_fd` doit être un fd userfaultfd valide couvrant cette région.
    pub fn recall_n_pages(&self, count: usize, uffd_fd: RawFd) -> Result<usize> {
        if count == 0 {
            return Ok(0);
        }

        let mut recalled = 0;

        for _ in 0..count {
            // Pop LIFO (dernière évinvée en premier)
            let page_id = match self.eviction_order.lock().unwrap().pop_back() {
                Some(p) => p,
                None => break,
            };

            let store_idx = match self.page_locations.lock().unwrap().get(&page_id).copied() {
                Some(idx) => idx,
                None => continue, // déjà rapatriée (race avec uffd handler)
            };

            let vm_id = self.vm_id;
            let store = self.store.clone();

            let data = match tokio::task::block_in_place(|| {
                self.tokio_handle
                    .block_on(store.get_page_from(vm_id, page_id, store_idx))
            })
            .with_context(|| format!("recall : GET page={page_id} store[{store_idx}]"))?
            {
                Some(bytes) => bytes,
                None => {
                    // Page absente du primaire — fallback sur replica si réplication activée
                    if self.replication_enabled.load(Ordering::Relaxed)
                        && self.store.num_stores() > 1
                    {
                        let replica_idx = (store_idx + 1) % self.store.num_stores();
                        let store2 = self.store.clone();
                        match tokio::task::block_in_place(|| {
                            self.tokio_handle.block_on(store2.get_page_from(
                                vm_id,
                                page_id,
                                replica_idx,
                            ))
                        })
                        .ok()
                        .flatten()
                        {
                            Some(bytes) => {
                                warn!(
                                    page_id,
                                    store_idx,
                                    replica_idx,
                                    "recall : fallback replica (primaire absent)"
                                );
                                bytes
                            }
                            None => {
                                warn!(
                                    page_id,
                                    store_idx,
                                    "recall : page absente du store ET du replica — ignorée"
                                );
                                self.page_locations.lock().unwrap().remove(&page_id);
                                continue;
                            }
                        }
                    } else {
                        warn!(
                            page_id,
                            store_idx, "recall : page absente du store — ignorée"
                        );
                        self.page_locations.lock().unwrap().remove(&page_id);
                        continue;
                    }
                }
            };

            let mut arr = [0u8; PAGE_SIZE];
            arr.copy_from_slice(&data);

            let page_addr = self.base as u64 + page_id * PAGE_SIZE as u64;

            // SAFETY: uffd_fd valide, page_addr dans la région enregistrée, arr valide.
            unsafe {
                crate::uffd::recall_inject(uffd_fd, page_addr, &arr)?;
            }

            // Suppression du store primaire pour libérer sa RAM
            let _ = tokio::task::block_in_place(|| {
                self.tokio_handle
                    .block_on(store.delete_page_from(vm_id, page_id, store_idx))
            });

            // Suppression du replica si réplication activée
            if self.replication_enabled.load(Ordering::Relaxed) && self.store.num_stores() > 1 {
                let replica_idx = (store_idx + 1) % self.store.num_stores();
                let store2 = self.store.clone();
                let _ = tokio::task::block_in_place(|| {
                    self.tokio_handle
                        .block_on(store2.delete_page_from(vm_id, page_id, replica_idx))
                });
            }

            // Démarquage + mise à jour capacité (item 3)
            self.page_locations.lock().unwrap().remove(&page_id);
            self.cluster.track_recall(store_idx);
            self.evictor.mark_present(page_id);
            self.metrics.pages_recalled.fetch_add(1, Ordering::Relaxed);
            self.metrics.local_present.fetch_add(1, Ordering::Relaxed);

            recalled += 1;
        }

        if recalled > 0 {
            debug!(recalled, "pages rapatriées (recall LIFO)");
        }

        Ok(recalled)
    }

    pub fn base_ptr(&self) -> *mut libc::c_void {
        self.base as *mut libc::c_void
    }

    pub fn backend_proc_fd_path(&self) -> Option<std::path::PathBuf> {
        self.backend.proc_fd_path()
    }

    pub fn write_backend_metadata(&self, path: &std::path::Path) -> Result<()> {
        self.backend.write_metadata_file(path)
    }

    pub fn remote_count(&self) -> usize {
        self.page_locations.lock().unwrap().len()
    }

    pub fn remote_cap(&self) -> usize {
        self.vm_requested_pages
    }

    pub fn is_remote(&self, page_id: u64) -> bool {
        self.page_locations.lock().unwrap().contains_key(&page_id)
    }

    /// Supprime toutes les pages encore distantes des stores.
    ///
    /// À appeler avant l'arrêt de l'agent : la RAM étant volatile,
    /// les pages orphelines sur les stores n'ont plus aucun sens après extinction de la VM.
    pub fn purge_remote_pages(&self) {
        let page_ids: Vec<(u64, usize)> = self
            .page_locations
            .lock()
            .unwrap()
            .iter()
            .map(|(&pid, &sidx)| (pid, sidx))
            .collect();

        if page_ids.is_empty() {
            return;
        }

        info!(
            count = page_ids.len(),
            vm_id = self.vm_id,
            "purge des pages distantes"
        );

        let replication = self.replication_enabled.load(Ordering::Relaxed);
        let num_stores = self.store.num_stores();
        let vm_id = self.vm_id;
        let store = self.store.clone();
        let cluster = self.cluster.clone();
        self.tokio_handle.block_on(async move {
            for (page_id, store_idx) in page_ids {
                let _ = store.delete_page_from(vm_id, page_id, store_idx).await;
                cluster.track_recall(store_idx);
                if replication && num_stores > 1 {
                    let replica_idx = (store_idx + 1) % num_stores;
                    let _ = store.delete_page_from(vm_id, page_id, replica_idx).await;
                }
            }
        });

        self.page_locations.lock().unwrap().clear();
        self.eviction_order.lock().unwrap().clear();
        info!(vm_id = self.vm_id, "purge terminée");
    }

    /// Active ou désactive la réplication write-through à chaud.
    ///
    /// Appelé automatiquement par le daemon quand tous les stores utilisent Ceph
    /// (la redondance est assurée par Ceph — la réplication devient inutile).
    pub fn set_replication_enabled(&self, enabled: bool) {
        self.replication_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Insère directement une page dans page_locations (tests uniquement).
    #[cfg(test)]
    pub fn test_insert_remote(&self, page_id: u64, store_idx: usize) {
        self.page_locations
            .lock()
            .unwrap()
            .insert(page_id, store_idx);
        self.eviction_order.lock().unwrap().push_back(page_id);
    }
}

impl Drop for MemoryRegion {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.size) };
        info!("région mémoire libérée");
    }
}

// ─── Utilitaire système ───────────────────────────────────────────────────────

/// Lit /proc/meminfo, retourne MemAvailable en Mio.
pub fn read_mem_available_mib() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_mem_available_mib_returns_value() {
        let mib = read_mem_available_mib();
        assert!(mib.is_some());
        assert!(mib.unwrap() > 0);
    }

    #[test]
    fn test_read_mem_available_plausible() {
        let mib = read_mem_available_mib().unwrap();
        assert!(mib < 4 * 1024 * 1024);
    }

    // ── Tests du plafond vm_requested_pages ──────────────────────────────────

    fn make_test_region(num_pages: usize, vm_requested_mib: u64) -> MemoryRegion {
        use crate::metrics::AgentMetrics;
        use crate::remote::RemoteStorePool;
        use crate::shared_memory::{MemoryBackendKind, MemoryBackendOptions};

        // Runtime dédié, leaké pour que le handle reste valide le temps du test.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        Box::leak(Box::new(rt));

        let store = Arc::new(RemoteStorePool::new(
            vec!["127.0.0.1:9999".into()],
            50,
            vec![],
        ));
        let metrics = Arc::new(AgentMetrics::default());
        let cluster = Arc::new(crate::cluster::ClusterState::new(
            vec!["127.0.0.1:9999".into()],
            vec!["127.0.0.1:9200".into()],
        ));
        MemoryRegion::allocate(
            PAGE_SIZE * num_pages,
            99,
            vm_requested_mib,
            store,
            metrics,
            handle,
            MemoryBackendOptions {
                kind: MemoryBackendKind::Anonymous,
                memfd_name: String::new(),
            },
            cluster,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
    }

    #[test]
    fn test_cap_zero_rejects_first_eviction() {
        // vm_requested_mib=0 → vm_requested_pages=0 → toute éviction dépasse immédiatement le cap
        let region = make_test_region(4, 0);
        assert_eq!(region.remote_cap(), 0);
        let err = region.evict_page_to(0, 0).unwrap_err();
        assert!(
            err.to_string().contains("cap vm_requested"),
            "attendu erreur cap, obtenu : {err}"
        );
    }

    #[test]
    fn test_cap_bounded_by_num_pages() {
        // vm_requested_mib=1 → 256 pages en théorie ; mais region n'a que 8 pages → cap=8
        let region = make_test_region(8, 1);
        assert_eq!(region.remote_cap(), 8);
    }

    #[test]
    fn test_cap_hit_when_full() {
        // Injecter directement 2 pages distantes dans une région de cap=2
        let region = make_test_region(4, 0); // cap=0
                                             // cap=0, toute tentative est rejetée
        assert_eq!(region.remote_cap(), 0);
        let err = region.evict_page_to(0, 0).unwrap_err();
        assert!(err.to_string().contains("cap vm_requested"));
    }

    #[test]
    fn test_insert_remote_increments_count() {
        let region = make_test_region(4, 1); // cap=min(256,4)=4
        region.test_insert_remote(0, 0);
        region.test_insert_remote(1, 0);
        assert_eq!(region.remote_count(), 2);
    }
}
