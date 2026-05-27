"""Tests de la politique de migration — live vs cold, choix de cible."""

import time
import pytest

from controller.migration_policy import (
    MigrationCandidate,
    MigrationPolicy,
    MigrationReason,
    MigrationThresholds,
    MigrationType,
    NodeState,
    VmState,
)


# ─── Helpers ──────────────────────────────────────────────────────────────────

def make_node(
    node_id:   str,
    ram_pct:   float,
    vcpu_used: int = 10,
    vcpu_total: int = 24,
    gpu_total: int = 0,
    gpu_free: int = 0,
    disk_pressure: float = 0.0,
    vms: list = None,
) -> NodeState:
    total_kb = 32 * 1024 * 1024  # 32 Go
    used_kb  = int(total_kb * ram_pct / 100)
    avail_kb = total_kb - used_kb
    return NodeState(
        node_id         = node_id,
        mem_total_kb    = total_kb,
        mem_available_kb = avail_kb,
        vcpu_total      = vcpu_total,
        vcpu_free       = vcpu_total - vcpu_used,
        gpu_total_vram_mib = gpu_total,
        gpu_free_vram_mib = gpu_free,
        disk_pressure_pct = disk_pressure,
        local_vms       = vms or [],
    )


def make_vm(
    vm_id:       int,
    status:      str   = "running",
    max_mem_mib: int   = 4096,
    avg_cpu_pct: float = 50.0,
    throttle:    float = 0.0,
    remote_pct:  float = 0.0,
    gpu_budget:  int   = 0,
    idle_since:  float = None,
) -> VmState:
    total_pages  = max_mem_mib * 256
    remote_pages = int(total_pages * remote_pct / 100)
    return VmState(
        vm_id          = vm_id,
        status         = status,
        max_mem_mib    = max_mem_mib,
        rss_kb         = max_mem_mib * 1024,
        remote_pages   = remote_pages,
        avg_cpu_pct    = avg_cpu_pct,
        throttle_ratio = throttle,
        gpu_vram_budget_mib = gpu_budget,
        idle_since     = idle_since,
    )


def default_policy() -> MigrationPolicy:
    return MigrationPolicy(MigrationThresholds())


# ─── pick_migration_type ──────────────────────────────────────────────────────

class TestPickMigrationType:
    def test_stopped_vm_always_cold(self):
        policy = default_policy()
        vm = make_vm(1, status="stopped", avg_cpu_pct=0.0)
        assert policy.pick_migration_type(vm, node_ram_pct=50.0) == MigrationType.COLD

    def test_active_vm_normal_pressure_live(self):
        policy = default_policy()
        vm = make_vm(1, status="running", avg_cpu_pct=80.0)
        assert policy.pick_migration_type(vm, node_ram_pct=87.0) == MigrationType.LIVE

    def test_active_vm_critical_pressure_cold(self):
        """RAM > 95% → cold forcée même si VM active."""
        policy = default_policy()
        vm = make_vm(1, status="running", avg_cpu_pct=70.0)
        assert policy.pick_migration_type(vm, node_ram_pct=96.0) == MigrationType.COLD

    def test_idle_running_vm_cold(self):
        """VM running mais idle depuis > 60s → cold acceptable."""
        policy = default_policy()
        idle_since = time.monotonic() - 90.0  # idle depuis 90s
        vm = make_vm(1, status="running", avg_cpu_pct=1.0, idle_since=idle_since)
        assert policy.pick_migration_type(vm, node_ram_pct=87.0) == MigrationType.COLD

    def test_idle_too_short_still_live(self):
        """VM idle mais depuis seulement 30s → pas assez → live."""
        policy = default_policy()
        idle_since = time.monotonic() - 30.0  # seuil = 60s
        vm = make_vm(1, status="running", avg_cpu_pct=2.0, idle_since=idle_since)
        assert policy.pick_migration_type(vm, node_ram_pct=87.0) == MigrationType.LIVE

    def test_vm_active_not_idle(self):
        """VM à 50% CPU ne doit pas être classée idle."""
        policy = default_policy()
        idle_since = time.monotonic() - 120.0
        vm = make_vm(1, status="running", avg_cpu_pct=50.0, idle_since=idle_since)
        assert policy.pick_migration_type(vm, node_ram_pct=87.0) == MigrationType.LIVE


