"""
Point d'entrée CLI du controller omega-remote-paging.

Usage :
    python -m controller.main monitor --interval 10
    python -m controller.main status
    python -m controller.main policy --dry-run
"""

from __future__ import annotations

import json
import os
import time
import sys
import logging
from typing import Dict, List, Optional, Tuple

import click
import requests
import structlog

from .admission import AdmissionController, VmSpec
from .cluster import ClusterState, NodeInfo, VmEntry
from .cpu_admission import (
    ClusterVcpuPoolPlanner,
    VcpuPoolConfig,
    VcpuPoolDecision,
    VcpuPoolVm,
)
from .metrics import MetricsCollector
from .gpu_global import GpuPlacementAction, choose_gpu_placement
from .migration_policy import (
    MigrationCandidate,
    MigrationPolicy,
    MigrationThresholds,
    MigrationReason,
    MigrationType,
    NodeState as MigNodeState,
    VmState,
)
from .policy import PolicyEngine, PolicyInput, Decision
from .proxmox import ProxmoxClient
from .rust_policy import call_policy
from .store_client import poll_all_stores

# ─── Configuration du logging structuré ───────────────────────────────────────

def setup_logging(level: str = "INFO", fmt: str = "console") -> None:
    logging.basicConfig(
        level  = getattr(logging, level.upper(), logging.INFO),
        stream = sys.stderr,
    )

    if fmt == "json":
        structlog.configure(
            processors=[
                structlog.processors.TimeStamper(fmt="iso"),
                structlog.processors.add_log_level,
                structlog.processors.JSONRenderer(),
            ],
            wrapper_class=structlog.make_filtering_bound_logger(logging.INFO),
            logger_factory=structlog.PrintLoggerFactory(),
        )
    else:
        structlog.configure(
            processors=[
                structlog.processors.TimeStamper(fmt="%H:%M:%S"),
                structlog.processors.add_log_level,
                structlog.dev.ConsoleRenderer(),
            ],
            wrapper_class=structlog.make_filtering_bound_logger(logging.INFO),
            logger_factory=structlog.PrintLoggerFactory(),
        )


log = structlog.get_logger()

GPU_MIGRATION_COOLDOWN_SECS = 120.0
GPU_LOAD_IMPROVEMENT_PCT = 10.0
ADMISSION_MIGRATION_COOLDOWN_SECS = 60.0
DRAIN_MIGRATION_COOLDOWN_SECS = 30.0

# ─── Groupe CLI principal ──────────────────────────────────────────────────────

@click.group()
@click.option("--log-level",  default="INFO",    show_default=True, help="Niveau de log")
@click.option("--log-format", default="console", show_default=True, help="Format : console ou json")
@click.pass_context
def cli(ctx: click.Context, log_level: str, log_format: str) -> None:
    """Controller de politique de paging distant — cluster omega-remote-paging (3 nœuds)."""
    setup_logging(log_level, log_format)
    ctx.ensure_object(dict)
    ctx.obj["log_level"]  = log_level
    ctx.obj["log_format"] = log_format


# ─── Commande : status ────────────────────────────────────────────────────────

@cli.command()
@click.option("--stores", default="127.0.0.1:9100,127.0.0.1:9101",
              help="Adresses des stores (host:port séparés par des virgules)")
@click.option("--proxmox-url", default="", help="URL de base Proxmox VE API (optionnel)")
def status(stores: str, proxmox_url: str) -> None:
    """Affiche le statut des stores et la situation mémoire locale."""
    collector = MetricsCollector()
    proxmox   = ProxmoxClient(base_url=proxmox_url or "https://proxmox-a:8006")

    # Métriques locales
    mem_snap = collector.snapshot()
    log.info("métriques mémoire locales", **mem_snap)

    # Statut des stores
    store_list = [s.strip() for s in stores.split(",") if s.strip()]
    log.info("interrogation des stores", count=len(store_list))
    statuses = poll_all_stores(store_list)
    for s in statuses:
        log.info(
            "store status",
            addr      = s.addr,
            reachable = s.reachable,
            stats     = s.stats,
        )

    # Vue cluster Proxmox (stub V1)
    cluster_summary = proxmox.cluster_memory_summary()
    log.info("résumé cluster (stub V1)", **cluster_summary)


# ─── Commande : policy ────────────────────────────────────────────────────────

@cli.command()
@click.option("--threshold-enable",  default=70.0, show_default=True,
              help="% RAM pour activer le remote paging")
@click.option("--threshold-migrate", default=90.0, show_default=True,
              help="% RAM pour recommander la migration")
@click.option("--dry-run", is_flag=True, default=False,
              help="Affiche la décision sans l'exécuter")
def policy(threshold_enable: float, threshold_migrate: float, dry_run: bool) -> None:
    """Évalue la politique de paging et affiche la décision."""
    collector = MetricsCollector()
    engine    = PolicyEngine(
        threshold_enable_pct  = threshold_enable,
        threshold_migrate_pct = threshold_migrate,
    )
    proxmox = ProxmoxClient()

    mem      = collector.read_meminfo()
    pressure = collector.read_pressure()

    inp    = PolicyInput(mem=mem, pressure=pressure)
    result = engine.evaluate(inp)

    log.info(
        "décision de politique",
        decision   = result.decision.value,
        reason     = result.reason,
        confidence = result.confidence,
        mem_usage  = f"{mem.usage_pct:.1f}%",
        swap_usage = f"{mem.swap_usage_pct:.1f}%",
    )

    if dry_run:
        log.info("mode dry-run — aucune action exécutée")
        return

    # Exécution de la décision
    _execute_decision(result.decision, proxmox)


# ─── Commande : monitor ───────────────────────────────────────────────────────

@cli.command()
@click.option("--interval", default=10, show_default=True, help="Intervalle en secondes")
@click.option("--stores",   default="127.0.0.1:9100,127.0.0.1:9101",
              help="Adresses des stores")
@click.option("--threshold-enable",  default=70.0, show_default=True)
@click.option("--threshold-migrate", default=90.0, show_default=True)
def monitor(
    interval:          int,
    stores:            str,
    threshold_enable:  float,
    threshold_migrate: float,
) -> None:
    """Boucle de monitoring : collecte, évalue, journalise en continu."""
    collector  = MetricsCollector()
    engine     = PolicyEngine(threshold_enable, threshold_migrate)
    proxmox    = ProxmoxClient()
    store_list = [s.strip() for s in stores.split(",") if s.strip()]

    log.info(
        "démarrage du monitoring",
        interval_s         = interval,
        stores             = store_list,
        policy_thresholds  = engine.describe_thresholds(),
    )

    prev_decision: Optional[Decision] = None

    while True:
        try:
            mem      = collector.read_meminfo()
            pressure = collector.read_pressure()
            inp      = PolicyInput(mem=mem, pressure=pressure)
            result   = engine.evaluate(inp)

            psi_some = pressure.some_avg10 if pressure else 0.0
            psi_full = pressure.full_avg10 if pressure else 0.0

            log.info(
                "cycle monitoring",
                mem_usage_pct  = round(mem.usage_pct, 1),
                swap_usage_pct = round(mem.swap_usage_pct, 1),
                psi_some_avg10 = psi_some,
                psi_full_avg10 = psi_full,
                decision       = result.decision.value,
                reason         = result.reason,
            )

            # Log de changement de décision (transition d'état)
            if result.decision != prev_decision:
                log.warning(
                    "changement de décision",
                    old = prev_decision.value if prev_decision else "initial",
                    new = result.decision.value,
                )
                prev_decision = result.decision

            # Statut des stores (toutes les N itérations en V2, ici à chaque cycle)
            store_statuses = poll_all_stores(store_list, timeout=1.0)
            for s in store_statuses:
                if not s.reachable:
                    log.warning("store injoignable", addr=s.addr)

        except KeyboardInterrupt:
            log.info("monitoring arrêté par l'utilisateur")
            break
        except Exception as e:
            log.error("erreur cycle monitoring", error=str(e))

        time.sleep(interval)


# ─── Commande : daemon ───────────────────────────────────────────────────────

@cli.command()
@click.option("--node", "nodes", multiple=True, required=True,
              help="URL API contrôle d'un nœud (répéter pour chaque nœud, ex: --node http://10.10.0.11:9300 --node http://10.10.0.12:9300)")
