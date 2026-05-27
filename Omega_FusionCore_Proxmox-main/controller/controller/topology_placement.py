"""
Moteur de placement topology-aware — correction de la limite L8.

# Problème corrigé

Le `PlacementEngine` original optimisait uniquement sur la RAM disponible.
Il ignorait :
  - Le rack physique (migrations intra-rack = réseau 10× plus rapide)
  - La zone de disponibilité (éviter les migrations inter-zone si possible)
  - La latence réseau mesurée entre nœuds
  - La charge CPU du nœud cible
  - Le nombre de migrations déjà en cours vers ce nœud

# Solution

`TopologyAwarePlacementEngine` calcule un **score multi-critères** pour chaque
nœud candidat, pondéré selon la topologie du cluster.

## Score de placement

```
score(nœud) = w_ram   × ram_score(nœud)
            + w_topo  × topology_score(nœud, source)
            + w_cpu   × cpu_score(nœud)
            + w_migr  × migration_load_score(nœud)
```

- `ram_score`       : RAM libre normalisée (0 → 1)
- `topology_score`  : 1.0 si même rack, 0.7 si même zone, 0.3 si inter-zone
- `cpu_score`       : 1 - cpu_usage (nœud peu chargé = bon)
- `migration_score` : 1 / (1 + migrations_en_cours) (nœud avec peu de migrations = bon)

Le nœud avec le **score le plus élevé** est choisi.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import Optional

from .cluster import ClusterState, NodeInfo, VmEntry
from .placement import (
    PlacementDecision, PlacementEngine, PlacementStrategy,
    NO_PLACEMENT,
)

logger = logging.getLogger(__name__)


# ─── Métadonnées topologiques ─────────────────────────────────────────────────

@dataclass
class NodeTopology:
    """
    Métadonnées topologiques d'un nœud.

    À configurer dans la config du controller (yaml ou env vars).
    Le nœud expose ces infos via /api/status ou la config locale.
    """
    node_id:           str
    rack:              str    = "rack-default"
    zone:              str    = "zone-default"
    # Latence réseau mesurée vers les autres nœuds (node_id → ms)
    latency_ms:        dict[str, float] = field(default_factory=dict)
    # Nombre de migrations en cours vers ce nœud (mis à jour par le daemon)
    active_migrations: int   = 0
    # Usage CPU actuel (0.0 – 1.0)
    cpu_usage:         float = 0.0

    def distance_to(self, other: "NodeTopology") -> str:
        """Retourne 'same_rack', 'same_zone' ou 'cross_zone'."""
        if self.rack == other.rack:
            return "same_rack"
        if self.zone == other.zone:
            return "same_zone"
        return "cross_zone"

    def latency_to(self, other_node_id: str) -> float:
        """Latence vers un nœud (ms). Retourne 10ms si non mesurée."""
        return self.latency_ms.get(other_node_id, 10.0)


# ─── Pondérations ─────────────────────────────────────────────────────────────

@dataclass
class PlacementWeights:
    """Pondérations pour le score multi-critères."""
    ram_weight:       float = 0.50   # RAM libre : critère principal
    topology_weight:  float = 0.25   # Proximité rack/zone
    cpu_weight:       float = 0.15   # Charge CPU du nœud cible
    migration_weight: float = 0.10   # Nombre de migrations en cours

    def validate(self) -> None:
        total = self.ram_weight + self.topology_weight + self.cpu_weight + self.migration_weight
        if abs(total - 1.0) > 0.001:
            raise ValueError(f"La somme des poids doit être 1.0, obtenu {total:.3f}")


# ─── Scores topologiques ──────────────────────────────────────────────────────

TOPOLOGY_SCORES = {
    "same_rack":  1.00,   # Même rack → réseau 10–40 Gbps, latence < 1 ms
    "same_zone":  0.65,   # Même datacenter → réseau 1–10 Gbps, latence < 5 ms
    "cross_zone": 0.20,   # Inter-datacenter → réseau WAN, latence 10–100 ms
}


# ─── Moteur topology-aware ────────────────────────────────────────────────────

class TopologyAwarePlacementEngine:
    """
    Moteur de placement avec score multi-critères topology-aware.

    Compatible avec l'interface de `PlacementEngine` (drop-in replacement).
    """

    def __init__(
        self,
        topology:          dict[str, NodeTopology],   # node_id → NodeTopology
        weights:           PlacementWeights = None,
        safety_margin:     float = 0.15,
        min_remote_pages:  int   = 256,
        max_latency_ms:    float = 50.0,   # Latence max acceptable pour migration
    ):
        self.topology         = topology
        self.weights          = weights or PlacementWeights()
        self.safety_margin    = safety_margin
        self.min_remote_pages = min_remote_pages
        self.max_latency_ms   = max_latency_ms
        self.weights.validate()

    def required_free_mib(self, vm: VmEntry) -> int:
        return int(vm.max_mem_mib * (1.0 + self.safety_margin))

    def _gpu_ok(self, node: NodeInfo, vm: VmEntry) -> bool:
        return (
            vm.gpu_vram_budget_mib == 0
            or (
                node.gpu_enabled
                and node.gpu_free_vram_mib >= vm.gpu_vram_budget_mib
            )
        )

    def score_node(
        self,
        candidate:   NodeInfo,
        source_id:   str,
        vm:          VmEntry,
        all_nodes:   list[NodeInfo],
    ) -> float:
        """
        Calcule le score de placement d'un nœud candidat.

        Retourne un score entre 0.0 (mauvais) et 1.0 (excellent).
        """
        w = self.weights
        required_mib = self.required_free_mib(vm)

        # ── Score RAM ──────────────────────────────────────────────────────
        # Normalisé : quelle fraction de RAM libre au-delà du requis ?
        excess_mib = candidate.mem_free_mib - required_mib
        max_free   = max((n.mem_free_mib for n in all_nodes), default=1)
        ram_score  = min(1.0, excess_mib / max(max_free, 1))
        ram_score  = max(0.0, ram_score)

        # ── Score topologique ──────────────────────────────────────────────
        topo_score = 0.5   # score par défaut si topologie inconnue
        src_topo   = self.topology.get(source_id)
        dst_topo   = self.topology.get(candidate.node_id)

        if src_topo and dst_topo:
            distance   = src_topo.distance_to(dst_topo)
            topo_score = TOPOLOGY_SCORES.get(distance, 0.3)

            # Pénalité si latence mesurée trop élevée
            lat = src_topo.latency_to(candidate.node_id)
            if lat > self.max_latency_ms:
                topo_score *= 0.5
                logger.debug(
                    "nœud %s pénalisé : latence %.0f ms > seuil %.0f ms",
                    candidate.node_id, lat, self.max_latency_ms,
                )

        # ── Score CPU / GPU ────────────────────────────────────────────────
        dst_cpu   = dst_topo.cpu_usage if dst_topo else 0.5
        cpu_score = 1.0 - min(1.0, dst_cpu)
        if vm.gpu_vram_budget_mib > 0:
            gpu_score = (
                candidate.gpu_free_vram_mib / candidate.gpu_total_vram_mib
                if candidate.gpu_total_vram_mib > 0 else 0.0
            )
            cpu_score = 0.5 * cpu_score + 0.5 * gpu_score

        # ── Score charge migrations ────────────────────────────────────────
        active    = dst_topo.active_migrations if dst_topo else 0
        mig_score = 1.0 / (1.0 + active)

        # ── Score total ────────────────────────────────────────────────────
        total = (
            w.ram_weight       * ram_score
          + w.topology_weight  * topo_score
          + w.cpu_weight       * cpu_score
          + w.migration_weight * mig_score
        )

        logger.debug(
            "score nœud %s : ram=%.2f topo=%.2f cpu=%.2f mig=%.2f → total=%.3f",
            candidate.node_id,
            ram_score, topo_score, cpu_score, mig_score, total,
        )

        return total

    def find_target(
        self,
        cluster:     ClusterState,
        source_node: str,
        vm:          VmEntry,
    ) -> Optional[tuple[NodeInfo, float]]:
        """
        Trouve le meilleur nœud cible avec son score.

        Retourne (NodeInfo, score) ou None si aucun candidat.
        """
        required = self.required_free_mib(vm)

        candidates = [
            n for n in cluster.reachable_nodes
            if n.node_id != source_node
            and n.mem_free_mib >= required
            and self._gpu_ok(n, vm)
        ]

        if not candidates:
            return None

        scored = [
            (node, self.score_node(node, source_node, vm, cluster.reachable_nodes))
            for node in candidates
        ]
        scored.sort(key=lambda x: x[1], reverse=True)

        best_node, best_score = scored[0]
        logger.info(
            "meilleur nœud pour vmid=%d : %s (score=%.3f, distance=%s)",
            vm.vmid, best_node.node_id, best_score,
            self.topology.get(source_node, NodeTopology(source_node))
                .distance_to(self.topology.get(best_node.node_id, NodeTopology(best_node.node_id)))
            if source_node in self.topology and best_node.node_id in self.topology
            else "inconnu",
        )

        return best_node, best_score

    def evaluate_migration(
        self,
        cluster:     ClusterState,
        source_node: str,
        vm:          VmEntry,
    ) -> PlacementDecision:
        """Évalue si et où migrer une VM. Compatible avec PlacementEngine."""

        if vm.remote_pages < self.min_remote_pages:
            return PlacementDecision(
                vmid=vm.vmid, source_node=source_node, target_node="",
                target_api_addr="", target_store_addr="",
                vm_max_mem_mib=vm.max_mem_mib, target_free_mib=0,
                confidence=0.0, strategy_used=PlacementStrategy.BEST_FIT,
                reason=f"pages distantes {vm.remote_pages} < seuil {self.min_remote_pages}",
            )

        result = self.find_target(cluster, source_node, vm)

        if result is None:
            return PlacementDecision(
                vmid=vm.vmid, source_node=source_node, target_node="",
                target_api_addr="", target_store_addr="",
                vm_max_mem_mib=vm.max_mem_mib, target_free_mib=0,
                confidence=0.0, strategy_used=PlacementStrategy.BEST_FIT,
                reason="aucun nœud candidat (RAM insuffisante ou topologie rejetée)",
            )

        target, score = result
        src_topo = self.topology.get(source_node, NodeTopology(source_node))
        dst_topo = self.topology.get(target.node_id, NodeTopology(target.node_id))
        distance = src_topo.distance_to(dst_topo)

        return PlacementDecision(
            vmid             = vm.vmid,
            source_node      = source_node,
            target_node      = target.node_id,
            target_api_addr  = target.api_addr,
            target_store_addr= target.store_addr,
            vm_max_mem_mib   = vm.max_mem_mib,
            target_free_mib  = target.mem_free_mib,
            confidence       = round(score, 3),
            strategy_used    = PlacementStrategy.BEST_FIT,
            reason           = (
                f"nœud {target.node_id} : score={score:.3f}, "
                f"distance={distance}, "
                f"RAM={target.mem_free_mib} Mio libre, "
                f"{vm.remote_pages} pages distantes"
            ),
        )

    def find_all_migrations(self, cluster: ClusterState) -> list[PlacementDecision]:
        """Évalue toutes les VMs candidates à la migration."""
        decisions = []
        for source_node, vm in cluster.vms_with_remote_pages(self.min_remote_pages):
            decision = self.evaluate_migration(cluster, source_node, vm)
            if decision.target_node:
                decisions.append(decision)

        decisions.sort(key=lambda d: -d.confidence)
        return decisions
