//! Configuration du daemon unifié V4.
//!
//! Chaque nœud du cluster exécute le même binaire avec sa propre configuration.
//! Un nœud peut être simultanément :
//!   - store     : accepte des pages distantes
//!   - monitor   : surveille les VMs locales QEMU
//!   - cluster   : participe à l'échange d'état du cluster

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name       = "omega-daemon",
    about      = "Daemon unifié de paging distant — cluster Proxmox (V4)",
    version    = env!("CARGO_PKG_VERSION"),
    long_about = None,
)]
pub struct Config {
    // ─── Identité du nœud ─────────────────────────────────────────────────
    /// Identifiant unique du nœud (ex: "pve-node1", "pve-node2")
    #[arg(long, env = "OMEGA_NODE_ID", default_value = "omega-node")]
    pub node_id: String,

    /// Adresse IP ou hostname de CE nœud (annoncée aux pairs)
    #[arg(long, env = "OMEGA_NODE_ADDR", default_value = "127.0.0.1")]
    pub node_addr: String,

    // ─── Store TCP (rôle store) ───────────────────────────────────────────
    /// Port d'écoute TCP du store de pages
    #[arg(long, env = "OMEGA_STORE_PORT", default_value_t = 9100)]
    pub store_port: u16,

    /// Limite maximale de pages stockées (0 = illimité)
    #[arg(long, env = "OMEGA_MAX_PAGES", default_value_t = 0)]
    pub max_pages: u64,

    // ─── API HTTP du cluster (rôle cluster) ───────────────────────────────
    /// Port HTTP de l'API cluster
    #[arg(long, env = "OMEGA_API_PORT", default_value_t = 9200)]
    pub api_port: u16,

    // ─── Pairs du cluster ─────────────────────────────────────────────────
    /// Adresses des pairs (host:api_port), séparées par des virgules.
    /// Ex: "192.168.1.2:9200,192.168.1.3:9200"
    #[arg(long, env = "OMEGA_PEERS", value_delimiter = ',', default_value = "")]
    pub peers: Vec<String>,

    // ─── Surveillance des VMs locales ─────────────────────────────────────
    /// Activer la surveillance des VMs QEMU locales
    #[arg(long, env = "OMEGA_MONITOR_VMS", default_value_t = true)]
    pub monitor_vms: bool,

    /// Répertoire des PIDs QEMU Proxmox
    #[arg(
        long,
        env = "OMEGA_QEMU_PID_DIR",
        default_value = "/var/run/qemu-server"
    )]
    pub qemu_pid_dir: String,

    /// Répertoire des configs VM Proxmox
    #[arg(
        long,
        env = "OMEGA_QEMU_CONF_DIR",
        default_value = "/etc/pve/qemu-server"
    )]
    pub qemu_conf_dir: String,

    // ─── Politique d'éviction ─────────────────────────────────────────────
    /// Seuil d'usage RAM (%) au-delà duquel on évince des pages
    #[arg(long, env = "OMEGA_EVICT_THRESHOLD_PCT", default_value_t = 75.0)]
    pub evict_threshold_pct: f64,

    /// Taille de la région mémoire de test userfaultfd (Mio, 0 = désactivé)
    #[arg(long, env = "OMEGA_TEST_REGION_MIB", default_value_t = 0)]
    pub test_region_mib: usize,

    /// vm_id utilisé pour la région de test userfaultfd
    #[arg(long, env = "OMEGA_TEST_VM_ID", default_value_t = 0)]
    pub test_vm_id: u32,

    // ─── Connexions sortantes vers d'autres stores ────────────────────────
    /// Timeout TCP vers les stores distants (ms)
    #[arg(long, env = "OMEGA_STORE_TIMEOUT_MS", default_value_t = 2000)]
    pub store_timeout_ms: u64,

    // ─── Logging ──────────────────────────────────────────────────────────
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log_level: String,

    #[arg(long, env = "OMEGA_LOG_FORMAT", default_value = "text")]
    pub log_format: String,

    /// Intervalle (secondes) des stats périodiques
    #[arg(long, env = "OMEGA_STATS_INTERVAL", default_value_t = 30)]
    pub stats_interval: u64,

    /// Intervalle du monitoring CPU local (ms)
    #[arg(long, env = "OMEGA_CPU_MONITOR_INTERVAL_MS", default_value_t = 1000)]
    pub cpu_monitor_interval_ms: u64,

    /// Activer le multiplexeur GPU local
    #[arg(long, env = "OMEGA_GPU_ENABLED", default_value_t = true)]
    pub gpu_enabled: bool,

    /// Socket Unix du multiplexeur GPU
    #[arg(
        long,
        env = "OMEGA_GPU_SOCKET_PATH",
        default_value = "/run/omega/gpu.sock"
    )]
    pub gpu_socket_path: String,

    /// Render node DRM explicite (si vide, découverte auto)
    #[arg(long, env = "OMEGA_GPU_RENDER_NODE", default_value = "")]
    pub gpu_render_node: String,

    /// VRAM totale forcée (Mio) pour publication cluster ; 0 = auto-détection
    #[arg(long, env = "OMEGA_GPU_TOTAL_VRAM_MIB", default_value_t = 0)]
    pub gpu_total_vram_mib: u64,
}

impl Config {
    /// Adresse d'écoute complète du store TCP
    pub fn store_addr(&self) -> String {
        format!("0.0.0.0:{}", self.store_port)
    }

    /// Adresse d'écoute complète de l'API HTTP
    pub fn api_addr(&self) -> String {
        format!("0.0.0.0:{}", self.api_port)
    }

    /// Adresse annoncée du store de ce nœud (pour les pairs)
    pub fn store_public_addr(&self) -> String {
        format!("{}:{}", self.node_addr, self.store_port)
    }

    /// Adresse annoncée de l'API de ce nœud (pour les pairs)
    pub fn api_public_addr(&self) -> String {
        format!("{}:{}", self.node_addr, self.api_port)
    }
}
