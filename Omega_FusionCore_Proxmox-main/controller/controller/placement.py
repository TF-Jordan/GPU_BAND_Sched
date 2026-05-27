"""
Moteur de placement — trouve le meilleur nœud cible pour migrer une VM.

# Algorithmes disponibles

| Algorithme      | Stratégie                                                        |
|-----------------|------------------------------------------------------------------|
| best_fit        | Nœud avec le moins de RAM libre suffisante (minimise le gaspillage) |
| first_fit       | Premier nœud avec assez de RAM (plus rapide, moins optimal)      |
| most_free       | Nœud avec le plus de RAM libre (maximise l'espace disponible)    |

# Contraintes

- Le nœud cible doit avoir assez de RAM disponible pour héberger la VM
- Le nœud source est exclu (pas de migration sur le même nœud)
- Les nœuds injoignables sont exclus
- Une marge de sécurité est appliquée (défaut : 10%)
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from enum import Enum
from typing import Optional

from .cluster import ClusterState, NodeInfo, VmEntry

logger = logging.getLogger(__name__)


class PlacementStrategy(str, Enum):
    BEST_FIT  = "best_fit"
    FIRST_FIT = "first_fit"
    MOST_FREE = "most_free"


@dataclass
class PlacementDecision:
    """Résultat d'une décision de placement."""
    vmid:            int
    source_node:     str
    target_node:     str
    target_api_addr: str
    target_store_addr: str
    vm_max_mem_mib:  int
    target_free_mib: int
    confidence:      float         # 0.0 – 1.0
    strategy_used:   PlacementStrategy
    reason:          str

    @property
    def feasible(self) -> bool:
        return bool(self.target_node)

    def to_dict(self) -> dict:
        return {
            "vmid":              self.vmid,
            "source_node":       self.source_node,
            "target_node":       self.target_node,
            "target_api_addr":   self.target_api_addr,
            "target_store_addr": self.target_store_addr,
            "vm_max_mem_mib":    self.vm_max_mem_mib,
            "target_free_mib":   self.target_free_mib,
            "confidence":        round(self.confidence, 2),
            "strategy":          self.strategy_used.value,
            "reason":            self.reason,
        }


NO_PLACEMENT = PlacementDecision(
    vmid=0, source_node="", target_node="", target_api_addr="", target_store_addr="",
    vm_max_mem_mib=0, target_free_mib=0, confidence=0.0,
    strategy_used=PlacementStrategy.BEST_FIT,
    reason="aucun nœud cible disponible",
)


