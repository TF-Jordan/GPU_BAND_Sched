"""
Benchmarks pytest-benchmark — controller Python.

Lancer :
    pytest controller/tests/test_benchmarks.py --benchmark-only -v

Comparer deux runs :
    pytest ... --benchmark-save=baseline
    pytest ... --benchmark-compare=baseline
"""

from __future__ import annotations

import socket
import struct
from unittest.mock import MagicMock, patch

import pytest
pytest.importorskip("pytest_benchmark", reason="pytest-benchmark non installé — pip install pytest-benchmark")

from controller.metrics import MemInfo, MemPressure, MetricsCollector
from controller.policy import PolicyEngine, PolicyInput
from controller.store_client import StoreClient, StoreStatus, poll_all_stores


# ─── Policy engine ────────────────────────────────────────────────────────────

def _make_mem(usage_pct: float) -> MemInfo:
    total = 8 * 1024 * 1024  # 8 Gio en Ko
    used  = int(total * usage_pct / 100)
    return MemInfo(
        total_kb      = total,
        free_kb       = total - used,
        available_kb  = total - used,
        buffers_kb    = 0,
        cached_kb     = 0,
        swap_total_kb = 2 * 1024 * 1024,
        swap_free_kb  = 1 * 1024 * 1024,
    )

def _make_psi(some: float = 0.0, full: float = 0.0) -> MemPressure:
    return MemPressure(
        some_avg10=some, some_avg60=some, some_avg300=some,
        full_avg10=full, full_avg60=full, full_avg300=full,
    )


def test_bench_policy_local_only(benchmark):
    """Politique en régime normal — chemin local_only."""
    engine = PolicyEngine()
    inp    = PolicyInput(mem=_make_mem(50.0), pressure=_make_psi(0.0))
    result = benchmark(engine.evaluate, inp)
    assert result.decision.value == "local_only"


def test_bench_policy_enable_remote(benchmark):
    """Politique sous pression modérée — chemin enable_remote."""
    engine = PolicyEngine()
    inp    = PolicyInput(mem=_make_mem(80.0), pressure=_make_psi(5.0))
    result = benchmark(engine.evaluate, inp)
    assert result.decision.value == "enable_remote"


def test_bench_policy_migrate(benchmark):
    """Politique en pression critique — chemin migrate."""
    engine = PolicyEngine()
    inp    = PolicyInput(mem=_make_mem(95.0), pressure=_make_psi(20.0, 8.0))
    inp.mem.swap_free_kb = 200_000  # swap presque plein
    result = benchmark(engine.evaluate, inp)
    assert result.decision.value == "migrate"


def test_bench_policy_throughput(benchmark):
    """Débit d'évaluations par seconde (cas le plus fréquent : local_only)."""
    engine = PolicyEngine()
    inp    = PolicyInput(mem=_make_mem(40.0), pressure=_make_psi(0.0))

    def evaluate_batch():
        for _ in range(1000):
            engine.evaluate(inp)

    benchmark(evaluate_batch)


# ─── MetricsCollector (lecture /proc simulée) ────────────────────────────────

MOCK_MEMINFO = """\
MemTotal:       8388608 kB
MemFree:        2097152 kB
MemAvailable:   3145728 kB
Buffers:         524288 kB
Cached:         1048576 kB
SwapTotal:      2097152 kB
SwapFree:       1048576 kB
"""

MOCK_PRESSURE = """\
some avg10=2.50 avg60=1.20 avg300=0.80 total=12345
full avg10=0.10 avg60=0.05 avg300=0.02 total=456
"""

def test_bench_metrics_read_meminfo(benchmark, tmp_path):
    """Lecture + parsing de /proc/meminfo (fichier réel sur tmpfs)."""
    f = tmp_path / "meminfo"
    f.write_text(MOCK_MEMINFO)
    collector = MetricsCollector(meminfo_path=str(f), pressure_path="/dev/null")
    result = benchmark(collector.read_meminfo)
    assert result.total_kb == 8_388_608


def test_bench_metrics_snapshot(benchmark, tmp_path):
    """Snapshot complet (meminfo + pressure)."""
    mf = tmp_path / "meminfo"
    pf = tmp_path / "pressure"
    mf.write_text(MOCK_MEMINFO)
    pf.write_text(MOCK_PRESSURE)
    collector = MetricsCollector(meminfo_path=str(mf), pressure_path=str(pf))
    result    = benchmark(collector.snapshot)
    assert "mem_usage_pct" in result


# ─── poll_all_stores parallèle ────────────────────────────────────────────────

def test_bench_poll_all_stores_parallel(benchmark):
    """
    poll_all_stores doit être sous-linéaire par rapport au nombre de stores.
    On mocke le socket pour ne pas dépendre d'un réseau réel.
    """
    PONG_FRAME = struct.pack(">HBBIQI",
        0x524D,   # magic
        0x02,     # Pong
        0,        # flags
        0,        # vm_id
        0,        # page_id
        0,        # payload_len
    )

    class FakeSocket:
        def __init__(self): self._buf = PONG_FRAME
        def settimeout(self, t): pass
        def setsockopt(self, *a): pass
        def connect(self, addr): pass
        def sendall(self, data): pass
        def recv(self, n):
            chunk, self._buf = self._buf[:n], self._buf[n:]
            return chunk
        def close(self): pass

    store_addrs = ["127.0.0.1:9100", "127.0.0.1:9101"]

    with patch("socket.socket", return_value=FakeSocket()):
        results = benchmark(poll_all_stores, store_addrs, 2.0)

    assert len(results) == 2
