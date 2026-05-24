use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use node_a_agent::shared_memory::MemoryBackendMetadata;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "omega-qemu-launcher",
    about = "Orchestration locale agent memfd + backend QEMU omega",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Démarre l'agent, attend les métadonnées, puis produit les arguments QEMU.
    Prepare(PrepareArgs),
    /// Prépare Omega puis lance un binaire QEMU réel avec les arguments omega injectés.
    ExecQemu(ExecQemuArgs),
    /// Déduit vm_id et RAM depuis une ligne QEMU/Proxmox, prépare Omega puis lance QEMU.
    ExecProxmox(ExecProxmoxArgs),
    /// Affiche l'état connu d'une VM préparée.
    Status(StateSelector),
    /// Arrête l'agent lancé pour une VM préparée.
    Stop(StopArgs),
    /// Recalcule les arguments QEMU depuis un état déjà écrit.
    QemuArgs(StateSelector),
    /// Écrit un wrapper shell réutilisable qui lance QEMU via Omega.
    WriteWrapper(WriteWrapperArgs),
    /// Écrit un wrapper shell destiné à remplacer le binaire QEMU appelé par Proxmox.
    WriteProxmoxWrapper(WriteProxmoxWrapperArgs),
    /// Vérifie les prérequis locaux pour lancer QEMU via Omega.
    Doctor(DoctorArgs),
    /// Arrête l'agent et nettoie l'état runtime d'une VM.
    Cleanup(CleanupArgs),
}

#[derive(Parser, Debug, Clone)]
struct PrepareArgs {
    #[arg(long)]
    vm_id: u32,

    /// Taille de RAM guest que QEMU doit mapper via le backend omega.
    #[arg(long)]
    size_mib: usize,

    /// Taille de région gérée par l'agent. Par défaut: même valeur que size-mib.
    #[arg(long)]
    region_mib: Option<usize>,

    #[arg(
        long,
        default_value = "127.0.0.1:9100,127.0.0.1:9101",
        value_delimiter = ','
    )]
    stores: Vec<String>,

    #[arg(long)]
    agent_bin: Option<PathBuf>,

    #[arg(long, default_value = "/var/lib/omega-qemu", env = "OMEGA_RUN_DIR")]
    run_dir: PathBuf,

    #[arg(long)]
    metadata_path: Option<PathBuf>,

    #[arg(long)]
    state_path: Option<PathBuf>,

    #[arg(long)]
    log_path: Option<PathBuf>,

    #[arg(long, default_value = "ram0")]
    object_id: String,

    #[arg(long, default_value_t = 30)]
    start_timeout_secs: u64,

    #[arg(long, default_value = "text")]
    log_format: String,

    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long, default_value_t = 2000)]
    store_timeout_ms: u64,

    #[arg(long)]
    memfd_name: Option<String>,
}

#[derive(Parser, Debug, Clone)]
struct StateSelector {
    #[arg(long)]
    vm_id: u32,

    #[arg(long, default_value = "/var/lib/omega-qemu", env = "OMEGA_RUN_DIR")]
    run_dir: PathBuf,

    #[arg(long)]
    state_path: Option<PathBuf>,
}

#[derive(Parser, Debug, Clone)]
struct StopArgs {
    #[command(flatten)]
    state: StateSelector,

    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,
}

#[derive(Parser, Debug, Clone)]
struct ExecQemuArgs {
    #[command(flatten)]
    prepare: PrepareArgs,

    #[arg(long)]
    qemu_bin: PathBuf,

    #[arg(last = true, trailing_var_arg = true)]
    qemu_args: Vec<String>,
}

#[derive(Parser, Debug, Clone)]
struct ExecProxmoxArgs {
    /// VMID forcé. Si absent, le launcher essaie de le déduire de la ligne QEMU.
    #[arg(long)]
    vm_id: Option<u32>,

    /// RAM guest forcée en MiB. Si absente, elle est déduite de `-m`.
    #[arg(long)]
    size_mib: Option<usize>,

    #[arg(
        long,
        default_value = "127.0.0.1:9100,127.0.0.1:9101",
        value_delimiter = ',',
        env = "OMEGA_STORES"
    )]
    stores: Vec<String>,

    #[arg(long, env = "OMEGA_AGENT_BIN")]
    agent_bin: Option<PathBuf>,

    #[arg(long, default_value = "/var/lib/omega-qemu", env = "OMEGA_RUN_DIR")]
    run_dir: PathBuf,

    #[arg(long, default_value = "ram0", env = "OMEGA_OBJECT_ID")]
    object_id: String,

    #[arg(long, default_value_t = 30, env = "OMEGA_START_TIMEOUT_SECS")]
    start_timeout_secs: u64,

    #[arg(long, default_value = "text", env = "OMEGA_LOG_FORMAT")]
    log_format: String,

    #[arg(long, default_value = "info", env = "OMEGA_LOG_LEVEL")]
    log_level: String,

    #[arg(long, default_value_t = 2000, env = "OMEGA_STORE_TIMEOUT_MS")]
    store_timeout_ms: u64,

    #[arg(long, env = "OMEGA_MEMFD_NAME")]
    memfd_name: Option<String>,

    /// Chemin vers omega-uffd-bridge.so. Injecté uniquement dans le vrai QEMU.
    #[arg(long, env = "OMEGA_BRIDGE_LIB")]
    bridge_lib: Option<PathBuf>,

    #[arg(long, env = "OMEGA_REAL_QEMU_BIN")]
    qemu_bin: PathBuf,

    #[arg(last = true, trailing_var_arg = true)]
    qemu_args: Vec<String>,
}

#[derive(Parser, Debug, Clone)]
struct WriteWrapperArgs {
    #[command(flatten)]
    prepare: PrepareArgs,

