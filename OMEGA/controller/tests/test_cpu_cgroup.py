"""Tests du moniteur CPU cgroups v2."""

import collections
import time
from pathlib import Path

import pytest
from unittest.mock import patch

from controller.cpu_cgroup_monitor import (
    WINDOW_SIZE,
    CgroupCpuConfig,
    CgroupCpuController,
    CgroupCpuMonitor,
    CgroupCpuStat,
    VmCpuWindow,
)


# ─── Helpers ──────────────────────────────────────────────────────────────────

def make_vm_cgroup(tmp_path: Path, vm_id: int) -> Path:
    """Crée une structure cgroup v2 simulée pour une VM."""
    scope = tmp_path / "machine.slice" / f"machine-qemu-{vm_id}-pve.scope"
    scope.mkdir(parents=True)

    (scope / "cpu.weight").write_text("100\n")
    (scope / "cpu.max").write_text("max 1000000\n")
    (scope / "cpu.stat").write_text(
        "usage_usec 5000000\n"
        "user_usec 3000000\n"
        "system_usec 2000000\n"
        "nr_periods 50\n"
        "nr_throttled 5\n"
        "throttled_usec 250000\n"
        "nr_burst_periods 0\n"
        "burst_usec 0\n"
    )
    (scope / "cpuset.cpus").write_text("0-3\n")
    return scope


def make_full_window(vm_id: int, usage_pct: float = 50.0, throttle_ratio: float = 0.0) -> VmCpuWindow:
    """Crée une VmCpuWindow déjà pleine (100 échantillons)."""
    window = VmCpuWindow(vm_id=vm_id)
    t0 = time.monotonic() - WINDOW_SIZE * 0.001  # WINDOW_SIZE ms en arrière
    for i in range(WINDOW_SIZE):
        # Simuler un échantillon : on pousse directement dans usage_pcts et samples
        stat = CgroupCpuStat(
            vm_id=vm_id,
            usage_usec=i * 1000,
            nr_periods=i + 1,
            nr_throttled=int((i + 1) * throttle_ratio),
            timestamp=t0 + i * 0.001,
        )
        window.samples.append(stat)
        window.usage_pcts.append(usage_pct)
    return window


# ─── CgroupCpuStat ────────────────────────────────────────────────────────────

class TestCgroupCpuStat:
    def test_throttle_ratio(self):
        stat = CgroupCpuStat(vm_id=1, nr_periods=100, nr_throttled=10)
        assert abs(stat.throttle_ratio - 0.10) < 0.001

    def test_not_throttled_at_zero(self):
        stat = CgroupCpuStat(vm_id=1, nr_periods=0, nr_throttled=0)
        assert not stat.is_throttled
        assert stat.throttle_ratio == 0.0

    def test_is_throttled(self):
        stat = CgroupCpuStat(vm_id=1, nr_periods=50, nr_throttled=5)
        assert stat.is_throttled

    def test_vcpu_usage_pct_since(self):
        t0 = time.monotonic()
        prev = CgroupCpuStat(vm_id=1, usage_usec=0,         timestamp=t0)
        curr = CgroupCpuStat(vm_id=1, usage_usec=2_000_000, timestamp=t0 + 4.0)
        # 2_000_000 µs / 4_000_000 µs = 50% (d'un vCPU)
        pct = curr.vcpu_usage_pct_since(prev)
        assert abs(pct - 50.0) < 1.0

    def test_vcpu_usage_pct_two_vcpus(self):
        """2 vCPU saturés → 200% relatif au temps physique écoulé."""
        t0 = time.monotonic()
        prev = CgroupCpuStat(vm_id=1, usage_usec=0,         timestamp=t0)
        curr = CgroupCpuStat(vm_id=1, usage_usec=2_000_000, timestamp=t0 + 1.0)
        # 2_000_000 µs / 1_000_000 µs = 200% (2 vCPU à fond)
        pct = curr.vcpu_usage_pct_since(prev)
        assert abs(pct - 200.0) < 1.0

    def test_vcpu_usage_pct_zero_elapsed(self):
        t0 = time.monotonic()
        s = CgroupCpuStat(vm_id=1, usage_usec=1000, timestamp=t0)
        assert s.vcpu_usage_pct_since(s) == 0.0


# ─── VmCpuWindow ──────────────────────────────────────────────────────────────

