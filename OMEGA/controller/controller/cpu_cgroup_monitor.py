"""
Moniteur CPU cgroups v2 — lecture à 1 ms, métriques par vCPU.

# Ce que cpu.max signifie en vCPU (pas en cœurs physiques)

  cpu.max = "N_vcpus × 1_000_000  1_000_000"

  Exemples concrets :
    "1000000 1000000"  →  1 vCPU  (100% d'un thread physique)
    "2000000 1000000"  →  2 vCPU  (200% — 2 threads en parallèle)
    "6000000 1000000"  →  6 vCPU  (600% — 6 threads en parallèle)

  IMPORTANT : cpu.max ne limite pas des cœurs physiques, il limite
  du TEMPS CPU. Une VM avec 6 vCPU peut les distribuer sur n'importe
  quels cœurs physiques disponibles — c'est le scheduler CFS du kernel
  qui décide. Ce qu'on contrôle : le QUOTA de temps que la VM peut
  consommer sur l'ensemble des cœurs physiques du nœud.

  Sur notre nœud avec 4 pCPU × 3 vCPU = 12 vCPU max :
    cpu.max max = "12000000 1000000"  →  utiliser les 12 vCPU à 100%

# Pourquoi lire à 1 ms

  cpu.stat est mis à jour par le kernel à chaque context switch.
  Lire à 1 ms permet de détecter immédiatement :
    - Un pic de charge (la VM atteint son quota en < 1 ms)
    - Un épisode de throttling (throttled_usec augmente soudainement)
    - Un relâchement de charge (la VM libère du vCPU)

  À 1 ms, on accumule les échantillons dans une fenêtre glissante
  de 100 ms (100 échantillons) pour calculer un usage_pct stable.
  Les décisions (hotplug, migration) ne se déclenchent que sur la
  moyenne glissante, pas sur chaque échantillon brut.

# Relation avec le daemon Rust

  Python (ce fichier)              Rust (vcpu_scheduler.rs + qmp_vcpu.rs)
  ─────────────────────            ──────────────────────────────────────
  Lit cpu.stat @ 1ms          →   POST /control/vm/{id}/vcpu/metrics
  Calcule moyenne 100ms        →   VcpuScheduler.update_from_cgroup()
  Détecte throttling           →   Décide hotplug ou migration
  Décide nouvelle config       →   QmpVcpuClient.hotplug_add() via QMP
  Écrit cpu.max (nouveau quota) ←  Quota vCPU confirmé
"""

from __future__ import annotations

import collections
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Deque, Dict, List, Optional, Tuple


# ─── Constantes ───────────────────────────────────────────────────────────────

# Intervalle de lecture des cgroups (1 ms = 0.001 s)
POLL_INTERVAL_SEC: float = 0.001

# Taille de la fenêtre glissante pour moyenner l'usage CPU
# 100 échantillons × 1 ms = fenêtre de 100 ms
WINDOW_SIZE: int = 100

# Seuil d'usage vCPU déclenchant un hotplug (% d'un vCPU)
# 80% = la VM utilise 80% de ses vCPU alloués → lui en donner plus
HOTPLUG_TRIGGER_PCT: float = 80.0

# Taux de throttling déclenchant une action (10% des périodes throttlées)
THROTTLE_TRIGGER_RATIO: float = 0.10


# ─── Structures ───────────────────────────────────────────────────────────────

@dataclass
class CgroupCpuStat:
    """
    Métriques CPU lues depuis cpu.stat d'un cgroup v2.

    Toutes les durées sont en microsecondes (µs).
    """
    vm_id:          int
    usage_usec:     int   = 0   # Temps CPU total consommé
    user_usec:      int   = 0   # Temps en espace utilisateur
    system_usec:    int   = 0   # Temps en espace noyau
    nr_periods:     int   = 0   # Nombre de périodes de quota écoulées
    nr_throttled:   int   = 0   # Périodes où le quota a été atteint
    throttled_usec: int   = 0   # Temps total passé throttlé
    timestamp:      float = field(default_factory=time.monotonic)

    @property
    def is_throttled(self) -> bool:
        return self.nr_periods > 0 and self.nr_throttled > 0

    @property
    def throttle_ratio(self) -> float:
        """Proportion de périodes throttlées (0.0 – 1.0)."""
        if self.nr_periods == 0:
            return 0.0
        return self.nr_throttled / self.nr_periods

    def vcpu_usage_pct_since(self, prev: "CgroupCpuStat") -> float:
        """
        Calcule le % de vCPU utilisé depuis le snapshot précédent.

        Retourne un % relatif au QUOTA alloué à la VM, pas aux cœurs physiques.
        Exemple :
          - VM avec 2 vCPU alloués, usage à 100% d'1 vCPU → retourne 50.0
          - VM avec 2 vCPU alloués, usage à 200% (2 vCPU saturés) → retourne 100.0
        """
        elapsed_usec = (self.timestamp - prev.timestamp) * 1_000_000
        if elapsed_usec <= 0:
            return 0.0
        delta_usec = max(0, self.usage_usec - prev.usage_usec)
        return (delta_usec / elapsed_usec) * 100.0