    #[arg(long)]
    qemu_bin: PathBuf,

    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct QemuLaunchState {
    vm_id: u32,
    size_mib: usize,
    object_id: String,
    agent_pid: u32,
    agent_bin: String,
    metadata_path: PathBuf,
    state_path: PathBuf,
    log_path: PathBuf,
    qemu_args: Vec<String>,
    metadata: MemoryBackendMetadata,
}

#[derive(Parser, Debug, Clone)]
struct WriteProxmoxWrapperArgs {
    #[arg(
        long,
        default_value = "127.0.0.1:9100,127.0.0.1:9101",
        value_delimiter = ','
    )]
    stores: Vec<String>,

    #[arg(long)]
    qemu_bin: PathBuf,

    #[arg(long)]
    agent_bin: Option<PathBuf>,

    #[arg(long, default_value = "/var/lib/omega-qemu", env = "OMEGA_RUN_DIR")]
    run_dir: PathBuf,

    #[arg(long, default_value = "ram0")]
    object_id: String,

    #[arg(long, default_value_t = 30)]
    start_timeout_secs: u64,

    #[arg(long, default_value = "text")]
    log_format: String,

    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long, default_value_t = 2000)]
    store_timeout_ms: u64,

    #[arg(long)]
    memfd_name: Option<String>,

    /// Chemin vers omega-uffd-bridge.so (si fourni, injecté via LD_PRELOAD dans QEMU).
    #[arg(long)]
    bridge_lib: Option<PathBuf>,

    #[arg(long)]
    output: PathBuf,
}

#[derive(Parser, Debug, Clone)]
struct DoctorArgs {
    #[arg(long, env = "OMEGA_REAL_QEMU_BIN")]
    qemu_bin: Option<PathBuf>,

    #[arg(long, env = "OMEGA_AGENT_BIN")]
    agent_bin: Option<PathBuf>,

    #[arg(long, env = "OMEGA_BRIDGE_LIB")]
    bridge_lib: Option<PathBuf>,

    #[arg(long, default_value = "/var/lib/omega-qemu", env = "OMEGA_RUN_DIR")]
    run_dir: PathBuf,
}

#[derive(Parser, Debug, Clone)]
struct CleanupArgs {
    #[command(flatten)]
    state: StateSelector,

    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,

    /// Conserve agent.log pour diagnostic.
    #[arg(long)]
    keep_log: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Prepare(args) => {
            let state = prepare(args)?;
            println!("{}", serde_json::to_string_pretty(&state)?);
        }
        Commands::ExecQemu(args) => {
            let code = exec_qemu(args)?;
            std::process::exit(code);
        }
        Commands::ExecProxmox(args) => {
            let code = exec_proxmox(args)?;
            std::process::exit(code);
        }
        Commands::Status(sel) => {
            let state = load_state(&sel)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&StatusOutput::from_state(&state))?
            );
        }
        Commands::Stop(args) => {
            let state = load_state(&args.state)?;
            stop_agent(&state, Duration::from_secs(args.timeout_secs))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&StatusOutput::from_state(&state))?
            );
        }
        Commands::QemuArgs(sel) => {
            let state = load_state(&sel)?;
            println!("{}", serde_json::to_string_pretty(&state.qemu_args)?);
        }
        Commands::WriteWrapper(args) => {
            let path = write_wrapper(args)?;
            println!("{}", path.display());
        }
        Commands::WriteProxmoxWrapper(args) => {
            let path = write_proxmox_wrapper(args)?;
            println!("{}", path.display());
        }
        Commands::Doctor(args) => {
            let report = doctor(args)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.ok {
                std::process::exit(2);
            }
        }
        Commands::Cleanup(args) => {
            let report = cleanup(args)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn prepare(args: PrepareArgs) -> Result<QemuLaunchState> {
    let state_path = args
        .state_path
        .clone()
        .unwrap_or_else(|| default_state_path(&args.run_dir, args.vm_id));
    if state_path.exists() {
        let existing = load_state_from_path(&state_path)?;
        if state_is_usable(&existing) {
            return Ok(existing);
        }
        let _ = stop_agent(&existing, Duration::from_secs(3));
    }

    let metadata_path = args
        .metadata_path
        .clone()
        .unwrap_or_else(|| default_metadata_path(&args.run_dir, args.vm_id));
    let log_path = args
        .log_path
        .clone()
        .unwrap_or_else(|| default_log_path(&args.run_dir, args.vm_id));
    let memfd_name = args
        .memfd_name
        .clone()
        .unwrap_or_else(|| format!("omega-vm-{}-ram", args.vm_id));
    let region_mib = args.region_mib.unwrap_or(args.size_mib);
    let agent_bin = resolve_agent_bin(args.agent_bin.clone())?;

    for path in [&state_path, &metadata_path, &log_path] {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("création du répertoire {}", parent.display()))?;
        }
    }

    // Supprimer les métadonnées d'un run précédent : wait_for_metadata retourne
    // dès que le fichier existe, sans vérifier que le PID qu'il contient est vivant.
    // Si on ne supprime pas, QEMU reçoit /proc/<ancien_pid>/fd/<fd> qui n'existe plus.
    if metadata_path.exists() {
        fs::remove_file(&metadata_path).ok();
    }

    let stdout = open_log_file(&log_path)?;
    let stderr = open_log_file(&log_path)?;

    let mut child = Command::new(&agent_bin);
    child
        .arg("--vm-id")
        .arg(args.vm_id.to_string())
        .arg("--stores")
        .arg(args.stores.join(","))
        .arg("--backend")
        .arg("memfd")
        .arg("--memfd-name")
        .arg(memfd_name)
        .arg("--export-metadata")
        .arg(&metadata_path)
        .arg("--mode")
        .arg("daemon")
        .arg("--region-mib")
        .arg(region_mib.to_string())
        .arg("--log-format")
        .arg(&args.log_format)
        .arg("--store-timeout-ms")
        .arg(args.store_timeout_ms.to_string())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .stdin(Stdio::null())
        .env("RUST_LOG", &args.log_level);

    let mut child = child
        .spawn()
        .with_context(|| format!("lancement de l'agent {}", agent_bin.display()))?;
    let agent_pid = child.id();

    let metadata = wait_for_metadata(&metadata_path, Duration::from_secs(args.start_timeout_secs))
        .with_context(|| format!("attente des métadonnées {}", metadata_path.display()))?;

    thread::sleep(Duration::from_millis(300));
    if let Some(status) = child
        .try_wait()
        .context("vérification de l'état du processus agent")?
    {
        bail!(
            "l'agent s'est arrêté prématurément (code {:?}) — voir {}",
            status.code(),
            log_path.display()
        );
    }

    let qemu_args = build_qemu_args(&args.object_id, args.size_mib, &metadata)?;

    let state = QemuLaunchState {
        vm_id: args.vm_id,
        size_mib: args.size_mib,
        object_id: args.object_id,
        agent_pid,
        agent_bin: agent_bin.display().to_string(),
        metadata_path,
        state_path: state_path.clone(),
        log_path,
        qemu_args,
        metadata,
    };

    fs::write(&state_path, serde_json::to_vec_pretty(&state)?)
        .with_context(|| format!("écriture de l'état {}", state_path.display()))?;

    Ok(state)
}