@click.option("--node-a", default="", hidden=True, help="Compatibilité ancienne syntaxe")
@click.option("--node-b", default="", hidden=True, help="Compatibilité ancienne syntaxe")
@click.option("--node-c", default="", hidden=True, help="Compatibilité ancienne syntaxe")
@click.option("--poll-interval", default=5, show_default=True, help="Intervalle de polling en secondes")
@click.option("--max-concurrent-migrations", default=1, show_default=True,
              help="Nombre maximum de migrations simultanées par cycle")
@click.option("--proxmox-url", default="https://127.0.0.1:8006", show_default=True,
              help="URL Proxmox VE API utilisée pour lire la configuration VM")
@click.option("--proxmox-token", default="", help="Token Proxmox VE API (sinon mode stub)")
@click.option("--with-local-disks", is_flag=True, default=False,
              help="Stockage local (LVM/ZFS) : active with-local-disks dans qm migrate")
@click.option("--auto-admit/--no-auto-admit", default=True, show_default=True,
              help="Configure automatiquement quotas et budgets des nouvelles VMs")
@click.option("--dry-run", is_flag=True, default=False,
              help="Évalue et journalise les recommandations sans déclencher les migrations")
def daemon(
    nodes: tuple,
    node_a: str,
    node_b: str,
    node_c: str,
    poll_interval: int,
    max_concurrent_migrations: int,
    proxmox_url: str,
    proxmox_token: str,
    with_local_disks: bool,
    auto_admit: bool,
    dry_run: bool,
) -> None:
    """
    Daemon de migration automatique : surveille le cluster et déclenche
    les migrations live/cold au moment optimal.

    Appelle GET /control/status sur chaque nœud, évalue MigrationPolicy,
    puis POST /control/migrate sur le nœud source pour chaque recommandation.

    Supporte N nœuds : --node http://ip1:9300 --node http://ip2:9300 ...
    Ancienne syntaxe --node-a/b/c toujours acceptée pour compatibilité.
    """
    # Construire node_urls depuis --node (nouvelle syntaxe) ou --node-a/b/c (ancienne)
    all_urls: list[str] = list(nodes)
    for legacy in (node_a, node_b, node_c):
        if legacy and legacy.rstrip("/") not in [u.rstrip("/") for u in all_urls]:
            all_urls.append(legacy)
    if not all_urls:
        raise click.UsageError("Au moins un --node est requis")
    node_urls: Dict[str, str] = {
        f"node-{chr(ord('a') + i)}": url.rstrip("/")
        for i, url in enumerate(all_urls)
    }
    policy = MigrationPolicy(MigrationThresholds())
    admission = AdmissionController()
    proxmox = ProxmoxClient(
        base_url=proxmox_url,
        api_token=proxmox_token,
        with_local_disks=with_local_disks,
    )
    vcpu_pool = ClusterVcpuPoolPlanner(VcpuPoolConfig())
    last_vcpu_migrations: Dict[int, float] = {}
    last_gpu_migrations: Dict[int, float] = {}
    last_admission_migrations: Dict[int, float] = {}

    log.info(
        "daemon de migration démarré",
        nodes=list(node_urls.keys()),
        poll_interval_s=poll_interval,
        auto_admit=auto_admit,
        dry_run=dry_run,
    )

    while True:
        try:
            node_states = _fetch_cluster_state(node_urls)
            if node_states:
                if auto_admit:
                    _reconcile_cluster_resources(
                        node_urls=node_urls,
                        node_states=node_states,
                        admission=admission,
                        proxmox=proxmox,
                        vcpu_pool=vcpu_pool,
                        last_vcpu_migrations=last_vcpu_migrations,
                        last_gpu_migrations=last_gpu_migrations,
                        last_admission_migrations=last_admission_migrations,
                        dry_run=dry_run,
                    )
                candidates = policy.evaluate(node_states)
                if candidates:
                    log.info(
                        "migrations recommandées",
                        count=len(candidates),
                        candidates=[
                            {
                                "vm_id": c.vm.vm_id,
                                "source": c.source,
                                "target": c.target,
                                "type": c.mtype.value,
                                "urgency": c.urgency,
                                "detail": c.detail,
                            }
                            for c in candidates
                        ],
                    )
                    if not dry_run:
                        _execute_migrations(
                            candidates[:max_concurrent_migrations],
                            node_urls,
                            node_states,
                        )
                else:
                    log.debug("aucune migration nécessaire")

        except KeyboardInterrupt:
            log.info("daemon arrêté par l'utilisateur")
            break
        except Exception as e:
            log.error("erreur cycle daemon", error=str(e))

        time.sleep(poll_interval)


# ─── Commande : migrate ──────────────────────────────────────────────────────

@cli.command()
@click.option("--source", required=True,
              help="URL API contrôle nœud source (ex: http://192.168.10.1:9300)")
@click.option("--vm-id",  required=True, type=int, help="ID de la VM à migrer")
@click.option("--target", required=True, help="ID du nœud cible (ex: node-b)")
@click.option("--type",   "mtype", default="live", show_default=True,
              type=click.Choice(["live", "cold"]), help="Type de migration")
def migrate(source: str, vm_id: int, target: str, mtype: str) -> None:
    """Déclenche manuellement une migration live ou cold via l'API contrôle."""
    url = source.rstrip("/") + "/control/migrate"
    payload = {"vm_id": vm_id, "target": target, "type": mtype}

    log.info("déclenchement migration manuelle", **payload, url=url)
    try:
        resp = requests.post(url, json=payload, timeout=10)
        resp.raise_for_status()
        data = resp.json()
        log.info("migration démarrée", task_id=data.get("task_id"), response=data)
    except requests.RequestException as exc:
        log.error("échec appel API migration", url=url, error=str(exc))
        sys.exit(1)


@cli.command("drain-node")
@click.option("--node", "nodes", multiple=True, required=True,
              help="URL API contrôle d'un nœud (répéter pour chaque nœud)")
@click.option("--node-a", default="", hidden=True)
@click.option("--node-b", default="", hidden=True)
@click.option("--node-c", default="", hidden=True)
@click.option("--source-node", required=True, help="ID du nœud à évacuer (ex: node-a, node-b, ...)")
@click.option("--poll-interval", default=5, show_default=True, help="Intervalle de polling en secondes")
@click.option("--timeout", default=900, show_default=True, help="Temps max d'attente en secondes")
@click.option(
    "--max-concurrent-migrations",
    default=1,
    show_default=True,
    help="Nombre maximum de migrations déclenchées par cycle",
)
@click.option("--dry-run", is_flag=True, default=False, help="Affiche seulement le plan d'évacuation")
def drain_node(
    nodes: tuple,
    node_a: str,
    node_b: str,
    node_c: str,
    source_node: str,
    poll_interval: int,
    timeout: int,
    max_concurrent_migrations: int,
    dry_run: bool,
) -> None:
    """Évacue toutes les VMs d'un nœud avant maintenance."""
    all_urls: list[str] = list(nodes)
    for legacy in (node_a, node_b, node_c):
        if legacy and legacy.rstrip("/") not in [u.rstrip("/") for u in all_urls]:
            all_urls.append(legacy)
    node_urls: Dict[str, str] = {
        f"node-{chr(ord('a') + i)}": url.rstrip("/")
        for i, url in enumerate(all_urls)
    }
    started: Dict[int, float] = {}
    deadline = time.monotonic() + max(1, timeout)

    log.info(
        "drain node démarré",
        source_node=source_node,
        poll_interval_s=poll_interval,
        timeout_s=timeout,
        dry_run=dry_run,
    )

    while True:
        node_states = _fetch_cluster_state(node_urls)
        source_state = node_states.get(source_node)
        if source_state is None:
            log.error("nœud source introuvable pour le drain", source_node=source_node)
            sys.exit(1)

        if not source_state.local_vms:
            log.info("drain node terminé", source_node=source_node)
            return

        plan = _plan_node_drain(source_node, node_states)
        if not plan:
            log.error(
                "drain node impossible avec la capacité actuelle",
                source_node=source_node,
                remaining_vms=[vm.vm_id for vm in source_state.local_vms],
            )
            sys.exit(2)

        if dry_run:
            log.info(
                "plan drain node",
                source_node=source_node,
                count=len(plan),
                migrations=[
                    {
                        "vm_id": c.vm.vm_id,
                        "source": c.source,
                        "target": c.target,
                        "type": c.mtype.value,
                        "reason": c.reason.value,
                    }
                    for c in plan
                ],
            )
            return

        now = time.monotonic()
        ready = [
            candidate
            for candidate in plan
            if now - started.get(candidate.vm.vm_id, 0.0) >= DRAIN_MIGRATION_COOLDOWN_SECS
        ]
        for candidate in ready[: max(1, max_concurrent_migrations)]:
            source_url = node_urls[candidate.source]
            launched = _start_auto_migration(
                source_url=source_url,
                node_states=node_states,
                source_node=candidate.source,
                vm=candidate.vm,
                target=candidate.target,
                detail="drain maintenance automatique",
                tracker=started,
                now=now,
                dry_run=False,
                reason="maintenance",
            )
            if launched:
                log.info(
                    "migration de drain déclenchée",
                    vm_id=candidate.vm.vm_id,
                    source=candidate.source,
                    target=candidate.target,
                )

        if time.monotonic() >= deadline:
            log.error(
                "drain node expiré",
                source_node=source_node,
                remaining_vms=[vm.vm_id for vm in source_state.local_vms],
            )
            sys.exit(3)

        time.sleep(poll_interval)


