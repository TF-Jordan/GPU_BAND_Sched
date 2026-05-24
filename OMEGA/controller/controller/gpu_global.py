"""
Planification GPU globale Omega.

Objectif:
  1. Utiliser le GPU local si la VM est deja sur un noeud GPU sain.
  2. Migrer la VM vers le meilleur noeud GPU si elle n'est pas bien placee.
  3. Basculer vers le proxy GPU reseau si la migration n'est pas possible.

Cette couche est volontairement deterministe: une meme vue cluster donne le
meme plan, ce qui rend les tests et les diagnostics exploitables.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Dict, Optional

from .migration_policy import NodeState, VmState


class GpuPlacementAction(str, Enum):
    LOCAL_GPU = "local_gpu"
    MIGRATE_TO_GPU = "migrate_to_gpu"
    REMOTE_PROXY = "remote_proxy"
    REJECT = "reject"


@dataclass(frozen=True)
class GpuPlacementDecision:
    action: GpuPlacementAction
    source_node: str
    target_node: str = ""
    proxmox_target: str = ""
    proxy_url: str = ""
    reason: str = ""

    @property
    def needs_migration(self) -> bool:
        return self.action == GpuPlacementAction.MIGRATE_TO_GPU

    @property
    def uses_network_proxy(self) -> bool:
        return self.action == GpuPlacementAction.REMOTE_PROXY


def choose_gpu_placement(
    *,
    source_node: str,
    vm: VmState,
    node_states: Dict[str, NodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
    proxy_port: int = 9400,
    fallback_network: bool = True,
) -> GpuPlacementDecision:
    """Retourne le plan GPU global pour une VM."""
    if gpu_budget_mib <= 0:
        return GpuPlacementDecision(
            action=GpuPlacementAction.REJECT,
            source_node=source_node,
            reason="aucun budget GPU demande",
        )

    source = node_states.get(source_node)
    if source is None:
        return GpuPlacementDecision(
            action=GpuPlacementAction.REJECT,
            source_node=source_node,
            reason="source inconnue dans l'etat cluster",
        )

    if _node_has_gpu_capacity(source, gpu_budget_mib):
        return GpuPlacementDecision(
            action=GpuPlacementAction.LOCAL_GPU,
            source_node=source_node,
            target_node=source_node,
            proxmox_target=source.proxmox_node_name or source_node,
            proxy_url=_proxy_url(source, source_node, proxy_port),
            reason="VM deja sur un noeud GPU avec VRAM disponible",
        )

    migration_target = _best_gpu_migration_target(
        source_node=source_node,
        vm=vm,
        node_states=node_states,
        required_vcpus=required_vcpus,
        gpu_budget_mib=gpu_budget_mib,
    )
    if migration_target is not None:
        target = node_states[migration_target]
        return GpuPlacementDecision(
            action=GpuPlacementAction.MIGRATE_TO_GPU,
            source_node=source_node,
            target_node=migration_target,
            proxmox_target=target.proxmox_node_name or migration_target,
            proxy_url=_proxy_url(target, migration_target, proxy_port),
            reason="meilleur noeud GPU capable d'heberger la VM",
        )

    if fallback_network:
        proxy_target = _best_gpu_proxy_target(
            node_states=node_states,
            gpu_budget_mib=gpu_budget_mib,
        )
        if proxy_target is not None:
            target = node_states[proxy_target]
            return GpuPlacementDecision(
                action=GpuPlacementAction.REMOTE_PROXY,
                source_node=source_node,
                target_node=proxy_target,
                proxmox_target=target.proxmox_node_name or proxy_target,
                proxy_url=_proxy_url(target, proxy_target, proxy_port),
                reason="aucune migration GPU possible; fallback proxy reseau",
            )

    return GpuPlacementDecision(
        action=GpuPlacementAction.REJECT,
        source_node=source_node,
        reason="aucun noeud GPU utilisable et fallback reseau indisponible",
    )


def _node_has_gpu_capacity(node: NodeState, gpu_budget_mib: int) -> bool:
    return node.gpu_total_vram_mib > 0 and node.gpu_free_vram_mib >= gpu_budget_mib


def _node_can_host_vm(
    node: NodeState,
    vm: VmState,
    required_vcpus: int,
    gpu_budget_mib: int,
) -> bool:
    if not _node_has_gpu_capacity(node, gpu_budget_mib):
        return False
    if node.vcpu_free < required_vcpus:
        return False
    if vm.max_mem_mib > 0 and node.mem_available_kb < vm.max_mem_mib * 1024:
        return False
    return True


def _best_gpu_migration_target(
    *,
    source_node: str,
    vm: VmState,
    node_states: Dict[str, NodeState],
    required_vcpus: int,
    gpu_budget_mib: int,
) -> Optional[str]:
    candidates = [
        node
        for node_id, node in node_states.items()
        if node_id != source_node
        and _node_can_host_vm(node, vm, required_vcpus, gpu_budget_mib)
    ]
    if not candidates:
        return None

    # Score intelligent: ne choisit pas juste "plus de VRAM". On penalise les
    # noeuds en pression disque et on garde un leger biais de consolidation pour
    # eviter d'eparpiller les VMs quand plusieurs cibles sont equivalentes.
    candidates.sort(
        key=lambda node: _gpu_node_score(node, vm, required_vcpus, gpu_budget_mib),
        reverse=True,
    )
    return candidates[0].node_id


def _best_gpu_proxy_target(
    *,
    node_states: Dict[str, NodeState],
    gpu_budget_mib: int,
) -> Optional[str]:
    candidates = [
        node
        for node in node_states.values()
        if _node_has_gpu_capacity(node, gpu_budget_mib)
    ]
    if not candidates:
        return None
    candidates.sort(
        key=lambda node: _gpu_node_score(
            node,
            vm=None,
            required_vcpus=0,
            gpu_budget_mib=gpu_budget_mib,
        ),
        reverse=True,
    )
    return candidates[0].node_id


def _gpu_node_score(
    node: NodeState,
    vm: Optional[VmState],
    required_vcpus: int,
    gpu_budget_mib: int,
) -> float:
    gpu_free_ratio = (
        node.gpu_free_vram_mib / node.gpu_total_vram_mib
        if node.gpu_total_vram_mib > 0 else 0.0
    )
    ram_free_ratio = (
        node.mem_available_kb / node.mem_total_kb
        if node.mem_total_kb > 0 else 0.0
    )
    vcpu_free_ratio = (
        node.vcpu_free / node.vcpu_total
        if node.vcpu_total > 0 else 0.0
    )
    disk_free_ratio = max(0.0, 1.0 - node.disk_pressure_pct / 100.0)
    consolidation_ratio = min(len(node.local_vms), 12) / 12.0

    # Reserve fit: une cible qui a juste assez de VRAM reste possible, mais elle
    # passe derriere une cible qui garde une marge exploitable pour d'autres VMs.
    reserve_ratio = (
        max(0, node.gpu_free_vram_mib - gpu_budget_mib) / node.gpu_total_vram_mib
        if node.gpu_total_vram_mib > 0 else 0.0
    )
    capacity_fit = 1.0
    if vm is not None:
        if vm.max_mem_mib > 0 and node.mem_available_kb < vm.max_mem_mib * 1024:
            capacity_fit -= 0.5
        if required_vcpus > 0 and node.vcpu_free < required_vcpus:
            capacity_fit -= 0.5

    return (
        gpu_free_ratio * 0.30
        + reserve_ratio * 0.25
        + ram_free_ratio * 0.18
        + vcpu_free_ratio * 0.12
        + disk_free_ratio * 0.15
        + consolidation_ratio * 0.05
        + capacity_fit
    )


def _proxy_url(node: NodeState, fallback_node_id: str, proxy_port: int) -> str:
    host = node.proxmox_node_name or fallback_node_id
    return f"http://{host}:{proxy_port}"