fn load_state(sel: &StateSelector) -> Result<QemuLaunchState> {
    let path = sel
        .state_path
        .clone()
        .unwrap_or_else(|| default_state_path(&sel.run_dir, sel.vm_id));
    load_state_from_path(&path)
}

fn load_state_from_path(path: &Path) -> Result<QemuLaunchState> {
    let raw = fs::read(path).with_context(|| format!("lecture de l'état {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("décodage de l'état {}", path.display()))
}

fn exec_qemu(args: ExecQemuArgs) -> Result<i32> {
    let state = prepare(args.prepare.clone())?;

    let mut cmd = Command::new(&args.qemu_bin);
    cmd.args(&state.qemu_args)
        .args(&args.qemu_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("OMEGA_QEMU_STATE_PATH", &state.state_path)
        .env("OMEGA_QEMU_METADATA_PATH", &state.metadata_path)
        .env("OMEGA_QEMU_AGENT_PID", state.agent_pid.to_string());

    let status = cmd
        .status()
        .with_context(|| format!("exécution de {}", args.qemu_bin.display()))?;

    let _ = stop_agent(&state, Duration::from_secs(10));

    Ok(status.code().unwrap_or(1))
}

/// Migration entrante : démarre l'agent omega local, injecte (ou patche) le backend
/// mémoire omega, puis exec() QEMU avec argv[0]="/usr/bin/kvm" pour que
/// parse_cmdline de Proxmox reconnaisse le processus.
fn exec_proxmox_incoming(args: ExecProxmoxArgs, vm_id: u32, size_mib: usize) -> Result<i32> {
    // Les args venant de Proxmox pour le nœud cible ne contiennent jamais l'objet
    // omega (il est injecté à l'exécution, pas dans la config). On distingue tout
    // de même les deux cas pour le patching de mem-path.
    let has_omega_object = args.qemu_args.windows(2).any(|w| {
        w[0] == "-object" && w[1].contains("memory-backend-file") && w[1].contains("mem-path=")
    });

    let prepare_args = PrepareArgs {
        vm_id,
        size_mib,
        region_mib: Some(size_mib),
        stores: args.stores.clone(),
        agent_bin: args.agent_bin.clone(),
        run_dir: args.run_dir.clone(),
        metadata_path: None,
        state_path: None,
        log_path: None,
        object_id: args.object_id.clone(),
        start_timeout_secs: args.start_timeout_secs,
        log_format: args.log_format.clone(),
        log_level: args.log_level.clone(),
        store_timeout_ms: args.store_timeout_ms,
        memfd_name: args.memfd_name.clone(),
    };
    let state = prepare(prepare_args)?;

    // Construire les args QEMU finaux avec le backend omega local.
    let final_args = if has_omega_object {
        // L'arg -object est déjà là (cas théorique) : patcher mem-path seulement.
        patch_migration_mem_path(&args.qemu_args, &state.object_id, &state.metadata)?
    } else {
        // Cas normal de migration Proxmox : injecter l'objet omega + patcher -machine.
        inject_omega_qemu_args(
            &args.qemu_args,
            &state.object_id,
            state.size_mib,
            &state.metadata,
        )?
    };

    // exec() : QEMU remplace ce processus → sd_notify depuis le bon PID.
    // argv[0] = "/usr/bin/kvm" pour que parse_cmdline de Proxmox reconnaisse le process.
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&args.qemu_bin);
    cmd.arg0("/usr/bin/kvm")
        .args(&final_args)
        .env("OMEGA_QEMU_STATE_PATH", &state.state_path)
        .env("OMEGA_QEMU_METADATA_PATH", &state.metadata_path)
        .env("OMEGA_QEMU_AGENT_PID", state.agent_pid.to_string())
        .env("OMEGA_QEMU_VM_ID", state.vm_id.to_string())
        .env("OMEGA_QEMU_SIZE_MIB", state.size_mib.to_string());
    if let Some(bridge_lib) = &args.bridge_lib {
        cmd.env("LD_PRELOAD", bridge_lib);
        cmd.env("OMEGA_BRIDGE_LIB", bridge_lib);
    }
    let err = cmd.exec();
    Err(anyhow::anyhow!(
        "exec QEMU migration ({}): {}",
        args.qemu_bin.display(),
        err
    ))
}

/// Remplace mem-path= dans l'arg -object memory-backend-file avec le chemin local.
fn patch_migration_mem_path(
    qemu_args: &[String],
    object_id: &str,
    metadata: &MemoryBackendMetadata,
) -> Result<Vec<String>> {
    let new_path = metadata
        .proc_fd_path
        .as_deref()
        .context("proc_fd_path absent des métadonnées")?;

    let mut out = Vec::with_capacity(qemu_args.len());
    let mut idx = 0;
    while idx < qemu_args.len() {
        if qemu_args[idx] == "-object" {
            if let Some(next) = qemu_args.get(idx + 1) {
                if next.contains("memory-backend-file")
                    && next.contains(&format!("id={object_id}"))
                    && next.contains("mem-path=")
                {
                    let patched = replace_mem_path_in_object_arg(next, new_path);
                    out.push(qemu_args[idx].clone());
                    out.push(patched);
                    idx += 2;
                    continue;
                }
            }
        }
        out.push(qemu_args[idx].clone());
        idx += 1;
    }
    Ok(out)
}

fn replace_mem_path_in_object_arg(arg: &str, new_path: &str) -> String {
    arg.split(',')
        .map(|part| {
            if part.starts_with("mem-path=") {
                format!("mem-path={new_path}")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Déduit la RAM depuis un arg -object memory-backend-file,size=NM,...
fn infer_size_mib_from_object_args(args: &[String]) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == "-object" {
            if let Some(next) = args.get(idx + 1) {
                if next.contains("memory-backend-file") {
                    for part in next.split(',') {
                        if let Some(val) = part.strip_prefix("size=") {
                            return parse_qemu_memory_mib(val);
                        }
                    }
                }
            }
        }
        idx += 1;
    }
    None
}

/// Remplace le processus courant par QEMU (exec syscall).
/// Proxmox crée un scope systemd pour le binaire qu'il lance (/usr/bin/kvm).
/// En utilisant exec(), QEMU devient le processus principal du scope et peut
/// envoyer sd_notify directement — sans quoi Proxmox timedout.
fn exec_bypass(qemu_bin: &Path, qemu_args: &[String]) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(qemu_bin);
    // Proxmox's parse_cmdline rejects argv[0] not ending in "kvm" or "qemu-*".
    // kvm.real → /usr/bin/kvm.real → fails the filter → vm_running_locally returns undef.
    // Use argv[0] = "/usr/bin/kvm" so Proxmox recognises the process.
    cmd.arg0("/usr/bin/kvm").args(qemu_args);
    let err = cmd.exec();
    Err(anyhow::anyhow!(
        "exec QEMU bypass échoué ({}): {}",
        qemu_bin.display(),
        err
    ))
}

fn exec_proxmox(args: ExecProxmoxArgs) -> Result<i32> {
    let is_incoming = args.qemu_args.iter().any(|a| a == "-incoming");

    let vm_id = match args.vm_id {
        Some(id) => id,
        None => match infer_vmid_from_qemu_args(&args.qemu_args) {
            Some(id) => id,
            None => {
                // Invocation utilitaire (détection flags CPU, etc.) — pas un vrai démarrage VM.
                return exec_bypass(&args.qemu_bin, &args.qemu_args);
            }
        },
    };
    let size_mib = match args.size_mib {
        Some(size) => size,
        None => match infer_size_mib_from_qemu_args(&args.qemu_args)
            .or_else(|| infer_size_mib_from_object_args(&args.qemu_args))
        {
            Some(size) => size,
            None if is_incoming => {
                // Migration sans RAM déductible : bypass sans omega.
                return exec_bypass(&args.qemu_bin, &args.qemu_args);
            }
            None => {
                bail!("impossible de déduire la RAM guest depuis les arguments QEMU/Proxmox");
            }
        },
    };

    if is_incoming {
        // Migration entrante : démarrer un agent local, patcher mem-path, puis exec().
        // Le nœud source envoie ses propres chemins /proc/PID/fd/FD qui n'existent pas ici.
        return exec_proxmox_incoming(args, vm_id, size_mib);
    }

    let prepare_args = PrepareArgs {
        vm_id,
        size_mib,
        region_mib: Some(size_mib),
        stores: args.stores.clone(),
        agent_bin: args.agent_bin.clone(),
        run_dir: args.run_dir.clone(),
        metadata_path: None,
        state_path: None,
        log_path: None,
        object_id: args.object_id.clone(),
        start_timeout_secs: args.start_timeout_secs,
        log_format: args.log_format.clone(),
        log_level: args.log_level.clone(),
        store_timeout_ms: args.store_timeout_ms,
        memfd_name: args.memfd_name.clone(),
    };

    let state = prepare(prepare_args)?;
    let final_qemu_args = inject_omega_qemu_args(
        &args.qemu_args,
        &state.object_id,
        state.size_mib,
        &state.metadata,
    )?;

    // exec() remplace ce processus par QEMU.
    // argv[0] = "/usr/bin/kvm" : convention QEMU "appelé sous le nom kvm → activer KVM",
    // et parse_cmdline de Proxmox accepte les processus dont argv[0] termine par "kvm".
    // Sans cela, vm_running_locally() retourne undef et `qm migrate --online` bascule
    // en migration offline (ou échoue).
    // Nettoyage agent : hookscript post-stop appelle omega-qemu-launcher stop.
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&args.qemu_bin);
    cmd.arg0("/usr/bin/kvm")
        .args(&final_qemu_args)
        .env("OMEGA_QEMU_STATE_PATH", &state.state_path)
        .env("OMEGA_QEMU_METADATA_PATH", &state.metadata_path)
        .env("OMEGA_QEMU_AGENT_PID", state.agent_pid.to_string())
        .env("OMEGA_QEMU_VM_ID", state.vm_id.to_string())
        .env("OMEGA_QEMU_SIZE_MIB", state.size_mib.to_string());
    if let Some(bridge_lib) = &args.bridge_lib {
        cmd.env("LD_PRELOAD", bridge_lib);
        cmd.env("OMEGA_BRIDGE_LIB", bridge_lib);
    }
    let err = cmd.exec();
    Err(anyhow::anyhow!(
        "exec QEMU ({}): {}",
        args.qemu_bin.display(),
        err
    ))
}

fn write_wrapper(args: WriteWrapperArgs) -> Result<PathBuf> {
    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("création du répertoire {}", parent.display()))?;
    }

    let launcher = std::env::current_exe().context("localisation du launcher courant")?;
    let shell = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nexec {launcher} exec-qemu \\\n  --vm-id {vm_id} \\\n  --size-mib {size_mib} \\\n  --stores '{stores}' \\\n  --run-dir '{run_dir}' \\\n  --object-id '{object_id}' \\\n  --start-timeout-secs {timeout} \\\n  --log-format '{log_format}' \\\n  --log-level '{log_level}' \\\n  --store-timeout-ms {store_timeout} \\\n  --memfd-name '{memfd_name}' \\\n  --qemu-bin '{qemu_bin}' \\\n  -- \"$@\"\n",
        launcher = shell_escape_path(&launcher),
        vm_id = args.prepare.vm_id,
        size_mib = args.prepare.size_mib,
        stores = shell_escape_str(&args.prepare.stores.join(",")),
        run_dir = shell_escape_path(&args.prepare.run_dir),
        object_id = shell_escape_str(&args.prepare.object_id),
        timeout = args.prepare.start_timeout_secs,
        log_format = shell_escape_str(&args.prepare.log_format),
        log_level = shell_escape_str(&args.prepare.log_level),
        store_timeout = args.prepare.store_timeout_ms,
        memfd_name = shell_escape_str(
            &args
                .prepare
                .memfd_name
                .clone()
                .unwrap_or_else(|| format!("omega-vm-{}-ram", args.prepare.vm_id)),
        ),
        qemu_bin = shell_escape_path(&args.qemu_bin),
    );

    fs::write(&args.output, shell)
        .with_context(|| format!("écriture du wrapper {}", args.output.display()))?;
    let mut perms = fs::metadata(&args.output)
        .with_context(|| format!("lecture des permissions {}", args.output.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&args.output, perms)
        .with_context(|| format!("chmod +x {}", args.output.display()))?;
    Ok(args.output)
}

