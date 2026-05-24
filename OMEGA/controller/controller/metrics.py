"""
Collecte de métriques mémoire locales depuis /proc.

En V1, on lit directement /proc/meminfo et /proc/pressure/memory sur le nœud A.
En V2, les métriques seront collectées via un endpoint HTTP sur l'agent.
"""

from __future__ import annotations

import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


@dataclass
class MemInfo:
    """Snapshot de /proc/meminfo."""
    total_kb:      int
    free_kb:       int
    available_kb:  int
    buffers_kb:    int
    cached_kb:     int
    swap_total_kb: int
    swap_free_kb:  int
    timestamp:     float = field(default_factory=time.time)

    @property
    def used_kb(self) -> int:
        return self.total_kb - self.available_kb

    @property
    def usage_pct(self) -> float:
        if self.total_kb == 0:
            return 0.0
        return (self.used_kb / self.total_kb) * 100.0

    @property
    def swap_used_kb(self) -> int:
        return self.swap_total_kb - self.swap_free_kb

    @property
    def swap_usage_pct(self) -> float:
        if self.swap_total_kb == 0:
            return 0.0
        return (self.swap_used_kb / self.swap_total_kb) * 100.0


@dataclass
class MemPressure:
    """Snapshot de /proc/pressure/memory (PSI — Pressure Stall Information)."""
    # avg10, avg60, avg300 sont en pourcentage (0.0–100.0)
    some_avg10:  float
    some_avg60:  float
    some_avg300: float
    full_avg10:  float
    full_avg60:  float
    full_avg300: float
    timestamp:   float = field(default_factory=time.time)


class MetricsCollector:
    """
    Collecte les métriques mémoire du nœud local.

    Peut être mockée dans les tests en remplaçant les chemins /proc.
    """

    def __init__(
        self,
        meminfo_path:  str = "/proc/meminfo",
        pressure_path: str = "/proc/pressure/memory",
    ):
        self._meminfo_path  = Path(meminfo_path)
        self._pressure_path = Path(pressure_path)

    def read_meminfo(self) -> MemInfo:
        """Lit et parse /proc/meminfo."""
        raw = self._meminfo_path.read_text()
        fields: dict[str, int] = {}

        for line in raw.splitlines():
            m = re.match(r"^(\w+):\s+(\d+)", line)
            if m:
                fields[m.group(1)] = int(m.group(2))

        return MemInfo(
            total_kb      = fields.get("MemTotal", 0),
            free_kb       = fields.get("MemFree", 0),
            available_kb  = fields.get("MemAvailable", 0),
            buffers_kb    = fields.get("Buffers", 0),
            cached_kb     = fields.get("Cached", 0),
            swap_total_kb = fields.get("SwapTotal", 0),
            swap_free_kb  = fields.get("SwapFree", 0),
        )

    def read_pressure(self) -> Optional[MemPressure]:
        """
        Lit /proc/pressure/memory (PSI).

        Retourne None si le fichier n'existe pas (kernel < 4.20 ou CONFIG_PSI=n).
        """
        if not self._pressure_path.exists():
            return None

        raw = self._pressure_path.read_text()
        result: dict[str, dict[str, float]] = {}

        # Format : "some avg10=X.XX avg60=X.XX avg300=X.XX total=N"
        for line in raw.splitlines():
            parts = line.split()
            if not parts:
                continue
            kind = parts[0]  # "some" ou "full"
            vals: dict[str, float] = {}
            for part in parts[1:]:
                if part.startswith("avg"):
                    k, v = part.split("=")
                    vals[k] = float(v)
            result[kind] = vals

        some = result.get("some", {})
        full = result.get("full", {})

        return MemPressure(
            some_avg10  = some.get("avg10",  0.0),
            some_avg60  = some.get("avg60",  0.0),
            some_avg300 = some.get("avg300", 0.0),
            full_avg10  = full.get("avg10",  0.0),
            full_avg60  = full.get("avg60",  0.0),
            full_avg300 = full.get("avg300", 0.0),
        )

    def snapshot(self) -> dict:
        """Retourne un snapshot complet sous forme de dict (loggable/JSON)."""
        mem = self.read_meminfo()
        psi = self.read_pressure()

        data: dict = {
            "mem_total_kb":     mem.total_kb,
            "mem_available_kb": mem.available_kb,
            "mem_used_kb":      mem.used_kb,
            "mem_usage_pct":    round(mem.usage_pct, 1),
            "swap_usage_pct":   round(mem.swap_usage_pct, 1),
            "timestamp":        mem.timestamp,
        }
        if psi:
            data.update({
                "psi_some_avg10":  psi.some_avg10,
                "psi_some_avg60":  psi.some_avg60,
                "psi_full_avg10":  psi.full_avg10,
            })

        return data