# ─── Helpers privés ───────────────────────────────────────────────────────────

def _fetch_cluster_state(node_urls: Dict[str, str]) -> Dict[str, MigNodeState]:
    """
    Interroge GET /control/status sur chaque nœud et construit
    les NodeState utilisés par MigrationPolicy.

    Les nœuds injoignables sont ignorés (logged comme warning).
    """
    states: Dict[str, MigNodeState] = {}

    for node_id, base_url in node_urls.items():
        url = base_url + "/control/status"
        try:
            resp = requests.get(url, timeout=3)
            resp.raise_for_status()
            data = resp.json()
            node_data = data.get("node", data)  # /control/status wraps under "node"
            gpu_data = node_data.get("gpu") or {}

            vms: List[VmState] = []
            for vm_entry in node_data.get("local_vms", []):
                vms.append(VmState(
                    vm_id          = vm_entry["vmid"],
                    status         = vm_entry.get("status", "unknown"),
                    max_mem_mib    = vm_entry.get("max_mem_mib", 0),
                    rss_kb         = vm_entry.get("rss_kb", 0),
                    remote_pages   = vm_entry.get("remote_pages", 0),
                    avg_cpu_pct    = vm_entry.get("avg_cpu_pct", 0.0),
                    throttle_ratio = vm_entry.get("throttle_ratio", 0.0),
                    gpu_vram_budget_mib = vm_entry.get("gpu_vram_budget_mib", 0),
                    disk_read_bps = vm_entry.get("disk_read_bps", 0.0),
                    disk_write_bps = vm_entry.get("disk_write_bps", 0.0),
                    disk_io_weight = vm_entry.get("disk_io_weight", 100),
                    disk_local_share_active = vm_entry.get("disk_local_share_active", False),
                ))

            mem_total_kb      = node_data.get("mem_total_kb", 0)
            mem_available_kb  = node_data.get("mem_available_kb", 0)
            vcpu_total        = node_data.get("vcpu_total", 24)
            vcpu_free         = node_data.get("vcpu_free", 24)

            states[node_id] = MigNodeState(
                node_id          = node_id,
                mem_total_kb     = mem_total_kb,
                mem_available_kb = mem_available_kb,
                proxmox_node_name = node_data.get("node_id", node_id),
                vcpu_total       = vcpu_total,
                vcpu_free        = vcpu_free,
                gpu_total_vram_mib = gpu_data.get("total_vram_mib", 0),
                gpu_free_vram_mib = gpu_data.get("free_vram_mib", 0),
                disk_pressure_pct = node_data.get("disk_pressure_pct", 0.0),
                local_vms        = vms,
            )
        except requests.RequestException as exc:
            log.warning("nœud injoignable", node_id=node_id, url=url, error=str(exc))

    return states


def _build_cluster_state(node_states: Dict[str, MigNodeState], node_urls: Dict[str, str]) -> ClusterState:
    nodes: List[NodeInfo] = []
    for node_id, state in node_states.items():
        vms = [
            VmEntry(
                vmid=vm.vm_id,
                max_mem_mib=vm.max_mem_mib,
                rss_kb=vm.rss_kb,
                remote_pages=vm.remote_pages,
                remote_mem_mib=vm.remote_pages * 4 // 1024,
                status=vm.status,
                gpu_vram_budget_mib=vm.gpu_vram_budget_mib,
                disk_read_bps=vm.disk_read_bps,
                disk_write_bps=vm.disk_write_bps,
                disk_io_weight=vm.disk_io_weight,
                disk_local_share_active=vm.disk_local_share_active,
            )
            for vm in state.local_vms
        ]
        nodes.append(NodeInfo(
            node_id=node_id,
            store_addr="",
            api_addr=node_urls[node_id],
            mem_total_kb=state.mem_total_kb,
            mem_available_kb=state.mem_available_kb,
            mem_usage_pct=state.ram_used_pct,
            pages_stored=0,
            store_used_kb=0,
            local_vms=vms,
            timestamp_secs=int(time.time()),
            gpu_enabled=state.gpu_total_vram_mib > 0,
            gpu_total_vram_mib=state.gpu_total_vram_mib,
            gpu_free_vram_mib=state.gpu_free_vram_mib,
            gpu_reserved_vram_mib=max(0, state.gpu_total_vram_mib - state.gpu_free_vram_mib),
            gpu_backend_name="",
            disk_pressure_pct=state.disk_pressure_pct,
            reachable=True,
        ))
    return ClusterState(nodes=nodes)


def _reconcile_cluster_resources(
    node_urls: Dict[str, str],
    node_states: Dict[str, MigNodeState],
    admission: AdmissionController,
    proxmox: ProxmoxClient,
    vcpu_pool: ClusterVcpuPoolPlanner,
    last_vcpu_migrations: Dict[int, float],
    dry_run: bool,
    last_gpu_migrations: Optional[Dict[int, float]] = None,
    last_admission_migrations: Optional[Dict[int, float]] = None,
) -> None:
    cluster = _build_cluster_state(node_states, node_urls)
    pending_vcpu_profiles: List[Tuple[int, str, str, VmState, dict, int, dict]] = []
    pool_vms: List[VcpuPoolVm] = []
    now = time.time()
    if last_gpu_migrations is None:
        last_gpu_migrations = {}
    if last_admission_migrations is None:
        last_admission_migrations = {}

    for node_id, node_state in node_states.items():
        source_url = node_urls[node_id]
        vcpu_status = _fetch_vcpu_status(source_url) or {}
        for vm in node_state.local_vms:
            quota = _fetch_vm_quota(source_url, vm.vm_id)
            if quota is None:
                if _ensure_vm_admitted(
                    source_node=node_id,
                    source_url=source_url,
                    vm=vm,
                    cluster=cluster,
                    admission=admission,
                    node_states=node_states,
                    last_admission_migrations=last_admission_migrations,
                    now=now,
                    dry_run=dry_run,
                ):
                    continue

            proxmox_node_name = node_state.proxmox_node_name or node_id
            config = proxmox.get_vm_config(proxmox_node_name, vm.vm_id)
            metadata = proxmox.parse_omega_metadata(config)
            gpu_budget = metadata.get("gpu_vram_mib", 0) or 0
            current_state = next(
                (
                    entry for entry in vcpu_status.get("vm_states", [])
                    if entry.get("vm_id") == vm.vm_id
                ),
                None,
            )
            current_vcpus = int(current_state.get("current_vcpus", 0)) if current_state else 0
            desired_profile = _normalize_vcpu_profile(metadata)
            required_vcpus = desired_profile[0] if desired_profile is not None else max(1, current_vcpus)

            if gpu_budget > 0 and _ensure_vm_gpu_placement(
                source_node=node_id,
                source_url=source_url,
                node_urls=node_urls,
                vm=vm,
                node_states=node_states,
                required_vcpus=required_vcpus,
                gpu_budget_mib=gpu_budget,
                last_gpu_migrations=last_gpu_migrations,
                now=now,
                dry_run=dry_run,
            ):
                continue

            pending_vcpu_profiles.append((
                _vcpu_profile_deficit(vm.vm_id, metadata, vcpu_status),
                node_id,
                source_url,
                vm,
                metadata,
                gpu_budget or vm.gpu_vram_budget_mib,
                vcpu_status,
            ))
            profile = _normalize_vcpu_profile(metadata)
            if profile is None:
                continue
            desired_min, desired_max = profile
            pool_vms.append(VcpuPoolVm(
                vmid=vm.vm_id,
                node_id=node_id,
                current_vcpus=current_vcpus,
                min_vcpus=desired_min,
                max_vcpus=desired_max,
            ))

    pool_decisions = vcpu_pool.plan(
        node_free_slots={node_id: state.vcpu_free for node_id, state in node_states.items()},
        vms=pool_vms,
        last_migration_at=last_vcpu_migrations,
        now=now,
    )
    decisions_by_vmid = {decision.vmid: decision for decision in pool_decisions}
    if pool_decisions:
        log.info(
            "plan du pool vCPU cluster",
            decisions=[
                {
                    "vm_id": decision.vmid,
                    "source": decision.source_node,
                    "action": decision.action,
                    "target": decision.target_node or None,
                    "cpu_deficit": decision.cpu_deficit,
                    "gain_vcpus": decision.gain_vcpus,
                    "cooldown_remaining_secs": round(decision.cooldown_remaining_secs, 1),
                }
                for decision in pool_decisions
            ],
        )

    pending_vcpu_profiles.sort(key=lambda item: (item[0], item[3].vm_id))
    for (
        _deficit,
        source_node,
        source_url,
        vm,
        metadata,
        gpu_budget_mib,
        vcpu_status,
    ) in pending_vcpu_profiles:
        migration_started = _ensure_vm_vcpu_profile(
            source_node=source_node,
            source_url=source_url,
            vm=vm,
            metadata=metadata,
            node_states=node_states,
            gpu_budget_mib=gpu_budget_mib,
            vcpu_status=vcpu_status,
            pool_decision=decisions_by_vmid.get(vm.vm_id),
            last_vcpu_migrations=last_vcpu_migrations,
            now=now,
            dry_run=dry_run,
            node_urls=node_urls,
        )
        if migration_started:
            continue
        if gpu_budget_mib > 0 and vm.gpu_vram_budget_mib != gpu_budget_mib:
            _ensure_vm_gpu_budget(
                source_url=source_url,
                vm_id=vm.vm_id,
                gpu_budget_mib=gpu_budget_mib,
                dry_run=dry_run,
            )