fn write_proxmox_wrapper(args: WriteProxmoxWrapperArgs) -> Result<PathBuf> {
    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("création du répertoire {}", parent.display()))?;
    }

    let launcher = std::env::current_exe().context("localisation du launcher courant")?;
    let mut lines = vec![
        "#!/usr/bin/env bash".to_string(),
        "set -euo pipefail".to_string(),
        format!(
            "export OMEGA_STORES='{}'",
            shell_escape_str(&args.stores.join(","))
        ),
        format!(
            "export OMEGA_RUN_DIR='{}'",
            shell_escape_path(&args.run_dir)
        ),
        format!(
            "export OMEGA_OBJECT_ID='{}'",
            shell_escape_str(&args.object_id)
        ),
        format!(
            "export OMEGA_START_TIMEOUT_SECS='{}'",
            args.start_timeout_secs
        ),
        format!(
            "export OMEGA_LOG_FORMAT='{}'",
            shell_escape_str(&args.log_format)
        ),
        format!(
            "export OMEGA_LOG_LEVEL='{}'",
            shell_escape_str(&args.log_level)
        ),
        format!("export OMEGA_STORE_TIMEOUT_MS='{}'", args.store_timeout_ms),
        format!(
            "export OMEGA_REAL_QEMU_BIN='{}'",
            shell_escape_path(&args.qemu_bin)
        ),
    ];
    if let Some(agent_bin) = args.agent_bin {
        lines.push(format!(
            "export OMEGA_AGENT_BIN='{}'",
            shell_escape_path(&agent_bin)
        ));
    }
    if let Some(memfd_name) = args.memfd_name {
        lines.push(format!(
            "export OMEGA_MEMFD_NAME='{}'",
            shell_escape_str(&memfd_name)
        ));
    }
    if let Some(bridge_lib) = args.bridge_lib {
        lines.push(format!(
            "export OMEGA_BRIDGE_LIB='{}'",
            shell_escape_path(&bridge_lib)
        ));
    }
    // Proxmox vérifie la version QEMU avec --version/-version avant chaque démarrage.
    // Le passer directement au vrai binaire pour éviter l'échec de exec-proxmox.
    lines.push(format!(
        "if [ \"${{1:-}}\" = \"--version\" ] || [ \"${{1:-}}\" = \"-version\" ]; then exec \"$OMEGA_REAL_QEMU_BIN\" \"$@\"; fi"
    ));
    lines.push(format!(
        "exec {} exec-proxmox -- \"$@\"",
        shell_escape_path(&launcher)
    ));

    fs::write(&args.output, lines.join("\n") + "\n")
        .with_context(|| format!("écriture du wrapper {}", args.output.display()))?;
    let mut perms = fs::metadata(&args.output)
        .with_context(|| format!("lecture des permissions {}", args.output.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&args.output, perms)
        .with_context(|| format!("chmod +x {}", args.output.display()))?;
    Ok(args.output)
}