@dataclass
class VmCpuWindow:
    """
    Fenêtre glissante de métriques CPU pour une VM.

    Accumule WINDOW_SIZE échantillons (lus à 1 ms) et calcule
    la moyenne d'usage et le taux de throttling sur 100 ms.
    """
    vm_id:       int
    samples:     Deque[CgroupCpuStat]     = field(default_factory=lambda: collections.deque(maxlen=WINDOW_SIZE))
    usage_pcts:  Deque[float]             = field(default_factory=lambda: collections.deque(maxlen=WINDOW_SIZE))

    def push(self, stat: CgroupCpuStat, prev: CgroupCpuStat) -> None:
        usage_pct = stat.vcpu_usage_pct_since(prev)
        self.samples.append(stat)
        self.usage_pcts.append(usage_pct)

    @property
    def avg_usage_pct(self) -> float:
        """Moyenne d'usage vCPU sur la fenêtre (%)."""
        if not self.usage_pcts:
            return 0.0
        return sum(self.usage_pcts) / len(self.usage_pcts)

    @property
    def max_usage_pct(self) -> float:
        """Pic d'usage vCPU sur la fenêtre (%)."""
        if not self.usage_pcts:
            return 0.0
        return max(self.usage_pcts)

    @property
    def avg_throttle_ratio(self) -> float:
        """Taux de throttling moyen sur la fenêtre."""
        if not self.samples:
            return 0.0
        return sum(s.throttle_ratio for s in self.samples) / len(self.samples)

    @property
    def latest(self) -> Optional[CgroupCpuStat]:
        return self.samples[-1] if self.samples else None

    def is_full(self) -> bool:
        return len(self.samples) == WINDOW_SIZE


@dataclass
class CgroupCpuConfig:
    """
    Configuration CPU d'un cgroup v2 en termes de vCPU.

    Les valeurs sont converties en µs pour écriture dans cpu.max :
      num_vcpus × 1_000_000 µs  /  période de 1_000_000 µs
    """
    vm_id:       int
    num_vcpus:   Optional[int] = None  # None = illimité
    weight:      int           = 100   # priorité relative (1–10000)
    period_usec: int           = 1_000_000  # période standard : 1 seconde
    cpuset:      Optional[str] = None  # épinglage cœurs : "0-3", "0,2", ...

    @property
    def quota_usec(self) -> Optional[int]:
        """Quota en µs correspondant au nombre de vCPUs."""
        if self.num_vcpus is None:
            return None
        return self.num_vcpus * self.period_usec

    def cpu_max_str(self) -> str:
        """Valeur à écrire dans cpu.max."""
        if self.quota_usec is None:
            return f"max {self.period_usec}"
        return f"{self.quota_usec} {self.period_usec}"

    def vcpu_description(self) -> str:
        """Description lisible pour les logs."""
        if self.num_vcpus is None:
            return "illimité"
        return f"{self.num_vcpus} vCPU(s)"


# ─── Contrôleur cgroup ────────────────────────────────────────────────────────

