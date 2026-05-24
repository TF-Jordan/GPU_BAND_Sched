"""
Monitor mémoire pour containers LXC — correction de la limite L7.

# Problème corrigé

omega-remote-paging ne supportait que les VMs QEMU/KVM (accès QMP balloon,
hook Proxmox, userfaultfd). Les containers LXC — pourtant très utilisés dans
Proxmox — n'étaient pas surveillés ni pris en compte dans les décisions
de migration.

# Solution

Les containers LXC dans Proxmox sont des namespaces Linux avec des cgroups v2.
Le kernel expose leur pression mémoire via :
  `/sys/fs/cgroup/lxc/{ctid}/memory.pressure`
  `/sys/fs/cgroup/lxc/{ctid}/memory.current`
  `/sys/fs/cgroup/lxc/{ctid}/memory.max`

Ce module surveille ces fichiers et émet des alertes quand un container
est sous pression mémoire, permettant au controller de :
  1. Ajuster la politique de placement pour les futures VMs
  2. Déclencher une migration du container (si Proxmox le supporte)
  3. Logguer pour alerting externe (Prometheus, Grafana)

# Différences LXC vs VM

| Aspect           | VM QEMU                    | Container LXC               |
|------------------|----------------------------|-----------------------------|
| Isolation mémoire | complète (MMU séparée)     | partagée (même kernel)      |
| Paging distant   | userfaultfd possible       | non applicable              |
| Migration live   | supportée par Proxmox      | supportée (CT migration)    |
| Monitoring       | QMP balloon                | cgroup v2 memory.pressure   |

# Format memory.pressure (PSI — Pressure Stall Information)

```
some avg10=0.00 avg60=0.00 avg300=0.00 total=0
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
```

- `some` : au moins un thread était bloqué sur de la mémoire
- `full` : TOUS les threads étaient bloqués (saturation totale)
- `avg10` : moyenne sur 10 secondes (réaction rapide)
- `avg60` : moyenne sur 60 secondes (tendance)
"""

from __future__ import annotations

import os
import re
import logging
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional, Callable

logger = logging.getLogger(__name__)


# ─── Structures ───────────────────────────────────────────────────────────────

@dataclass
class LxcMemoryPressure:
    """PSI memory pressure d'un container LXC."""
    ctid:          int
    some_avg10:    float   # % du temps où ≥1 thread était bloqué (10s)
    some_avg60:    float
    full_avg10:    float   # % du temps où tous les threads étaient bloqués (10s)
    full_avg60:    float
    mem_current_kb: int    # RAM utilisée actuellement
    mem_limit_kb:  int     # Limite cgroup (0 = illimité)
    timestamp:     float = field(default_factory=time.time)

    @property
    def usage_pct(self) -> float:
        if self.mem_limit_kb <= 0:
            return 0.0
        return (self.mem_current_kb / self.mem_limit_kb) * 100.0

    @property
    def is_under_pressure(self) -> bool:
        """Vrai si le container souffre de manque de mémoire."""
        return self.some_avg10 > 0.5 or self.full_avg10 > 0.1

    @property
    def is_critical(self) -> bool:
        """Vrai si la situation est critique (nécessite action immédiate)."""
        return self.full_avg10 > 1.0 or self.usage_pct > 95.0

    def to_dict(self) -> dict:
        return {
            "ctid":           self.ctid,
            "some_avg10":     round(self.some_avg10, 3),
            "some_avg60":     round(self.some_avg60, 3),
            "full_avg10":     round(self.full_avg10, 3),
            "full_avg60":     round(self.full_avg60, 3),
            "mem_current_kb": self.mem_current_kb,
            "mem_limit_kb":   self.mem_limit_kb,
            "usage_pct":      round(self.usage_pct, 1),
            "is_under_pressure": self.is_under_pressure,
            "is_critical":    self.is_critical,
        }


# ─── Lecteur cgroup ───────────────────────────────────────────────────────────