def _fetch_vm_quota(source_url: str, vm_id: int) -> Optional[dict]:
    url = source_url.rstrip("/") + f"/control/vm/{vm_id}/quota"
    try:
        resp = requests.get(url, timeout=3)
        if resp.status_code == 404:
            return None
        resp.raise_for_status()
        return resp.json().get("quota")
    except requests.RequestException:
        return None


def _fetch_vcpu_status(source_url: str) -> Optional[dict]:
    url = source_url.rstrip("/") + "/control/vcpu/status"
    try:
        resp = requests.get(url, timeout=3)
        resp.raise_for_status()
        return resp.json()
    except requests.RequestException:
        return None


def _normalize_vcpu_profile(metadata: dict) -> Optional[Tuple[int, int]]:
    desired_min = metadata.get("min_vcpus")
    desired_max = metadata.get("max_vcpus")

    if desired_min is None and desired_max is None:
        return None

    if desired_min is None:
        desired_min = desired_max
    if desired_max is None:
        desired_max = desired_min

    desired_min = max(1, int(desired_min or 1))
    desired_max = max(desired_min, int(desired_max or desired_min))
    return desired_min, desired_max


def _vcpu_profile_deficit(vm_id: int, metadata: dict, vcpu_status: dict) -> int:
    profile = _normalize_vcpu_profile(metadata)
    if profile is None:
        return 0

    desired_min, _desired_max = profile
    current_state = next(
        (
            entry for entry in vcpu_status.get("vm_states", [])
            if entry.get("vm_id") == vm_id
        ),
        None,
    )
    current_vcpus = int(current_state.get("current_vcpus", 0)) if current_state else 0
    return max(0, desired_min - current_vcpus)


def _target_can_host_vm(
    node_state: MigNodeState,
    vm: VmState,
    required_vcpus: int,
    gpu_budget_mib: int,
) -> bool:
    if node_state.vcpu_free < required_vcpus:
        return False
    if vm.max_mem_mib > 0 and node_state.mem_available_kb < vm.max_mem_mib * 1024:
        return False
    if gpu_budget_mib > 0 and node_state.gpu_free_vram_mib < gpu_budget_mib:
        return False
    return True


def _best_reconciliation_target(
    source_node: str,
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
) -> Optional[str]:
    candidates = [
        state
        for node_id, state in node_states.items()
        if node_id != source_node
        and _target_can_host_vm(state, vm, required_vcpus, gpu_budget_mib)
    ]
    if not candidates:
        return None

    candidates.sort(
        key=lambda state: (
            state.vcpu_free,
            state.mem_available_kb,
            state.gpu_free_vram_mib,
        ),
        reverse=True,
    )
    return candidates[0].node_id


def _find_vcpu_consolidation_plan(
    needy_vm: VmState,
    needy_source: str,
    desired_vcpus: int,
    gpu_budget_mib: int,
    node_states: Dict[str, MigNodeState],
    last_vcpu_migrations: Dict[int, float],
    now: float,
) -> Optional[Tuple[str, List[Tuple[VmState, str, str]]]]:
    """
    Bin-packing vCPU : trouve un plan de réorganisation qui libère assez de
    vCPUs sur un nœud cible en déplaçant d'autres VMs vers d'autres nœuds.

    Retourne (target_node, [(vm_à_déplacer, nœud_source, nœud_destination), ...])
    ou None si aucun plan n'est faisable.

    Algorithme :
      Pour chaque nœud T candidat (trié par vcpu_free desc) :
        1. Calculer combien de vCPUs T doit encore libérer.
        2. Pour chaque VM sur T (triée par vcpu_usage desc) :
             - Chercher un nœud destination D ≠ T, ≠ needy_source
               qui peut accueillir cette VM (RAM + GPU).
             - Si trouvé, l'ajouter au plan d'éviction.
             - Arrêter dès que T a assez de vCPUs libres pour needy_vm.
        3. Si un plan complet est trouvé, retourner (T, plan).
    """
    VCPU_MIGRATION_COOLDOWN = 120.0

    # Trier les candidats par vCPUs libres décroissant (le meilleur d'abord)
    candidates = [
        (node_id, state)
        for node_id, state in node_states.items()
        if node_id != needy_source
    ]
    candidates.sort(key=lambda x: x[1].vcpu_free, reverse=True)

    for target_node, target_state in candidates:
        needed_free = desired_vcpus - target_state.vcpu_free
        if needed_free <= 0:
            # Ce nœud a déjà assez de place — _best_reconciliation_target aurait dû le trouver.
            continue

        # VMs sur ce nœud cible, triées par consommation vCPU estimée décroissante
        vms_on_target = sorted(
            target_state.local_vms,
            key=lambda v: v.avg_cpu_pct * v.max_mem_mib,
            reverse=True,
        )

        plan: List[Tuple[VmState, str, str]] = []
        freed = 0
        # Simulation : vcpu_free et mem_available_kb ajustés après chaque déplacement
        sim_vcpu = {nid: s.vcpu_free for nid, s in node_states.items()}
        sim_mem  = {nid: s.mem_available_kb for nid, s in node_states.items()}

        for candidate_vm in vms_on_target:
            if candidate_vm.vm_id == needy_vm.vm_id:
                continue

            last_mig = last_vcpu_migrations.get(candidate_vm.vm_id)
            if last_mig is not None and now - last_mig < VCPU_MIGRATION_COOLDOWN:
                continue

            # Estimation des vCPUs libérés : basée sur avg_cpu_pct (1 slot par tranche de 25%)
            vcpus_est = max(1, int(candidate_vm.avg_cpu_pct / 25) + 1)
            mem_mib   = candidate_vm.max_mem_mib

            # Chercher une destination en utilisant les valeurs simulées (pas les valeurs initiales)
            destinations = [
                nid
                for nid, s in node_states.items()
                if nid != target_node
                and nid != needy_source
                and sim_vcpu.get(nid, 0) >= vcpus_est
                and sim_mem.get(nid, 0) >= mem_mib * 1024
                and (gpu_budget_mib == 0 or s.gpu_free_vram_mib >= gpu_budget_mib)
            ]
            if not destinations:
                continue

            # Prendre la destination avec le plus de vCPUs libres simulés
            destinations.sort(key=lambda nid: sim_vcpu.get(nid, 0), reverse=True)
            dest_id = destinations[0]

            plan.append((candidate_vm, target_node, dest_id))
            freed += vcpus_est
            # Mise à jour de la simulation pour les prochaines itérations
            sim_vcpu[target_node] = sim_vcpu.get(target_node, 0) + vcpus_est
            sim_vcpu[dest_id]     = max(0, sim_vcpu.get(dest_id, 0) - vcpus_est)
            sim_mem[dest_id]      = max(0, sim_mem.get(dest_id, 0) - mem_mib * 1024)

            if freed >= needed_free:
                # Plan complet — vérifier que needy_vm pourra bien aller sur target_node
                projected_vcpu_free = sim_vcpu.get(target_node, 0)
                projected_mem_free  = sim_mem.get(target_node, 0)
                if (projected_vcpu_free >= desired_vcpus
                        and projected_mem_free >= needy_vm.max_mem_mib * 1024):
                    return (target_node, plan)
                break  # ce nœud ne convient pas même après évictions, essayer le suivant

    return None