fn doctor(args: DoctorArgs) -> Result<DoctorReport> {
    let mut checks = Vec::new();

    checks.push(check_path("run_dir_parent", args.run_dir.parent(), true));
    checks.push(check_userfaultfd());

    if let Some(qemu_bin) = args.qemu_bin {
        checks.push(check_file("qemu_bin", &qemu_bin, true));
    }
    if let Some(agent_bin) = args.agent_bin {
        checks.push(check_file("agent_bin", &agent_bin, true));
    } else {
        checks.push(check_agent_resolvable());
    }
    if let Some(bridge_lib) = args.bridge_lib {
        checks.push(check_file("bridge_lib", &bridge_lib, false));
    }

    let ok = checks.iter().all(|check| check.ok);
    Ok(DoctorReport { ok, checks })
}

fn cleanup(args: CleanupArgs) -> Result<CleanupReport> {
    let state_path = args
        .state
        .state_path
        .clone()
        .unwrap_or_else(|| default_state_path(&args.state.run_dir, args.state.vm_id));
    let vm_dir = default_vm_dir(&args.state.run_dir, args.state.vm_id);
    let mut stopped_agent = false;
    let mut removed = Vec::new();

    if state_path.exists() {
        if let Ok(state) = load_state_from_path(&state_path) {
            stop_agent(&state, Duration::from_secs(args.timeout_secs)).ok();
            stopped_agent = true;
        }
    }

    let paths = [
        default_metadata_path(&args.state.run_dir, args.state.vm_id),
        default_state_path(&args.state.run_dir, args.state.vm_id),
        vm_dir.join("uffd.sock"),
    ];
    for path in paths {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("suppression {}", path.display()))?;
            removed.push(path);
        }
    }

    let log_path = default_log_path(&args.state.run_dir, args.state.vm_id);
    if !args.keep_log && log_path.exists() {
        fs::remove_file(&log_path)
            .with_context(|| format!("suppression {}", log_path.display()))?;
        removed.push(log_path);
    }

    Ok(CleanupReport {
        vm_id: args.state.vm_id,
        stopped_agent,
        removed,
    })
}

