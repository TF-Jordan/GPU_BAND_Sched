"""
Contrôleur d'admission vCPU — gestion élastique des cœurs virtuels.

Modèle :
  - 1 pCPU physique = 3 vCPU
  - 1 slot vCPU peut être partagé entre MAX_VMS_PER_SLOT=3 VMs maximum
    (les instructions en attente sont chargées dans la file d'un autre vCPU libre)
  - Chaque VM déclare un min_vcpus et un max_vcpus à la création
  - Elle démarre avec min_vcpus, obtient des hotplugs jusqu'à max_vcpus selon la demande
  - Si le nœud n'a plus de vCPU disponibles ET qu'une VM a besoin de plus que son quota
    actuel → migration vers un nœud moins chargé

Invariants garantis :
  - allocated_vcpus(vm) ∈ [min_vcpus, max_vcpus]
  - sum(allocated_vcpus) ≤ num_pcpus * VCPU_PER_PCPU * MAX_VMS_PER_SLOT
  - Un vCPU partagé entre > MAX_VMS_PER_SLOT VMs → refus d'admission / migration
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

# ── Constantes du modèle ─────────────────────────────────────────────────────

VCPU_PER_PCPU: int     = 3   # vCPU physiques par cœur
MAX_VMS_PER_SLOT: int  = 3   # VMs max pouvant partager un vCPU slot
STEAL_THRESHOLD_PCT: float = 10.0  # steal% au-delà duquel on envisage la migration


# ── Structures de données ─────────────────────────────────────────────────────

@dataclass
class VmCpuSpec:
    """Spécification CPU d'une VM à sa création."""
    vmid:       int
    min_vcpus:  int   # minimum garanti (VM démarre avec ça)
    max_vcpus:  int   # plafond absolu demandé par la VM

    def __post_init__(self) -> None:
        if self.min_vcpus < 1:
            raise ValueError(f"min_vcpus doit être ≥ 1, reçu {self.min_vcpus}")
        if self.max_vcpus < self.min_vcpus:
            raise ValueError(
                f"max_vcpus ({self.max_vcpus}) < min_vcpus ({self.min_vcpus})"
            )


@dataclass
class VmCpuState:
    """État courant des vCPU d'une VM sur un nœud."""
    vmid:              int
    min_vcpus:         int
    max_vcpus:         int
    allocated_vcpus:   int          # actuellement allouées
    cpu_usage_pct:     float = 0.0  # % CPU moyen sur la fenêtre
    steal_pct:         float = 0.0  # steal time % récent
    node_id:           str  = ""

    def needs_more_vcpus(self) -> bool:
        """Vrai si la VM est sous pression CPU et peut encore recevoir des vCPUs."""
        return (
            self.cpu_usage_pct >= 80.0
            and self.allocated_vcpus < self.max_vcpus
        )

    def needs_migration(self) -> bool:
        """
        Vrai si la VM est à son max et souffre de steal — elle a besoin d'un nœud
        plus libre, pas d'un hotplug supplémentaire.
        """
        return (
            self.steal_pct >= STEAL_THRESHOLD_PCT
            and self.allocated_vcpus >= self.max_vcpus
        )


@dataclass
class NodeCpuCapacity:
    """Capacité CPU d'un nœud telle que déclarée / observée."""
    node_id:       str
    num_pcpus:     int
    steal_pct:     float = 0.0   # steal % courant lu sur /proc/stat
    cpu_usage_pct: float = 0.0   # utilisation globale

    @property
    def total_vcpu_slots(self) -> int:
        """Nombre total de slots vCPU disponibles (3 vCPU × pCPU × 3 VMs)."""
        return self.num_pcpus * VCPU_PER_PCPU * MAX_VMS_PER_SLOT

    def is_overloaded(self) -> bool:
        return self.steal_pct >= STEAL_THRESHOLD_PCT or self.cpu_usage_pct >= 90.0


@dataclass
class VcpuPoolConfig:
    """
    Paramètres du pool logique cluster-wide.

    - `min_gain_vcpus` : amélioration minimale requise pour autoriser une
      migration best-effort vers une cible partielle.
    - `migration_cooldown_secs` : délai minimum entre deux migrations
      automatiques CPU d'une même VM.
    """

    min_gain_vcpus: int = 1
    migration_cooldown_secs: float = 60.0


