"""
Daemon de migration automatique — cœur du controller V4.

# Principe

Le MigrationDaemon tourne en boucle et :
  1. Collecte l'état de tous les omega-daemons du cluster
  2. Identifie les VMs avec trop de pages distantes (≥ min_remote_pages)
  3. Trouve le meilleur nœud cible avec le PlacementEngine
  4. Déclenche la migration live via l'API Proxmox
  5. Attend la fin de la migration
  6. Supprime les pages distantes devenues inutiles

# Évitement des migrations en double

Un set `migrating_vms` protège contre le déclenchement de deux migrations
simultanées pour la même VM.

# Journalisation

Tous les événements importants (décision, démarrage, succès, échec) sont
journalisés avec structlog pour intégration future dans un système de monitoring.
"""

from __future__ import annotations

import time
import logging
import threading
from dataclasses import dataclass, field
from typing import Optional

from .cluster import ClusterState, ClusterStateCollector
from .placement import PlacementEngine, PlacementDecision, PlacementStrategy
from .proxmox import ProxmoxClient, MigrationTask
from .resilient_collector import ResilientCollector
from .topology_placement import TopologyAwarePlacementEngine, NodeTopology, PlacementWeights

logger = logging.getLogger(__name__)


# ─── Historique des migrations ────────────────────────────────────────────────

@dataclass
class MigrationRecord:
    """Entrée dans l'historique des migrations."""
    vmid:        int
    source:      str
    target:      str
    started_at:  float
    ended_at:    float      = 0.0
    success:     bool       = False
    pages_freed: int        = 0
    error:       str        = ""

    @property
    def duration_secs(self) -> float:
        if self.ended_at:
            return self.ended_at - self.started_at
        return time.time() - self.started_at


# ─── Configuration ────────────────────────────────────────────────────────────

@dataclass
class MigrationDaemonConfig:
    """Configuration du daemon de migration."""
    # Adresses des omega-daemons (host:api_port)
    peer_addrs:        list[str]
    # Proxmox API
    proxmox_url:       str         = "https://127.0.0.1:8006"
    proxmox_token:     str         = ""
    proxmox_verify_ssl: bool       = False
    # Politique
    strategy:          PlacementStrategy = PlacementStrategy.BEST_FIT
    min_remote_pages:  int         = 256    # ~1 Mio de pages distantes pour déclencher
    safety_margin:     float       = 0.15   # 15% de marge RAM
    # Timings
    poll_interval:     float       = 15.0   # secondes entre cycles
    migration_timeout: float       = 600.0  # secondes max par migration
    post_cleanup_delay: float      = 5.0    # attente après migration avant cleanup
    # Sécurité
    max_concurrent:    int         = 1      # migrations simultanées max (V4 = 1)
    dry_run:           bool        = False  # ne déclenche rien si True
    # L4 — résilience du collecteur
    collector_retries: int         = 3
    circuit_failure_threshold: int = 3
    # L8 — topologie réseau du cluster (node_id → NodeTopology)
    topology:          dict        = None   # None = pas de topologie connue


# ─── Daemon ───────────────────────────────────────────────────────────────────

