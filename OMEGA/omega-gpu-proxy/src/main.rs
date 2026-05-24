use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use clap::Parser;
use omega_gpu_proxy::{
    GpuProxy, JobRequest, JobSnapshot, ProxyConfig, SetBudgetRequest, StatusSnapshot,
};
use serde_json::{json, Value};
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "omega-gpu-proxy",
    about = "Proxy GPU applicatif Omega pour VMs Proxmox",
    version = env!("CARGO_PKG_VERSION")
)]
struct Args {
    /// Adresse d'écoute HTTP.
    #[arg(long, env = "OMEGA_GPU_PROXY_LISTEN", default_value = "0.0.0.0:9400")]
    listen: SocketAddr,

    /// Identifiant du nœud GPU.
    #[arg(long, env = "OMEGA_NODE_ID", default_value = "omega-gpu-node")]
    node_id: String,

    /// Nombre de jobs GPU exécutés en parallèle.
    #[arg(long, env = "OMEGA_GPU_PROXY_MAX_CONCURRENT", default_value_t = 1)]
    max_concurrent_jobs: usize,

    /// VRAM totale logique exposée par le proxy (Mio). 0 = auto via nvidia-smi si possible.
    #[arg(long, env = "OMEGA_GPU_PROXY_TOTAL_VRAM_MIB", default_value_t = 0)]
    total_vram_mib: u64,

    /// Taille maximale d'un job matrix_multiply.
    #[arg(long, env = "OMEGA_GPU_PROXY_MAX_MATRIX_N", default_value_t = 512)]
    max_matrix_n: usize,

    /// Programme worker externe à lancer pour exécuter les jobs GPU.
    /// Contrat: JSON job sur stdin, JSON résultat sur stdout.
    #[arg(long, env = "OMEGA_GPU_PROXY_BACKEND_COMMAND")]
    backend_command: Option<String>,

    /// Timeout d'un job exécuté par le worker externe.
    #[arg(
        long,
        env = "OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS",
        default_value_t = 300
    )]
    backend_timeout_secs: u64,

    /// Token API attendu dans Authorization: Bearer ... ou X-Omega-GPU-Token.
    #[arg(long, env = "OMEGA_GPU_PROXY_API_TOKEN")]
    api_token: Option<String>,

    /// Fichier contenant le token API. Prioritaire si --api-token est absent.
    #[arg(long, env = "OMEGA_GPU_PROXY_API_TOKEN_FILE")]
    api_token_file: Option<String>,

    /// Logging.
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log_level: String,
}

#[derive(Clone)]
struct AuthConfig {
    token: Option<Arc<String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    setup_logging(&args.log_level);
    let auth = AuthConfig {
        token: load_api_token(args.api_token.as_deref(), args.api_token_file.as_deref())?
            .map(Arc::new),
    };

    let proxy = Arc::new(GpuProxy::new(ProxyConfig {
        node_id: args.node_id,
        max_concurrent_jobs: args.max_concurrent_jobs.max(1),
        total_vram_mib: args.total_vram_mib,
        max_matrix_n: args.max_matrix_n,
        backend_command: args.backend_command,
        backend_timeout_secs: args.backend_timeout_secs,
    }));

    let mut app = Router::new()
        .route("/health", get(health))
        .route("/gpu/status", get(status))
        .route("/metrics", get(metrics))
        .route("/v1/vm/:vm_id/budget", post(set_budget))
        .route("/v1/vm/:vm_id/budget", delete(delete_budget))
        .route("/v1/jobs", post(submit_job))
        .route("/v1/jobs/:job_id", get(get_job))
        .route("/v1/jobs/:job_id", delete(cancel_job))
        .with_state(proxy);

    if auth.token.is_some() {
        app = app.layer(middleware::from_fn_with_state(auth, require_token));
        info!("sécurité proxy GPU activée: token API requis hors /health");
    } else {
        info!("sécurité proxy GPU désactivée: aucun token configuré");
    }

    info!(addr = %args.listen, "omega-gpu-proxy démarré");
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    if let Err(e) = axum::serve(listener, app).await {
        error!(error = %e, "omega-gpu-proxy terminé avec erreur");
    }
    Ok(())
}

fn setup_logging(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn load_api_token(cli_token: Option<&str>, token_file: Option<&str>) -> Result<Option<String>> {
    if let Some(token) = cli_token {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    let Some(path) = token_file else {
        return Ok(None);
    };
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("lecture token proxy GPU: {path}"))?
        .trim()
        .to_string();
    if token.is_empty() {
        Ok(None)
    } else {
        Ok(Some(token))
    }
}

async fn require_token(
    State(auth): State<AuthConfig>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, (StatusCode, Json<Value>)> {
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    let Some(expected) = auth.token.as_deref() else {
        return Ok(next.run(req).await);
    };

    let bearer_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| token.trim() == expected.as_str())
        .unwrap_or(false);
    let header_ok = req
        .headers()
        .get("x-omega-gpu-token")
        .and_then(|value| value.to_str().ok())
        .map(|token| token.trim() == expected.as_str())
        .unwrap_or(false);

    if bearer_ok || header_ok {
        Ok(next.run(req).await)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized", "message": "token GPU proxy invalide ou absent"})),
        ))
    }
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "omega-gpu-proxy"}))
}

async fn status(State(proxy): State<Arc<GpuProxy>>) -> Json<StatusSnapshot> {
    Json(proxy.status())
}

async fn metrics(State(proxy): State<Arc<GpuProxy>>) -> String {
    proxy.prometheus_metrics()
}

async fn set_budget(
    State(proxy): State<Arc<GpuProxy>>,
    Path(vm_id): Path<u32>,
    Json(req): Json<SetBudgetRequest>,
) -> Result<Json<StatusSnapshot>, (StatusCode, Json<Value>)> {
    proxy
        .set_budget(vm_id, req.vram_budget_mib)
        .map_err(api_error)?;
    Ok(Json(proxy.status()))
}

async fn delete_budget(
    State(proxy): State<Arc<GpuProxy>>,
    Path(vm_id): Path<u32>,
) -> Json<StatusSnapshot> {
    proxy.delete_budget(vm_id);
    Json(proxy.status())
}

async fn submit_job(
    State(proxy): State<Arc<GpuProxy>>,
    Json(req): Json<JobRequest>,
) -> Result<Json<JobSnapshot>, (StatusCode, Json<Value>)> {
    let snapshot = proxy.submit(req).await.map_err(api_error)?;
    Ok(Json(snapshot))
}

async fn get_job(
    State(proxy): State<Arc<GpuProxy>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobSnapshot>, (StatusCode, Json<Value>)> {
    proxy.job(&job_id).map(Json).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "job_not_found"})),
        )
    })
}

async fn cancel_job(
    State(proxy): State<Arc<GpuProxy>>,
    Path(job_id): Path<String>,
) -> Result<Json<JobSnapshot>, (StatusCode, Json<Value>)> {
    proxy.cancel(&job_id).map(Json).map_err(api_error)
}

fn api_error(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "gpu_proxy_error",
            "message": err.to_string(),
        })),
    )
}