# ─── Sélection de cible ───────────────────────────────────────────────────────

class TestTargetSelection:
    def test_picks_node_with_most_free_ram(self):
        policy = default_policy()
        vm = make_vm(1, max_mem_mib=2048)
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=70.0),
            "node-c": make_node("node-c", ram_pct=50.0),  # meilleur
        }
        target = policy._best_target("node-a", vm, nodes)
        assert target == "node-c"

    def test_no_target_when_all_nodes_full(self):
        policy = default_policy()
        vm = make_vm(1, max_mem_mib=16000)  # très grosse VM
        nodes = {
            "node-a": make_node("node-a", ram_pct=90.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=85.0),  # trop plein pour accepter
            "node-c": make_node("node-c", ram_pct=82.0),  # trop plein aussi
        }
        # La VM demande 16 Go → les nœuds B et C ne peuvent pas absorber
        target = policy._best_target("node-a", vm, nodes)
        assert target is None

    def test_excludes_source_from_targets(self):
        policy = default_policy()
        vm = make_vm(1, max_mem_mib=1024)
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0),
        }
        target = policy._best_target("node-a", vm, nodes)
        assert target == "node-b"
        assert target != "node-a"

    def test_vcpu_target_picks_most_free(self):
        policy = default_policy()
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, vcpu_used=22, vcpu_total=24),
            "node-b": make_node("node-b", ram_pct=40.0, vcpu_used=5,  vcpu_total=24),
            "node-c": make_node("node-c", ram_pct=40.0, vcpu_used=12, vcpu_total=24),
        }
        target = policy._best_target_vcpu("node-a", make_vm(99), nodes)
        assert target == "node-b"  # 19 vCPU libres vs 12 pour node-c

    def test_target_selection_penalizes_disk_pressure(self):
        policy = default_policy()
        vm = make_vm(1, max_mem_mib=2048)
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=35.0, disk_pressure=1.0),
            "node-c": make_node("node-c", ram_pct=20.0, disk_pressure=95.0),
        }
        target = policy._best_target("node-a", vm, nodes)
        assert target == "node-b"

    def test_gpu_target_selection_prefers_healthy_gpu_node(self):
        policy = default_policy()
        vm = make_vm(1, max_mem_mib=2048, gpu_budget=2048)
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, gpu_total=8192, gpu_free=1024, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=20.0, gpu_total=8192, gpu_free=6144, disk_pressure=95.0),
            "node-c": make_node("node-c", ram_pct=35.0, gpu_total=8192, gpu_free=4096, disk_pressure=1.0),
        }
        target = policy._best_target("node-a", vm, nodes)
        assert target == "node-c"


# ─── evaluate — scénarios complets ───────────────────────────────────────────

