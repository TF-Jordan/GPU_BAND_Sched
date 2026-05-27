"""Tests de la politique de paging."""

import pytest
from controller.policy import PolicyEngine, PolicyInput, Decision
from controller.metrics import MemInfo, MemPressure


def make_mem(usage_pct: float, swap_pct: float = 0.0) -> MemInfo:
    """Construit un MemInfo avec un usage mémoire donné.

    On utilise un total de 10_000_000 Ko (multiple de 100) pour que les
    pourcentages se représentent sans erreur de troncature entière.
    """
    total = 10_000_000  # Ko — divisible par 100, évite la précision flottante
    used  = int(total * usage_pct / 100)
    avail = total - used

    swap_total = 4_000_000  # Ko
    swap_used  = int(swap_total * swap_pct / 100)

    return MemInfo(
        total_kb      = total,
        free_kb       = avail // 2,
        available_kb  = avail,
        buffers_kb    = 0,
        cached_kb     = 0,
        swap_total_kb = swap_total,
        swap_free_kb  = swap_total - swap_used,
    )


def make_pressure(some_avg10: float = 0.0, full_avg10: float = 0.0) -> MemPressure:
    return MemPressure(
        some_avg10  = some_avg10,
        some_avg60  = some_avg10,
        some_avg300 = some_avg10,
        full_avg10  = full_avg10,
        full_avg60  = full_avg10,
        full_avg300 = full_avg10,
    )


class TestPolicyEngine:
    engine = PolicyEngine(
        threshold_enable_pct  = 70.0,
        threshold_migrate_pct = 90.0,
        psi_some_warn         = 10.0,
        psi_full_critical     = 5.0,
        swap_critical_pct     = 80.0,
    )

    def _eval(self, usage_pct: float, swap_pct: float = 0.0,
              psi_some: float = 0.0, psi_full: float = 0.0) -> Decision:
        inp = PolicyInput(
            mem      = make_mem(usage_pct, swap_pct),
            pressure = make_pressure(psi_some, psi_full),
        )
        return self.engine.evaluate(inp).decision

    # ------------------------------------------------------------------ local_only

    def test_low_usage_is_local_only(self):
        assert self._eval(50.0) == Decision.LOCAL_ONLY

    def test_usage_at_threshold_minus_one_is_local_only(self):
        assert self._eval(69.9) == Decision.LOCAL_ONLY

    def test_zero_usage_is_local_only(self):
        assert self._eval(0.0) == Decision.LOCAL_ONLY

    # ------------------------------------------------------------------ enable_remote

    def test_usage_at_threshold_is_enable_remote(self):
        assert self._eval(70.0) == Decision.ENABLE_REMOTE

    def test_usage_above_threshold_is_enable_remote(self):
        assert self._eval(80.0) == Decision.ENABLE_REMOTE

    def test_high_psi_some_triggers_enable_remote(self):
        # PSI some élevé même si RAM OK → enable_remote
        assert self._eval(60.0, psi_some=15.0) == Decision.ENABLE_REMOTE

    def test_usage_below_migrate_without_signals_is_enable_remote(self):
        # 89% → under migrate threshold → enable_remote (pas migrate)
        assert self._eval(89.0) == Decision.ENABLE_REMOTE

    # ------------------------------------------------------------------ migrate

    def test_full_critical_signals_trigger_migrate(self):
        # RAM > 90%, swap > 80%, PSI full > 5% → migrate
        result = self._eval(92.0, swap_pct=85.0, psi_full=8.0)
        assert result == Decision.MIGRATE

    def test_two_signals_enough_for_migrate(self):
        # RAM > 90% + swap > 80% sans PSI → migrate (2/3 signaux)
        result = self._eval(91.0, swap_pct=82.0, psi_full=0.0)
        assert result == Decision.MIGRATE

    def test_one_signal_not_enough_for_migrate(self):
        # Seulement RAM > 90%, swap et PSI OK → enable_remote
        result = self._eval(91.0, swap_pct=10.0, psi_full=0.0)
        assert result == Decision.ENABLE_REMOTE

    # ------------------------------------------------------------------ propriétés du résultat

    def test_output_has_reason(self):
        inp = PolicyInput(mem=make_mem(75.0), pressure=make_pressure())
        out = self.engine.evaluate(inp)
        assert out.reason
        assert len(out.reason) > 10

    def test_output_confidence_in_range(self):
        for usage in [10.0, 50.0, 70.0, 85.0, 95.0]:
            inp = PolicyInput(mem=make_mem(usage), pressure=make_pressure())
            out = self.engine.evaluate(inp)
            assert 0.0 <= out.confidence <= 1.0, f"confidence hors plage pour usage={usage}"

    def test_output_to_dict(self):
        inp = PolicyInput(mem=make_mem(75.0), pressure=make_pressure())
        out = self.engine.evaluate(inp)
        d   = out.to_dict()
        assert "decision" in d
        assert "reason"   in d
        assert "confidence" in d
        assert "timestamp"  in d

    def test_describe_thresholds(self):
        desc = self.engine.describe_thresholds()
        assert desc["threshold_enable_pct"]  == 70.0
        assert desc["threshold_migrate_pct"] == 90.0

    # ------------------------------------------------------------------ edge cases

    def test_no_pressure_info(self):
        """Fonctionne sans PSI (kernel trop vieux ou non configuré)."""
        inp = PolicyInput(mem=make_mem(75.0), pressure=None)
        out = self.engine.evaluate(inp)
        assert out.decision in (Decision.LOCAL_ONLY, Decision.ENABLE_REMOTE, Decision.MIGRATE)

    def test_custom_thresholds(self):
        """Les seuils personnalisés sont respectés."""
        strict_engine = PolicyEngine(threshold_enable_pct=50.0, threshold_migrate_pct=75.0)
        inp = PolicyInput(mem=make_mem(60.0), pressure=make_pressure())
        out = strict_engine.evaluate(inp)
        assert out.decision == Decision.ENABLE_REMOTE