class CgroupCpuController:
    """
    Lit et écrit les fichiers cgroups v2 CPU des VMs Proxmox.

    Chemin Proxmox VE 7/8 :
      /sys/fs/cgroup/machine.slice/machine-qemu-{vmid}-pve.scope/
    """

    def __init__(self, cgroup_root: str = "/sys/fs/cgroup") -> None:
        self._root = Path(cgroup_root)

    def find_vm_cgroup(self, vm_id: int) -> Optional[Path]:
        base = self._root / "machine.slice"
        if not base.exists():
            return None
        for pattern in [
            f"machine-qemu*{vm_id}*.scope",
            f"machine-qemu\\x2d{vm_id}*.scope",
        ]:
            matches = list(base.glob(pattern))
            if matches:
                return matches[0]
        svc = self._root / "system.slice" / f"qemu-server@{vm_id}.service"
        if svc.exists():
            return svc
        return None

    # ── Lecture ───────────────────────────────────────────────────────────

    def read_stat(self, vm_id: int) -> Optional[CgroupCpuStat]:
        cg = self.find_vm_cgroup(vm_id)
        if not cg:
            return None
        stat_file = cg / "cpu.stat"
        if not stat_file.exists():
            return None
        stat = CgroupCpuStat(vm_id=vm_id, timestamp=time.monotonic())
        try:
            for line in stat_file.read_text().splitlines():
                parts = line.split()
                if len(parts) < 2:
                    continue
                key, val = parts[0], int(parts[1])
                if   key == "usage_usec":     stat.usage_usec     = val
                elif key == "user_usec":      stat.user_usec       = val
                elif key == "system_usec":    stat.system_usec     = val
                elif key == "nr_periods":     stat.nr_periods      = val
                elif key == "nr_throttled":   stat.nr_throttled    = val
                elif key == "throttled_usec": stat.throttled_usec  = val
        except (OSError, ValueError):
            return None
        return stat

    def read_weight(self, vm_id: int) -> Optional[int]:
        cg = self.find_vm_cgroup(vm_id)
        if not cg:
            return None
        try:
            return int((cg / "cpu.weight").read_text().strip())
        except (OSError, ValueError):
            return None

    def read_max(self, vm_id: int) -> Tuple[Optional[int], int]:
        """Retourne (quota_usec ou None, period_usec)."""
        cg = self.find_vm_cgroup(vm_id)
        if not cg:
            return None, 1_000_000
        try:
            content = (cg / "cpu.max").read_text().strip()
            parts = content.split()
            period = int(parts[1]) if len(parts) > 1 else 1_000_000
            if parts[0] == "max":
                return None, period
            return int(parts[0]), period
        except (OSError, ValueError, IndexError):
            return None, 1_000_000

    def read_vcpu_count(self, vm_id: int) -> Optional[int]:
        """
        Déduit le nombre de vCPU alloués depuis cpu.max.
        Retourne None si illimité.
        """
        quota, period = self.read_max(vm_id)
        if quota is None or period == 0:
            return None
        return quota // period  # quota_usec / period_usec = nb vCPU

    def list_active_vms(self) -> List[int]:
        base = self._root / "machine.slice"
        if not base.exists():
            return []
        vmids = []
        for entry in base.iterdir():
            name = entry.name
            if "qemu" in name and name.endswith(".scope"):
                for part in name.replace("\\x2d", "-").split("-"):
                    try:
                        vmid = int(part)
                        if vmid > 100:
                            vmids.append(vmid)
                            break
                    except ValueError:
                        continue
        return vmids

    # ── Écriture ──────────────────────────────────────────────────────────

    def apply(self, config: CgroupCpuConfig) -> bool:
        """
        Applique une configuration vCPU sur le cgroup d'une VM.

        Exemple :
          config = CgroupCpuConfig(vm_id=101, num_vcpus=4, weight=150)
          → écrit "4000000 1000000" dans cpu.max
          → écrit "150" dans cpu.weight
          La VM ne peut pas consommer plus de 4 vCPU de temps CPU.
        """
        cg = self.find_vm_cgroup(config.vm_id)
        if not cg:
            return False
        try:
            (cg / "cpu.weight").write_text(str(config.weight))
        except OSError:
            return False
        try:
            (cg / "cpu.max").write_text(config.cpu_max_str())
        except OSError:
            pass
        if config.cpuset is not None:
            cpuset_path = cg / "cpuset.cpus"
            if cpuset_path.exists():
                try:
                    cpuset_path.write_text(config.cpuset)
                except OSError:
                    pass
        return True

    def reset(self, vm_id: int) -> bool:
        """Remet les paramètres CPU par défaut (illimité, weight=100)."""
        return self.apply(CgroupCpuConfig(vm_id=vm_id, num_vcpus=None, weight=100))


# ─── Moniteur haute fréquence (1 ms) ─────────────────────────────────────────