@dataclass
class VcpuPoolVm:
    """VM candidate au rééquilibrage du pool logique vCPU."""
    vmid: int
    node_id: str
    current_vcpus: int
    min_vcpus: int
    max_vcpus: int

    @property
    def cpu_deficit(self) -> int:
        return max(0, self.min_vcpus - self.current_vcpus)


@dataclass
class VcpuPoolDecision:
    """
    Décision du planificateur de pool.

    `action` ∈ {apply_local, migrate_full, migrate_partial, wait_cooldown, wait_deficit}
    """

    vmid: int
    source_node: str
    action: str
    cpu_deficit: int
    current_vcpus: int
    min_vcpus: int
    max_vcpus: int
    target_node: str = ""
    gain_vcpus: int = 0
    cooldown_remaining_secs: float = 0.0
    reason: str = ""


class ClusterVcpuPoolPlanner:
    """
    Planificateur cluster-wide du pool logique de vCPU.

    Politique :
      - les plus petits déficits sont résolus d'abord
      - on hotplug localement dès que possible
      - sinon on migre vers un nœud qui satisfait totalement le min_vcpus
      - sinon on autorise une migration partielle si elle améliore le cluster
      - sinon on garde la VM vivante et on réessaie plus tard
    """

    def __init__(self, config: VcpuPoolConfig = VcpuPoolConfig()) -> None:
        self.config = config

    def plan(
        self,
        node_free_slots: Dict[str, int],
        vms: List[VcpuPoolVm],
        last_migration_at: Optional[Dict[int, float]] = None,
        now: Optional[float] = None,
    ) -> List[VcpuPoolDecision]:
        now = now if now is not None else 0.0
        last_migration_at = last_migration_at or {}
        remaining = dict(node_free_slots)
        decisions: List[VcpuPoolDecision] = []

        for vm in sorted(vms, key=lambda item: (item.cpu_deficit, item.vmid)):
            if vm.cpu_deficit <= 0:
                continue

            source_free = remaining.get(vm.node_id, 0)
            if source_free >= vm.cpu_deficit:
                remaining[vm.node_id] = max(0, source_free - vm.cpu_deficit)
                decisions.append(VcpuPoolDecision(
                    vmid=vm.vmid,
                    source_node=vm.node_id,
                    action="apply_local",
                    cpu_deficit=vm.cpu_deficit,
                    current_vcpus=vm.current_vcpus,
                    min_vcpus=vm.min_vcpus,
                    max_vcpus=vm.max_vcpus,
                    reason="capacité locale suffisante dans le pool",
                ))
                continue

            last_move = last_migration_at.get(vm.vmid)
            if last_move is not None:
                cooldown_remaining = self.config.migration_cooldown_secs - (now - last_move)
                if cooldown_remaining > 0:
                    decisions.append(VcpuPoolDecision(
                        vmid=vm.vmid,
                        source_node=vm.node_id,
                        action="wait_cooldown",
                        cpu_deficit=vm.cpu_deficit,
                        current_vcpus=vm.current_vcpus,
                        min_vcpus=vm.min_vcpus,
                        max_vcpus=vm.max_vcpus,
                        cooldown_remaining_secs=max(0.0, cooldown_remaining),
                        reason="cooldown de migration CPU actif",
                    ))
                    continue

            full_target = self._best_full_fit_target(remaining, vm)
            if full_target is not None:
                gain = max(0, remaining.get(full_target, 0) - source_free)
                remaining[vm.node_id] = source_free + max(1, vm.current_vcpus)
                remaining[full_target] = max(0, remaining.get(full_target, 0) - vm.min_vcpus)
                decisions.append(VcpuPoolDecision(
                    vmid=vm.vmid,
                    source_node=vm.node_id,
                    action="migrate_full",
                    cpu_deficit=vm.cpu_deficit,
                    current_vcpus=vm.current_vcpus,
                    min_vcpus=vm.min_vcpus,
                    max_vcpus=vm.max_vcpus,
                    target_node=full_target,
                    gain_vcpus=gain,
                    reason="migration vers une cible qui satisfait entièrement le minimum",
                ))
                continue

            partial = self._best_partial_fit_target(remaining, vm)
            if partial is not None:
                target_node, gain = partial
                remaining[vm.node_id] = source_free + max(1, vm.current_vcpus)
                remaining[target_node] = max(
                    0,
                    remaining.get(target_node, 0) - max(1, vm.current_vcpus),
                )
                decisions.append(VcpuPoolDecision(
                    vmid=vm.vmid,
                    source_node=vm.node_id,
                    action="migrate_partial",
                    cpu_deficit=vm.cpu_deficit,
                    current_vcpus=vm.current_vcpus,
                    min_vcpus=vm.min_vcpus,
                    max_vcpus=vm.max_vcpus,
                    target_node=target_node,
                    gain_vcpus=gain,
                    reason="migration best-effort qui améliore le pool sans satisfaire l'idéal complet",
                ))
                continue

            decisions.append(VcpuPoolDecision(
                vmid=vm.vmid,
                source_node=vm.node_id,
                action="wait_deficit",
                cpu_deficit=vm.cpu_deficit,
                current_vcpus=vm.current_vcpus,
                min_vcpus=vm.min_vcpus,
                max_vcpus=vm.max_vcpus,
                reason="aucun nœud ne peut améliorer suffisamment ce déficit pour l'instant",
            ))

        return decisions

    def _best_full_fit_target(
        self,
        remaining: Dict[str, int],
        vm: VcpuPoolVm,
    ) -> Optional[str]:
        source_free = remaining.get(vm.node_id, 0)
        released_vcpus = max(1, vm.current_vcpus)
        candidates = [
            (
                node_id,
                free_slots,
                self._cluster_improvement_score(
                    remaining,
                    source_node=vm.node_id,
                    target_node=node_id,
                    source_after=source_free + released_vcpus,
                    target_after=free_slots - vm.min_vcpus,
                ),
            )
            for node_id, free_slots in remaining.items()
            if node_id != vm.node_id and free_slots >= vm.min_vcpus
        ]
        if not candidates:
            return None
        candidates.sort(key=lambda item: (item[2], item[1], item[0]), reverse=True)
        return candidates[0][0]

    def _best_partial_fit_target(
        self,
        remaining: Dict[str, int],
        vm: VcpuPoolVm,
    ) -> Optional[Tuple[str, int]]:
        source_free = remaining.get(vm.node_id, 0)
        min_required = max(1, vm.current_vcpus)
        candidates: List[Tuple[str, int, Tuple[int, int, int, int]]] = []
        for node_id, free_slots in remaining.items():
            if node_id == vm.node_id:
                continue
            if free_slots < min_required:
                continue

            score = self._cluster_improvement_score(
                remaining,
                source_node=vm.node_id,
                target_node=node_id,
                source_after=source_free + min_required,
                target_after=free_slots - min_required,
            )
            source_relief = max(0, min_required)
            improvement = source_relief + max(0, score[0]) + max(0, score[1])
            if improvement >= self.config.min_gain_vcpus:
                candidates.append((node_id, improvement, score))

        if not candidates:
            return None

        candidates.sort(key=lambda item: (item[2], item[1], item[0]), reverse=True)
        return candidates[0][0], candidates[0][1]

    def _cluster_improvement_score(
        self,
        remaining: Dict[str, int],
        source_node: str,
        target_node: str,
        source_after: int,
        target_after: int,
    ) -> Tuple[int, int, int, int]:
        before_values = list(remaining.values())
        before_floor = min(before_values) if before_values else 0
        before_spread = (max(before_values) - before_floor) if before_values else 0

        simulated = dict(remaining)
        simulated[source_node] = max(0, source_after)
        simulated[target_node] = max(0, target_after)

        after_values = list(simulated.values())
        after_floor = min(after_values) if after_values else 0
        after_spread = (max(after_values) - after_floor) if after_values else 0

        floor_gain = after_floor - before_floor
        spread_gain = before_spread - after_spread
        return (
            floor_gain,
            spread_gain,
            max(0, target_after),
            max(0, source_after),
        )