class TestVmCpuWindow:
    def test_empty_window_returns_zero(self):
        w = VmCpuWindow(vm_id=1)
        assert w.avg_usage_pct == 0.0
        assert w.max_usage_pct == 0.0
        assert w.avg_throttle_ratio == 0.0
        assert w.latest is None

    def test_push_single_sample(self):
        t0 = time.monotonic()
        w = VmCpuWindow(vm_id=1)
        prev = CgroupCpuStat(vm_id=1, usage_usec=0,         timestamp=t0)
        curr = CgroupCpuStat(vm_id=1, usage_usec=1_000_000, timestamp=t0 + 1.0)
        w.push(curr, prev)
        assert len(w.samples) == 1
        assert abs(w.avg_usage_pct - 100.0) < 1.0
        assert w.latest is curr

    def test_is_full_after_window_size_pushes(self):
        t0 = time.monotonic()
        w = VmCpuWindow(vm_id=1)
        assert not w.is_full()
        prev = CgroupCpuStat(vm_id=1, usage_usec=0, timestamp=t0)
        for i in range(WINDOW_SIZE):
            curr = CgroupCpuStat(vm_id=1, usage_usec=(i + 1) * 1000, timestamp=t0 + (i + 1) * 0.001)
            w.push(curr, prev)
            prev = curr
        assert w.is_full()

    def test_sliding_window_evicts_old_samples(self):
        t0 = time.monotonic()
        w = VmCpuWindow(vm_id=1)
        # Remplir avec WINDOW_SIZE + 10 échantillons
        prev = CgroupCpuStat(vm_id=1, usage_usec=0, timestamp=t0)
        for i in range(WINDOW_SIZE + 10):
            curr = CgroupCpuStat(vm_id=1, usage_usec=(i + 1) * 1000, timestamp=t0 + (i + 1) * 0.001)
            w.push(curr, prev)
            prev = curr
        assert len(w.samples) == WINDOW_SIZE  # maxlen respecté

    def test_avg_throttle_ratio(self):
        w = make_full_window(vm_id=1, usage_pct=50.0, throttle_ratio=0.2)
        assert abs(w.avg_throttle_ratio - 0.2) < 0.05

    def test_max_usage_pct(self):
        w = VmCpuWindow(vm_id=1)
        w.usage_pcts.extend([10.0, 20.0, 90.0, 30.0])
        assert w.max_usage_pct == 90.0


# ─── CgroupCpuConfig ──────────────────────────────────────────────────────────

class TestCgroupCpuConfig:
    def test_num_vcpus_derives_quota(self):
        cfg = CgroupCpuConfig(vm_id=100, num_vcpus=4)
        assert cfg.quota_usec == 4_000_000
        assert cfg.period_usec == 1_000_000

    def test_cpu_max_str_limited(self):
        cfg = CgroupCpuConfig(vm_id=1, num_vcpus=2, period_usec=1_000_000)
        assert cfg.cpu_max_str() == "2000000 1000000"

    def test_cpu_max_str_unlimited(self):
        cfg = CgroupCpuConfig(vm_id=1, num_vcpus=None, period_usec=1_000_000)
        assert cfg.cpu_max_str() == "max 1000000"

    def test_default_weight(self):
        cfg = CgroupCpuConfig(vm_id=1)
        assert cfg.weight == 100

    def test_vcpu_description_limited(self):
        cfg = CgroupCpuConfig(vm_id=1, num_vcpus=4)
        assert "4" in cfg.vcpu_description()

    def test_vcpu_description_unlimited(self):
        cfg = CgroupCpuConfig(vm_id=1, num_vcpus=None)
        assert cfg.vcpu_description() == "illimité"

    def test_quota_usec_none_when_unlimited(self):
        cfg = CgroupCpuConfig(vm_id=1, num_vcpus=None)
        assert cfg.quota_usec is None


# ─── CgroupCpuController — lecture ───────────────────────────────────────────

