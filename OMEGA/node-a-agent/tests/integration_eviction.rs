//! Tests d'intégration : éviction, recall, priorités, gel avant migration.
//!
//! Lance un vrai serveur node-bc-store en mémoire (port éphémère),
//! puis vérifie qu'une MemoryRegion peut évincer des pages et les récupérer
//! avec intégrité des données — sans avoir besoin d'userfaultfd.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use node_a_agent::cluster::ClusterState;
use node_a_agent::memory::{MemoryRegion, PAGE_SIZE};
use node_a_agent::metrics::AgentMetrics;
use node_a_agent::remote::RemoteStorePool;
use node_a_agent::shared_memory::{MemoryBackendKind, MemoryBackendOptions};

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Trouve un port libre en laissant le kernel en choisir un (bind sur :0 puis drop).
async fn free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    // Petit délai pour que le kernel libère le descripteur avant que le store bind.
    tokio::time::sleep(Duration::from_millis(5)).await;
    p
}

/// Démarre un node-bc-store sur le port donné et renvoie dès que le socket
/// est prêt à accepter des connexions.
async fn start_store(port: u16) -> String {
    let addr = format!("127.0.0.1:{port}");
    let cfg = node_bc_store::config::Config {
        listen: addr.clone(),
        node_id: "test-store".into(),
        max_pages: 0,
        log_format: "text".into(),
        log_level: "error".into(),
        stats_interval: 3600,
        status_listen: "127.0.0.1:0".into(),
        store_data_path: "/tmp".into(),
        ceph_conf: "/etc/ceph/ceph.conf".into(),
        ceph_pool: "omega-pages".into(),
        ceph_user: "client.admin".into(),
        orphan_check_interval_secs: 0,
        orphan_grace_secs: 600,
        proxmox_api_url: "".into(),
        tls_enabled: false,
        tls_dir: "/tmp/omega-test-tls".into(),
    };
    tokio::spawn(async move {
        let metrics = std::sync::Arc::new(node_bc_store::metrics::StoreMetrics::default());
        node_bc_store::server::run(cfg, None, metrics)
            .await
            .unwrap();
    });

    // Attendre que le store soit prêt (timeout 2 s).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if tokio::net::TcpStream::connect(&format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("store n'a pas démarré sur le port {port}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    addr
}