@dataclass
class CpuAdmissionDecision:
    """Résultat d'une décision d'admission CPU."""
    admitted:        bool
    vmid:            int
    node_id:         str
    allocated_vcpus: int   # vCPUs accordées au démarrage (= min_vcpus si admitted)
    max_vcpus:       int   # plafond conservé
    reason:          str   = ""

    def admission_payload(self) -> dict:
        """Corps prêt pour POST /control/vm/{vmid}/vcpu."""
        return {
            "vmid":            self.vmid,
            "allocated_vcpus": self.allocated_vcpus,
            "max_vcpus":       self.max_vcpus,
        }


# ── Registre local (par nœud) ─────────────────────────────────────────────────

class NodeVcpuRegistry:
    """
    Registre des vCPU allouées sur un nœud unique.
    Thread-safe, utilisé par le daemon Rust via le control_api ou par le controller.
    """

    def __init__(self, node_id: str, num_pcpus: int) -> None:
        self._node_id   = node_id
        self._num_pcpus = num_pcpus
        self._lock      = threading.RLock()
        self._vms: Dict[int, VmCpuState] = {}  # vmid → state

    # ── Capacité ─────────────────────────────────────────────────────────────

    @property
    def total_vcpu_slots(self) -> int:
        return self._num_pcpus * VCPU_PER_PCPU * MAX_VMS_PER_SLOT

    def allocated_vcpus(self) -> int:
        with self._lock:
            return sum(v.allocated_vcpus for v in self._vms.values())

    def free_vcpu_slots(self) -> int:
        return max(0, self.total_vcpu_slots - self.allocated_vcpus())

    # ── Admission ────────────────────────────────────────────────────────────

    def admit(self, spec: VmCpuSpec) -> CpuAdmissionDecision:
        """
        Tente d'admettre une VM sur ce nœud avec spec.min_vcpus vCPUs.
        Échoue si plus assez de slots disponibles.
        """
        with self._lock:
            if spec.vmid in self._vms:
                return CpuAdmissionDecision(
                    admitted=False,
                    vmid=spec.vmid,
                    node_id=self._node_id,
                    allocated_vcpus=0,
                    max_vcpus=spec.max_vcpus,
                    reason=f"VM {spec.vmid} déjà enregistrée sur ce nœud",
                )

            free = self.free_vcpu_slots()
            if free < spec.min_vcpus:
                return CpuAdmissionDecision(
                    admitted=False,
                    vmid=spec.vmid,
                    node_id=self._node_id,
                    allocated_vcpus=0,
                    max_vcpus=spec.max_vcpus,
                    reason=(
                        f"slots libres insuffisants : {free} < {spec.min_vcpus} requis"
                    ),
                )

            state = VmCpuState(
                vmid=spec.vmid,
                min_vcpus=spec.min_vcpus,
                max_vcpus=spec.max_vcpus,
                allocated_vcpus=spec.min_vcpus,
                node_id=self._node_id,
            )
            self._vms[spec.vmid] = state

            return CpuAdmissionDecision(
                admitted=True,
                vmid=spec.vmid,
                node_id=self._node_id,
                allocated_vcpus=spec.min_vcpus,
                max_vcpus=spec.max_vcpus,
                reason="admis",
            )

    def release(self, vmid: int) -> None:
        with self._lock:
            self._vms.pop(vmid, None)

    # ── Hotplug ──────────────────────────────────────────────────────────────

    def try_hotplug(self, vmid: int, delta: int = 1) -> Tuple[bool, str]:
        """
        Tente d'allouer `delta` vCPUs supplémentaires à la VM.
        Retourne (succès, raison).
        """
        with self._lock:
            vm = self._vms.get(vmid)
            if vm is None:
                return False, f"VM {vmid} inconnue"
            if vm.allocated_vcpus + delta > vm.max_vcpus:
                return False, (
                    f"plafond atteint : {vm.allocated_vcpus}+{delta} > {vm.max_vcpus}"
                )
            free = self.free_vcpu_slots()
            if free < delta:
                return False, f"slots libres insuffisants : {free} < {delta}"

            vm.allocated_vcpus += delta
            return True, f"hotplug +{delta} → {vm.allocated_vcpus} vCPUs"

    # ── Métriques ────────────────────────────────────────────────────────────

    def update_metrics(self, vmid: int, cpu_usage_pct: float, steal_pct: float) -> None:
        with self._lock:
            vm = self._vms.get(vmid)
            if vm:
                vm.cpu_usage_pct = cpu_usage_pct
                vm.steal_pct     = steal_pct

    def vms_needing_hotplug(self) -> List[int]:
        with self._lock:
            return [v.vmid for v in self._vms.values() if v.needs_more_vcpus()]

    def vms_needing_migration(self) -> List[int]:
        with self._lock:
            return [v.vmid for v in self._vms.values() if v.needs_migration()]

    def snapshot(self) -> List[VmCpuState]:
        with self._lock:
            return list(self._vms.values())

    def get_vm(self, vmid: int) -> Optional[VmCpuState]:
        with self._lock:
            return self._vms.get(vmid)