def _best_partial_reconciliation_target(
    source_node: str,
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    current_vcpus: int,
    gpu_budget_mib: int,
) -> Optional[str]:
    source_state = node_states[source_node]
    required_vcpus = max(1, current_vcpus)
    candidates = [
        state
        for node_id, state in node_states.items()
        if node_id != source_node
        and _target_can_host_vm(state, vm, required_vcpus, gpu_budget_mib)
        and state.vcpu_free > source_state.vcpu_free
    ]
    if not candidates:
        return None

    candidates.sort(
        key=lambda state: (
            state.vcpu_free - source_state.vcpu_free,
            state.mem_available_kb,
            state.gpu_free_vram_mib,
        ),
        reverse=True,
    )
    return candidates[0].node_id


def _gpu_candidate_target(
    source_node: str,
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
) -> Optional[str]:
    source_state = node_states[source_node]
    candidates = [
        state
        for node_id, state in node_states.items()
        if node_id != source_node
        and state.gpu_total_vram_mib > 0
        and _target_can_host_vm(state, vm, required_vcpus, gpu_budget_mib)
    ]
    if not candidates:
        return None

    candidates.sort(
        key=lambda state: (
            state.gpu_used_pct,
            -state.gpu_free_vram_mib,
            state.vcpu_used_pct,
            state.ram_used_pct,
        )
    )
    best = candidates[0]
    if (
        source_state.gpu_total_vram_mib == 0
        or source_state.gpu_free_vram_mib < gpu_budget_mib
        or source_state.gpu_used_pct - best.gpu_used_pct >= GPU_LOAD_IMPROVEMENT_PCT
    ):
        return best.node_id
    return None


def _best_gpu_eviction_target(
    source_node: str,
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    blocked_nodes: set[str],
) -> Optional[str]:
    candidates = [
        state
        for node_id, state in node_states.items()
        if node_id not in blocked_nodes
        and node_id != source_node
        and state.gpu_total_vram_mib > 0
        and _target_can_host_vm(state, vm, 1, vm.gpu_vram_budget_mib)
    ]
    if not candidates:
        return None
    candidates.sort(
        key=lambda state: (
            state.gpu_used_pct,
            -state.gpu_free_vram_mib,
            state.vcpu_used_pct,
            state.ram_used_pct,
        )
    )
    return candidates[0].node_id


def _find_gpu_space_creation_plan(
    source_node: str,
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
    last_gpu_migrations: Dict[int, float],
    now: float,
) -> Optional[Tuple[str, List[Tuple[VmState, str]]]]:
    source_state = node_states[source_node]
    candidates = [
        state
        for node_id, state in node_states.items()
        if node_id != source_node
        and state.gpu_total_vram_mib >= gpu_budget_mib
        and state.mem_available_kb >= vm.max_mem_mib * 1024
        and state.vcpu_free >= required_vcpus
    ]
    candidates.sort(
        key=lambda state: (
            state.gpu_used_pct,
            -state.gpu_free_vram_mib,
            state.vcpu_used_pct,
            state.ram_used_pct,
        )
    )

    for target in candidates:
        improvement = source_state.gpu_used_pct - target.gpu_used_pct
        if (
            source_state.gpu_total_vram_mib > 0
            and source_state.gpu_free_vram_mib >= gpu_budget_mib
            and improvement < GPU_LOAD_IMPROVEMENT_PCT
        ):
            continue

        needed_free = gpu_budget_mib - target.gpu_free_vram_mib
        if needed_free <= 0:
            return (target.node_id, [])

        movable: List[Tuple[int, VmState, str]] = []
        for resident in target.local_vms:
            if resident.vm_id == vm.vm_id or resident.gpu_vram_budget_mib <= 0:
                continue
            last_resident_migration = last_gpu_migrations.get(resident.vm_id)
            if (
                last_resident_migration is not None
                and now - last_resident_migration < GPU_MIGRATION_COOLDOWN_SECS
            ):
                continue
            eviction_target = _best_gpu_eviction_target(
                source_node=target.node_id,
                vm=resident,
                node_states=node_states,
                blocked_nodes={target.node_id},
            )
            if eviction_target is None:
                continue
            movable.append((resident.gpu_vram_budget_mib, resident, eviction_target))

        movable.sort(key=lambda item: (-item[0], item[1].vm_id))
        selected: List[Tuple[VmState, str]] = []
        freed = 0
        for budget, resident, eviction_target in movable:
            selected.append((resident, eviction_target))
            freed += budget
            if freed >= needed_free:
                return (target.node_id, selected)

    return None


def _start_auto_migration(
    source_url: str,
    node_states: Dict[str, MigNodeState],
    source_node: str,
    vm: VmState,
    target: str,
    detail: str,
    tracker: Dict[int, float],
    now: float,
    dry_run: bool,
    reason: Optional[str] = None,
) -> bool:
    proxmox_target = _proxmox_target_name(node_states, target)
    log.warning(
        detail,
        vm_id=vm.vm_id,
        source=source_node,
        target=target,
        proxmox_target=proxmox_target,
    )
    if dry_run:
        return True

    payload = {
        "vm_id": vm.vm_id,
        "target": proxmox_target,
        "type": _auto_migration_type(vm),
    }
    if reason:
        payload["reason"] = reason
    try:
        resp = requests.post(source_url.rstrip("/") + "/control/migrate", json=payload, timeout=10)
        resp.raise_for_status()
        tracker[vm.vm_id] = now
        return True
    except requests.RequestException as exc:
        log.error("échec migration automatique", vm_id=vm.vm_id, source=source_node, error=str(exc))
        return False