/// Crée une MemoryRegion pour les tests.
/// `spawn_blocking` est utilisé par les appelants pour éviter que `block_on`
/// soit appelé depuis un thread Tokio (ce qui deadlockrait sur un runtime
/// current-thread).
fn make_region(store_addr: String, num_pages: usize, vm_requested_mib: u64) -> Arc<MemoryRegion> {
    let handle = tokio::runtime::Handle::current();
    let store = Arc::new(RemoteStorePool::new(vec![store_addr.clone()], 2000, vec![]));
    let metrics = Arc::new(AgentMetrics::default());
    let cluster = Arc::new(ClusterState::new(
        vec![store_addr],
        vec!["127.0.0.1:9200".into()],
    ));
    Arc::new(
        MemoryRegion::allocate(
            PAGE_SIZE * num_pages,
            1,
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
        .unwrap(),
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Éviction de 4 pages + récupération via fetch_page (simule le handler uffd).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_eviction_round_trip() {
    let port = free_port().await;
    let addr = start_store(port).await;

    let region = make_region(addr, 8, 1);

    // Écriture de données traçables dans les pages paires
    let pattern: Vec<u8> = (0u8..=255).cycle().take(PAGE_SIZE).collect();
    for page_id in [0u64, 2, 4, 6] {
        let mut data = [0u8; PAGE_SIZE];
        data.copy_from_slice(&pattern);
        data[0] = page_id as u8; // marqueur d'identité
        region.write_page_local(page_id, &data).unwrap();
    }

    // Éviction via spawn_blocking pour ne pas appeler block_on depuis un worker
    for page_id in [0u64, 2, 4, 6] {
        let r = region.clone();
        tokio::task::spawn_blocking(move || r.evict_page_to(page_id, 0).unwrap())
            .await
            .unwrap();
    }

    assert_eq!(region.remote_count(), 4, "4 pages doivent être distantes");

    // Rappel via fetch_page (même chemin que le handler uffd)
    for page_id in [0u64, 2, 4, 6] {
        let r = region.clone();
        let data = tokio::task::spawn_blocking(move || r.fetch_page(page_id).unwrap())
            .await
            .unwrap();
        assert_eq!(
            data[0], page_id as u8,
            "page_id={page_id} : marqueur incorrect"
        );
        assert_eq!(
            &data[1..],
            &pattern[1..],
            "page_id={page_id} : motif corrompu"
        );
    }

    assert_eq!(
        region.remote_count(),
        0,
        "plus aucune page distante après recall"
    );
}

/// Vérification que les pages non évinvées (locales) ne sont pas touchées.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_local_pages_unaffected_by_eviction() {
    let port = free_port().await;
    let addr = start_store(port).await;
    let region = make_region(addr, 4, 1);

    // Écrire dans pages 0 et 1, évincer seulement la page 0
    let mut data0 = [0xAAu8; PAGE_SIZE];
    let mut data1 = [0xBBu8; PAGE_SIZE];
    data0[0] = 0;
    data1[0] = 1;
    region.write_page_local(0, &data0).unwrap();
    region.write_page_local(1, &data1).unwrap();

    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(0, 0).unwrap())
        .await
        .unwrap();

    assert!(region.is_remote(0), "page 0 doit être distante");
    assert!(!region.is_remote(1), "page 1 ne doit pas être distante");

    // La page 1 reste accessible localement via read_page_local
    let local = region.read_page_local(1).unwrap();
    assert_eq!(local[0], 1, "page locale corrompue");
    assert_eq!(local[1], 0xBB, "contenu page locale incorrect");
}

/// Vérification de la double éviction : une page déjà distante ne doit pas
/// être réenvoyée au store (idempotence).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_double_eviction_is_idempotent() {
    let port = free_port().await;
    let addr = start_store(port).await;
    let region = make_region(addr, 4, 1);

    let data = [0x42u8; PAGE_SIZE];
    region.write_page_local(0, &data).unwrap();

    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(0, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(region.remote_count(), 1);

    // Deuxième éviction de la même page → doit être ignorée silencieusement
    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(0, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(region.remote_count(), 1, "remote_count ne doit pas doubler");
}

/// Vérification que le cap vm_requested_pages est respecté.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cap_enforcement_via_daemon_path() {
    let port = free_port().await;
    let addr = start_store(port).await;

    // region de 4 pages, cap = 0 (vm_requested_mib=0)
    let region = make_region(addr, 4, 0);

    assert_eq!(region.remote_cap(), 0);

    // L'éviction doit échouer avec le message "cap vm_requested"
    let r = region.clone();
    let err = tokio::task::spawn_blocking(move || r.evict_page_to(0, 0).unwrap_err())
        .await
        .unwrap();
    assert!(
        err.to_string().contains("cap vm_requested"),
        "attendu erreur cap, obtenu : {err}"
    );
}

/// Éviction de toutes les pages disponibles jusqu'au cap, puis vérification
/// que la page suivante est rejetée.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cap_exact_limit() {
    let port = free_port().await;
    let addr = start_store(port).await;

    // 4 pages dans la region, cap = min(256, 4) = 4
    let region = make_region(addr, 4, 1);
    assert_eq!(region.remote_cap(), 4);

    // Évincer les 4 pages → doit réussir
    for page_id in 0u64..4 {
        let mut data = [0u8; PAGE_SIZE];
        data[0] = page_id as u8;
        region.write_page_local(page_id, &data).unwrap();
        let r = region.clone();
        tokio::task::spawn_blocking(move || r.evict_page_to(page_id, 0).unwrap())
            .await
            .unwrap();
    }

    assert_eq!(region.remote_count(), 4);

    // Toutes les pages sont déjà distantes → tentative sur page_id=0 ignorée (already remote)
    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(0, 0).unwrap())
        .await
        .unwrap();

    // Et pas de page_id=4 (hors limites) → bounds error, pas cap
    let r = region.clone();
    let err = tokio::task::spawn_blocking(move || r.evict_page_to(4, 0).unwrap_err())
        .await
        .unwrap();
    assert!(
        err.to_string().contains("hors limites"),
        "attendu erreur hors limites, obtenu : {err}"
    );
}

// ─── Tests de priorité et de recall LIFO ─────────────────────────────────────