class TestEvaluateScenarios:
    def test_no_migration_when_cluster_healthy(self):
        policy = default_policy()
        nodes = {
            "node-a": make_node("node-a", ram_pct=40.0),
            "node-b": make_node("node-b", ram_pct=50.0),
            "node-c": make_node("node-c", ram_pct=35.0),
        }
        assert policy.evaluate(nodes) == []

    def test_memory_pressure_triggers_migration(self):
        policy = default_policy()
        vm = make_vm(101, status="running", avg_cpu_pct=60.0, max_mem_mib=4096)
        nodes = {
            "node-a": make_node("node-a", ram_pct=90.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=40.0),
        }
        candidates = policy.evaluate(nodes)
        assert len(candidates) == 1
        assert candidates[0].vm.vm_id == 101
        assert candidates[0].reason == MigrationReason.MEMORY_PRESSURE
        assert candidates[0].mtype == MigrationType.LIVE  # VM active
        assert candidates[0].target == "node-b"

    def test_critical_pressure_forces_cold(self):
        policy = default_policy()
        vm = make_vm(102, status="running", avg_cpu_pct=40.0, max_mem_mib=4096)
        nodes = {
            "node-a": make_node("node-a", ram_pct=96.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0),
        }
        candidates = policy.evaluate(nodes)
        assert len(candidates) == 1
        assert candidates[0].mtype == MigrationType.COLD
        assert candidates[0].urgency == 2  # urgente

    def test_idle_vm_gets_cold_on_high_pressure(self):
        policy = default_policy()
        idle_since = time.monotonic() - 120.0
        vm = make_vm(103, status="running", avg_cpu_pct=1.0,
                     idle_since=idle_since, max_mem_mib=2048)
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0),
        }
        candidates = policy.evaluate(nodes)
        assert len(candidates) == 1
        assert candidates[0].mtype == MigrationType.COLD

    def test_excessive_remote_paging_triggers_migration(self):
        policy = default_policy()
        vm = make_vm(104, status="running", avg_cpu_pct=20.0,
                     max_mem_mib=8192, remote_pct=70.0)  # 70% en distant
        nodes = {
            "node-a": make_node("node-a", ram_pct=60.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=20.0),
        }
        candidates = policy.evaluate(nodes)
        assert len(candidates) == 1
        assert candidates[0].reason == MigrationReason.EXCESSIVE_REMOTE_PAGING
        assert candidates[0].vm.vm_id == 104

    def test_cpu_saturation_triggers_live_migration(self):
        policy = default_policy()
        vm = make_vm(105, status="running", avg_cpu_pct=90.0, throttle=0.40)
        nodes = {
            "node-a": make_node("node-a", ram_pct=50.0, vcpu_used=22, vcpu_total=24, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0, vcpu_used=5,  vcpu_total=24),
        }
        candidates = policy.evaluate(nodes)
        cpu_candidates = [c for c in candidates if c.reason == MigrationReason.CPU_SATURATION]
        assert len(cpu_candidates) == 1
        assert cpu_candidates[0].mtype == MigrationType.LIVE  # VM throttlée = active

    def test_gpu_budget_filters_invalid_targets(self):
        policy = default_policy()
        vm = make_vm(108, status="running", gpu_budget=4096)
        nodes = {
            "node-a": make_node("node-a", ram_pct=88.0, gpu_total=8192, gpu_free=1024, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0, gpu_total=8192, gpu_free=2048),
            "node-c": make_node("node-c", ram_pct=30.0, gpu_total=8192, gpu_free=6144),
        }
        target = policy._best_target("node-a", vm, nodes)
        assert target == "node-c"

    def test_gpu_saturation_triggers_migration(self):
        policy = default_policy()
        vm = make_vm(109, status="running", gpu_budget=4096, avg_cpu_pct=30.0)
        nodes = {
            "node-a": make_node("node-a", ram_pct=40.0, gpu_total=8192, gpu_free=256, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=25.0, gpu_total=8192, gpu_free=6144),
        }
        candidates = policy.evaluate(nodes)
        gpu_candidates = [c for c in candidates if c.reason == MigrationReason.GPU_SATURATION]
        assert len(gpu_candidates) == 1
        assert gpu_candidates[0].target == "node-b"

    def test_stopped_vm_gets_cold(self):
        policy = default_policy()
        vm = make_vm(106, status="stopped", avg_cpu_pct=0.0, max_mem_mib=4096)
        nodes = {
            "node-a": make_node("node-a", ram_pct=90.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0),
        }
        candidates = policy.evaluate(nodes)
        assert any(c.mtype == MigrationType.COLD for c in candidates)

    def test_each_vm_appears_at_most_once(self):
        """Même si plusieurs raisons s'accumulent, la VM ne migre qu'une fois."""
        policy = default_policy()
        vm = make_vm(107, status="running", avg_cpu_pct=60.0,
                     max_mem_mib=4096, remote_pct=65.0)
        nodes = {
            "node-a": make_node("node-a", ram_pct=92.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=25.0),
        }
        candidates = policy.evaluate(nodes)
        vm_ids = [c.vm.vm_id for c in candidates]
        assert vm_ids.count(107) == 1

    def test_urgency_sort_puts_critical_first(self):
        policy = default_policy()
        vm_low  = make_vm(201, status="running", avg_cpu_pct=20.0, max_mem_mib=2048)
        vm_high = make_vm(202, status="running", avg_cpu_pct=60.0, max_mem_mib=4096)
        nodes = {
            "node-a": make_node("node-a", ram_pct=87.0, vms=[vm_low]),   # haute
            "node-b": make_node("node-b", ram_pct=97.0, vms=[vm_high]),  # critique
            "node-c": make_node("node-c", ram_pct=20.0),
        }
        candidates = policy.evaluate(nodes)
        assert len(candidates) >= 2
        # La VM critique (node-b, 97%) doit apparaître en premier
        assert candidates[0].urgency == 2

    def test_to_api_payload_format(self):
        policy = default_policy()
        vm = make_vm(301, max_mem_mib=2048)
        nodes = {
            "node-a": make_node("node-a", ram_pct=90.0, vms=[vm]),
            "node-b": make_node("node-b", ram_pct=30.0),
        }
        candidates = policy.evaluate(nodes)
        assert len(candidates) > 0
        payload = candidates[0].to_api_payload()
        assert "vm_id"  in payload
        assert "source" in payload
        assert "target" in payload
        assert "type"   in payload
        assert "reason" in payload
        assert payload["type"] in ("live", "cold")