class CgroupV2Reader:
    """
    Lit les fichiers cgroup v2 d'un container LXC.

    Chemin Proxmox : /sys/fs/cgroup/lxc/{ctid}/memory.*
    Chemin alternatif : /sys/fs/cgroup/system.slice/lxc@{ctid}.service/memory.*
    """

    # Patterns de chemin cgroup selon la version de Proxmox
    CGROUP_PATTERNS = [
        "/sys/fs/cgroup/lxc/{ctid}",
        "/sys/fs/cgroup/system.slice/lxc@{ctid}.service",
        "/sys/fs/cgroup/lxc.payload.{ctid}",
    ]

    def __init__(self, cgroup_base: Optional[str] = None):
        self.cgroup_base = cgroup_base

    def find_cgroup_dir(self, ctid: int) -> Optional[Path]:
        """Trouve le répertoire cgroup d'un container."""
        if self.cgroup_base:
            p = Path(self.cgroup_base.format(ctid=ctid))
            if p.exists():
                return p

        for pattern in self.CGROUP_PATTERNS:
            p = Path(pattern.format(ctid=ctid))
            if p.exists():
                return p
        return None

    def read_pressure(self, ctid: int) -> Optional[LxcMemoryPressure]:
        """
        Lit les PSI memory pressure et l'usage RAM d'un container.

        Retourne None si le container n'existe pas ou si cgroup v2 n'est pas dispo.
        """
        cgroup_dir = self.find_cgroup_dir(ctid)
        if cgroup_dir is None:
            return None

        pressure_file = cgroup_dir / "memory.pressure"
        current_file  = cgroup_dir / "memory.current"
        max_file      = cgroup_dir / "memory.max"

        if not pressure_file.exists():
            logger.debug("ctid=%d : memory.pressure absent (cgroup v1 ?)", ctid)
            return None

        try:
            pressure = self._parse_pressure(pressure_file.read_text())
            current  = self._read_int_kb(current_file)
            limit    = self._read_int_kb(max_file)  # "max" si illimité

            return LxcMemoryPressure(
                ctid           = ctid,
                some_avg10     = pressure.get("some_avg10", 0.0),
                some_avg60     = pressure.get("some_avg60", 0.0),
                full_avg10     = pressure.get("full_avg10", 0.0),
                full_avg60     = pressure.get("full_avg60", 0.0),
                mem_current_kb = current,
                mem_limit_kb   = limit,
            )
        except Exception as e:
            logger.debug("erreur lecture cgroup ctid=%d : %s", ctid, e)
            return None

    @staticmethod
    def _parse_pressure(content: str) -> dict[str, float]:
        """
        Parse le format PSI :
        some avg10=0.05 avg60=0.01 avg300=0.00 total=1234
        full avg10=0.00 avg60=0.00 avg300=0.00 total=0
        """
        result = {}
        pattern = re.compile(r'(\w+)\s+avg10=(\S+)\s+avg60=(\S+)')

        for line in content.strip().splitlines():
            m = pattern.match(line)
            if m:
                kind   = m.group(1)    # "some" ou "full"
                avg10  = float(m.group(2))
                avg60  = float(m.group(3))
                result[f"{kind}_avg10"] = avg10
                result[f"{kind}_avg60"] = avg60
        return result

    @staticmethod
    def _read_int_kb(path: Path) -> int:
        """Lit un fichier cgroup contenant un entier en octets → converti en Ko."""
        if not path.exists():
            return 0
        content = path.read_text().strip()
        if content == "max":
            return 0   # illimité
        try:
            return int(content) // 1024
        except ValueError:
            return 0

    def list_lxc_ctids(self) -> list[int]:
        """
        Découverte automatique des containers LXC actifs sur le nœud.

        Cherche les répertoires cgroup correspondant à des containers LXC.
        """
        ctids = []
        base_dirs = [
            Path("/sys/fs/cgroup/lxc"),
            Path("/sys/fs/cgroup/system.slice"),
        ]

        for base in base_dirs:
            if not base.exists():
                continue
            for entry in base.iterdir():
                # Format Proxmox : répertoire numérique (ctid) ou "lxc@{ctid}.service"
                name = entry.name
                if name.isdigit():
                    ctids.append(int(name))
                elif name.startswith("lxc@") and name.endswith(".service"):
                    try:
                        ctid_str = name[4:-8]   # "lxc@100.service" → "100"
                        ctids.append(int(ctid_str))
                    except ValueError:
                        pass

        return sorted(set(ctids))


# ─── Monitor ──────────────────────────────────────────────────────────────────

# Type du callback : (LxcMemoryPressure) → None
PressureCallback = Callable[[LxcMemoryPressure], None]


