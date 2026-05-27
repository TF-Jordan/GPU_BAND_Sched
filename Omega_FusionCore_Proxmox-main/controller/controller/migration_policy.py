"""
Politique de migration de VMs — décide quand migrer, vers où, et comment.

# Rôle dans l'architecture

Le controller Python a une vue globale du cluster (il parle à tous les nœuds).
C'est lui qui :
  1. Collecte l'état de chaque nœud via GET /control/status
  2. Décide quel nœud est surchargé et lequel peut accueillir une VM
  3. Choisit le type de migration (live ou cold)
  4. Appelle POST /control/migrate sur le nœud source

Le daemon Rust reçoit l'ordre et exécute `qm migrate`.

# Règles de décision live vs cold

  ┌──────────────────────┬─────────────────────┬───────────────────┐
  │ État VM              │ Pression nœud       │ Type              │
  ├──────────────────────┼─────────────────────┼───────────────────┤
  │ Stopped              │ peu importe         │ COLD              │
  │ Running, CPU > 5%    │ RAM > 85%           │ LIVE              │
  │ Running, CPU < 5%    │ RAM > 85% (> 60s)  │ COLD              │
  │ Running, CPU > 5%    │ RAM > 95% (critique)│ COLD (urgence)    │
  │ Running, throttlé    │ vCPU saturé         │ LIVE              │
  └──────────────────────┴─────────────────────┴───────────────────┘

# Choix de la cible

  La cible est le nœud avec le meilleur score composite. Le score tient compte
  de la RAM, du CPU, de la pression disque et, pour les VMs GPU, de la VRAM.
  On n'envoie jamais vers un nœud qui ne peut pas accepter la VM.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, List, Optional, Tuple

from .rust_policy import call_policy

# ─── Types ────────────────────────────────────────────────────────────────────

class MigrationType(str, Enum):
    LIVE = "live"   # VM reste allumée, KVM pre-copy
    COLD = "cold"   # VM stoppée, transférée, redémarrée


class MigrationReason(str, Enum):
    MEMORY_PRESSURE      = "memory_pressure"
    CPU_SATURATION       = "cpu_saturation"
    DISK_SATURATION      = "disk_saturation"
    GPU_SATURATION       = "gpu_saturation"
    EXCESSIVE_REMOTE_PAGING = "excessive_remote_paging"
    MAINTENANCE_DRAIN    = "maintenance_drain"
    ADMIN_REQUEST        = "admin_request"


@dataclass
class NodeState:
    """Snapshot de l'état d'un nœud tel que retourné par GET /control/status."""
    node_id:          str
    mem_total_kb:     int
    mem_available_kb: int
    proxmox_node_name: str     = ""
    vcpu_total:       int       = 24
    vcpu_free:        int       = 24
    gpu_total_vram_mib: int     = 0
    gpu_free_vram_mib: int      = 0
    disk_pressure_pct: float    = 0.0
    local_vms:        List[VmState] = field(default_factory=list)

    @property
    def ram_used_pct(self) -> float:
        if self.mem_total_kb == 0:
            return 0.0
        return (self.mem_total_kb - self.mem_available_kb) / self.mem_total_kb * 100.0

    @property
    def vcpu_used_pct(self) -> float:
        if self.vcpu_total == 0:
            return 0.0
        return (self.vcpu_total - self.vcpu_free) / self.vcpu_total * 100.0

    def placement_score(self) -> float:
        """Score de capacité d'accueil : plus haut = meilleur nœud cible."""
        ram_free_ratio  = 1.0 - self.ram_used_pct  / 100.0
        vcpu_free_ratio = 1.0 - self.vcpu_used_pct / 100.0
        disk_free_ratio = 1.0 - self.disk_pressure_pct / 100.0
        return ram_free_ratio * 0.5 + vcpu_free_ratio * 0.3 + disk_free_ratio * 0.2

    @property
    def gpu_used_pct(self) -> float:
        if self.gpu_total_vram_mib == 0:
            return 0.0
        return (
            (self.gpu_total_vram_mib - self.gpu_free_vram_mib)
            / self.gpu_total_vram_mib
            * 100.0
        )

    def can_accept_vm(self, vm: "VmState") -> bool:
        """Vérifie si ce nœud peut accueillir la VM sans dépasser 80%."""
        vm_ram_kb = vm.max_mem_mib * 1024
        new_avail = self.mem_available_kb - vm_ram_kb
        new_avail_pct = new_avail / self.mem_total_kb * 100.0 if self.mem_total_kb else 0.0
        ram_ok  = new_avail_pct >= 20.0  # garder 20% libres
        vcpu_ok = self.vcpu_used_pct < 80.0
        disk_ok = self.disk_pressure_pct < 15.0
        gpu_ok = (
            vm.gpu_vram_budget_mib == 0
            or (
                self.gpu_total_vram_mib > 0
                and self.gpu_free_vram_mib >= vm.gpu_vram_budget_mib
            )
        )
        return ram_ok and vcpu_ok and disk_ok and gpu_ok