def _ensure_vm_gpu_placement(
    source_node: str,
    source_url: str,
    node_urls: Dict[str, str],
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
    last_gpu_migrations: Dict[int, float],
    now: float,
    dry_run: bool,
) -> bool:
    if gpu_budget_mib <= 0:
        return False

    fallback_network = os.environ.get("OMEGA_GPU_FALLBACK_NETWORK", "1") != "0"
    global_decision = choose_gpu_placement(
        source_node=source_node,
        vm=vm,
        node_states=node_states,
        required_vcpus=required_vcpus,
        gpu_budget_mib=gpu_budget_mib,
        proxy_port=int(os.environ.get("OMEGA_GPU_PROXY_PORT", "9400")),
        fallback_network=fallback_network,
    )
    if global_decision.action == GpuPlacementAction.LOCAL_GPU:
        _ensure_vm_gpu_budget(
            source_url=source_url,
            vm_id=vm.vm_id,
            gpu_budget_mib=gpu_budget_mib,
            dry_run=dry_run,
        )
        return False

    if global_decision.action == GpuPlacementAction.MIGRATE_TO_GPU:
        last_vm_migration = last_gpu_migrations.get(vm.vm_id)
        if (
            last_vm_migration is not None
            and now - last_vm_migration < GPU_MIGRATION_COOLDOWN_SECS
        ):
            log.info(
                "contrôleur GPU global — migration en cooldown",
                vm_id=vm.vm_id,
                source=source_node,
                target=global_decision.target_node,
                cooldown_remaining_secs=round(
                    GPU_MIGRATION_COOLDOWN_SECS - (now - last_vm_migration), 1
                ),
            )
            return False
        return _start_auto_migration(
            source_url=source_url,
            node_states=node_states,
            source_node=source_node,
            vm=vm,
            target=global_decision.target_node,
            detail="contrôleur GPU global — migration vers nœud GPU",
            tracker=last_gpu_migrations,
            now=now,
            dry_run=dry_run,
            reason="gpu_global_placement",
        )

    rust_plan = _rust_gpu_rebalance_plan(
        source_node=source_node,
        vm=vm,
        node_states=node_states,
        required_vcpus=required_vcpus,
        gpu_budget_mib=gpu_budget_mib,
        last_gpu_migrations=last_gpu_migrations,
        now=now,
    )
    if rust_plan is not None:
        action = rust_plan.get("action", "none")
        if action == "migrate":
            return _start_auto_migration(
                source_url=source_url,
                node_states=node_states,
                source_node=source_node,
                vm=vm,
                target=rust_plan.get("target_node", ""),
                detail="rééquilibrage automatique GPU (Rust)",
                tracker=last_gpu_migrations,
                now=now,
                dry_run=dry_run,
            )
        if action == "evict_then_migrate":
            evictions = rust_plan.get("evictions") or []
            for eviction in evictions:
                resident_source = eviction["source"]
                resident_url = node_urls[resident_source]
                resident_vm = next(
                    (
                        resident
                        for resident in node_states[resident_source].local_vms
                        if resident.vm_id == eviction["vm"]["vm_id"]
                    ),
                    None,
                )
                if resident_vm is None:
                    continue
                migrated = _start_auto_migration(
                    source_url=resident_url,
                    node_states=node_states,
                    source_node=resident_source,
                    vm=resident_vm,
                    target=eviction["target"],
                    detail="création automatique d'espace GPU par évacuation d'une autre VM (Rust)",
                    tracker=last_gpu_migrations,
                    now=now,
                    dry_run=dry_run,
                )
                if migrated:
                    return True
            return False
        if action == "none":
            return False

    last_vm_migration = last_gpu_migrations.get(vm.vm_id)
    if (
        last_vm_migration is not None
        and now - last_vm_migration < GPU_MIGRATION_COOLDOWN_SECS
    ):
        return False

    direct_target = _gpu_candidate_target(
        source_node=source_node,
        vm=vm,
        node_states=node_states,
        required_vcpus=required_vcpus,
        gpu_budget_mib=gpu_budget_mib,
    )
    if direct_target:
        return _start_auto_migration(
            source_url=source_url,
            node_states=node_states,
            source_node=source_node,
            vm=vm,
            target=direct_target,
            detail="rééquilibrage automatique GPU vers le nœud le moins chargé",
            tracker=last_gpu_migrations,
            now=now,
            dry_run=dry_run,
        )

    plan = _find_gpu_space_creation_plan(
        source_node=source_node,
        vm=vm,
        node_states=node_states,
        required_vcpus=required_vcpus,
        gpu_budget_mib=gpu_budget_mib,
        last_gpu_migrations=last_gpu_migrations,
        now=now,
    )
    if plan is None:
        if global_decision.action == GpuPlacementAction.REMOTE_PROXY:
            proxy_ok = _ensure_remote_gpu_proxy_budget(
                decision_proxy_url=global_decision.proxy_url,
                vm_id=vm.vm_id,
                gpu_budget_mib=gpu_budget_mib,
                dry_run=dry_run,
            )
            log.warning(
                "contrôleur GPU global — fallback proxy réseau",
                vm_id=vm.vm_id,
                source=source_node,
                proxy_node=global_decision.target_node,
                proxy_url=global_decision.proxy_url,
                budget_configured=proxy_ok,
                reason=global_decision.reason,
            )
            return proxy_ok
        return False

    target_node, evictions = plan
    if not evictions:
        return _start_auto_migration(
            source_url=source_url,
            node_states=node_states,
            source_node=source_node,
            vm=vm,
            target=target_node,
            detail="rééquilibrage automatique GPU",
            tracker=last_gpu_migrations,
            now=now,
            dry_run=dry_run,
        )

    for resident, eviction_target in evictions:
        resident_source_node = next(
            (node_id for node_id, state in node_states.items() if resident in state.local_vms),
            None,
        )
        if resident_source_node is None:
            log.error("source inconnue pour évacuation GPU", vm_id=resident.vm_id)
            return False
        resident_source_url = node_urls[resident_source_node]
        migrated = _start_auto_migration(
            source_url=resident_source_url,
            node_states=node_states,
            source_node=resident_source_node,
            vm=resident,
            target=eviction_target,
            detail="création automatique d'espace GPU par évacuation d'une autre VM",
            tracker=last_gpu_migrations,
            now=now,
            dry_run=dry_run,
        )
        if migrated:
            return True

    return False