# ── Contrôleur d'admission cluster ───────────────────────────────────────────

class CpuAdmissionController:
    """
    Admission CPU au niveau cluster — choisit le nœud cible et applique
    le modèle élastique min/max.

    Le controller Python l'utilise pour décider où placer une VM et avec
    combien de vCPUs elle démarre.
    """

    def __init__(self, nodes: Dict[str, NodeCpuCapacity]) -> None:
        """
        nodes : { node_id → NodeCpuCapacity }
        """
        self._nodes:     Dict[str, NodeCpuCapacity]     = nodes
        self._registries: Dict[str, NodeVcpuRegistry]   = {
            nid: NodeVcpuRegistry(nid, cap.num_pcpus)
            for nid, cap in nodes.items()
        }
        self._lock = threading.Lock()

    # ── Sélection du nœud ────────────────────────────────────────────────────

    def _best_node_for(self, spec: VmCpuSpec) -> Optional[str]:
        """
        Choisit le nœud avec le plus de slots libres ET non surchargé.
        Retourne None si aucun nœud ne peut accueillir spec.min_vcpus.
        """
        candidates = []
        for nid, reg in self._registries.items():
            cap   = self._nodes[nid]
            free  = reg.free_vcpu_slots()
            if free >= spec.min_vcpus and not cap.is_overloaded():
                candidates.append((nid, free))

        if not candidates:
            return None

        # Le nœud avec le plus de slots libres en premier
        candidates.sort(key=lambda x: -x[1])
        return candidates[0][0]

    # ── Admission ────────────────────────────────────────────────────────────

    def admit(
        self,
        spec: VmCpuSpec,
        preferred_node: Optional[str] = None,
    ) -> CpuAdmissionDecision:
        """
        Admet une VM dans le cluster.  Retourne une décision avec node_id et
        le nombre de vCPUs allouées au démarrage (= min_vcpus).
        """
        with self._lock:
            # 1. Essayer le nœud préféré d'abord
            if preferred_node and preferred_node in self._registries:
                cap = self._nodes[preferred_node]
                reg = self._registries[preferred_node]
                if reg.free_vcpu_slots() >= spec.min_vcpus and not cap.is_overloaded():
                    return reg.admit(spec)

            # 2. Meilleur nœud disponible
            target = self._best_node_for(spec)
            if target is None:
                return CpuAdmissionDecision(
                    admitted=False,
                    vmid=spec.vmid,
                    node_id="",
                    allocated_vcpus=0,
                    max_vcpus=spec.max_vcpus,
                    reason="aucun nœud disponible avec assez de slots vCPU libres",
                )

            return self._registries[target].admit(spec)

    def release(self, vmid: int, node_id: str) -> None:
        reg = self._registries.get(node_id)
        if reg:
            reg.release(vmid)

    # ── Hotplug cluster ───────────────────────────────────────────────────────

    def try_hotplug(self, vmid: int, node_id: str, delta: int = 1) -> Tuple[bool, str]:
        reg = self._registries.get(node_id)
        if reg is None:
            return False, f"nœud {node_id} inconnu"
        return reg.try_hotplug(vmid, delta)

    # ── Métriques ────────────────────────────────────────────────────────────

    def update_node_capacity(self, node_id: str, cap: NodeCpuCapacity) -> None:
        with self._lock:
            self._nodes[node_id] = cap

    def update_vm_metrics(
        self,
        vmid: int,
        node_id: str,
        cpu_usage_pct: float,
        steal_pct: float,
    ) -> None:
        reg = self._registries.get(node_id)
        if reg:
            reg.update_metrics(vmid, cpu_usage_pct, steal_pct)

    def cluster_snapshot(self) -> Dict[str, List[VmCpuState]]:
        return {nid: reg.snapshot() for nid, reg in self._registries.items()}

    def hotplug_candidates(self) -> Dict[str, List[int]]:
        """VMs par nœud ayant besoin d'un hotplug vCPU."""
        return {
            nid: reg.vms_needing_hotplug()
            for nid, reg in self._registries.items()
        }

    def migration_candidates(self) -> Dict[str, List[int]]:
        """VMs par nœud devant être migrées (steal trop élevé, au max de vCPU)."""
        return {
            nid: reg.vms_needing_migration()
            for nid, reg in self._registries.items()
        }

    def free_slots_per_node(self) -> Dict[str, int]:
        return {nid: reg.free_vcpu_slots() for nid, reg in self._registries.items()}
