"""Tests du monitor mémoire LXC (cgroup v2 PSI) — L7."""

import time
import threading
import tempfile
import os
import pytest

from controller.lxc_monitor import (
    LxcMemoryPressure, CgroupV2Reader, LxcMemoryMonitor,
)


# ─── LxcMemoryPressure ────────────────────────────────────────────────────────

class TestLxcMemoryPressure:
    def make_pressure(self, some_avg10=0.0, full_avg10=0.0,
                      mem_current_kb=1024, mem_limit_kb=4096):
        return LxcMemoryPressure(
            ctid=100, some_avg10=some_avg10, some_avg60=some_avg10,
            full_avg10=full_avg10, full_avg60=full_avg10,
            mem_current_kb=mem_current_kb, mem_limit_kb=mem_limit_kb,
        )

    def test_not_under_pressure_when_all_zero(self):
        p = self.make_pressure()
        assert not p.is_under_pressure

    def test_under_pressure_when_some_avg10_high(self):
        p = self.make_pressure(some_avg10=1.0)
        assert p.is_under_pressure

    def test_under_pressure_when_full_avg10_nonzero(self):
        p = self.make_pressure(full_avg10=0.2)
        assert p.is_under_pressure

    def test_critical_when_full_avg10_very_high(self):
        p = self.make_pressure(full_avg10=2.0)
        assert p.is_critical

    def test_critical_when_usage_over_95pct(self):
        p = self.make_pressure(mem_current_kb=3900, mem_limit_kb=4000)
        assert p.is_critical

    def test_not_critical_when_normal(self):
        p = self.make_pressure(some_avg10=0.1, full_avg10=0.0,
                               mem_current_kb=2000, mem_limit_kb=4096)
        assert not p.is_critical

    def test_usage_pct_calculation(self):
        p = self.make_pressure(mem_current_kb=2048, mem_limit_kb=4096)
        assert abs(p.usage_pct - 50.0) < 0.01

    def test_usage_pct_zero_when_no_limit(self):
        p = self.make_pressure(mem_current_kb=2048, mem_limit_kb=0)
        assert p.usage_pct == 0.0

    def test_to_dict_contains_required_keys(self):
        p = self.make_pressure(some_avg10=0.5, full_avg10=0.1)
        d = p.to_dict()
        for key in ("ctid", "some_avg10", "full_avg10", "usage_pct",
                    "is_under_pressure", "is_critical"):
            assert key in d, f"clé manquante : {key}"

    def test_to_dict_values_match(self):
        p = self.make_pressure(some_avg10=1.5, full_avg10=0.3,
                               mem_current_kb=3000, mem_limit_kb=4096)
        d = p.to_dict()
        assert d["ctid"] == 100
        assert d["is_under_pressure"] is True
        assert d["is_critical"] is False


# ─── CgroupV2Reader ───────────────────────────────────────────────────────────