fn resolve_agent_bin(agent_bin: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = agent_bin {
        return Ok(path);
    }

    let current = std::env::current_exe().context("localisation du binaire courant")?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("node-a-agent");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    Ok(PathBuf::from("node-a-agent"))
}

fn default_vm_dir(run_dir: &Path, vm_id: u32) -> PathBuf {
    run_dir.join(format!("vm-{vm_id}"))
}

fn default_metadata_path(run_dir: &Path, vm_id: u32) -> PathBuf {
    default_vm_dir(run_dir, vm_id).join("memory.json")
}

fn default_state_path(run_dir: &Path, vm_id: u32) -> PathBuf {
    default_vm_dir(run_dir, vm_id).join("state.json")
}

fn default_log_path(run_dir: &Path, vm_id: u32) -> PathBuf {
    default_vm_dir(run_dir, vm_id).join("agent.log")
}

fn open_log_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("ouverture du log {}", path.display()))
}

fn wait_for_metadata(path: &Path, timeout: Duration) -> Result<MemoryBackendMetadata> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            let raw = fs::read(path)
                .with_context(|| format!("lecture des métadonnées {}", path.display()))?;
            let meta: MemoryBackendMetadata = serde_json::from_slice(&raw)
                .with_context(|| format!("décodage des métadonnées {}", path.display()))?;
            return Ok(meta);
        }
        thread::sleep(Duration::from_millis(200));
    }
    bail!("timeout d'attente des métadonnées");
}

fn build_qemu_args(
    object_id: &str,
    size_mib: usize,
    metadata: &MemoryBackendMetadata,
) -> Result<Vec<String>> {
    Ok(vec![
        "-object".into(),
        build_omega_object_arg(object_id, size_mib, metadata)?,
        "-machine".into(),
        format!("memory-backend={object_id}"),
    ])
}

fn build_omega_object_arg(
    object_id: &str,
    size_mib: usize,
    metadata: &MemoryBackendMetadata,
) -> Result<String> {
    let proc_fd_path = metadata
        .proc_fd_path
        .as_deref()
        .context("proc_fd_path absent des métadonnées — le backend memfd est requis")?;
    Ok(format!(
        "memory-backend-file,id={object_id},size={}M,mem-path={proc_fd_path},share=on",
        size_mib,
    ))
}

fn inject_omega_qemu_args(
    qemu_args: &[String],
    object_id: &str,
    size_mib: usize,
    metadata: &MemoryBackendMetadata,
) -> Result<Vec<String>> {
    let object_arg = build_omega_object_arg(object_id, size_mib, metadata)?;
    let mut out = Vec::with_capacity(qemu_args.len() + 6);
    let mut saw_machine = false;
    let mut saw_object = false;
    let mut saw_accel = false;
    let mut idx = 0;
    while idx < qemu_args.len() {
        let current = &qemu_args[idx];
        if current == "-object" {
            if let Some(next) = qemu_args.get(idx + 1) {
                if next.contains("memory-backend-file") && next.contains(&format!("id={object_id}"))
                {
                    saw_object = true;
                }
                out.push(current.clone());
                out.push(next.clone());
                idx += 2;
                continue;
            }
        }
        if current == "-machine" {
            if let Some(next) = qemu_args.get(idx + 1) {
                let patched = patch_machine_arg(next, object_id)?;
                saw_machine = true;
                out.push(current.clone());
                out.push(patched);
                idx += 2;
                continue;
            }
        }
        if current == "-accel" || current == "-enable-kvm" {
            saw_accel = true;
        }
        out.push(current.clone());
        idx += 1;
    }

    if !saw_object {
        out.push("-object".into());
        out.push(object_arg);
    }
    if !saw_machine {
        out.push("-machine".into());
        out.push(format!("memory-backend={object_id}"));
    }
    // Ensure KVM is explicitly enabled. QEMU auto-enables KVM only when argv[0]
    // ends in "kvm" — but wrapper scripts can overwrite argv[0], breaking this
    // convention. Explicit -accel kvm is more robust and ensures consistent
    // behaviour across all nodes regardless of binary naming.
    if !saw_accel {
        out.push("-accel".into());
        out.push("kvm".into());
    }

    Ok(out)
}

fn patch_machine_arg(arg: &str, object_id: &str) -> Result<String> {
    if arg.contains("memory-backend=") {
        if arg.contains(&format!("memory-backend={object_id}")) {
            return Ok(arg.to_string());
        }
        bail!("argument -machine incompatible: memory-backend déjà défini ({arg})");
    }
    if arg.is_empty() {
        return Ok(format!("memory-backend={object_id}"));
    }
    Ok(format!("{arg},memory-backend={object_id}"))
}