def _rust_gpu_rebalance_plan(
    source_node: str,
    vm: VmState,
    node_states: Dict[str, MigNodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
    last_gpu_migrations: Dict[int, float],
    now: float,
) -> Optional[dict]:
    plan = call_policy(
        "evaluate-gpu-rebalance",
        {
            "config": {
                "migration_cooldown_secs": GPU_MIGRATION_COOLDOWN_SECS,
                "load_improvement_pct": GPU_LOAD_IMPROVEMENT_PCT,
            },
            "source_node": source_node,
            "required_vcpus": required_vcpus,
            "gpu_budget_mib": gpu_budget_mib,
            "now": now,
            "last_gpu_migrations": last_gpu_migrations,
            "vm": {
                "vm_id": vm.vm_id,
                "status": vm.status,
                "max_mem_mib": vm.max_mem_mib,
                "rss_kb": vm.rss_kb,
                "remote_pages": vm.remote_pages,
                "avg_cpu_pct": vm.avg_cpu_pct,
                "throttle_ratio": vm.throttle_ratio,
                "gpu_vram_budget_mib": vm.gpu_vram_budget_mib,
                "idle_duration_secs": vm.idle_duration_secs() if vm.idle_since is not None else None,
            },
            "nodes": [
                {
                    "node_id": node.node_id,
                    "mem_total_kb": node.mem_total_kb,
                    "mem_available_kb": node.mem_available_kb,
                    "vcpu_total": node.vcpu_total,
                    "vcpu_free": node.vcpu_free,
                    "gpu_total_vram_mib": node.gpu_total_vram_mib,
                    "gpu_free_vram_mib": node.gpu_free_vram_mib,
                    "local_vms": [
                        {
                            "vm_id": local_vm.vm_id,
                            "status": local_vm.status,
                            "max_mem_mib": local_vm.max_mem_mib,
                            "rss_kb": local_vm.rss_kb,
                            "remote_pages": local_vm.remote_pages,
                            "avg_cpu_pct": local_vm.avg_cpu_pct,
                            "throttle_ratio": local_vm.throttle_ratio,
                            "gpu_vram_budget_mib": local_vm.gpu_vram_budget_mib,
                            "idle_duration_secs": local_vm.idle_duration_secs()
                            if local_vm.idle_since is not None
                            else None,
                        }
                        for local_vm in node.local_vms
                    ],
                }
                for node in node_states.values()
            ],
        },
    )
    if not isinstance(plan, dict):
        return None
    return plan


def _proxmox_target_name(node_states: Dict[str, MigNodeState], target_node: str) -> str:
    state = node_states.get(target_node)
    if state is None:
        return target_node
    return state.proxmox_node_name or target_node


def _auto_migration_type(vm: VmState) -> str:
    if vm.status == "stopped":
        return "cold"
    return "live"


def _ensure_vm_admitted(
    source_node: str,
    source_url: str,
    vm: VmState,
    cluster: ClusterState,
    admission: AdmissionController,
    node_states: Dict[str, MigNodeState],
    last_admission_migrations: Dict[int, float],
    now: float,
    dry_run: bool,
) -> bool:
    decision = admission.admit(cluster, VmSpec(vmid=vm.vm_id, max_mem_mib=vm.max_mem_mib))
    if not decision.admitted:
        log.warning(
            "admission automatique impossible",
            vm_id=vm.vm_id,
            source=source_node,
            reason=decision.reason,
        )
        return False

    if decision.placement_node and decision.placement_node != source_node:
        last_attempt = last_admission_migrations.get(vm.vm_id)
        if (
            last_attempt is not None
            and now - last_attempt < ADMISSION_MIGRATION_COOLDOWN_SECS
        ):
            log.info(
                "repositionnement automatique déjà en attente",
                vm_id=vm.vm_id,
                source=source_node,
                target=decision.placement_node,
                cooldown_remaining_secs=round(
                    ADMISSION_MIGRATION_COOLDOWN_SECS - (now - last_attempt), 1
                ),
            )
            return True
        proxmox_target = _proxmox_target_name(node_states, decision.placement_node)
        log.warning(
            "repositionnement automatique de VM",
            vm_id=vm.vm_id,
            source=source_node,
            target=decision.placement_node,
            proxmox_target=proxmox_target,
        )
        if dry_run:
            return True
        payload = {
            "vm_id": vm.vm_id,
            "target": proxmox_target,
            "type": _auto_migration_type(vm),
        }
        try:
            resp = requests.post(source_url.rstrip("/") + "/control/migrate", json=payload, timeout=10)
            resp.raise_for_status()
            last_admission_migrations[vm.vm_id] = now
            data = resp.json()
            log.info(
                "repositionnement automatique accepté",
                vm_id=vm.vm_id,
                source=source_node,
                target=decision.placement_node,
                task_id=data.get("task_id"),
            )
        except requests.RequestException as exc:
            log.error("échec repositionnement automatique", vm_id=vm.vm_id, error=str(exc))
        return True

    payload = decision.quota_payload()
    log.info(
        "configuration automatique du quota",
        vm_id=vm.vm_id,
        node=source_node,
        local_budget_mib=payload["local_budget_mib"],
        remote_budget_mib=payload["remote_budget_mib"],
    )
    if dry_run:
        return
    try:
        resp = requests.post(
            source_url.rstrip("/") + f"/control/vm/{vm.vm_id}/quota",
            json=payload,
            timeout=10,
        )
        resp.raise_for_status()
    except requests.RequestException as exc:
        log.error("échec configuration quota automatique", vm_id=vm.vm_id, error=str(exc))
    return False


def _ensure_vm_gpu_budget(
    source_url: str,
    vm_id: int,
    gpu_budget_mib: int,
    dry_run: bool,
) -> None:
    log.info(
        "configuration automatique du budget GPU",
        vm_id=vm_id,
        gpu_budget_mib=gpu_budget_mib,
    )
    if dry_run:
        return
    try:
        resp = requests.post(
            source_url.rstrip("/") + f"/control/vm/{vm_id}/gpu",
            json={"vram_budget_mib": gpu_budget_mib},
            timeout=10,
        )
        resp.raise_for_status()
    except requests.RequestException as exc:
        log.error("échec configuration budget GPU automatique", vm_id=vm_id, error=str(exc))


def _ensure_remote_gpu_proxy_budget(
    decision_proxy_url: str,
    vm_id: int,
    gpu_budget_mib: int,
    dry_run: bool,
) -> bool:
    proxy_url = os.environ.get("OMEGA_GPU_PROXY_URL") or decision_proxy_url
    if not proxy_url:
        return False
    headers = {"Content-Type": "application/json"}
    token = os.environ.get("OMEGA_GPU_PROXY_API_TOKEN", "")
    token_file = os.environ.get("OMEGA_GPU_PROXY_API_TOKEN_FILE", "")
    if not token and token_file:
        try:
            with open(token_file, "r", encoding="utf-8") as fh:
                token = fh.read().strip()
        except OSError:
            token = ""
    if token:
        headers["Authorization"] = f"Bearer {token}"

    log.info(
        "configuration budget GPU sur proxy réseau",
        vm_id=vm_id,
        proxy_url=proxy_url,
        gpu_budget_mib=gpu_budget_mib,
    )
    if dry_run:
        return True
    try:
        resp = requests.post(
            proxy_url.rstrip("/") + f"/v1/vm/{vm_id}/budget",
            json={"vram_budget_mib": gpu_budget_mib},
            headers=headers,
            timeout=5,
        )
        resp.raise_for_status()
        return True
    except requests.RequestException as exc:
        log.error(
            "échec configuration budget GPU proxy réseau",
            vm_id=vm_id,
            proxy_url=proxy_url,
            error=str(exc),
        )
        return False


def _ensure_vm_vcpu_profile(
    source_node: str,
    source_url: str,
    vm: VmState,
    metadata: dict,
    node_states: Dict[str, MigNodeState],
    gpu_budget_mib: int,
    vcpu_status: dict,
    pool_decision: Optional[VcpuPoolDecision],
    last_vcpu_migrations: Dict[int, float],
    now: float,
    dry_run: bool,
    node_urls: Optional[Dict[str, str]] = None,
) -> bool:
    profile = _normalize_vcpu_profile(metadata)
    if profile is None:
        return False

    desired_min, desired_max = profile
    current_state = next(
        (
            entry for entry in vcpu_status.get("vm_states", [])
            if entry.get("vm_id") == vm.vm_id
        ),
        None,
    )

    current_vcpus = int(current_state.get("current_vcpus", 0)) if current_state else 0
    current_min = int(current_state.get("min_vcpus", current_vcpus)) if current_state else 0
    current_max = int(current_state.get("max_vcpus", current_vcpus)) if current_state else 0

    if current_state and current_min == desired_min and current_max == desired_max:
        return False

    if pool_decision is not None:
        if pool_decision.action == "apply_local":
            log.info(
                "configuration automatique du profil vCPU",
                vm_id=vm.vm_id,
                min_vcpus=desired_min,
                max_vcpus=desired_max,
                pool_action=pool_decision.action,
                cpu_deficit=pool_decision.cpu_deficit,
            )
            if dry_run:
                return False
            try:
                resp = requests.post(
                    source_url.rstrip("/") + f"/control/vm/{vm.vm_id}/vcpu",
                    json={"min_vcpus": desired_min, "max_vcpus": desired_max},
                    timeout=10,
                )
                resp.raise_for_status()
            except requests.RequestException as exc:
                log.error("échec configuration vCPU automatique", vm_id=vm.vm_id, error=str(exc))
            return False

        if pool_decision.action in {"migrate_full", "migrate_partial"}:
            proxmox_target = _proxmox_target_name(node_states, pool_decision.target_node)
            log.warning(
                "migration automatique pilotée par le pool vCPU",
                vm_id=vm.vm_id,
                source=source_node,
                target=pool_decision.target_node,
                proxmox_target=proxmox_target,
                resolution=pool_decision.action,
                current_vcpus=pool_decision.current_vcpus,
                cpu_deficit=pool_decision.cpu_deficit,
                gain_vcpus=pool_decision.gain_vcpus,
                min_vcpus=desired_min,
                max_vcpus=desired_max,
            )
            if dry_run:
                return True
            payload = {
                "vm_id": vm.vm_id,
                "target": proxmox_target,
                "type": _auto_migration_type(vm),
            }
            try:
                resp = requests.post(source_url.rstrip("/") + "/control/migrate", json=payload, timeout=10)
                resp.raise_for_status()
                last_vcpu_migrations[vm.vm_id] = now
            except requests.RequestException as exc:
                log.error("échec migration automatique CPU", vm_id=vm.vm_id, error=str(exc))
            return True

        if pool_decision.action == "wait_cooldown":
            log.info(
                "profil vCPU en attente de cooldown",
                vm_id=vm.vm_id,
                source=source_node,
                current_vcpus=pool_decision.current_vcpus,
                desired_min=desired_min,
                desired_max=desired_max,
                cpu_deficit=pool_decision.cpu_deficit,
                cooldown_remaining_secs=round(pool_decision.cooldown_remaining_secs, 1),
            )
            return False

        if pool_decision.action == "wait_deficit":
            log.warning(
                "profil vCPU en déficit, VM conservée en attente",
                vm_id=vm.vm_id,
                source=source_node,
                current_vcpus=pool_decision.current_vcpus,
                desired_min=desired_min,
                desired_max=desired_max,
                cpu_deficit=pool_decision.cpu_deficit,
            )
            return False

    required_extra = max(0, desired_min - current_vcpus)
    source_state = node_states[source_node]
    if source_state.vcpu_free < required_extra:
        target = _best_reconciliation_target(
            source_node=source_node,
            vm=vm,
            node_states=node_states,
            required_vcpus=desired_min,
            gpu_budget_mib=gpu_budget_mib,
        )
        resolution = "full_fit"
        if not target:
            target = _best_partial_reconciliation_target(
                source_node=source_node,
                vm=vm,
                node_states=node_states,
                current_vcpus=current_vcpus,
                gpu_budget_mib=gpu_budget_mib,
            )
            resolution = "partial_fit"

        if not target:
            # Aucun nœud ne peut accueillir la VM directement.
            # Tentative de consolidation bin-packing : réorganiser des VMs sur
            # d'autres nœuds pour créer assez de vCPUs libres sur un seul nœud.
            consolidation = _find_vcpu_consolidation_plan(
                needy_vm=vm,
                needy_source=source_node,
                desired_vcpus=desired_min,
                gpu_budget_mib=gpu_budget_mib,
                node_states=node_states,
                last_vcpu_migrations=last_vcpu_migrations,
                now=now,
            )
            if consolidation:
                target_node, evictions = consolidation
                log.warning(
                    "consolidation vCPU bin-packing : réorganisation de %d VM(s) "
                    "pour libérer %d vCPUs sur %s",
                    len(evictions), required_extra, target_node,
                    vm_id=vm.vm_id,
                    source=source_node,
                    evictions=[
                        {"vm_id": ev_vm.vm_id, "from": ev_src, "to": ev_tgt}
                        for ev_vm, ev_src, ev_tgt in evictions
                    ],
                )
                if not dry_run:
                    for ev_vm, ev_src, ev_tgt in evictions:
                        ev_url = node_urls.get(ev_src, source_url)
                        _start_auto_migration(
                            source_url=ev_url,
                            node_states=node_states,
                            source_node=ev_src,
                            vm=ev_vm,
                            target=ev_tgt,
                            detail="réorganisation bin-packing vCPU",
                            tracker=last_vcpu_migrations,
                            now=now,
                            dry_run=False,
                        )
                return True

            log.warning(
                "profil vCPU en déficit, VM conservée en attente",
                vm_id=vm.vm_id,
                source=source_node,
                current_vcpus=current_vcpus,
                desired_min=desired_min,
                desired_max=desired_max,
                cpu_deficit=required_extra,
            )
            return False

        proxmox_target = _proxmox_target_name(node_states, target)
        log.warning(
            "migration automatique pour satisfaire le profil vCPU",
            vm_id=vm.vm_id,
            source=source_node,
            target=target,
            proxmox_target=proxmox_target,
            resolution=resolution,
            current_vcpus=current_vcpus,
            cpu_deficit=required_extra,
            min_vcpus=desired_min,
            max_vcpus=desired_max,
        )
        if dry_run:
            return True
        payload = {
            "vm_id": vm.vm_id,
            "target": proxmox_target,
            "type": _auto_migration_type(vm),
        }
        try:
            resp = requests.post(source_url.rstrip("/") + "/control/migrate", json=payload, timeout=10)
            resp.raise_for_status()
        except requests.RequestException as exc:
            log.error("échec migration automatique CPU", vm_id=vm.vm_id, error=str(exc))
        return True

    log.info(
        "configuration automatique du profil vCPU",
        vm_id=vm.vm_id,
        min_vcpus=desired_min,
        max_vcpus=desired_max,
    )
    if dry_run:
        return
    try:
        resp = requests.post(
            source_url.rstrip("/") + f"/control/vm/{vm.vm_id}/vcpu",
            json={"min_vcpus": desired_min, "max_vcpus": desired_max},
            timeout=10,
        )
        resp.raise_for_status()
    except requests.RequestException as exc:
        log.error("échec configuration vCPU automatique", vm_id=vm.vm_id, error=str(exc))
    return False


def _execute_migrations(
    candidates: List[MigrationCandidate],
    node_urls: Dict[str, str],
    node_states: Dict[str, MigNodeState],
) -> None:
    """Appelle POST /control/migrate pour chaque candidat."""
    for c in candidates:
        source_url = node_urls.get(c.source)
        if not source_url:
            log.error("nœud source inconnu", source=c.source)
            continue

        url = source_url.rstrip("/") + "/control/migrate"
        payload = c.to_api_payload()
        payload["target"] = _proxmox_target_name(node_states, c.target)
        try:
            resp = requests.post(url, json=payload, timeout=10)
            resp.raise_for_status()
            data = resp.json()
            log.info(
                "migration déclenchée",
                vm_id   = c.vm.vm_id,
                source  = c.source,
                target  = c.target,
                type    = c.mtype.value,
                task_id = data.get("task_id"),
            )
        except requests.RequestException as exc:
            log.error(
                "échec déclenchement migration",
                vm_id  = c.vm.vm_id,
                source = c.source,
                error  = str(exc),
            )


def _clone_node_state(state: MigNodeState) -> MigNodeState:
    return MigNodeState(
        node_id=state.node_id,
        mem_total_kb=state.mem_total_kb,
        mem_available_kb=state.mem_available_kb,
        proxmox_node_name=state.proxmox_node_name,
        vcpu_total=state.vcpu_total,
        vcpu_free=state.vcpu_free,
        gpu_total_vram_mib=state.gpu_total_vram_mib,
        gpu_free_vram_mib=state.gpu_free_vram_mib,
        local_vms=list(state.local_vms),
    )


def _drain_sort_key(vm: VmState) -> Tuple[int, int, float, int]:
    return (
        vm.gpu_vram_budget_mib,
        vm.max_mem_mib,
        vm.avg_cpu_pct,
        vm.vm_id,
    )


def _apply_drain_reservation(target: MigNodeState, vm: VmState) -> None:
    target.mem_available_kb = max(0, target.mem_available_kb - vm.max_mem_mib * 1024)
    target.vcpu_free = max(0, target.vcpu_free - 1)
    if vm.gpu_vram_budget_mib > 0:
        target.gpu_free_vram_mib = max(0, target.gpu_free_vram_mib - vm.gpu_vram_budget_mib)
    target.local_vms.append(vm)


def _plan_node_drain(
    source_node: str,
    node_states: Dict[str, MigNodeState],
) -> List[MigrationCandidate]:
    source_state = node_states.get(source_node)
    if source_state is None:
        return []

    simulated = {
        node_id: _clone_node_state(state)
        for node_id, state in node_states.items()
    }
    source_sim = simulated[source_node]
    vms = sorted(source_sim.local_vms, key=_drain_sort_key, reverse=True)

    plan: List[MigrationCandidate] = []
    for vm in vms:
        target = _best_reconciliation_target(
            source_node=source_node,
            vm=vm,
            node_states=simulated,
            required_vcpus=1,
            gpu_budget_mib=vm.gpu_vram_budget_mib,
        )
        if target is None:
            return []

        candidate = MigrationCandidate(
            vm=vm,
            source=source_node,
            target=target,
            mtype=MigrationType(_auto_migration_type(vm)),
            reason=MigrationReason.MAINTENANCE_DRAIN,
            urgency=2,
            detail="drain maintenance",
        )
        plan.append(candidate)
        _apply_drain_reservation(simulated[target], vm)

    return plan


def _execute_decision(decision: Decision, proxmox: ProxmoxClient) -> None:
    """
    Exécute la décision prise par la politique.

    V1 : journalisation seulement pour migrate (l'API Proxmox est un stub).
    V2 : appels API réels.
    """
    if decision == Decision.LOCAL_ONLY:
        log.info("action : rien (mémoire suffisante)")

    elif decision == Decision.ENABLE_REMOTE:
        log.info(
            "action : remote paging activé",
            note="en V1 l'agent active le paging automatiquement — aucune commande à envoyer",
        )

    elif decision == Decision.MIGRATE:
        best_node = proxmox.get_node_with_most_free_ram(exclude_node="node-a")
        log.warning(
            "action : migration recommandée (stub V1)",
            target_node = best_node,
            note        = "migration non exécutée en V1 — implémenter proxmox.migrate_vm() en V2",
        )


if __name__ == "__main__":
    cli()