class TestCgroupCpuControllerRead:
    def test_find_vm_cgroup(self, tmp_path):
        make_vm_cgroup(tmp_path, 101)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        cg = ctrl.find_vm_cgroup(101)
        assert cg is not None
        assert "101" in str(cg)

    def test_find_vm_cgroup_missing(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        assert ctrl.find_vm_cgroup(9999) is None

    def test_read_stat(self, tmp_path):
        make_vm_cgroup(tmp_path, 102)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        stat = ctrl.read_stat(102)
        assert stat is not None
        assert stat.usage_usec     == 5_000_000
        assert stat.user_usec      == 3_000_000
        assert stat.system_usec    == 2_000_000
        assert stat.nr_periods     == 50
        assert stat.nr_throttled   == 5
        assert stat.throttled_usec == 250_000

    def test_read_stat_missing_returns_none(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        assert ctrl.read_stat(9999) is None

    def test_read_weight(self, tmp_path):
        make_vm_cgroup(tmp_path, 103)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        assert ctrl.read_weight(103) == 100

    def test_read_max_unlimited(self, tmp_path):
        make_vm_cgroup(tmp_path, 104)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        quota, period = ctrl.read_max(104)
        assert quota is None
        assert period == 1_000_000

    def test_read_max_limited(self, tmp_path):
        make_vm_cgroup(tmp_path, 105)
        scope = tmp_path / "machine.slice" / "machine-qemu-105-pve.scope"
        (scope / "cpu.max").write_text("4000000 1000000\n")
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        quota, period = ctrl.read_max(105)
        assert quota  == 4_000_000
        assert period == 1_000_000

    def test_read_vcpu_count(self, tmp_path):
        make_vm_cgroup(tmp_path, 106)
        scope = tmp_path / "machine.slice" / "machine-qemu-106-pve.scope"
        (scope / "cpu.max").write_text("3000000 1000000\n")
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        assert ctrl.read_vcpu_count(106) == 3

    def test_read_vcpu_count_unlimited(self, tmp_path):
        make_vm_cgroup(tmp_path, 107)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        assert ctrl.read_vcpu_count(107) is None

    def test_list_active_vms(self, tmp_path):
        make_vm_cgroup(tmp_path, 200)
        make_vm_cgroup(tmp_path, 201)
        make_vm_cgroup(tmp_path, 202)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        vms = ctrl.list_active_vms()
        assert len(vms) == 3
        for vmid in [200, 201, 202]:
            assert vmid in vms

    def test_list_active_vms_empty(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        assert ctrl.list_active_vms() == []


# ─── CgroupCpuController — écriture ──────────────────────────────────────────

class TestCgroupCpuControllerWrite:
    def test_apply_weight(self, tmp_path):
        make_vm_cgroup(tmp_path, 103)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        cfg = CgroupCpuConfig(vm_id=103, weight=200)
        assert ctrl.apply(cfg)
        assert ctrl.read_weight(103) == 200

    def test_apply_quota_via_num_vcpus(self, tmp_path):
        make_vm_cgroup(tmp_path, 104)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        cfg = CgroupCpuConfig(vm_id=104, num_vcpus=2)
        assert ctrl.apply(cfg)
        quota, period = ctrl.read_max(104)
        assert quota  == 2_000_000
        assert period == 1_000_000

    def test_apply_cpuset(self, tmp_path):
        make_vm_cgroup(tmp_path, 105)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        cfg = CgroupCpuConfig(vm_id=105, cpuset="0-1")
        assert ctrl.apply(cfg)
        scope = tmp_path / "machine.slice" / "machine-qemu-105-pve.scope"
        assert (scope / "cpuset.cpus").read_text().strip() == "0-1"

    def test_apply_returns_false_for_missing_vm(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        cfg = CgroupCpuConfig(vm_id=9999)
        assert not ctrl.apply(cfg)

    def test_reset_restores_defaults(self, tmp_path):
        make_vm_cgroup(tmp_path, 106)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        # Appliquer une config restrictive
        ctrl.apply(CgroupCpuConfig(vm_id=106, num_vcpus=1, weight=50))
        assert ctrl.read_weight(106) == 50
        # Réinitialiser
        ctrl.reset(106)
        assert ctrl.read_weight(106) == 100

    def test_reset_clears_quota(self, tmp_path):
        make_vm_cgroup(tmp_path, 107)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        ctrl.apply(CgroupCpuConfig(vm_id=107, num_vcpus=2))
        quota_before, _ = ctrl.read_max(107)
        assert quota_before == 2_000_000
        ctrl.reset(107)
        quota_after, _ = ctrl.read_max(107)
        assert quota_after is None  # illimité après reset


# ─── CgroupCpuMonitor ─────────────────────────────────────────────────────────

class TestCgroupCpuMonitor:
    def test_callback_called_when_window_full_and_threshold_met(self, tmp_path):
        make_vm_cgroup(tmp_path, 300)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))

        pressures = []

        def on_pressure(vm_id, usage_pct, throttle_ratio):
            pressures.append((vm_id, usage_pct, throttle_ratio))

        monitor = CgroupCpuMonitor(
            controller         = ctrl,
            poll_interval      = 0.001,
            on_pressure        = on_pressure,
            usage_threshold    = 0.0,  # toujours déclencher
            throttle_threshold = 0.0,
        )

        # Pré-remplir la fenêtre avec 99 échantillons (manque 1 pour être pleine)
        monitor._windows[300] = make_full_window(300, usage_pct=90.0, throttle_ratio=0.2)
        # Retirer 1 échantillon pour simuler window à 99/100
        monitor._windows[300].samples.pop()
        monitor._windows[300].usage_pcts.pop()

        # Insérer un stat précédent
        t0 = time.monotonic() - 0.001
        monitor._prev_stats[300] = CgroupCpuStat(
            vm_id       = 300,
            usage_usec  = 4_000_000,  # légèrement moins que le 5_000_000 du fichier
            nr_periods  = 40,
            nr_throttled = 4,
            timestamp   = t0,
        )

        with patch.object(ctrl, "list_active_vms", return_value=[300]):
            monitor._poll_all()

        assert len(pressures) == 1
        assert pressures[0][0] == 300

    def test_no_callback_below_threshold(self, tmp_path):
        make_vm_cgroup(tmp_path, 301)
        # Réécrire cpu.stat pour avoir 0 throttling et 0 usage
        scope = tmp_path / "machine.slice" / "machine-qemu-301-pve.scope"
        (scope / "cpu.stat").write_text(
            "usage_usec 0\nuser_usec 0\nsystem_usec 0\n"
            "nr_periods 100\nnr_throttled 0\nthrottled_usec 0\n"
        )
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        pressures = []
        monitor = CgroupCpuMonitor(
            controller          = ctrl,
            poll_interval       = 0.001,
            on_pressure         = lambda *a: pressures.append(a),
            usage_threshold     = 80.0,
            throttle_threshold  = 0.1,
        )

        # Fenêtre pleine avec usage et throttle à zéro
        monitor._windows[301] = make_full_window(301, usage_pct=0.0, throttle_ratio=0.0)
        # Retirer 1 pour que _poll_all le complète
        monitor._windows[301].samples.pop()
        monitor._windows[301].usage_pcts.pop()

        t0 = time.monotonic() - 0.001
        monitor._prev_stats[301] = CgroupCpuStat(
            vm_id=301, usage_usec=0, timestamp=t0
        )
        with patch.object(ctrl, "list_active_vms", return_value=[301]):
            monitor._poll_all()
        assert len(pressures) == 0

    def test_no_callback_on_first_poll_no_prev(self, tmp_path):
        """Sans stat précédente, pas de callback (pas de delta)."""
        make_vm_cgroup(tmp_path, 302)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        pressures = []
        monitor = CgroupCpuMonitor(
            ctrl,
            on_pressure=lambda *a: pressures.append(a),
            usage_threshold=0.0,
            throttle_threshold=0.0,
        )
        with patch.object(ctrl, "list_active_vms", return_value=[302]):
            monitor._poll_all()
        assert len(pressures) == 0

    def test_start_stop(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl    = CgroupCpuController(cgroup_root=str(tmp_path))
        monitor = CgroupCpuMonitor(ctrl, poll_interval=0.05)
        monitor.start()
        time.sleep(0.15)
        monitor.stop()
        # Ne doit pas lever d'exception

    def test_snapshot_empty_initially(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl    = CgroupCpuController(cgroup_root=str(tmp_path))
        monitor = CgroupCpuMonitor(ctrl)
        assert monitor.snapshot() == {}

    def test_snapshot_contains_metrics(self, tmp_path):
        make_vm_cgroup(tmp_path, 303)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        monitor = CgroupCpuMonitor(ctrl)
        monitor._windows[303] = make_full_window(303, usage_pct=75.0)
        snap = monitor.snapshot()
        assert 303 in snap
        assert abs(snap[303]["avg_usage_pct"] - 75.0) < 1.0
        assert snap[303]["samples"] == WINDOW_SIZE

    def test_vm_window_returns_none_for_unknown(self, tmp_path):
        (tmp_path / "machine.slice").mkdir()
        ctrl    = CgroupCpuController(cgroup_root=str(tmp_path))
        monitor = CgroupCpuMonitor(ctrl)
        assert monitor.vm_window(9999) is None

    def test_vm_window_returns_window_after_polling(self, tmp_path):
        make_vm_cgroup(tmp_path, 304)
        ctrl = CgroupCpuController(cgroup_root=str(tmp_path))
        monitor = CgroupCpuMonitor(ctrl)
        monitor._windows[304] = make_full_window(304)
        assert monitor.vm_window(304) is not None