class CgroupCpuMonitor:
    """
    Boucle de monitoring CPU à 1 ms.

    Algorithme :
      1. Toutes les 1 ms : lire cpu.stat de chaque VM active
      2. Calculer l'usage vCPU instantané depuis la lecture précédente
      3. Accumuler dans une fenêtre glissante de 100 échantillons (= 100 ms)
      4. Sur chaque échantillon : vérifier si la moyenne 100 ms dépasse les seuils
      5. Si oui : appeler on_pressure(vm_id, avg_usage_pct, avg_throttle_ratio)

    Le callback on_pressure est appelé au maximum une fois par fenêtre
    (rate-limiting interne) pour éviter les décisions en rafale.

    Métriques exposées :
      - avg_usage_pct     : % moyen de vCPU utilisés sur 100 ms
      - max_usage_pct     : pic d'usage sur 100 ms
      - avg_throttle_ratio: proportion de périodes throttlées sur 100 ms
    """

    def __init__(
        self,
        controller:          CgroupCpuController,
        poll_interval:       float = POLL_INTERVAL_SEC,   # 1 ms par défaut
        window_size:         int   = WINDOW_SIZE,          # 100 échantillons
        on_pressure:         Optional[Callable[[int, float, float], None]] = None,
        usage_threshold:     float = HOTPLUG_TRIGGER_PCT,  # 80%
        throttle_threshold:  float = THROTTLE_TRIGGER_RATIO,  # 10%
    ) -> None:
        self._ctrl              = controller
        self._interval          = poll_interval
        self._window_size       = window_size
        self._on_pressure       = on_pressure
        self._usage_thresh      = usage_threshold
        self._throttle_thresh   = throttle_threshold

        # vm_id → stat précédente (pour calculer le delta)
        self._prev_stats:  Dict[int, CgroupCpuStat] = {}
        # vm_id → fenêtre glissante
        self._windows:     Dict[int, VmCpuWindow]   = {}
        # vm_id → timestamp de la dernière décision (rate-limiting)
        self._last_action: Dict[int, float]          = {}

        self._running = False
        self._thread: Optional[threading.Thread] = None
        self._lock = threading.Lock()

    def start(self) -> None:
        self._running = True
        self._thread = threading.Thread(target=self._loop, daemon=True, name="cgroup-cpu-monitor")
        self._thread.start()

    def stop(self) -> None:
        self._running = False
        if self._thread:
            self._thread.join(timeout=2)

    # ── Boucle interne ────────────────────────────────────────────────────

    def _loop(self) -> None:
        while self._running:
            t_start = time.monotonic()
            self._poll_all()
            elapsed = time.monotonic() - t_start
            sleep = max(0.0, self._interval - elapsed)
            if sleep > 0:
                time.sleep(sleep)

    def _poll_all(self) -> None:
        vm_ids = self._ctrl.list_active_vms()
        now = time.monotonic()

        for vm_id in vm_ids:
            stat = self._ctrl.read_stat(vm_id)
            if stat is None:
                continue

            with self._lock:
                prev = self._prev_stats.get(vm_id)

                if prev is not None:
                    # Assurer / créer la fenêtre
                    if vm_id not in self._windows:
                        self._windows[vm_id] = VmCpuWindow(vm_id=vm_id)

                    window = self._windows[vm_id]
                    window.push(stat, prev)

                    # Évaluer seulement si la fenêtre est pleine (100 ms de données)
                    if window.is_full():
                        avg_usage    = window.avg_usage_pct
                        avg_throttle = window.avg_throttle_ratio

                        # Rate-limiting : max 1 décision par fenêtre (100 ms)
                        last = self._last_action.get(vm_id, 0.0)
                        if now - last >= self._interval * self._window_size:
                            if (avg_usage    >= self._usage_thresh or
                                avg_throttle >= self._throttle_thresh):
                                if self._on_pressure:
                                    self._on_pressure(vm_id, avg_usage, avg_throttle)
                                self._last_action[vm_id] = now

                self._prev_stats[vm_id] = stat

    # ── API publique ──────────────────────────────────────────────────────

    def snapshot(self) -> Dict[int, dict]:
        """Retourne les métriques courantes de toutes les VMs."""
        with self._lock:
            return {
                vm_id: {
                    "avg_usage_pct":    w.avg_usage_pct,
                    "max_usage_pct":    w.max_usage_pct,
                    "avg_throttle_ratio": w.avg_throttle_ratio,
                    "samples":          len(w.samples),
                }
                for vm_id, w in self._windows.items()
            }

    def vm_window(self, vm_id: int) -> Optional[VmCpuWindow]:
        with self._lock:
            return self._windows.get(vm_id)