class MigrationDaemon:
    """
    Daemon de migration automatique.

    Peut être lancé dans un thread dédié ou via la commande CLI.
    """

    def __init__(self, cfg: MigrationDaemonConfig):
        self.cfg      = cfg
        self.running  = threading.Event()
        self.running.set()


        # L4 — collecteur résilient avec retry + circuit-breaker
        self.collector = ResilientCollector(
            peer_api_addrs      = cfg.peer_addrs,
            timeout             = 3.0,
            max_retries         = cfg.collector_retries,
            failure_threshold   = cfg.circuit_failure_threshold,
            open_duration_secs  = 30.0,
            stale_max_age_secs  = 120.0,
        )

        # L8 — moteur topology-aware si une topologie est fournie, sinon standard
        if cfg.topology:
            self.placement = TopologyAwarePlacementEngine(
                topology         = cfg.topology,
                safety_margin    = cfg.safety_margin,
                min_remote_pages = cfg.min_remote_pages,
            )
        else:
            self.placement = PlacementEngine(
                strategy         = cfg.strategy,
                safety_margin    = cfg.safety_margin,
                min_remote_pages = cfg.min_remote_pages,
            )
        self.proxmox = ProxmoxClient(
            base_url   = cfg.proxmox_url,
            api_token  = cfg.proxmox_token,
            verify_ssl = cfg.proxmox_verify_ssl,
        )

        # État interne
        self._migrating_vms:   set[int]              = set()
        self._migration_lock:  threading.Lock        = threading.Lock()
        self._history:         list[MigrationRecord] = []
        self._cycle_count:     int                   = 0

    def stop(self) -> None:
        """Arrête la boucle principale."""
        self.running.clear()
        logger.info("daemon de migration : arrêt demandé")

    def run(self) -> None:
        """
        Boucle principale du daemon.

        Bloquante — à lancer dans un thread dédié ou directement depuis main.
        """
        logger.info(
            "daemon de migration démarré : %d pairs, intervalle=%.0fs, "
            "stratégie=%s, min_pages=%d, dry_run=%s",
            len(self.cfg.peer_addrs), self.cfg.poll_interval,
            self.cfg.strategy.value, self.cfg.min_remote_pages,
            self.cfg.dry_run,
        )

        while self.running.is_set():
            try:
                self._run_cycle()
            except Exception as e:
                logger.error("erreur cycle migration : %s", e, exc_info=True)

            # Attente inter-cycle (interruptible par stop())
            self.running.wait(timeout=self.cfg.poll_interval)

        logger.info("daemon de migration terminé")

    def _run_cycle(self) -> None:
        """Un cycle complet : collect → place → migrate → cleanup."""
        self._cycle_count += 1
        cycle = self._cycle_count

        logger.debug("cycle %d : collecte de l'état du cluster", cycle)

        # ── 1. Collecte de l'état ──────────────────────────────────────────
        cluster = self.collector.collect()
        reachable = len(cluster.reachable_nodes)
        total     = len(cluster.nodes)

        logger.info(
            "cycle %d : %d/%d nœuds joignables, %d VMs total, "
            "%d VMs avec pages distantes",
            cycle, reachable, total,
            sum(len(n.local_vms) for n in cluster.reachable_nodes),
            len(cluster.vms_with_remote_pages(1)),
        )

        if reachable == 0:
            logger.warning("cycle %d : aucun nœud joignable — cycle ignoré", cycle)
            return

        # ── 2. Décisions de placement ──────────────────────────────────────
        decisions = self.placement.find_all_migrations(cluster)

        if not decisions:
            logger.debug("cycle %d : aucune migration requise", cycle)
            return

        logger.info("cycle %d : %d migration(s) envisagée(s)", cycle, len(decisions))

        # ── 3. Exécution des migrations ────────────────────────────────────
        migrations_done = 0
        for decision in decisions:
            # Limite de concurrence
            with self._migration_lock:
                active = len(self._migrating_vms)
            if active >= self.cfg.max_concurrent:
                logger.info(
                    "limite de %d migration(s) simultanée(s) atteinte — report",
                    self.cfg.max_concurrent,
                )
                break

            # Évitement doublon
            with self._migration_lock:
                if decision.vmid in self._migrating_vms:
                    logger.debug("vmid=%d déjà en migration — skip", decision.vmid)
                    continue
                self._migrating_vms.add(decision.vmid)

            try:
                success = self._execute_migration(decision, cluster)
                if success:
                    migrations_done += 1
            finally:
                with self._migration_lock:
                    self._migrating_vms.discard(decision.vmid)

        logger.info("cycle %d : %d migration(s) réussie(s)", cycle, migrations_done)

    def _execute_migration(
        self,
        decision: PlacementDecision,
        cluster:  ClusterState,
    ) -> bool:
        """
        Exécute une migration complète :
        1. Déclenche la migration Proxmox
        2. Attend la fin
        3. Supprime les pages distantes sur tous les stores

        Retourne True si succès.
        """
        vmid        = decision.vmid
        source      = decision.source_node
        target      = decision.target_node

        record = MigrationRecord(
            vmid       = vmid,
            source     = source,
            target     = target,
            started_at = time.time(),
        )

        logger.info(
            "MIGRATION : vmid=%d %s → %s (%d Mio RAM, %d pages distantes, confiance=%.2f)",
            vmid, source, target,
            decision.vm_max_mem_mib,
            cluster.node_by_id(source) and next(
                (vm.remote_pages for _, vm in cluster.vms_with_remote_pages(1) if _ == source),
                0
            ) or 0,
            decision.confidence,
        )

        if self.cfg.dry_run:
            logger.warning("DRY-RUN : migration vmid=%d simulée (non exécutée)", vmid)
            self._history.append(record)
            return True

        # ── Déclenchement ──────────────────────────────────────────────────
        task = self.proxmox.migrate_vm(
            vmid        = vmid,
            source_node = source,
            target_node = target,
            online      = True,
        )

        if task is None:
            logger.error("migration vmid=%d : déclenchement échoué", vmid)
            record.error = "déclenchement échoué"
            record.ended_at = time.time()
            self._history.append(record)
            return False

        # ── Attente ────────────────────────────────────────────────────────
        logger.info("migration vmid=%d en cours (UPID=%s)...", vmid, task.upid)
        success = self.proxmox.wait_for_migration(
            task          = task,
            poll_interval = 5.0,
            max_wait_secs = self.cfg.migration_timeout,
        )

        record.ended_at = time.time()
        record.success  = success

        if not success:
            logger.error(
                "migration vmid=%d ÉCHOUÉE après %.0fs",
                vmid, record.duration_secs,
            )
            record.error = task.status
            self._history.append(record)
            return False

        logger.info(
            "migration vmid=%d RÉUSSIE en %.0fs",
            vmid, record.duration_secs,
        )

        # ── Nettoyage des pages distantes ──────────────────────────────────
        time.sleep(self.cfg.post_cleanup_delay)
        pages_freed = self._cleanup_remote_pages(vmid, cluster)
        record.pages_freed = pages_freed

        logger.info(
            "cleanup post-migration vmid=%d : %d pages supprimées",
            vmid, pages_freed,
        )

        self._history.append(record)
        return True

    def _cleanup_remote_pages(self, vmid: int, cluster: ClusterState) -> int:
        """
        Supprime les pages d'une VM sur tous les stores du cluster.

        Appelé après une migration réussie. La VM est maintenant sur son nœud
        cible avec toute sa RAM locale — les pages distantes sont obsolètes.

        Retourne le nombre total de pages supprimées.
        """
        total_deleted = 0

        for node in cluster.reachable_nodes:
            # Combien de pages de cette VM sur ce store ?
            pages_here = sum(
                vm.remote_pages
                for _, vm in cluster.vms_with_remote_pages(0)
                # Note: dans un vrai déploiement, on interrogerait /api/pages/{vmid}
                # sur chaque nœud, pas juste les "vms_with_remote_pages"
            )

            # On appelle DELETE /api/pages/{vmid} sur tous les nœuds qui pourraient en avoir
            deleted = self.collector.delete_vm_pages(node.api_addr, vmid)
            if deleted:
                logger.debug("pages vmid=%d supprimées sur %s", vmid, node.node_id)
                total_deleted += 1  # approximation (nombre de nœuds nettoyés)

        return total_deleted

    @property
    def history(self) -> list[MigrationRecord]:
        return list(self._history)

    def stats(self) -> dict:
        done      = [r for r in self._history if r.ended_at > 0]
        successes = [r for r in done if r.success]
        failures  = [r for r in done if not r.success]

        avg_duration = (
            sum(r.duration_secs for r in successes) / len(successes)
            if successes else 0.0
        )

        return {
            "cycles_run":        self._cycle_count,
            "migrations_total":  len(done),
            "migrations_ok":     len(successes),
            "migrations_failed": len(failures),
            "avg_duration_secs": round(avg_duration, 1),
            "currently_migrating": list(self._migrating_vms),
        }