fn infer_vmid_from_qemu_args(args: &[String]) -> Option<u32> {
    let mut idx = 0;
    while idx < args.len() {
        let current = &args[idx];
        if current == "-id" {
            if let Some(next) = args.get(idx + 1) {
                if let Some(vmid) = extract_first_u32(next) {
                    return Some(vmid);
                }
            }
        }
        if current == "-name" {
            if let Some(next) = args.get(idx + 1) {
                if let Some(vmid) = extract_tagged_u32(next, "guest=")
                    .or_else(|| extract_tagged_u32(next, "vmid="))
                    .or_else(|| extract_first_u32(next))
                {
                    return Some(vmid);
                }
            }
        }
        idx += 1;
    }
    None
}

fn infer_size_mib_from_qemu_args(args: &[String]) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == "-m" {
            if let Some(next) = args.get(idx + 1) {
                return parse_qemu_memory_mib(next);
            }
        }
        idx += 1;
    }
    None
}

fn parse_qemu_memory_mib(value: &str) -> Option<usize> {
    let field = if let Some(rest) = value.strip_prefix("size=") {
        rest.split(',').next().unwrap_or(rest)
    } else {
        value.split(',').next().unwrap_or(value)
    };
    if field.is_empty() {
        return None;
    }

    let suffix = field.chars().last()?;
    let digits = match suffix {
        'K' | 'k' | 'M' | 'm' | 'G' | 'g' | 'T' | 't' => &field[..field.len() - 1],
        _ if suffix.is_ascii_digit() => field,
        _ => return None,
    };
    let base: usize = digits.parse().ok()?;
    match suffix {
        'K' | 'k' => Some(base / 1024),
        'M' | 'm' => Some(base),
        'G' | 'g' => Some(base.saturating_mul(1024)),
        'T' | 't' => Some(base.saturating_mul(1024usize * 1024usize)),
        _ => Some(base),
    }
}

