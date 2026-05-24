"""
Contrôleur d'admission GPU — budget VRAM par VM, slots de commandes.

Modèle :
  - Chaque nœud possède un GPU avec une VRAM totale connue
  - Chaque VM reçoit un budget VRAM (Mio) à sa création
  - Le daemon Rust (gpu_multiplexer) applique ce budget localement
  - Ce contrôleur Python gère la vue cluster : quel nœud a assez de VRAM libre

Invariant :
  sum(vm.vram_budget_mib for vm on node) ≤ node.vram_total_mib × VRAM_OVERCOMMIT_RATIO
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Dict, List, Optional

# Ratio de surcommit VRAM — par défaut on ne surcommite pas (1.0)
# (contrairement à la RAM qui bénéficie du paging, la VRAM n'a pas de mémoire d'appoint)
VRAM_OVERCOMMIT_RATIO: float = 1.0


@dataclass
class VmGpuSpec:
    """Spécification GPU d'une VM à sa création."""
    vmid:            int
    vram_budget_mib: int   # VRAM réservée pour cette VM (0 = pas de GPU)


@dataclass
class NodeGpuCapacity:
    """Capacité GPU d'un nœud."""
    node_id:         str
    vram_total_mib:  int   # VRAM physique totale
    gpu_name:        str = "unknown"
    driver_version:  str = ""
    gpu_util_pct:    float = 0.0  # utilisation GPU courante (0–100)
    vram_used_mib:   int   = 0    # VRAM utilisée par les VMs actives (suivi local)

    @property
    def vram_budget_mib(self) -> int:
        """VRAM utilisable après application du ratio de surcommit."""
        return int(self.vram_total_mib * VRAM_OVERCOMMIT_RATIO)

    @property
    def vram_free_mib(self) -> int:
        return max(0, self.vram_budget_mib - self.vram_used_mib)

    def can_host_vm(self, spec: VmGpuSpec) -> bool:
        return self.vram_free_mib >= spec.vram_budget_mib


@dataclass
class GpuAdmissionDecision:
    """Résultat d'une décision d'admission GPU."""
    admitted:        bool
    vmid:            int
    node_id:         str
    vram_budget_mib: int
    reason:          str = ""

    def admission_payload(self) -> dict:
        """Corps prêt pour POST /control/vm/{vmid}/gpu."""
        return {
            "vmid":            self.vmid,
            "vram_budget_mib": self.vram_budget_mib,
        }


class GpuAdmissionController:
    """
    Admission GPU au niveau cluster.

    Appelé par le controller Python après la décision d'admission RAM.
    Si la VM ne demande pas de GPU (vram_budget_mib=0), la décision est
    immédiatement admitted=True sans occuper de ressource.
    """

    def __init__(self, nodes: Dict[str, NodeGpuCapacity]) -> None:
        self._nodes = nodes
        self._lock  = threading.Lock()
        # vm_id → (node_id, vram_budget_mib) pour pouvoir libérer
        self._allocations: Dict[int, tuple[str, int]] = {}

    # ── Sélection du nœud ────────────────────────────────────────────────────

    def _best_node_for(
        self,
        spec: VmGpuSpec,
        preferred_node: Optional[str] = None,
    ) -> Optional[str]:
        """Nœud avec le plus de VRAM libre pouvant accueillir la VM."""
        if preferred_node and preferred_node in self._nodes:
            if self._nodes[preferred_node].can_host_vm(spec):
                return preferred_node

        candidates = [
            (nid, cap.vram_free_mib)
            for nid, cap in self._nodes.items()
            if cap.can_host_vm(spec)
        ]
        if not candidates:
            return None
        candidates.sort(key=lambda x: -x[1])
        return candidates[0][0]

    # ── Admission ────────────────────────────────────────────────────────────

    def admit(
        self,
        spec: VmGpuSpec,
        preferred_node: Optional[str] = None,
    ) -> GpuAdmissionDecision:
        """
        Admet une VM.  Si vram_budget_mib == 0 → pas de GPU requis → admitted sans
        consommer de ressource (la VM utilisera le rendu logiciel ou sera sans GPU).
        """
        if spec.vram_budget_mib == 0:
            return GpuAdmissionDecision(
                admitted=True,
                vmid=spec.vmid,
                node_id=preferred_node or "",
                vram_budget_mib=0,
                reason="pas de GPU requis",
            )

        with self._lock:
            if spec.vmid in self._allocations:
                return GpuAdmissionDecision(
                    admitted=False,
                    vmid=spec.vmid,
                    node_id="",
                    vram_budget_mib=0,
                    reason=f"VM {spec.vmid} a déjà un GPU alloué",
                )

            target = self._best_node_for(spec, preferred_node)
            if target is None:
                # Calculer la VRAM libre max pour le message d'erreur
                max_free = max(
                    (cap.vram_free_mib for cap in self._nodes.values()),
                    default=0,
                )
                return GpuAdmissionDecision(
                    admitted=False,
                    vmid=spec.vmid,
                    node_id="",
                    vram_budget_mib=0,
                    reason=(
                        f"VRAM insuffisante sur tous les nœuds "
                        f"(max libre : {max_free} Mio, demandé : {spec.vram_budget_mib} Mio)"
                    ),
                )

            self._nodes[target].vram_used_mib += spec.vram_budget_mib
            self._allocations[spec.vmid] = (target, spec.vram_budget_mib)

            return GpuAdmissionDecision(
                admitted=True,
                vmid=spec.vmid,
                node_id=target,
                vram_budget_mib=spec.vram_budget_mib,
                reason="admis",
            )

    def release(self, vmid: int) -> None:
        """Libère le budget VRAM d'une VM."""
        with self._lock:
            alloc = self._allocations.pop(vmid, None)
            if alloc:
                node_id, budget = alloc
                if node_id in self._nodes:
                    self._nodes[node_id].vram_used_mib = max(
                        0, self._nodes[node_id].vram_used_mib - budget
                    )

    # ── Métriques ────────────────────────────────────────────────────────────

    def update_node_capacity(self, node_id: str, cap: NodeGpuCapacity) -> None:
        with self._lock:
            self._nodes[node_id] = cap

    def cluster_snapshot(self) -> List[dict]:
        with self._lock:
            return [
                {
                    "node_id":        nid,
                    "vram_total_mib": cap.vram_total_mib,
                    "vram_used_mib":  cap.vram_used_mib,
                    "vram_free_mib":  cap.vram_free_mib,
                    "gpu_util_pct":   cap.gpu_util_pct,
                    "gpu_name":       cap.gpu_name,
                }
                for nid, cap in self._nodes.items()
            ]

    def allocations_snapshot(self) -> Dict[int, tuple]:
        with self._lock:
            return dict(self._allocations)
