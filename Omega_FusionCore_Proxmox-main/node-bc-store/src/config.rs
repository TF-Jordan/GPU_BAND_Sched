//! Configuration CLI du store.

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name        = "node-bc-store",
    about       = "Store de pages mémoire distantes — nœuds B/C du cluster omega-remote-paging",
    version     = env!("CARGO_PKG_VERSION"),
    long_about  = None,
)]
pub struct Config {
    /// Adresse d'écoute TCP (IP:port)
    #[arg(long, default_value = "0.0.0.0:9100", env = "STORE_LISTEN")]
    pub listen: String,

    /// Identifiant du nœud (ex: "node-b", "node-c") — utilisé dans les logs
    #[arg(long, default_value = "node-store", env = "STORE_NODE_ID")]
    pub node_id: String,

    /// Limite maximale de pages stockées (0 = illimité)
    #[arg(long, default_value_t = 0, env = "STORE_MAX_PAGES")]
    pub max_pages: u64,

    /// Format de log : "text" ou "json"
    #[arg(long, default_value = "text", env = "STORE_LOG_FORMAT")]
    pub log_format: String,

    /// Niveau de log (RUST_LOG syntax)
    #[arg(long, default_value = "info", env = "RUST_LOG")]
    pub log_level: String,

    /// Intervalle (secondes) d'affichage des stats périodiques
    #[arg(long, default_value_t = 30, env = "STORE_STATS_INTERVAL")]
    pub stats_interval: u64,

    /// Adresse d'écoute du serveur HTTP de status cluster
    #[arg(long, default_value = "0.0.0.0:9200", env = "STORE_STATUS_LISTEN")]
    pub status_listen: String,

    /// Chemin du répertoire de données du store (utilisé pour les métriques disque local).
    #[arg(long, default_value = "/var/lib/omega-store", env = "STORE_DATA_PATH")]
    pub store_data_path: String,

    // ─── Ceph RADOS (auto-détecté — aucune activation manuelle requise) ───────
    /// Chemin du fichier de configuration Ceph.
    #[arg(long, default_value = "/etc/ceph/ceph.conf", env = "STORE_CEPH_CONF")]
    pub ceph_conf: String,

    /// Pool RADOS où stocker les pages.
    #[arg(long, default_value = "omega-pages", env = "STORE_CEPH_POOL")]
    pub ceph_pool: String,

    /// Utilisateur Ceph (ex: "client.omega" ou "client.admin").
    #[arg(long, default_value = "client.admin", env = "STORE_CEPH_USER")]
    pub ceph_user: String,

    // ─── TLS (chiffrement du canal de paging) ────────────────────────────────
    /// Active le chiffrement TLS sur le canal de paging TCP (port 9100).
    /// Le certificat est auto-signé et auto-généré au premier démarrage.
    #[arg(long, default_value_t = false, env = "STORE_TLS_ENABLED")]
    pub tls_enabled: bool,

    /// Répertoire où stocker les fichiers TLS (cert.pem, key.pem).
    #[arg(long, default_value = "/etc/omega-store/tls", env = "STORE_TLS_DIR")]
    pub tls_dir: String,

    // ─── Nettoyage des pages orphelines ──────────────────────────────────────
    /// Intervalle (secondes) entre deux passes de détection d'orphelins.
    /// 0 = désactivé.
    #[arg(long, default_value_t = 300, env = "STORE_ORPHAN_CHECK_INTERVAL_SECS")]
    pub orphan_check_interval_secs: u64,

    /// Délai de grâce (secondes) avant suppression d'un orphelin détecté.
    /// Evite de supprimer des pages d'une VM en cours de migration ou de redémarrage.
    #[arg(long, default_value_t = 600, env = "STORE_ORPHAN_GRACE_SECS")]
    pub orphan_grace_secs: u64,

    /// URL du cluster Proxmox pour interroger la liste des VMs.
    /// Vide = utiliser `pvesh` local (node store est sur un nœud Proxmox).
    #[arg(long, default_value = "", env = "STORE_PROXMOX_API_URL")]
    pub proxmox_api_url: String,
}