# ─── NodeState helpers ────────────────────────────────────────────────────────

class TestNodeState:
    def test_ram_used_pct(self):
        node = make_node("n", ram_pct=75.0)
        assert abs(node.ram_used_pct - 75.0) < 0.5

    def test_can_accept_small_vm(self):
        node = make_node("n", ram_pct=50.0)
        vm = make_vm(1, max_mem_mib=2048)  # 2 Go dans 32 Go à 50%
        assert node.can_accept_vm(vm)

    def test_cannot_accept_vm_when_would_exceed_80pct(self):
        # nœud à 70% → ajouter 20% → dépasse 80%
        total_kb = 32 * 1024 * 1024
        vm_mib = int(total_kb * 0.15 / 1024)  # ~15% du nœud
        node = make_node("n", ram_pct=70.0)
        vm = make_vm(1, max_mem_mib=vm_mib)
        # 70% + 15% = 85% > 80% → ne peut pas accueillir
        assert not node.can_accept_vm(vm)

    def test_placement_score_higher_for_less_loaded(self):
        n1 = make_node("a", ram_pct=30.0, vcpu_used=5)
        n2 = make_node("b", ram_pct=70.0, vcpu_used=18)
        assert n1.placement_score() > n2.placement_score()


# ─── VmState helpers ──────────────────────────────────────────────────────────

class TestVmState:
    def test_remote_pct_zero_when_no_remote_pages(self):
        vm = make_vm(1, remote_pct=0.0)
        assert vm.remote_pct == 0.0

    def test_remote_pct_computed_correctly(self):
        vm = make_vm(1, max_mem_mib=4096, remote_pct=50.0)
        assert abs(vm.remote_pct - 50.0) < 1.0

    def test_idle_duration_grows_with_time(self):
        t0 = time.monotonic() - 30.0
        vm = make_vm(1, idle_since=t0)
        assert vm.idle_duration_secs() >= 30.0

    def test_idle_duration_zero_when_no_idle_since(self):
        vm = make_vm(1, idle_since=None)
        assert vm.idle_duration_secs() == 0.0