@dataclass
class VmState:
    """État d'une VM locale sur un nœud."""
    vm_id:          int
    status:         str          # "running" | "stopped" | "unknown"
    max_mem_mib:    int
    rss_kb:         int          = 0
    remote_pages:   int          = 0
    avg_cpu_pct:    float        = 0.0
    throttle_ratio: float        = 0.0
    gpu_vram_budget_mib: int     = 0
    disk_read_bps: float         = 0.0
    disk_write_bps: float        = 0.0
    disk_io_weight: int          = 100
    disk_local_share_active: bool = False
    idle_since:     Optional[float] = None   # timestamp monotonic depuis quand idle

    @property
    def is_running(self) -> bool:
        return self.status == "running"

    @property
    def is_stopped(self) -> bool:
        return self.status == "stopped"

    @property
    def remote_pct(self) -> float:
        total_pages = self.max_mem_mib * 256  # pages 4Ko dans max_mem_mib
        if total_pages == 0:
            return 0.0
        return self.remote_pages / total_pages * 100.0

    @property
    def disk_total_bps(self) -> float:
        return self.disk_read_bps + self.disk_write_bps

    def idle_duration_secs(self) -> float:
        if self.idle_since is None:
            return 0.0
        return time.monotonic() - self.idle_since


@dataclass
class MigrationCandidate:
    """Résultat d'évaluation : une VM à migrer avec sa cible et son type."""
    vm:        VmState
    source:    str
    target:    str
    mtype:     MigrationType
    reason:    MigrationReason
    urgency:   int          = 0   # 0 = faible, 1 = normale, 2 = urgente
    detail:    str          = ""

    def to_api_payload(self) -> dict:
        return {
            "vm_id":  self.vm.vm_id,
            "source": self.source,
            "target": self.target,
            "type":   self.mtype.value,
            "reason": self.reason.value,
            "detail": self.detail,
        }


# ─── Seuils ───────────────────────────────────────────────────────────────────

@dataclass
class MigrationThresholds:
    # RAM
    ram_high_pct:      float = 85.0   # pression haute → live migration
    ram_critical_pct:  float = 95.0   # pression critique → cold forcée

    # vCPU
    vcpu_throttle_trigger: float = 0.30   # throttle > 30% → migrer
    vcpu_saturation_pct:   float = 90.0   # nœud > 90% utilisé → migrer
    disk_high_pct:         float = 10.0   # PSI I/O avg10 > 10% → disque tendu
    disk_hot_vm_bps:       float = 8.0 * 1024.0 * 1024.0  # VM très active en disque

    # Remote paging
    remote_paging_pct: float = 60.0   # > 60% de la RAM en distant → migrer

    # GPU
    gpu_high_pct: float = 90.0

    # Idle detection
    idle_cpu_pct:        float = 5.0    # < 5% CPU → VM idle
    idle_duration_secs:  float = 60.0   # idle depuis > 60s → cold acceptable

    # Cible
    target_max_ram_pct:  float = 80.0   # nœud cible doit avoir < 80% RAM utilisée
    target_max_vcpu_pct: float = 80.0   # nœud cible doit avoir < 80% vCPU utilisés


# ─── Politique principale ─────────────────────────────────────────────────────