class LxcMemoryMonitor:
    """
    Surveille la pression mémoire de tous les containers LXC du nœud.

    Tourne dans un thread dédié (non-bloquant pour l'appelant).

    Exemple d'utilisation :
    ```python
    def on_pressure(p: LxcMemoryPressure):
        print(f"CT {p.ctid} sous pression : {p.usage_pct:.1f}% RAM, "
              f"PSI-full={p.full_avg10:.2f}%")

    monitor = LxcMemoryMonitor(poll_interval=15, on_pressure=on_pressure)
    monitor.start()
    # ...
    monitor.stop()
    ```
    """

    def __init__(
        self,
        poll_interval:     int   = 15,
        pressure_threshold: float = 0.5,  # some_avg10 > ce seuil → alerte
        critical_threshold: float = 1.0,  # full_avg10 > ce seuil → critique
        on_pressure:       Optional[PressureCallback] = None,
        on_critical:       Optional[PressureCallback] = None,
        cgroup_base:       Optional[str] = None,
    ):
        self.poll_interval        = poll_interval
        self.pressure_threshold   = pressure_threshold
        self.critical_threshold   = critical_threshold
        self.on_pressure          = on_pressure
        self.on_critical          = on_critical

        self._reader  = CgroupV2Reader(cgroup_base)
        self._running = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._last_readings: dict[int, LxcMemoryPressure] = {}

    def start(self) -> None:
        """Lance le monitor dans un thread de fond."""
        self._running.set()
        self._thread = threading.Thread(
            target=self._run,
            name="lxc-memory-monitor",
            daemon=True,
        )
        self._thread.start()
        logger.info(
            "LxcMemoryMonitor démarré (interval=%ds, "
            "seuil pression=%.1f%%, seuil critique=%.1f%%)",
            self.poll_interval, self.pressure_threshold, self.critical_threshold,
        )

    def stop(self) -> None:
        """Arrête le monitor et attend la fin du thread."""
        self._running.clear()
        if self._thread:
            self._thread.join(timeout=self.poll_interval + 2)
        logger.info("LxcMemoryMonitor arrêté")

    def latest_readings(self) -> dict[int, LxcMemoryPressure]:
        """Retourne le dernier relevé pour chaque container (thread-safe en lecture)."""
        return dict(self._last_readings)

    def _run(self) -> None:
        """Boucle principale du monitor."""
        while self._running.is_set():
            try:
                self._poll_all()
            except Exception as e:
                logger.error("erreur poll LXC : %s", e, exc_info=True)
            self._running.wait(timeout=self.poll_interval)

    def _poll_all(self) -> None:
        """Interroge tous les containers LXC actifs."""
        ctids = self._reader.list_lxc_ctids()

        if not ctids:
            logger.debug("aucun container LXC détecté via cgroup")
            return

        pressured  = 0
        critical_n = 0

        for ctid in ctids:
            reading = self._reader.read_pressure(ctid)
            if reading is None:
                continue

            self._last_readings[ctid] = reading

            if reading.is_critical:
                critical_n += 1
                logger.error(
                    "CT %d CRITIQUE : RAM %.1f%%, PSI-full-10s=%.2f%% "
                    "(%d Ko / %d Ko) — action requise",
                    ctid, reading.usage_pct, reading.full_avg10,
                    reading.mem_current_kb, reading.mem_limit_kb,
                )
                if self.on_critical:
                    self.on_critical(reading)
                elif self.on_pressure:
                    self.on_pressure(reading)

            elif reading.is_under_pressure:
                pressured += 1
                logger.warning(
                    "CT %d sous pression : RAM %.1f%%, PSI-some-10s=%.2f%%",
                    ctid, reading.usage_pct, reading.some_avg10,
                )
                if self.on_pressure:
                    self.on_pressure(reading)

            else:
                logger.debug(
                    "CT %d ok : RAM %.1f%%, PSI-some-10s=%.2f%%",
                    ctid, reading.usage_pct, reading.some_avg10,
                )

        logger.info(
            "LXC poll : %d containers, %d sous pression, %d critiques",
            len(ctids), pressured, critical_n,
        )

    # ─── Métriques Prometheus ──────────────────────────────────────────────

    def prometheus_metrics(self, node_id: str) -> str:
        """Retourne les métriques PSI au format Prometheus."""
        lines = [
            "# HELP omega_lxc_mem_usage_pct Utilisation RAM container LXC (%)",
            "# TYPE omega_lxc_mem_usage_pct gauge",
            "# HELP omega_lxc_psi_some_avg10 PSI memory some avg10 (%)",
            "# TYPE omega_lxc_psi_some_avg10 gauge",
            "# HELP omega_lxc_psi_full_avg10 PSI memory full avg10 (%)",
            "# TYPE omega_lxc_psi_full_avg10 gauge",
        ]
        for ctid, r in self._last_readings.items():
            lbl = f'node="{node_id}",ctid="{ctid}"'
            lines.extend([
                f"omega_lxc_mem_usage_pct{{{lbl}}} {r.usage_pct:.2f}",
                f"omega_lxc_psi_some_avg10{{{lbl}}} {r.some_avg10:.4f}",
                f"omega_lxc_psi_full_avg10{{{lbl}}} {r.full_avg10:.4f}",
            ])
        return "\n".join(lines) + "\n"
