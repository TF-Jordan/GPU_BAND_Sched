use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::sync::Semaphore;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub node_id: String,
    pub max_concurrent_jobs: usize,
    pub total_vram_mib: u64,
    pub max_matrix_n: usize,
    pub backend_command: Option<String>,
    pub backend_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetBudgetRequest {
    pub vram_budget_mib: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobRequest {
    pub vm_id: u32,
    #[serde(default = "default_kind")]
    pub kind: JobKind,
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub vram_mib: u64,
    #[serde(default)]
    pub payload: Value,
}

fn default_kind() -> JobKind {
    JobKind::MatrixMultiply
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    MatrixMultiply,
    Inference,
    VideoEncode,
    Render,
    Custom,
    Echo,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobSnapshot {
    pub job_id: String,
    pub vm_id: u32,
    pub kind: JobKind,
    pub state: JobState,
    pub priority: u8,
    pub vram_mib: u64,
    pub submitted_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u128>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct JobRecord {
    job_id: String,
    vm_id: u32,
    kind: JobKind,
    state: JobState,
    priority: u8,
    vram_mib: u64,
    payload: Value,
    submitted_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    duration_ms: Option<u128>,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerJobRequest {
    job_id: String,
    vm_id: u32,
    kind: JobKind,
    priority: u8,
    vram_mib: u64,
    payload: Value,
}

impl JobRecord {
    fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            job_id: self.job_id.clone(),
            vm_id: self.vm_id,
            kind: self.kind.clone(),
            state: self.state.clone(),
            priority: self.priority,
            vram_mib: self.vram_mib,
            submitted_at: self.submitted_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            duration_ms: self.duration_ms,
            result: self.result.clone(),
            error: self.error.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VmBudgetSnapshot {
    pub vm_id: u32,
    pub vram_budget_mib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub node_id: String,
    pub backend: BackendSnapshot,
    pub max_concurrent_jobs: usize,
    pub running_jobs: usize,
    pub queued_jobs: usize,
    pub completed_jobs: u64,
    pub failed_jobs: u64,
    pub total_vram_mib: u64,
    pub reserved_vram_mib: u64,
    pub free_vram_mib: u64,
    pub budgets: Vec<VmBudgetSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackendSnapshot {
    pub kind: String,
    pub gpu_available: bool,
    pub name: Option<String>,
    pub driver: Option<String>,
    pub total_vram_mib: Option<u64>,
    pub note: String,
}

pub struct GpuProxy {
    cfg: ProxyConfig,
    semaphore: Arc<Semaphore>,
    budgets: DashMap<u32, u64>,
    jobs: DashMap<String, JobRecord>,
    metrics: ProxyMetrics,
    backend: BackendSnapshot,
}

#[derive(Default)]
struct ProxyMetrics {
    completed_jobs: std::sync::atomic::AtomicU64,
    failed_jobs: std::sync::atomic::AtomicU64,
}

impl GpuProxy {
    pub fn new(mut cfg: ProxyConfig) -> Self {
        let backend = detect_backend(cfg.backend_command.as_deref());
        if cfg.total_vram_mib == 0 {
            cfg.total_vram_mib = backend.total_vram_mib.unwrap_or(0);
        }

        Self {
            semaphore: Arc::new(Semaphore::new(cfg.max_concurrent_jobs.max(1))),
            cfg,
            budgets: DashMap::new(),
            jobs: DashMap::new(),
            metrics: ProxyMetrics::default(),
            backend,
        }
    }

    pub fn set_budget(&self, vm_id: u32, vram_budget_mib: u64) -> Result<()> {
        let current = self.budgets.get(&vm_id).map(|v| *v).unwrap_or(0);
        let reserved_without_current = self.reserved_vram_mib().saturating_sub(current);
        if self.cfg.total_vram_mib > 0
            && reserved_without_current.saturating_add(vram_budget_mib) > self.cfg.total_vram_mib
        {
            bail!(
                "budget VRAM refusé: demandé={} MiB, libre={} MiB",
                vram_budget_mib,
                self.cfg
                    .total_vram_mib
                    .saturating_sub(reserved_without_current)
            );
        }
        if vram_budget_mib == 0 {
            self.budgets.remove(&vm_id);
        } else {
            self.budgets.insert(vm_id, vram_budget_mib);
        }
        Ok(())
    }

    pub fn delete_budget(&self, vm_id: u32) {
        self.budgets.remove(&vm_id);
    }

    pub async fn submit(self: &Arc<Self>, req: JobRequest) -> Result<JobSnapshot> {
        self.validate_request(&req)?;
        let job_id = Uuid::new_v4().to_string();
        let record = JobRecord {
            job_id: job_id.clone(),
            vm_id: req.vm_id,
            kind: req.kind,
            state: JobState::Queued,
            priority: req.priority,
            vram_mib: req.vram_mib,
            payload: req.payload,
            submitted_at: Utc::now(),
            started_at: None,
            finished_at: None,
            duration_ms: None,
            result: None,
            error: None,
        };
        let snapshot = record.snapshot();
        self.jobs.insert(job_id.clone(), record);

        let proxy = Arc::clone(self);
        tokio::spawn(async move {
            proxy.run_job(job_id).await;
        });

        Ok(snapshot)
    }

    pub fn job(&self, job_id: &str) -> Option<JobSnapshot> {
        self.jobs.get(job_id).map(|entry| entry.snapshot())
    }

    pub fn cancel(&self, job_id: &str) -> Result<JobSnapshot> {
        let mut job = self
            .jobs
            .get_mut(job_id)
            .with_context(|| format!("job introuvable: {job_id}"))?;
        match job.state {
            JobState::Queued => {
                job.state = JobState::Cancelled;
                job.finished_at = Some(Utc::now());
                Ok(job.snapshot())
            }
            JobState::Running => bail!("job déjà en cours: annulation non supportée en v1"),
            _ => Ok(job.snapshot()),
        }
    }

    pub fn status(&self) -> StatusSnapshot {
        let mut budgets: Vec<_> = self
            .budgets
            .iter()
            .map(|entry| VmBudgetSnapshot {
                vm_id: *entry.key(),
                vram_budget_mib: *entry.value(),
            })
            .collect();
        budgets.sort_by_key(|b| b.vm_id);

        let mut running_jobs = 0;
        let mut queued_jobs = 0;
        for job in self.jobs.iter() {
            match job.state {
                JobState::Running => running_jobs += 1,
                JobState::Queued => queued_jobs += 1,
                _ => {}
            }
        }

        let reserved = self.reserved_vram_mib();
        StatusSnapshot {
            node_id: self.cfg.node_id.clone(),
            backend: self.backend.clone(),
            max_concurrent_jobs: self.cfg.max_concurrent_jobs,
            running_jobs,
            queued_jobs,
            completed_jobs: self
                .metrics
                .completed_jobs
                .load(std::sync::atomic::Ordering::Relaxed),
            failed_jobs: self
                .metrics
                .failed_jobs
                .load(std::sync::atomic::Ordering::Relaxed),
            total_vram_mib: self.cfg.total_vram_mib,
            reserved_vram_mib: reserved,
            free_vram_mib: self.cfg.total_vram_mib.saturating_sub(reserved),
            budgets,
        }
    }

    pub fn prometheus_metrics(&self) -> String {
        let snap = self.status();
        format!(
            "# HELP omega_gpu_proxy_jobs_running Jobs GPU applicatifs en cours\n\
             omega_gpu_proxy_jobs_running{{node=\"{node}\"}} {running}\n\
             # HELP omega_gpu_proxy_jobs_queued Jobs GPU applicatifs en attente\n\
             omega_gpu_proxy_jobs_queued{{node=\"{node}\"}} {queued}\n\
             # HELP omega_gpu_proxy_jobs_completed_total Jobs GPU applicatifs terminés\n\
             omega_gpu_proxy_jobs_completed_total{{node=\"{node}\"}} {completed}\n\
             # HELP omega_gpu_proxy_jobs_failed_total Jobs GPU applicatifs échoués\n\
             omega_gpu_proxy_jobs_failed_total{{node=\"{node}\"}} {failed}\n\
             # HELP omega_gpu_proxy_vram_total_mib VRAM logique totale du proxy\n\
             omega_gpu_proxy_vram_total_mib{{node=\"{node}\"}} {total}\n\
             # HELP omega_gpu_proxy_vram_reserved_mib VRAM logique réservée\n\
             omega_gpu_proxy_vram_reserved_mib{{node=\"{node}\"}} {reserved}\n",
            node = snap.node_id,
            running = snap.running_jobs,
            queued = snap.queued_jobs,
            completed = snap.completed_jobs,
            failed = snap.failed_jobs,
            total = snap.total_vram_mib,
            reserved = snap.reserved_vram_mib,
        )
    }

    fn validate_request(&self, req: &JobRequest) -> Result<()> {
        let budget = self.budgets.get(&req.vm_id).map(|v| *v).unwrap_or(0);
        if req.vram_mib > budget {
            bail!(
                "job refusé: VM {} demande {} MiB mais son budget est {} MiB",
                req.vm_id,
                req.vram_mib,
                budget
            );
        }
        if req.kind == JobKind::MatrixMultiply {
            let n = req.payload.get("n").and_then(Value::as_u64).unwrap_or(64) as usize;
            if n == 0 || n > self.cfg.max_matrix_n {
                bail!(
                    "matrix_multiply invalide: n={} doit être entre 1 et {}",
                    n,
                    self.cfg.max_matrix_n
                );
            }
        }
        Ok(())
    }

    async fn run_job(self: Arc<Self>, job_id: String) {
        let permit = match self.semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(e) => {
                self.mark_failed(&job_id, format!("sémaphore fermé: {e}"));
                return;
            }
        };

        {
            let Some(mut job) = self.jobs.get_mut(&job_id) else {
                return;
            };
            if job.state == JobState::Cancelled {
                return;
            }
            job.state = JobState::Running;
            job.started_at = Some(Utc::now());
        }

        let started = Instant::now();
        let job_to_execute = {
            let Some(job) = self.jobs.get(&job_id) else {
                return;
            };
            job.clone()
        };
        let outcome = self.execute_job(&job_to_execute).await;
        drop(permit);

        match outcome {
            Ok(result) => {
                if let Some(mut job) = self.jobs.get_mut(&job_id) {
                    job.state = JobState::Succeeded;
                    job.finished_at = Some(Utc::now());
                    job.duration_ms = Some(started.elapsed().as_millis());
                    job.result = Some(result);
                }
                self.metrics
                    .completed_jobs
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                self.mark_failed(&job_id, e.to_string());
            }
        }
    }

    fn mark_failed(&self, job_id: &str, error: String) {
        if let Some(mut job) = self.jobs.get_mut(job_id) {
            job.state = JobState::Failed;
            job.finished_at = Some(Utc::now());
            job.error = Some(error);
        }
        self.metrics
            .failed_jobs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn reserved_vram_mib(&self) -> u64 {
        self.budgets.iter().map(|entry| *entry.value()).sum()
    }

    async fn execute_job(&self, job: &JobRecord) -> Result<Value> {
        if let Some(command) = self.cfg.backend_command.as_deref() {
            let request = WorkerJobRequest {
                job_id: job.job_id.clone(),
                vm_id: job.vm_id,
                kind: job.kind.clone(),
                priority: job.priority,
                vram_mib: job.vram_mib,
                payload: job.payload.clone(),
            };
            return execute_external_worker(command, self.cfg.backend_timeout_secs, &request).await;
        }

        execute_reference_job(&job.kind, &job.payload)
    }
}

fn execute_reference_job(kind: &JobKind, payload: &Value) -> Result<Value> {
    match kind {
        JobKind::MatrixMultiply => execute_matrix_multiply(payload),
        JobKind::Echo => Ok(payload.clone()),
        JobKind::Inference | JobKind::VideoEncode | JobKind::Render | JobKind::Custom => {
            bail!(
                "job {:?} nécessite un backend externe via --backend-command",
                kind
            )
        }
    }
}

async fn execute_external_worker(
    command: &str,
    timeout_secs: u64,
    request: &WorkerJobRequest,
) -> Result<Value> {
    let mut child = TokioCommand::new(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("démarrage backend externe impossible: {command}"))?;

    let stdin = child
        .stdin
        .as_mut()
        .context("stdin backend externe indisponible")?;
    let body = serde_json::to_vec(request)?;
    stdin.write_all(&body).await?;
    stdin.write_all(b"\n").await?;
    drop(child.stdin.take());

    let wait = child.wait_with_output();
    let output = timeout(Duration::from_secs(timeout_secs.max(1)), wait)
        .await
        .with_context(|| {
            format!(
                "backend externe expiré après {}s pour job {}",
                timeout_secs.max(1),
                request.job_id
            )
        })?
        .context("attente backend externe échouée")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "backend externe a échoué: status={} stderr={}",
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("stdout backend non UTF-8")?;
    let value: Value = serde_json::from_str(stdout.trim()).with_context(|| {
        format!(
            "stdout backend externe invalide JSON pour job {}: {}",
            request.job_id,
            stdout.trim()
        )
    })?;

    Ok(json!({
        "backend_used": "external_worker",
        "worker": command,
        "output": value,
    }))
}

fn execute_matrix_multiply(payload: &Value) -> Result<Value> {
    let n = payload.get("n").and_then(Value::as_u64).unwrap_or(64) as usize;
    let seed = payload.get("seed").and_then(Value::as_u64).unwrap_or(1);
    let a = deterministic_matrix(n, seed);
    let b = deterministic_matrix(n, seed.wrapping_mul(6364136223846793005).wrapping_add(1));
    let mut c = vec![0.0f64; n * n];

    for i in 0..n {
        for k in 0..n {
            let aik = a[i * n + k];
            for j in 0..n {
                c[i * n + j] += aik * b[k * n + j];
            }
        }
    }

    let checksum = c.iter().fold(0.0f64, |acc, v| acc + v);
    Ok(json!({
        "backend_used": "cpu_reference",
        "operation": "matrix_multiply",
        "n": n,
        "checksum": checksum,
    }))
}

fn deterministic_matrix(n: usize, seed: u64) -> Vec<f64> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(n * n);
    for _ in 0..(n * n) {
        state = state
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        out.push(((state >> 32) as f64) / (u32::MAX as f64));
    }
    out
}

fn detect_backend(external_worker: Option<&str>) -> BackendSnapshot {
    if let Some(worker) = external_worker {
        return BackendSnapshot {
            kind: "external_worker".to_string(),
            gpu_available: true,
            name: Some(worker.to_string()),
            driver: None,
            total_vram_mib: None,
            note: "Backend applicatif externe activé. Le worker reçoit le job en JSON sur stdin et retourne un JSON sur stdout.".to_string(),
        };
    }

    if let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = text.lines().find(|l| !l.trim().is_empty()) {
                let fields: Vec<_> = line.split(',').map(|s| s.trim().to_string()).collect();
                let total = fields.get(2).and_then(|v| v.parse::<u64>().ok());
                return BackendSnapshot {
                    kind: "nvidia_smi_detected".to_string(),
                    gpu_available: true,
                    name: fields.get(0).cloned(),
                    driver: fields.get(1).cloned(),
                    total_vram_mib: total,
                    note: "GPU NVIDIA détecté. La v1 exécute le job de référence côté proxy; brancher un worker CUDA/PyTorch pour exécution GPU native.".to_string(),
                };
            }
        }
    }

    BackendSnapshot {
        kind: "cpu_reference".to_string(),
        gpu_available: false,
        name: None,
        driver: None,
        total_vram_mib: None,
        note: "Aucun backend GPU natif détecté; le proxy reste utilisable pour valider API, budgets et file d'attente.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_job_above_vm_budget() {
        let proxy = Arc::new(GpuProxy::new(ProxyConfig {
            node_id: "test".to_string(),
            max_concurrent_jobs: 1,
            total_vram_mib: 1024,
            max_matrix_n: 64,
            backend_command: None,
            backend_timeout_secs: 30,
        }));
        proxy.set_budget(100, 128).unwrap();
        let err = proxy
            .submit(JobRequest {
                vm_id: 100,
                kind: JobKind::MatrixMultiply,
                priority: 0,
                vram_mib: 256,
                payload: json!({"n": 8}),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("budget"));
    }

    #[tokio::test]
    async fn matrix_job_completes() {
        let proxy = Arc::new(GpuProxy::new(ProxyConfig {
            node_id: "test".to_string(),
            max_concurrent_jobs: 1,
            total_vram_mib: 1024,
            max_matrix_n: 64,
            backend_command: None,
            backend_timeout_secs: 30,
        }));
        proxy.set_budget(100, 128).unwrap();
        let submitted = proxy
            .submit(JobRequest {
                vm_id: 100,
                kind: JobKind::MatrixMultiply,
                priority: 0,
                vram_mib: 64,
                payload: json!({"n": 8, "seed": 7}),
            })
            .await
            .unwrap();

        for _ in 0..100 {
            let job = proxy.job(&submitted.job_id).unwrap();
            if job.state == JobState::Succeeded {
                assert!(job.result.unwrap().get("checksum").is_some());
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job did not complete");
    }

    #[test]
    fn set_budget_rejects_overcommit() {
        let proxy = GpuProxy::new(ProxyConfig {
            node_id: "test".to_string(),
            max_concurrent_jobs: 1,
            total_vram_mib: 128,
            max_matrix_n: 64,
            backend_command: None,
            backend_timeout_secs: 30,
        });
        proxy.set_budget(1, 100).unwrap();
        assert!(proxy.set_budget(2, 100).is_err());
    }

    #[test]
    fn deterministic_matrix_result_is_stable() {
        let a = execute_matrix_multiply(&json!({"n": 4, "seed": 1})).unwrap();
        let b = execute_matrix_multiply(&json!({"n": 4, "seed": 1})).unwrap();
        assert_eq!(a["checksum"], b["checksum"]);
    }

    #[test]
    fn status_reports_budget_totals() {
        let proxy = GpuProxy::new(ProxyConfig {
            node_id: "test".to_string(),
            max_concurrent_jobs: 2,
            total_vram_mib: 1024,
            max_matrix_n: 64,
            backend_command: None,
            backend_timeout_secs: 30,
        });
        proxy.set_budget(1, 128).unwrap();
        proxy.set_budget(2, 256).unwrap();
        let status = proxy.status();
        assert_eq!(status.reserved_vram_mib, 384);
        assert_eq!(status.free_vram_mib, 640);
    }

    #[tokio::test]
    async fn external_worker_returns_json_output() {
        let request = WorkerJobRequest {
            job_id: "job-1".to_string(),
            vm_id: 100,
            kind: JobKind::Echo,
            priority: 0,
            vram_mib: 1,
            payload: json!({"hello": "gpu"}),
        };
        let result = execute_external_worker("/bin/cat", 5, &request)
            .await
            .unwrap();
        assert_eq!(result["backend_used"], "external_worker");
        assert_eq!(result["output"]["vm_id"], 100);
        assert_eq!(result["output"]["payload"]["hello"], "gpu");
    }
}