fn extract_tagged_u32(value: &str, tag: &str) -> Option<u32> {
    let start = value.find(tag)? + tag.len();
    let slice = &value[start..];
    let digits: String = slice.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn extract_first_u32(value: &str) -> Option<u32> {
    let mut digits = String::new();
    let mut in_digits = false;
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            in_digits = true;
        } else if in_digits {
            break;
        }
    }
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn pid_is_running(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn state_is_usable(state: &QemuLaunchState) -> bool {
    if !pid_is_running(state.agent_pid) {
        return false;
    }
    match state.metadata.proc_fd_path.as_deref() {
        Some(path) => Path::new(path).exists(),
        None => false,
    }
}

fn stop_agent(state: &QemuLaunchState, timeout: Duration) -> Result<()> {
    if !pid_is_running(state.agent_pid) {
        return Ok(());
    }

    let rc = unsafe { libc::kill(state.agent_pid as i32, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        bail!("échec SIGTERM agent {}: {err}", state.agent_pid);
    }

    let start = Instant::now();
    while start.elapsed() < timeout {
        if !pid_is_running(state.agent_pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }

    bail!(
        "l'agent {} ne s'est pas arrêté dans le délai",
        state.agent_pid
    );
}

fn shell_escape_str(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn shell_escape_path(path: &Path) -> String {
    shell_escape_str(&path.display().to_string())
}

#[derive(Debug, Serialize)]
struct StatusOutput {
    vm_id: u32,
    agent_pid: u32,
    agent_running: bool,
    metadata_path: PathBuf,
    state_path: PathBuf,
    log_path: PathBuf,
    qemu_args: Vec<String>,
}

impl StatusOutput {
    fn from_state(state: &QemuLaunchState) -> Self {
        Self {
            vm_id: state.vm_id,
            agent_pid: state.agent_pid,
            agent_running: pid_is_running(state.agent_pid),
            metadata_path: state.metadata_path.clone(),
            state_path: state.state_path.clone(),
            log_path: state.log_path.clone(),
            qemu_args: state.qemu_args.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct CleanupReport {
    vm_id: u32,
    stopped_agent: bool,
    removed: Vec<PathBuf>,
}

fn check_file(name: &'static str, path: &Path, executable: bool) -> DoctorCheck {
    match fs::metadata(path) {
        Ok(meta) if !meta.is_file() => DoctorCheck {
            name,
            ok: false,
            detail: format!("{} existe mais n'est pas un fichier", path.display()),
        },
        Ok(meta) if executable && (meta.permissions().mode() & 0o111 == 0) => DoctorCheck {
            name,
            ok: false,
            detail: format!("{} n'est pas exécutable", path.display()),
        },
        Ok(_) => DoctorCheck {
            name,
            ok: true,
            detail: path.display().to_string(),
        },
        Err(err) => DoctorCheck {
            name,
            ok: false,
            detail: format!("{}: {err}", path.display()),
        },
    }
}

fn check_path(name: &'static str, path: Option<&Path>, writable: bool) -> DoctorCheck {
    let Some(path) = path else {
        return DoctorCheck {
            name,
            ok: false,
            detail: "chemin parent absent".into(),
        };
    };
    match fs::metadata(path) {
        Ok(meta) if !meta.is_dir() => DoctorCheck {
            name,
            ok: false,
            detail: format!("{} existe mais n'est pas un répertoire", path.display()),
        },
        Ok(meta) if writable && meta.permissions().readonly() => DoctorCheck {
            name,
            ok: false,
            detail: format!("{} n'est pas modifiable", path.display()),
        },
        Ok(_) => DoctorCheck {
            name,
            ok: true,
            detail: path.display().to_string(),
        },
        Err(err) => DoctorCheck {
            name,
            ok: false,
            detail: format!("{}: {err}", path.display()),
        },
    }
}

fn check_agent_resolvable() -> DoctorCheck {
    match resolve_agent_bin(None) {
        Ok(path) => check_file("agent_bin", &path, true),
        Err(err) => DoctorCheck {
            name: "agent_bin",
            ok: false,
            detail: err.to_string(),
        },
    }
}

fn check_userfaultfd() -> DoctorCheck {
    let unprivileged = fs::read_to_string("/proc/sys/vm/unprivileged_userfaultfd")
        .ok()
        .map(|value| value.trim().to_string());
    let fd = unsafe { libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd >= 0 {
        unsafe {
            libc::close(fd as i32);
        }
        return DoctorCheck {
            name: "userfaultfd",
            ok: true,
            detail: format!(
                "SYS_userfaultfd disponible{}",
                unprivileged
                    .map(|v| format!("; unprivileged_userfaultfd={v}"))
                    .unwrap_or_default()
            ),
        };
    }
    let err = std::io::Error::last_os_error();
    DoctorCheck {
        name: "userfaultfd",
        ok: false,
        detail: format!(
            "SYS_userfaultfd indisponible: {err}{}",
            unprivileged
                .map(|v| format!("; /proc/sys/vm/unprivileged_userfaultfd={v}"))
                .unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_args_are_built_consistently() {
        let meta = MemoryBackendMetadata {
            backend: node_a_agent::shared_memory::MemoryBackendKind::Memfd,
            size_bytes: 2048 * 1024 * 1024,
            pid: 12345,
            proc_fd_path: Some("/proc/12345/fd/5".to_string()),
        };
        let args = build_qemu_args("ram0", 2048, &meta).unwrap();
        assert_eq!(args[0], "-object");
        assert!(args[1].contains("memory-backend-file,id=ram0"));
        assert!(args[1].contains("size=2048M"));
        assert!(args[1].contains("mem-path=/proc/12345/fd/5"));
        assert_eq!(args[2], "-machine");
        assert_eq!(args[3], "memory-backend=ram0");
    }

    #[test]
    fn shell_escape_wraps_single_quotes() {
        let escaped = shell_escape_path(Path::new("/tmp/omega'qemu"));
        assert_eq!(escaped, "/tmp/omega'\"'\"'qemu");
    }

    #[test]
    fn default_paths_are_vm_scoped() {
        let run_dir = Path::new("/tmp/omega-qemu");
        assert_eq!(
            default_metadata_path(run_dir, 9004),
            PathBuf::from("/tmp/omega-qemu/vm-9004/memory.json")
        );
        assert_eq!(
            default_state_path(run_dir, 9004),
            PathBuf::from("/tmp/omega-qemu/vm-9004/state.json")
        );
        assert_eq!(
            default_log_path(run_dir, 9004),
            PathBuf::from("/tmp/omega-qemu/vm-9004/agent.log")
        );
    }

    #[test]
    fn parse_vmid_from_proxmox_args() {
        let args = vec![
            "-name".to_string(),
            "guest=9004,debug-threads=on".to_string(),
            "-id".to_string(),
            "9004".to_string(),
        ];
        assert_eq!(infer_vmid_from_qemu_args(&args), Some(9004));
    }

    #[test]
    fn parse_memory_mib_from_qemu_args() {
        let args = vec!["-m".to_string(), "size=2G,slots=255".to_string()];
        assert_eq!(infer_size_mib_from_qemu_args(&args), Some(2048));
    }

    #[test]
    fn injects_memory_backend_into_machine_arg() {
        let meta = MemoryBackendMetadata {
            backend: node_a_agent::shared_memory::MemoryBackendKind::Memfd,
            size_bytes: 512 * 1024 * 1024,
            pid: 12345,
            proc_fd_path: Some("/proc/12345/fd/5".to_string()),
        };
        let args = vec![
            "-machine".to_string(),
            "type=pc-i440fx-9.0".to_string(),
            "-m".to_string(),
            "512".to_string(),
        ];
        let patched = inject_omega_qemu_args(&args, "ram0", 512, &meta).unwrap();
        assert!(patched
            .iter()
            .any(|arg| arg.contains("memory-backend-file,id=ram0")));
        assert!(patched
            .windows(2)
            .any(|w| w[0] == "-machine" && w[1].contains("memory-backend=ram0")));
    }

    #[test]
    fn proxmox_wrapper_mentions_exec_proxmox() {
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("qemu-system-x86_64-omega");
        let path = write_proxmox_wrapper(WriteProxmoxWrapperArgs {
            stores: vec!["127.0.0.1:9100".into()],
            qemu_bin: PathBuf::from("/usr/local/bin/qemu-system-x86_64"),
            agent_bin: Some(PathBuf::from("/usr/local/bin/node-a-agent")),
            run_dir: PathBuf::from("/var/lib/omega-qemu"),
            object_id: "ram0".into(),
            start_timeout_secs: 30,
            log_format: "text".into(),
            log_level: "info".into(),
            store_timeout_ms: 2000,
            memfd_name: None,
            bridge_lib: Some(PathBuf::from("/usr/local/lib/omega-uffd-bridge.so")),
            output: output.clone(),
        })
        .unwrap();
        let shell = fs::read_to_string(path).unwrap();
        assert!(shell.contains("exec-proxmox"));
        assert!(shell.contains("OMEGA_REAL_QEMU_BIN"));
        assert!(shell.contains("OMEGA_BRIDGE_LIB"));
        assert!(!shell.contains("export LD_PRELOAD"));
    }

    #[test]
    fn cleanup_removes_runtime_files_without_log_when_requested() {
        let tempdir = tempfile::tempdir().unwrap();
        let run_dir = tempdir.path();
        let vm_id = 42;
        fs::create_dir_all(default_vm_dir(run_dir, vm_id)).unwrap();
        fs::write(default_metadata_path(run_dir, vm_id), "{}").unwrap();
        fs::write(default_state_path(run_dir, vm_id), "{}").unwrap();
        fs::write(default_log_path(run_dir, vm_id), "log").unwrap();

        let report = cleanup(CleanupArgs {
            state: StateSelector {
                vm_id,
                run_dir: run_dir.to_path_buf(),
                state_path: None,
            },
            timeout_secs: 1,
            keep_log: true,
        })
        .unwrap();

        assert_eq!(report.vm_id, vm_id);
        assert!(!default_metadata_path(run_dir, vm_id).exists());
        assert!(!default_state_path(run_dir, vm_id).exists());
        assert!(default_log_path(run_dir, vm_id).exists());
    }
}
