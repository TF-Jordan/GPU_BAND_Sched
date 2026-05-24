use std::io::{self, Read};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use omega_daemon::policy_engine::{
    admit_batch, admit_vm, evaluate_gpu_rebalance, evaluate_migrations, pick_migration_type,
    AdmissionBatchRequest, AdmissionRequest, GpuRebalanceRequest, MigrationEvaluateRequest,
    PickMigrationTypeRequest,
};

#[derive(Parser)]
#[command(name = "omega-policy")]
#[command(about = "Moteur de politique cluster en Rust pour omega-remote-paging")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Admit,
    AdmitBatch,
    EvaluateMigrations,
    EvaluateGpuRebalance,
    PickMigrationType,
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("lecture stdin impossible")?;
    Ok(buf)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let input = read_stdin()?;

    match cli.command {
        Command::Admit => {
            let req: AdmissionRequest =
                serde_json::from_str(&input).context("JSON admit invalide")?;
            let out = admit_vm(&req.config, &req.cluster, &req.vm);
            println!("{}", serde_json::to_string(&out)?);
        }
        Command::AdmitBatch => {
            let req: AdmissionBatchRequest =
                serde_json::from_str(&input).context("JSON admit-batch invalide")?;
            let out = admit_batch(&req.config, &req.cluster, &req.vms);
            println!("{}", serde_json::to_string(&out)?);
        }
        Command::EvaluateMigrations => {
            let req: MigrationEvaluateRequest =
                serde_json::from_str(&input).context("JSON evaluate-migrations invalide")?;
            let out = evaluate_migrations(&req.thresholds, &req.nodes);
            println!("{}", serde_json::to_string(&out)?);
        }
        Command::EvaluateGpuRebalance => {
            let req: GpuRebalanceRequest =
                serde_json::from_str(&input).context("JSON evaluate-gpu-rebalance invalide")?;
            let out = evaluate_gpu_rebalance(&req);
            println!("{}", serde_json::to_string(&out)?);
        }
        Command::PickMigrationType => {
            let req: PickMigrationTypeRequest =
                serde_json::from_str(&input).context("JSON pick-migration-type invalide")?;
            let out = pick_migration_type(&req.thresholds, &req.vm, req.node_ram_pct);
            println!("{}", serde_json::to_string(&out)?);
        }
    }

    Ok(())
}