class PlacementEngine:
    """
    Moteur de placement de VMs dans le cluster.

    Prend en entrée l'état du cluster et retourne des décisions de migration.
    """

    def __init__(
        self,
        strategy:         PlacementStrategy = PlacementStrategy.BEST_FIT,
        safety_margin:    float             = 0.15,  # 15% de marge en plus de la RAM VM
        min_remote_pages: int               = 256,   # pages distantes min pour déclencher
    ):
        self.strategy         = strategy
        self.safety_margin    = safety_margin
        self.min_remote_pages = min_remote_pages

    def required_free_mib(self, vm: VmEntry) -> int:
        """RAM nécessaire sur le nœud cible (VM max + marge de sécurité)."""
        return int(vm.max_mem_mib * (1.0 + self.safety_margin))

    def _gpu_ok(self, node: NodeInfo, vm: VmEntry) -> bool:
        return (
            vm.gpu_vram_budget_mib == 0
            or (
                node.gpu_enabled
                and node.gpu_free_vram_mib >= vm.gpu_vram_budget_mib
            )
        )

    def find_target(
        self,
        cluster:     ClusterState,
        source_node: str,
        vm:          VmEntry,
    ) -> Optional[NodeInfo]:
        """
        Trouve le meilleur nœud cible pour une VM.

        Exclut le nœud source et les nœuds sans assez de RAM.
        """
        required = self.required_free_mib(vm)

        candidates = [
            n for n in cluster.reachable_nodes
            if n.node_id != source_node
            and n.mem_free_mib >= required
            and self._gpu_ok(n, vm)
        ]

        if not candidates:
            logger.debug(
                "aucun candidat pour vmid=%d (requis=%d Mio, %d nœuds en lice)",
                vm.vmid, required, len(cluster.reachable_nodes),
            )
            return None

        if self.strategy == PlacementStrategy.BEST_FIT:
            # Nœud avec le moins de RAM libre parmi ceux qui en ont assez
            # → minimise le gaspillage (bin packing)
            return min(candidates, key=lambda n: n.mem_free_mib)

        elif self.strategy == PlacementStrategy.FIRST_FIT:
            return candidates[0]

        elif self.strategy == PlacementStrategy.MOST_FREE:
            # Nœud le plus vide → maximise les chances d'accueillir d'autres VMs
            return max(candidates, key=lambda n: n.mem_free_mib)

        return candidates[0]  # fallback

    def evaluate_migration(
        self,
        cluster:     ClusterState,
        source_node: str,
        vm:          VmEntry,
    ) -> PlacementDecision:
        """Évalue si et où migrer une VM. Retourne une PlacementDecision."""

        if vm.remote_pages < self.min_remote_pages:
            return PlacementDecision(
                vmid=vm.vmid, source_node=source_node, target_node="",
                target_api_addr="", target_store_addr="",
                vm_max_mem_mib=vm.max_mem_mib, target_free_mib=0,
                confidence=0.0, strategy_used=self.strategy,
                reason=f"pages distantes {vm.remote_pages} < seuil {self.min_remote_pages}",
            )

        target = self.find_target(cluster, source_node, vm)

        if target is None:
            required = self.required_free_mib(vm)
            best_available = max(
                (n.mem_free_mib for n in cluster.reachable_nodes if n.node_id != source_node),
                default=0,
            )
            return PlacementDecision(
                vmid=vm.vmid, source_node=source_node, target_node="",
                target_api_addr="", target_store_addr="",
                vm_max_mem_mib=vm.max_mem_mib, target_free_mib=best_available,
                confidence=0.0, strategy_used=self.strategy,
                reason=(
                    f"aucun nœud avec assez de RAM : requis {required} Mio, "
                    f"max disponible {best_available} Mio"
                ),
            )

        # Confiance : proportionnelle à la marge disponible au-delà du requis
        required   = self.required_free_mib(vm)
        excess_pct = (target.mem_free_mib - required) / max(required, 1)
        confidence = min(1.0, 0.7 + excess_pct * 0.3)
        gpu_note = (
            f"GPU requis {vm.gpu_vram_budget_mib} Mio, "
            if vm.gpu_vram_budget_mib else ""
        )

        return PlacementDecision(
            vmid             = vm.vmid,
            source_node      = source_node,
            target_node      = target.node_id,
            target_api_addr  = target.api_addr,
            target_store_addr= target.store_addr,
            vm_max_mem_mib   = vm.max_mem_mib,
            target_free_mib  = target.mem_free_mib,
            confidence       = confidence,
            strategy_used    = self.strategy,
            reason           = (
                f"nœud {target.node_id} : {target.mem_free_mib} Mio libre "
                f"(requis {required} Mio, {vm.remote_pages} pages distantes, "
                f"{gpu_note}stratégie {self.strategy.value})"
            ),
        )

    def find_all_migrations(self, cluster: ClusterState) -> list[PlacementDecision]:
        """
        Évalue toutes les VMs du cluster et retourne les décisions de migration.

        Retourne uniquement les décisions avec un target_node non vide
        (i.e., où la migration est réalisable).
        """
        decisions = []
        for source_node, vm in cluster.vms_with_remote_pages(self.min_remote_pages):
            decision = self.evaluate_migration(cluster, source_node, vm)
            if decision.target_node:
                decisions.append(decision)
                logger.info(
                    "migration possible : vmid=%d %s→%s (%d pages distantes, confiance %.2f)",
                    vm.vmid, source_node, decision.target_node,
                    vm.remote_pages, decision.confidence,
                )
            else:
                logger.debug(
                    "migration impossible : vmid=%d sur %s — %s",
                    vm.vmid, source_node, decision.reason,
                )

        # Tri par priorité : confiance desc, puis taille des pages distantes desc
        decisions.sort(key=lambda d: (-d.confidence, -d.vm_max_mem_mib))
        return decisions