class TestCgroupV2Reader:
    """Tests avec des fichiers cgroup simulés dans un répertoire temporaire."""

    def _write_cgroup_files(self, base_dir, ctid,
                            pressure_content, current_kb, limit_kb):
        """Crée des fichiers cgroup simulés."""
        cgroup_dir = os.path.join(base_dir, str(ctid))
        os.makedirs(cgroup_dir, exist_ok=True)

        with open(os.path.join(cgroup_dir, "memory.pressure"), "w") as f:
            f.write(pressure_content)

        with open(os.path.join(cgroup_dir, "memory.current"), "w") as f:
            f.write(str(current_kb * 1024))

        limit_str = "max" if limit_kb == 0 else str(limit_kb * 1024)
        with open(os.path.join(cgroup_dir, "memory.max"), "w") as f:
            f.write(limit_str)

        return cgroup_dir

    def test_reads_pressure_from_files(self):
        with tempfile.TemporaryDirectory() as base:
            pressure = (
                "some avg10=0.50 avg60=0.20 avg300=0.05 total=1234\n"
                "full avg10=0.10 avg60=0.05 avg300=0.01 total=100\n"
            )
            self._write_cgroup_files(base, 100, pressure, 2048, 4096)

            reader = CgroupV2Reader(cgroup_base=os.path.join(base, "{ctid}"))
            result = reader.read_pressure(100)

        assert result is not None
        assert abs(result.some_avg10 - 0.50) < 0.001
        assert abs(result.full_avg10 - 0.10) < 0.001
        assert result.mem_current_kb == 2048
        assert result.mem_limit_kb == 4096

    def test_returns_none_for_unknown_ctid(self):
        with tempfile.TemporaryDirectory() as base:
            reader = CgroupV2Reader(cgroup_base=os.path.join(base, "{ctid}"))
            result = reader.read_pressure(999)
        assert result is None

    def test_handles_unlimited_memory(self):
        with tempfile.TemporaryDirectory() as base:
            pressure = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
            self._write_cgroup_files(base, 200, pressure, 1024, 0)  # limit=0 → "max"

            reader = CgroupV2Reader(cgroup_base=os.path.join(base, "{ctid}"))
            result = reader.read_pressure(200)

        assert result is not None
        assert result.mem_limit_kb == 0   # illimité
        assert result.usage_pct == 0.0   # pas de limite → 0%

    def test_parse_pressure_format(self):
        content = (
            "some avg10=1.23 avg60=0.45 avg300=0.12 total=9876\n"
            "full avg10=0.56 avg60=0.23 avg300=0.07 total=321\n"
        )
        parsed = CgroupV2Reader._parse_pressure(content)
        assert abs(parsed["some_avg10"] - 1.23) < 0.001
        assert abs(parsed["some_avg60"] - 0.45) < 0.001
        assert abs(parsed["full_avg10"] - 0.56) < 0.001

    def test_parse_pressure_handles_missing_lines(self):
        """Ne doit pas planter si une ligne manque."""
        content = "some avg10=0.10 avg60=0.05 avg300=0.00 total=10\n"
        parsed = CgroupV2Reader._parse_pressure(content)
        assert "some_avg10" in parsed
        assert "full_avg10" not in parsed   # absent = non parsé

    def test_read_int_kb_converts_bytes(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False) as f:
            f.write("4194304")   # 4 Mio en octets
            name = f.name
        try:
            from pathlib import Path
            result = CgroupV2Reader._read_int_kb(Path(name))
            assert result == 4096   # 4 Mio en Ko
        finally:
            os.unlink(name)


# ─── LxcMemoryMonitor ─────────────────────────────────────────────────────────

class TestLxcMemoryMonitor:
    def test_start_stop(self):
        """Le monitor doit démarrer et s'arrêter proprement."""
        monitor = LxcMemoryMonitor(poll_interval=1)
        monitor.start()
        time.sleep(0.1)
        monitor.stop()   # Ne doit pas bloquer

    def test_callback_called_on_pressure(self):
        """Le callback doit être appelé quand un container est sous pression."""
        from unittest.mock import patch

        with tempfile.TemporaryDirectory() as base:
            # Créer un container sous pression
            cgroup_dir = os.path.join(base, "100")
            os.makedirs(cgroup_dir)
            with open(os.path.join(cgroup_dir, "memory.pressure"), "w") as f:
                f.write("some avg10=2.00 avg60=1.00 avg300=0.50 total=99999\n"
                        "full avg10=0.50 avg60=0.20 avg300=0.10 total=1000\n")
            with open(os.path.join(cgroup_dir, "memory.current"), "w") as f:
                f.write(str(3900 * 1024))
            with open(os.path.join(cgroup_dir, "memory.max"), "w") as f:
                f.write(str(4096 * 1024))

            received = []

            def on_pressure(p):
                received.append(p)

            monitor = LxcMemoryMonitor(
                poll_interval      = 1,
                pressure_threshold = 0.5,
                on_pressure        = on_pressure,
                cgroup_base        = os.path.join(base, "{ctid}"),
            )

            # Patcher list_lxc_ctids pour retourner notre ctid de test
            with patch.object(monitor._reader, "list_lxc_ctids", return_value=[100]):
                monitor._poll_all()

        assert len(received) >= 1
        assert received[0].ctid == 100
        assert received[0].is_under_pressure

    def test_latest_readings_empty_initially(self):
        monitor = LxcMemoryMonitor()
        assert monitor.latest_readings() == {}

    def test_prometheus_metrics_format(self):
        monitor = LxcMemoryMonitor()
        # Injecter des lectures simulées
        monitor._last_readings[100] = LxcMemoryPressure(
            ctid=100, some_avg10=0.5, some_avg60=0.2,
            full_avg10=0.1, full_avg60=0.05,
            mem_current_kb=2048, mem_limit_kb=4096,
        )

        metrics = monitor.prometheus_metrics("pve-a")
        assert "omega_lxc_mem_usage_pct" in metrics
        assert "omega_lxc_psi_some_avg10" in metrics
        assert 'ctid="100"' in metrics
        assert 'node="pve-a"' in metrics