class MigrationPolicy:
    """
    Évalue l'état du cluster et retourne des recommandations de migration.

    Utilisation typique :
        policy = MigrationPolicy(thresholds)
        candidates = policy.evaluate(node_states)
        for c in candidates[:2]:              # exécuter max 2 migrations simultanées
            daemon_api.post(c.source, "/control/migrate", c.to_api_payload())
    """

    def __init__(self, thresholds: MigrationThresholds = MigrationThresholds()) -> None:
        self.thresholds = thresholds

    def evaluate(
        self,
        node_states: Dict[str, NodeState],
    ) -> List[MigrationCandidate]:
        """
        Analyse tous les nœuds et retourne les migrations recommandées,
        triées par urgence décroissante.
        """
        rust_candidates = call_policy(
            "evaluate-migrations",
            {
                "thresholds": {
                    "ram_high_pct": self.thresholds.ram_high_pct,
                    "ram_critical_pct": self.thresholds.ram_critical_pct,
                    "vcpu_throttle_trigger": self.thresholds.vcpu_throttle_trigger,
                    "vcpu_saturation_pct": self.thresholds.vcpu_saturation_pct,
                    "remote_paging_pct": self.thresholds.remote_paging_pct,
                    "gpu_high_pct": self.thresholds.gpu_high_pct,
                    "disk_high_pct": self.thresholds.disk_high_pct,
                    "disk_hot_vm_bps": self.thresholds.disk_hot_vm_bps,
                    "idle_cpu_pct": self.thresholds.idle_cpu_pct,
                    "idle_duration_secs": self.thresholds.idle_duration_secs,
                    "target_max_ram_pct": self.thresholds.target_max_ram_pct,
                    "target_max_vcpu_pct": self.thresholds.target_max_vcpu_pct,
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
                        "disk_pressure_pct": node.disk_pressure_pct,
                        "local_vms": [
                            {
                                "vm_id": vm.vm_id,
                                "status": vm.status,
                                "max_mem_mib": vm.max_mem_mib,
                                "rss_kb": vm.rss_kb,
                                "remote_pages": vm.remote_pages,
                                "avg_cpu_pct": vm.avg_cpu_pct,
                                "throttle_ratio": vm.throttle_ratio,
                                "gpu_vram_budget_mib": vm.gpu_vram_budget_mib,
                                "disk_read_bps": vm.disk_read_bps,
                                "disk_write_bps": vm.disk_write_bps,
                                "disk_io_weight": vm.disk_io_weight,
                                "disk_local_share_active": vm.disk_local_share_active,
                                "idle_duration_secs": vm.idle_duration_secs() if vm.idle_since is not None else None,
                            }
                            for vm in node.local_vms
                        ],
                    }
                    for node in node_states.values()
                ],
            },
        )
        if rust_candidates is not None:
            return [
                MigrationCandidate(
                    vm=VmState(
                        vm_id=item["vm"]["vm_id"],
                        status=item["vm"]["status"],
                        max_mem_mib=item["vm"]["max_mem_mib"],
                        rss_kb=item["vm"].get("rss_kb", 0),
                        remote_pages=item["vm"].get("remote_pages", 0),
                        avg_cpu_pct=item["vm"].get("avg_cpu_pct", 0.0),
                        throttle_ratio=item["vm"].get("throttle_ratio", 0.0),
                        gpu_vram_budget_mib=item["vm"].get("gpu_vram_budget_mib", 0),
                        disk_read_bps=item["vm"].get("disk_read_bps", 0.0),
                        disk_write_bps=item["vm"].get("disk_write_bps", 0.0),
                        disk_io_weight=item["vm"].get("disk_io_weight", 100),
                        disk_local_share_active=item["vm"].get("disk_local_share_active", False),
                    ),
                    source=item["source"],
                    target=item["target"],
                    mtype=MigrationType(item["mtype"]),
                    reason=MigrationReason(item["reason"]),
                    urgency=item.get("urgency", 0),
                    detail=item.get("detail", ""),
                )
                for item in rust_candidates
            ]

        candidates: List[MigrationCandidate] = []
        t = self.thresholds

        for node_id, node in node_states.items():
            for vm in node.local_vms:

                # ── 1. Remote paging excessif ──────────────────────────────
                if vm.remote_pct > t.remote_paging_pct:
                    target = self._best_target(node_id, vm, node_states)
                    if target:
                        mtype = self._pick_type(vm, critical=False)
                        candidates.append(MigrationCandidate(
                            vm=vm, source=node_id, target=target,
                            mtype=mtype,
                            reason=MigrationReason.EXCESSIVE_REMOTE_PAGING,
                            urgency=1,
                            detail=f"{vm.remote_pct:.0f}% RAM stockée à distance",
                        ))

                # ── 2. Pression RAM critique (≥ 95%) ───────────────────────
                if node.ram_used_pct >= t.ram_critical_pct:
                    target = self._best_target(node_id, vm, node_states)
                    if target:
                        candidates.append(MigrationCandidate(
                            vm=vm, source=node_id, target=target,
                            mtype=MigrationType.COLD,  # urgence = cold forcée
                            reason=MigrationReason.MEMORY_PRESSURE,
                            urgency=2,
                            detail=f"RAM nœud {node.ram_used_pct:.0f}% (critique ≥ {t.ram_critical_pct:.0f}%)",
                        ))

                # ── 3. Pression RAM haute (≥ 85%) ──────────────────────────
                elif node.ram_used_pct >= t.ram_high_pct:
                    target = self._best_target(node_id, vm, node_states)
                    if target:
                        mtype = self._pick_type(vm, critical=False)
                        candidates.append(MigrationCandidate(
                            vm=vm, source=node_id, target=target,
                            mtype=mtype,
                            reason=MigrationReason.MEMORY_PRESSURE,
                            urgency=1,
                            detail=f"RAM nœud {node.ram_used_pct:.0f}% (haute ≥ {t.ram_high_pct:.0f}%)",
                        ))

                # ── 4. Saturation vCPU ─────────────────────────────────────
                if (vm.throttle_ratio > t.vcpu_throttle_trigger
                        and node.vcpu_used_pct > t.vcpu_saturation_pct):
                    target = self._best_target_vcpu(node_id, vm, node_states)
                    if target:
                        candidates.append(MigrationCandidate(
                            vm=vm, source=node_id, target=target,
                            mtype=MigrationType.LIVE,  # VM throttlée = active
                            reason=MigrationReason.CPU_SATURATION,
                            urgency=1,
                            detail=(
                                f"throttle {vm.throttle_ratio:.0%}, "
                                f"nœud {node.vcpu_used_pct:.0f}% vCPU"
                            ),
                        ))

                # ── 5. Saturation disque ────────────────────────────────
                if (
                    node.disk_pressure_pct >= t.disk_high_pct
                    and vm.disk_total_bps >= t.disk_hot_vm_bps
                ):
                    target = self._best_target(node_id, vm, node_states)
                    if target:
                        candidates.append(MigrationCandidate(
                            vm=vm, source=node_id, target=target,
                            mtype=MigrationType.LIVE,
                            reason=MigrationReason.DISK_SATURATION,
                            urgency=1,
                            detail=(
                                f"PSI disque {node.disk_pressure_pct:.1f}% "
                                f"VM {vm.disk_total_bps / 1024 / 1024:.1f} Mio/s"
                            ),
                        ))

                # ── 6. Saturation GPU réservée ────────────────────────────
                if (
                    vm.gpu_vram_budget_mib > 0
                    and node.gpu_total_vram_mib > 0
                    and node.gpu_used_pct >= t.gpu_high_pct
                ):
                    target = self._best_target(node_id, vm, node_states)
                    if target:
                        candidates.append(MigrationCandidate(
                            vm=vm, source=node_id, target=target,
                            mtype=self._pick_type(vm, critical=False),
                            reason=MigrationReason.GPU_SATURATION,
                            urgency=1,
                            detail=(
                                f"GPU réservé {node.gpu_used_pct:.0f}% "
                                f"({vm.gpu_vram_budget_mib} Mio pour la VM)"
                            ),
                        ))

        # Dédupliquer (une VM ne migre qu'une fois) et trier par urgence
        seen: set = set()
        unique: List[MigrationCandidate] = []
        for c in sorted(candidates, key=lambda x: -x.urgency):
            if c.vm.vm_id not in seen:
                seen.add(c.vm.vm_id)
                unique.append(c)

        return unique

    def pick_migration_type(
        self,
        vm: VmState,
        node_ram_pct: float,
    ) -> MigrationType:
        """
        API publique pour choisir le type de migration d'une VM spécifique.
        Utilisée par l'admin ou le controller pour une décision manuelle.
        """
        rust_type = call_policy(
            "pick-migration-type",
            {
                "thresholds": {
                    "ram_high_pct": self.thresholds.ram_high_pct,
                    "ram_critical_pct": self.thresholds.ram_critical_pct,
                    "vcpu_throttle_trigger": self.thresholds.vcpu_throttle_trigger,
                    "vcpu_saturation_pct": self.thresholds.vcpu_saturation_pct,
                    "remote_paging_pct": self.thresholds.remote_paging_pct,
                    "gpu_high_pct": self.thresholds.gpu_high_pct,
                    "disk_high_pct": self.thresholds.disk_high_pct,
                    "disk_hot_vm_bps": self.thresholds.disk_hot_vm_bps,
                    "idle_cpu_pct": self.thresholds.idle_cpu_pct,
                    "idle_duration_secs": self.thresholds.idle_duration_secs,
                    "target_max_ram_pct": self.thresholds.target_max_ram_pct,
                    "target_max_vcpu_pct": self.thresholds.target_max_vcpu_pct,
                },
                "vm": {
                    "vm_id": vm.vm_id,
                    "status": vm.status,
                    "max_mem_mib": vm.max_mem_mib,
                    "rss_kb": vm.rss_kb,
                    "remote_pages": vm.remote_pages,
                    "avg_cpu_pct": vm.avg_cpu_pct,
                    "throttle_ratio": vm.throttle_ratio,
                    "gpu_vram_budget_mib": vm.gpu_vram_budget_mib,
                    "disk_read_bps": vm.disk_read_bps,
                    "disk_write_bps": vm.disk_write_bps,
                    "disk_io_weight": vm.disk_io_weight,
                    "disk_local_share_active": vm.disk_local_share_active,
                    "idle_duration_secs": vm.idle_duration_secs() if vm.idle_since is not None else None,
                },
                "node_ram_pct": node_ram_pct,
            },
        )
        if rust_type is not None:
            return MigrationType(rust_type)

        critical = node_ram_pct >= self.thresholds.ram_critical_pct
        return self._pick_type(vm, critical=critical)

    # ── Méthodes internes ─────────────────────────────────────────────────

    def _pick_type(self, vm: VmState, critical: bool) -> MigrationType:
        """
        Choisit entre LIVE et COLD selon l'état de la VM.

        COLD si :
          - VM stoppée
          - Pression critique (live trop lent, trop de dirty pages)
          - VM idle depuis > idle_duration_secs
        LIVE sinon (VM active → minimiser le downtime).
        """
        if vm.is_stopped:
            return MigrationType.COLD
        if critical:
            return MigrationType.COLD
        if self._is_idle(vm):
            return MigrationType.COLD
        return MigrationType.LIVE

    def _is_idle(self, vm: VmState) -> bool:
        t = self.thresholds
        return (
            vm.avg_cpu_pct < t.idle_cpu_pct
            and vm.idle_duration_secs() >= t.idle_duration_secs
        )

    def _best_target(
        self,
        source_id: str,
        vm: VmState,
        nodes: Dict[str, NodeState],
    ) -> Optional[str]:
        """Retourne le meilleur nœud cible (hors source) pour la VM."""
        eligible = [
            (nid, n) for nid, n in nodes.items()
            if nid != source_id and n.can_accept_vm(vm)
        ]
        if not eligible:
            return None
        return max(eligible, key=lambda x: self._placement_score_for_vm(x[1], vm))[0]

    def _best_target_vcpu(
        self,
        source_id: str,
        vm: VmState,
        nodes: Dict[str, NodeState],
    ) -> Optional[str]:
        """Retourne le nœud avec le plus de vCPU libres."""
        eligible = [
            (nid, n) for nid, n in nodes.items()
            if nid != source_id
            and n.vcpu_used_pct < self.thresholds.target_max_vcpu_pct
            and n.can_accept_vm(vm)
        ]
        if not eligible:
            return None
        return max(
            eligible,
            key=lambda x: (x[1].vcpu_free, self._placement_score_for_vm(x[1], vm)),
        )[0]

    def _placement_score_for_vm(self, node: NodeState, vm: VmState) -> float:
        ram_free_ratio  = 1.0 - node.ram_used_pct / 100.0
        vcpu_free_ratio = 1.0 - node.vcpu_used_pct / 100.0
        disk_free_ratio = max(0.0, 1.0 - node.disk_pressure_pct / 100.0)
        consolidation_ratio = min(len(node.local_vms), 12) / 12.0
        if vm.gpu_vram_budget_mib <= 0:
            return (
                ram_free_ratio * 0.50
                + vcpu_free_ratio * 0.30
                + disk_free_ratio * 0.15
                + consolidation_ratio * 0.05
            )
        gpu_free_ratio = (
            node.gpu_free_vram_mib / node.gpu_total_vram_mib
            if node.gpu_total_vram_mib > 0 else 0.0
        )
        return (
            gpu_free_ratio * 0.35
            + ram_free_ratio * 0.30
            + vcpu_free_ratio * 0.20
            + disk_free_ratio * 0.10
            + consolidation_ratio * 0.05
        )