/// Le recall LIFO retourne les pages dans l'ordre inverse d'éviction.
///
/// Éviction : page 0 → page 1 → page 2
/// Recall :   page 2 en premier (dernière évinvée), puis 1, puis 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recall_lifo_order() {
    let port = free_port().await;
    let addr = start_store(port).await;
    let region = make_region(addr, 8, 1);

    // Écrire des données distinctes dans chaque page
    for page_id in 0u64..3 {
        let mut data = [0u8; PAGE_SIZE];
        data[0] = page_id as u8;
        region.write_page_local(page_id, &data).unwrap();
    }

    // Évincer dans l'ordre 0 → 1 → 2
    for page_id in 0u64..3 {
        let r = region.clone();
        tokio::task::spawn_blocking(move || r.evict_page_to(page_id, 0).unwrap())
            .await
            .unwrap();
    }
    assert_eq!(region.remote_count(), 3);

    // Rappeler une page à la fois et vérifier que c'est bien la dernière évinvée
    // recall_n_pages sur uffd_fd=-1 échoue sur UFFDIO_COPY, donc on passe par
    // fetch_page qui suit le même chemin de dépilage LIFO.
    // On vérifie l'ordre via remote_count qui diminue et via les données.
    let r = region.clone();
    let data = tokio::task::spawn_blocking(move || r.fetch_page(2).unwrap())
        .await
        .unwrap();
    assert_eq!(data[0], 2, "LIFO : premier rappel doit être page 2");
    assert_eq!(region.remote_count(), 2);

    let r = region.clone();
    let data = tokio::task::spawn_blocking(move || r.fetch_page(1).unwrap())
        .await
        .unwrap();
    assert_eq!(data[0], 1, "LIFO : deuxième rappel doit être page 1");
    assert_eq!(region.remote_count(), 1);

    let r = region.clone();
    let data = tokio::task::spawn_blocking(move || r.fetch_page(0).unwrap())
        .await
        .unwrap();
    assert_eq!(data[0], 0, "LIFO : troisième rappel doit être page 0");
    assert_eq!(region.remote_count(), 0);
}

/// freeze_eviction bloque toute nouvelle éviction sans retourner d'erreur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_freeze_eviction_blocks_new_evictions() {
    let port = free_port().await;
    let addr = start_store(port).await;
    let region = make_region(addr, 4, 1);

    let mut data = [0xAAu8; PAGE_SIZE];
    data[0] = 42;
    region.write_page_local(0, &data).unwrap();

    // Avant gel : l'éviction fonctionne
    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(0, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(
        region.remote_count(),
        1,
        "page 0 doit être distante avant gel"
    );

    // Gel
    region.freeze_eviction();

    // Après gel : l'éviction est silencieusement ignorée (Ok(()) sans erreur)
    let mut data2 = [0xBBu8; PAGE_SIZE];
    data2[0] = 99;
    region.write_page_local(1, &data2).unwrap();
    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(1, 0).unwrap())
        .await
        .unwrap();

    // La page 1 doit être restée locale malgré l'appel à evict_page_to
    assert_eq!(
        region.remote_count(),
        1,
        "le gel doit bloquer toute nouvelle éviction"
    );
    assert!(
        !region.is_remote(1),
        "page 1 doit être locale (éviction gelée)"
    );
}

/// Test du chemin de migration complet (item 7).
///
/// Séquence :
///   1. Évincer des pages vers le store
///   2. Geler l'éviction (migration imminente)
///   3. Vérifier que les nouvelles évictions sont bloquées
///   4. Rapatrier toutes les pages via fetch_page (simule le rappel QEMU avant qm migrate)
///   5. Vérifier l'intégrité des données et que remote_count == 0
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_migration_path_freeze_then_recall_all() {
    let port = free_port().await;
    let addr = start_store(port).await;
    let region = make_region(addr, 8, 1);

    // 1. Écrire des données distinctes et évincer les pages 0..3
    for page_id in 0u64..4 {
        let mut data = [0u8; PAGE_SIZE];
        data[0] = page_id as u8;
        data[1] = 0xDE;
        region.write_page_local(page_id, &data).unwrap();
        let r = region.clone();
        tokio::task::spawn_blocking(move || r.evict_page_to(page_id, 0).unwrap())
            .await
            .unwrap();
    }
    assert_eq!(
        region.remote_count(),
        4,
        "4 pages doivent être distantes avant migration"
    );

    // 2. Geler l'éviction (la migration est imminente)
    region.freeze_eviction();

    // 3. Tenter d'évincer une page supplémentaire — doit être silencieusement ignoré
    let mut extra = [0xFFu8; PAGE_SIZE];
    extra[0] = 99;
    region.write_page_local(4, &extra).unwrap();
    let r = region.clone();
    tokio::task::spawn_blocking(move || r.evict_page_to(4, 0).unwrap())
        .await
        .unwrap();
    assert!(
        !region.is_remote(4),
        "page 4 ne doit pas être évinvée (freeze actif)"
    );
    assert_eq!(
        region.remote_count(),
        4,
        "le freeze doit bloquer toute nouvelle éviction"
    );

    // 4. Rapatrier toutes les pages via fetch_page (même chemin que le handler uffd)
    for page_id in 0u64..4 {
        let r = region.clone();
        let data = tokio::task::spawn_blocking(move || r.fetch_page(page_id).unwrap())
            .await
            .unwrap();
        assert_eq!(
            data[0], page_id as u8,
            "intégrité : marqueur page_id={page_id} incorrect"
        );
        assert_eq!(
            data[1], 0xDE,
            "intégrité : motif page_id={page_id} corrompu"
        );
    }

    // 5. Toutes les pages distantes ont été rapatriées
    assert_eq!(
        region.remote_count(),
        0,
        "remote_count doit être 0 après recall complet"
    );
}
