"""Tests du collecteur de métriques (avec fichiers /proc mockés)."""

import pytest
import tempfile
import os

from controller.metrics import MetricsCollector


MEMINFO_SAMPLE = """\
MemTotal:       16384000 kB
MemFree:         2048000 kB
MemAvailable:    4096000 kB
Buffers:          512000 kB
Cached:          3072000 kB
SwapCached:            0 kB
Active:          8192000 kB
Inactive:        2048000 kB
SwapTotal:       4194304 kB
SwapFree:        3145728 kB
Dirty:              1024 kB
Writeback:             0 kB
AnonPages:       6291456 kB
Mapped:           524288 kB
Shmem:            131072 kB
"""

PRESSURE_SAMPLE = """\
some avg10=3.50 avg60=1.20 avg300=0.40 total=1234567
full avg10=0.80 avg60=0.30 avg300=0.10 total=234567
"""


class TestMemInfoParsing:
    def setup_method(self):
        self.tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".meminfo", delete=False)
        self.tmp.write(MEMINFO_SAMPLE)
        self.tmp.close()

    def teardown_method(self):
        os.unlink(self.tmp.name)

    def test_total_parsed(self):
        c   = MetricsCollector(meminfo_path=self.tmp.name)
        mem = c.read_meminfo()
        assert mem.total_kb == 16_384_000

    def test_available_parsed(self):
        c   = MetricsCollector(meminfo_path=self.tmp.name)
        mem = c.read_meminfo()
        assert mem.available_kb == 4_096_000

    def test_usage_pct_computed(self):
        c   = MetricsCollector(meminfo_path=self.tmp.name)
        mem = c.read_meminfo()
        # used = total - available = 12_288_000
        expected_pct = (12_288_000 / 16_384_000) * 100
        assert abs(mem.usage_pct - expected_pct) < 0.1

    def test_swap_usage_pct_computed(self):
        c   = MetricsCollector(meminfo_path=self.tmp.name)
        mem = c.read_meminfo()
        # swap_used = 4_194_304 - 3_145_728 = 1_048_576
        expected_swap_pct = (1_048_576 / 4_194_304) * 100
        assert abs(mem.swap_usage_pct - expected_swap_pct) < 0.1


class TestPressureParsing:
    def setup_method(self):
        self.tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".pressure", delete=False)
        self.tmp.write(PRESSURE_SAMPLE)
        self.tmp.close()

    def teardown_method(self):
        os.unlink(self.tmp.name)

    def test_some_avg10_parsed(self):
        c   = MetricsCollector(pressure_path=self.tmp.name)
        psi = c.read_pressure()
        assert psi is not None
        assert abs(psi.some_avg10 - 3.5) < 0.01

    def test_full_avg10_parsed(self):
        c   = MetricsCollector(pressure_path=self.tmp.name)
        psi = c.read_pressure()
        assert psi is not None
        assert abs(psi.full_avg10 - 0.8) < 0.01


class TestMissingPressure:
    def test_returns_none_when_no_file(self):
        c   = MetricsCollector(pressure_path="/nonexistent/pressure/memory")
        psi = c.read_pressure()
        assert psi is None


class TestSnapshot:
    def setup_method(self):
        self.mem_tmp = tempfile.NamedTemporaryFile(mode="w", delete=False)
        self.mem_tmp.write(MEMINFO_SAMPLE)
        self.mem_tmp.close()

    def teardown_method(self):
        os.unlink(self.mem_tmp.name)

    def test_snapshot_contains_expected_keys(self):
        c    = MetricsCollector(meminfo_path=self.mem_tmp.name, pressure_path="/nonexistent")
        snap = c.snapshot()
        assert "mem_total_kb"     in snap
        assert "mem_usage_pct"    in snap
        assert "swap_usage_pct"   in snap
        assert "timestamp"        in snap
