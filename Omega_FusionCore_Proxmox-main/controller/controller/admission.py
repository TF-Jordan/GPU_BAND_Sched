"""
Contrôleur d'admission cluster-wide — garantie de non-dépassement mémoire.

# Problème résolu

Sans contrôle d'admission, on pouvait créer une VM de 10 Gio sur un nœud
qui n'avait que 8 Gio de libre, espérant que les 2 Gio manquants seraient
compensés par du paging distant. Le problème : rien ne garantissait que
ces 2 Gio étaient disponibles sur d'autres nœuds, et rien n'empêchait
la VM de consommer plus que ses 10 Gio demandés.

# Invariant garanti

Pour toute VM v admise dans le cluster :

  local_pages(v) + remote_pages(v) ≤ max_mem_pages(v)

Avec :
  - local_pages(v)  ≤ local_budget(v)   (alloué sur le nœud hôte)
  - remote_pages(v) ≤ remote_budget(v)  (enforced par QuotaRegistry côté Rust)
  - local_budget(v) + remote_budget(v) = max_mem(v)

# Algorithme d'admission

1. Calculer `cluster_free = Σ node.mem_free_mib` pour tous les nœuds joignables
2. Si `cluster_free < vm.max_mem_mib` → REFUS (capacité cluster insuffisante)
3. Trouver le nœud avec le plus de RAM libre (topology-aware si config dispo)
4. `local_budget = min(best_node.mem_free_mib, vm.max_mem_mib)`
5. `remote_budget = vm.max_mem_mib - local_budget`
6. Vérifier que le remote_budget est distribuable sur les autres nœuds
7. Retourner `AdmissionDecision` avec le placement et les budgets

# Intégration

Après admission :
  - Le controller pousse le quota via `POST /control/vm/{vmid}/quota`
  - Le daemon store refuse tout PUT_PAGE qui dépasserait le quota remote
  - Le hook Proxmox post-start configure le quota automatiquement
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import Optional

from .cluster import ClusterState, NodeInfo, VmEntry
from .rust_policy import call_policy

logger = logging.getLogger(__name__)


# ─── Spécification d'une VM à admettre ───────────────────────────────────────

@dataclass
class VmSpec:
    """Caractéristiques d'une VM à créer ou migrer."""
    vmid:         int
    max_mem_mib:  int    # RAM demandée (ce que l'utilisateur a configuré)
    name:         str    = ""
    vcpus:        int    = 1
    # Contraintes optionnelles
    preferred_node: Optional[str] = None   # nœud préféré (affinité)
    forbidden_nodes: list[str]    = field(default_factory=list)


# ─── Décision d'admission ─────────────────────────────────────────────────────

@dataclass
class AdmissionDecision:
    """Résultat d'une décision d'admission."""
    admitted:          bool
    vmid:              int
    max_mem_mib:       int

    # Si admitted=True :
    placement_node:    str   = ""    # nœud où créer la VM
    local_budget_mib:  int   = 0     # RAM locale allouée sur ce nœud
    remote_budget_mib: int   = 0     # RAM distante max autorisée

    # Nœuds qui contribuent à la RAM distante (pour information)
    remote_nodes:      list[str] = field(default_factory=list)

    # Si admitted=False :
    reason:            str   = ""

    # Métriques
    cluster_free_mib:  int   = 0
    evaluated_at:      float = field(default_factory=time.time)

    @property
    def requires_remote(self) -> bool:
        """La VM a besoin de pages distantes (aucun nœud n'a assez seul)."""
        return self.remote_budget_mib > 0

    def to_dict(self) -> dict:
        return {
            "admitted":          self.admitted,
            "vmid":              self.vmid,
            "max_mem_mib":       self.max_mem_mib,
            "placement_node":    self.placement_node,
            "local_budget_mib":  self.local_budget_mib,
            "remote_budget_mib": self.remote_budget_mib,
            "remote_nodes":      self.remote_nodes,
            "requires_remote":   self.requires_remote,
            "cluster_free_mib":  self.cluster_free_mib,
            "reason":            self.reason,
        }

    def quota_payload(self) -> dict:
        """Payload à envoyer à `POST /control/vm/{vmid}/quota`."""
        return {
            "vm_id":             self.vmid,
            "max_mem_mib":       self.max_mem_mib,
            "local_budget_mib":  self.local_budget_mib,
            "remote_budget_mib": self.remote_budget_mib,
        }


