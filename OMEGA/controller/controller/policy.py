"""
Moteur de politique de paging distant.

# Décisions possibles

| Décision        | Signification                                              |
|-----------------|-------------------------------------------------------------|
| local_only      | Pas de paging distant — mémoire locale suffisante          |
| enable_remote   | Activer/maintenir le paging distant                        |
| migrate         | Migrer la VM vers un nœud avec plus de RAM disponible      |

# Architecture de la politique (extensible)

La classe `PolicyEngine` est conçue pour être facilement étendue :
- Les seuils sont configurables.
- On peut ajouter des stratégies via des sous-classes ou des plugins.
- En V2, la décision `migrate` déclenchera un appel à l'API Proxmox.
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from enum import Enum
from typing import Optional

from .metrics import MemInfo, MemPressure


class Decision(str, Enum):
    LOCAL_ONLY     = "local_only"
    ENABLE_REMOTE  = "enable_remote"
    MIGRATE        = "migrate"


@dataclass
class PolicyInput:
    """Données d'entrée pour une évaluation de politique."""
    mem:      MemInfo
    pressure: Optional[MemPressure]
    # Extensions futures :
    # vm_count: int = 0
    # remote_store_capacity_pct: float = 0.0
    # node_id: str = ""


@dataclass
class PolicyOutput:
    """Résultat d'une évaluation de politique."""
    decision:   Decision
    reason:     str
    confidence: float   # 0.0 – 1.0 (indicatif)
    timestamp:  float   = 0.0

    def __post_init__(self):
        if self.timestamp == 0.0:
            self.timestamp = time.time()

    def to_dict(self) -> dict:
        return {
            "decision":   self.decision.value,
            "reason":     self.reason,
            "confidence": round(self.confidence, 2),
            "timestamp":  self.timestamp,
        }


class PolicyEngine:
    """
    Moteur de politique de paging distant.

    Seuils par défaut :
    - mem_usage < 70%    → local_only
    - 70% ≤ mem_usage < 90% → enable_remote
    - mem_usage ≥ 90%   → migrate (si swap aussi saturé) ou enable_remote

    Les seuils PSI renforcent la décision si la pression est détectée.
    """

    def __init__(
        self,
        threshold_enable_pct:  float = 70.0,  # % RAM au-delà duquel on active le remote
        threshold_migrate_pct: float = 90.0,  # % RAM au-delà duquel on conseille la migration
        psi_some_warn:         float = 10.0,  # % PSI some avg10 → signal de pression
        psi_full_critical:     float = 5.0,   # % PSI full avg10 → pression critique
        swap_critical_pct:     float = 80.0,  # % swap utilisé → critique
    ):
        self.threshold_enable_pct  = threshold_enable_pct
        self.threshold_migrate_pct = threshold_migrate_pct
        self.psi_some_warn         = psi_some_warn
        self.psi_full_critical     = psi_full_critical
        self.swap_critical_pct     = swap_critical_pct

    def evaluate(self, inp: PolicyInput) -> PolicyOutput:
        """
        Évalue la situation mémoire et retourne une décision.

        La logique est intentionnellement lisible et linéaire pour être
        facilement auditée et modifiée sans risque d'effet de bord.
        """
        usage_pct  = inp.mem.usage_pct
        swap_pct   = inp.mem.swap_usage_pct
        psi_some   = inp.pressure.some_avg10 if inp.pressure else 0.0
        psi_full   = inp.pressure.full_avg10 if inp.pressure else 0.0

        # ----------------------------------------------------------------
        # Cas 1 : mémoire confortable → rien à faire
        # ----------------------------------------------------------------
        if usage_pct < self.threshold_enable_pct and psi_some < self.psi_some_warn:
            return PolicyOutput(
                decision   = Decision.LOCAL_ONLY,
                reason     = (
                    f"RAM {usage_pct:.1f}% < seuil {self.threshold_enable_pct}% "
                    f"et PSI-some {psi_some:.1f}% < {self.psi_some_warn}%"
                ),
                confidence = 1.0 - (usage_pct / self.threshold_enable_pct) * 0.5,
            )

        # ----------------------------------------------------------------
        # Cas 2 : pression critique → recommander migration
        #
        # On recommande la migration si TOUS ces signaux sont présents :
        # - RAM au-delà du seuil migrate
        # - swap très utilisé
        # - PSI full élevé (threads bloqués en attente de mémoire)
        # ----------------------------------------------------------------
        migrate_signals = [
            usage_pct  >= self.threshold_migrate_pct,
            swap_pct   >= self.swap_critical_pct,
            psi_full   >= self.psi_full_critical,
        ]
        num_signals = sum(migrate_signals)

        if num_signals >= 2:
            return PolicyOutput(
                decision   = Decision.MIGRATE,
                reason     = (
                    f"pression critique : RAM {usage_pct:.1f}%, "
                    f"swap {swap_pct:.1f}%, PSI-full {psi_full:.1f}% "
                    f"({num_signals}/3 signaux critiques)"
                ),
                confidence = min(1.0, num_signals / 3 + 0.2),
            )

        # ----------------------------------------------------------------
        # Cas 3 : pression modérée → activer/maintenir le remote paging
        # ----------------------------------------------------------------
        return PolicyOutput(
            decision   = Decision.ENABLE_REMOTE,
            reason     = (
                f"RAM {usage_pct:.1f}% ≥ seuil {self.threshold_enable_pct}% "
                f"ou PSI-some {psi_some:.1f}% ≥ {self.psi_some_warn}%"
            ),
            confidence = min(1.0, (usage_pct - self.threshold_enable_pct) /
                         (self.threshold_migrate_pct - self.threshold_enable_pct + 0.1) + 0.4),
        )

    def describe_thresholds(self) -> dict:
        """Retourne la configuration de la politique (pour les logs)."""
        return {
            "threshold_enable_pct":  self.threshold_enable_pct,
            "threshold_migrate_pct": self.threshold_migrate_pct,
            "psi_some_warn":         self.psi_some_warn,
            "psi_full_critical":     self.psi_full_critical,
            "swap_critical_pct":     self.swap_critical_pct,
        }