# ─── Admission Controller ─────────────────────────────────────────────────────

class AdmissionController:
    """
    Contrôleur d'admission de VMs dans le cluster.

    Garantit que :
    1. La VM ne sera créée que si le cluster a assez de RAM totale
    2. Le budget local + remote = max_mem (jamais plus)
    3. La RAM distante est disponible sur les nœuds restants
    """

    def __init__(
        self,
        safety_margin:       float = 0.10,   # 10% de marge sur chaque nœud
        prefer_local:        bool  = True,   # maximiser la RAM locale (réduire remote)
        min_remote_node_free: int  = 512,    # Mio minimum libre sur un nœud pour contribuer au remote
    ):
        self.safety_margin         = safety_margin
        self.prefer_local          = prefer_local
        self.min_remote_node_free  = min_remote_node_free

    def _effective_free(self, node: NodeInfo) -> int:
        """RAM libre effective d'un nœud (avec marge de sécurité)."""
        return int(node.mem_free_mib * (1.0 - self.safety_margin))

    def admit(self, cluster: ClusterState, vm: VmSpec) -> AdmissionDecision:
        """
        Décide si une VM peut être admise dans le cluster.

        Retourne une `AdmissionDecision` avec le placement et les budgets.
        """
        rust_decision = call_policy(
            "admit",
            {
                "config": {
                    "safety_margin": self.safety_margin,
                    "prefer_local": self.prefer_local,
                    "min_remote_node_free": self.min_remote_node_free,
                },
                "cluster": {
                    "nodes": [
                        {
                            "node_id": node.node_id,
                            "mem_total_kb": node.mem_total_kb,
                            "mem_available_kb": node.mem_available_kb,
                            "reachable": node.reachable,
                            "local_vms": [
                                {
                                    "vmid": entry.vmid,
                                    "max_mem_mib": entry.max_mem_mib,
                                }
                                for entry in node.local_vms
                            ],
                        }
                        for node in cluster.nodes
                    ]
                },
                "vm": {
                    "vmid": vm.vmid,
                    "max_mem_mib": vm.max_mem_mib,
                    "name": vm.name,
                    "vcpus": vm.vcpus,
                    "preferred_node": vm.preferred_node,
                    "forbidden_nodes": vm.forbidden_nodes,
                },
            },
        )
        if rust_decision is not None:
            return AdmissionDecision(**rust_decision)

        reachable = cluster.reachable_nodes
        if not reachable:
            return AdmissionDecision(
                admitted=False, vmid=vm.vmid, max_mem_mib=vm.max_mem_mib,
                reason="aucun nœud joignable dans le cluster",
            )

        # Exclure les nœuds interdits par la spécification
        candidates = [n for n in reachable if n.node_id not in vm.forbidden_nodes]
        if not candidates:
            return AdmissionDecision(
                admitted=False, vmid=vm.vmid, max_mem_mib=vm.max_mem_mib,
                reason="tous les nœuds sont dans la liste forbidden_nodes",
            )

        # ── 1. Capacité cluster totale ──────────────────────────────────────
        cluster_free = sum(self._effective_free(n) for n in candidates)

        logger.info(
            "admission vmid=%d max_mem=%d Mio, cluster_free=%d Mio (%d nœuds)",
            vm.vmid, vm.max_mem_mib, cluster_free, len(candidates),
        )

        if cluster_free < vm.max_mem_mib:
            return AdmissionDecision(
                admitted         = False,
                vmid             = vm.vmid,
                max_mem_mib      = vm.max_mem_mib,
                cluster_free_mib = cluster_free,
                reason           = (
                    f"capacité cluster insuffisante : {cluster_free} Mio dispo "
                    f"< {vm.max_mem_mib} Mio demandé — "
                    f"ajouter de la RAM ou éteindre des VMs"
                ),
            )

        # ── 2. Choisir le nœud de placement ────────────────────────────────
        placement_node = self._select_placement_node(candidates, vm)

        local_budget  = min(self._effective_free(placement_node), vm.max_mem_mib)
        remote_budget = vm.max_mem_mib - local_budget

        logger.info(
            "admission vmid=%d → nœud=%s, local=%d Mio, remote=%d Mio",
            vm.vmid, placement_node.node_id, local_budget, remote_budget,
        )

        # ── 3. Vérifier la disponibilité du budget remote ───────────────────
        remote_nodes: list[str] = []
        if remote_budget > 0:
            remote_ok, remote_nodes = self._check_remote_availability(
                candidates, placement_node.node_id, remote_budget,
            )
            if not remote_ok:
                return AdmissionDecision(
                    admitted         = False,
                    vmid             = vm.vmid,
                    max_mem_mib      = vm.max_mem_mib,
                    cluster_free_mib = cluster_free,
                    reason           = (
                        f"nœud {placement_node.node_id} a seulement {local_budget} Mio "
                        f"(besoin de {vm.max_mem_mib} Mio), et pas assez de RAM distante "
                        f"disponible sur les autres nœuds pour les {remote_budget} Mio restants"
                    ),
                )

        return AdmissionDecision(
            admitted          = True,
            vmid              = vm.vmid,
            max_mem_mib       = vm.max_mem_mib,
            placement_node    = placement_node.node_id,
            local_budget_mib  = local_budget,
            remote_budget_mib = remote_budget,
            remote_nodes      = remote_nodes,
            cluster_free_mib  = cluster_free,
            reason            = (
                f"admis sur {placement_node.node_id} "
                f"({local_budget} Mio local"
                + (f" + {remote_budget} Mio remote sur {remote_nodes}" if remote_budget else "")
                + ")"
            ),
        )

    def _select_placement_node(self, candidates: list[NodeInfo], vm: VmSpec) -> NodeInfo:
        """
        Sélectionne le meilleur nœud de placement.

        Priorités :
        1. Le nœud préféré (si configuré et s'il a assez de RAM)
        2. Le nœud qui peut accueillir toute la VM localement (préféré si `prefer_local`)
        3. Le nœud avec le plus de RAM libre
        """
        # Nœud préféré (affinité explicite)
        if vm.preferred_node:
            preferred = next(
                (n for n in candidates if n.node_id == vm.preferred_node
                 and self._effective_free(n) >= vm.max_mem_mib * 0.5),
                None,
            )
            if preferred:
                logger.debug("placement vmid=%d : nœud préféré %s", vm.vmid, vm.preferred_node)
                return preferred

        # Nœuds capables d'accueillir la VM entièrement en local
        full_capacity = [
            n for n in candidates
            if self._effective_free(n) >= vm.max_mem_mib
        ]

        if full_capacity and self.prefer_local:
            # Parmi ceux qui peuvent tout accueillir, prendre le plus chargé
            # (bin packing — minimise la fragmentation)
            selected = min(full_capacity, key=lambda n: self._effective_free(n))
            logger.debug(
                "placement vmid=%d : %s peut tout accueillir localement (%d Mio)",
                vm.vmid, selected.node_id, self._effective_free(selected),
            )
            return selected

        # Sinon, prendre le nœud avec le plus de RAM libre (pour minimiser le remote_budget)
        selected = max(candidates, key=lambda n: self._effective_free(n))
        logger.debug(
            "placement vmid=%d : %s (plus de RAM libre, %d Mio)",
            vm.vmid, selected.node_id, self._effective_free(selected),
        )
        return selected

    def _check_remote_availability(
        self,
        candidates:     list[NodeInfo],
        placement_id:   str,
        remote_budget:  int,
    ) -> tuple[bool, list[str]]:
        """
        Vérifie que les nœuds restants peuvent absorber le budget remote.

        Retourne (ok, liste des nœuds contributeurs).
        """
        others = [
            n for n in candidates
            if n.node_id != placement_id
            and self._effective_free(n) >= self.min_remote_node_free
        ]

        if not others:
            return False, []

        # Somme de la RAM disponible sur les nœuds restants
        others_free = sum(self._effective_free(n) for n in others)

        if others_free < remote_budget:
            logger.warning(
                "budget remote %d Mio > RAM disponible autres nœuds %d Mio",
                remote_budget, others_free,
            )
            return False, []

        # Nœuds qui contribueront (triés par RAM libre desc)
        contributors = sorted(others, key=lambda n: self._effective_free(n), reverse=True)
        return True, [n.node_id for n in contributors]

    # ─── Batch admission (plusieurs VMs à la fois) ────────────────────────

    def admit_batch(
        self,
        cluster:  ClusterState,
        vms:      list[VmSpec],
    ) -> list[AdmissionDecision]:
        """
        Évalue l'admission de plusieurs VMs en tenant compte des réservations mutuelles.

        Les VMs sont évaluées dans l'ordre : une VM admise "réserve" sa RAM,
        ce qui réduit la capacité disponible pour les suivantes.
        """
        rust_decisions = call_policy(
            "admit-batch",
            {
                "config": {
                    "safety_margin": self.safety_margin,
                    "prefer_local": self.prefer_local,
                    "min_remote_node_free": self.min_remote_node_free,
                },
                "cluster": {
                    "nodes": [
                        {
                            "node_id": node.node_id,
                            "mem_total_kb": node.mem_total_kb,
                            "mem_available_kb": node.mem_available_kb,
                            "reachable": node.reachable,
                            "local_vms": [
                                {
                                    "vmid": entry.vmid,
                                    "max_mem_mib": entry.max_mem_mib,
                                }
                                for entry in node.local_vms
                            ],
                        }
                        for node in cluster.nodes
                    ]
                },
                "vms": [
                    {
                        "vmid": vm.vmid,
                        "max_mem_mib": vm.max_mem_mib,
                        "name": vm.name,
                        "vcpus": vm.vcpus,
                        "preferred_node": vm.preferred_node,
                        "forbidden_nodes": vm.forbidden_nodes,
                    }
                    for vm in vms
                ],
            },
        )
        if rust_decisions is not None:
            return [AdmissionDecision(**decision) for decision in rust_decisions]

        decisions   = []
        # Simule les réservations en mémoire (sans modifier le cluster réel)
        reservations: dict[str, int] = {}  # node_id → Mio réservé

        for vm in vms:
            # Construire un cluster virtuel avec les réservations appliquées
            virtual_cluster = self._apply_reservations(cluster, reservations)
            decision        = self.admit(virtual_cluster, vm)
            decisions.append(decision)

            if decision.admitted:
                # Réserver la RAM sur le nœud de placement
                node_id = decision.placement_node
                reservations[node_id] = (
                    reservations.get(node_id, 0) + decision.local_budget_mib
                )
                logger.debug(
                    "réservation vmid=%d sur %s : +%d Mio (total %d Mio réservé)",
                    vm.vmid, node_id, decision.local_budget_mib,
                    reservations[node_id],
                )

        return decisions

    def _apply_reservations(
        self,
        cluster:      ClusterState,
        reservations: dict[str, int],
    ) -> ClusterState:
        """Construit un cluster virtuel avec les réservations déduites de la RAM libre."""
        from dataclasses import replace
        from .cluster import ClusterState as CS

        modified_nodes = []
        for node in cluster.nodes:
            reserved = reservations.get(node.node_id, 0)
            if reserved > 0:
                new_free_kb = max(0, node.mem_available_kb - reserved * 1024)
                modified_nodes.append(replace(node, mem_available_kb=new_free_kb))
            else:
                modified_nodes.append(node)

        return CS(nodes=modified_nodes)
