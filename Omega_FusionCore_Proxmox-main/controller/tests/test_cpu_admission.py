"""Tests du contrôleur d'admission vCPU."""

import pytest
from controller.cpu_admission import (
    ClusterVcpuPoolPlanner,
    VcpuPoolConfig,
    VcpuPoolVm,
    CpuAdmissionController,
    CpuAdmissionDecision,
    NodeCpuCapacity,
    NodeVcpuRegistry,
    VmCpuSpec,
    VmCpuState,
    VCPU_PER_PCPU,
    MAX_VMS_PER_SLOT,
    STEAL_THRESHOLD_PCT,
)


# ─── Fixtures ─────────────────────────────────────────────────────────────────

def make_registry(node_id="node1", num_pcpus=4):
    return NodeVcpuRegistry(node_id, num_pcpus)


def make_controller(pcpus_per_node=4, num_nodes=3):
    nodes = {
        f"node{i}": NodeCpuCapacity(
            node_id=f"node{i}",
            num_pcpus=pcpus_per_node,
        )
        for i in range(1, num_nodes + 1)
    }
    return CpuAdmissionController(nodes)


# ─── VmCpuSpec validation ─────────────────────────────────────────────────────

class TestVmCpuSpec:
    def test_valid_spec(self):
        spec = VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=4)
        assert spec.min_vcpus == 2
        assert spec.max_vcpus == 4

    def test_min_zero_raises(self):
        with pytest.raises(ValueError, match="min_vcpus doit être ≥ 1"):
            VmCpuSpec(vmid=100, min_vcpus=0, max_vcpus=4)

    def test_max_less_than_min_raises(self):
        with pytest.raises(ValueError, match="max_vcpus"):
            VmCpuSpec(vmid=100, min_vcpus=4, max_vcpus=2)

    def test_min_equals_max_ok(self):
        spec = VmCpuSpec(vmid=100, min_vcpus=3, max_vcpus=3)
        assert spec.min_vcpus == spec.max_vcpus


# ─── Capacité ─────────────────────────────────────────────────────────────────

class TestCapacity:
    def test_total_slots(self):
        reg = make_registry(num_pcpus=4)
        # 4 pCPUs × 3 vCPU × 3 VMs = 36
        assert reg.total_vcpu_slots == 36

    def test_free_slots_empty(self):
        reg = make_registry(num_pcpus=2)
        # 2 × 3 × 3 = 18 slots libres au départ
        assert reg.free_vcpu_slots() == 18

    def test_free_slots_after_admit(self):
        reg = make_registry(num_pcpus=2)
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=3, max_vcpus=6))
        assert reg.free_vcpu_slots() == 15  # 18 - 3


# ─── Admission locale (NodeVcpuRegistry) ──────────────────────────────────────

class TestNodeAdmission:
    def test_admit_success(self):
        reg = make_registry()
        dec = reg.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=6))
        assert dec.admitted
        assert dec.allocated_vcpus == 2
        assert dec.vmid == 100

    def test_admit_twice_fails(self):
        reg = make_registry()
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=6))
        dec = reg.admit(VmCpuSpec(vmid=100, min_vcpus=1, max_vcpus=2))
        assert not dec.admitted
        assert "déjà enregistrée" in dec.reason

    def test_admit_no_slots(self):
        # 1 pCPU = 9 slots total ; VM qui en demande 10 → refus
        reg = make_registry(num_pcpus=1)
        dec = reg.admit(VmCpuSpec(vmid=100, min_vcpus=10, max_vcpus=12))
        assert not dec.admitted
        assert "insuffisants" in dec.reason

    def test_release_frees_slots(self):
        reg = make_registry(num_pcpus=2)
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=6, max_vcpus=9))
        assert reg.free_vcpu_slots() == 12  # 18 - 6
        reg.release(100)
        assert reg.free_vcpu_slots() == 18

    def test_multiple_vms(self):
        reg = make_registry(num_pcpus=2)  # 18 slots
        for vmid in range(101, 107):       # 6 VMs × 2 = 12 vCPUs
            dec = reg.admit(VmCpuSpec(vmid=vmid, min_vcpus=2, max_vcpus=4))
            assert dec.admitted
        assert reg.free_vcpu_slots() == 6  # 18 - 12


# ─── Hotplug ──────────────────────────────────────────────────────────────────

class TestHotplug:
    def test_hotplug_success(self):
        reg = make_registry()
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=6))
        ok, msg = reg.try_hotplug(100, delta=2)
        assert ok
        vm = reg.get_vm(100)
        assert vm.allocated_vcpus == 4

    def test_hotplug_at_max_fails(self):
        reg = make_registry()
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=3, max_vcpus=3))
        ok, msg = reg.try_hotplug(100, delta=1)
        assert not ok
        assert "plafond" in msg

    def test_hotplug_no_slots_fails(self):
        reg = make_registry(num_pcpus=1)  # 9 slots
        # Remplir presque tout
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=8, max_vcpus=12))
        # 1 slot libre ; demander 2 → refus
        ok, msg = reg.try_hotplug(100, delta=2)
        assert not ok

    def test_hotplug_unknown_vm(self):
        reg = make_registry()
        ok, msg = reg.try_hotplug(999, delta=1)
        assert not ok
        assert "inconnue" in msg


# ─── Détection pression / migration ──────────────────────────────────────────

class TestPressureDetection:
    def test_needs_more_vcpus(self):
        vm = VmCpuState(
            vmid=100, min_vcpus=2, max_vcpus=6,
            allocated_vcpus=2, cpu_usage_pct=85.0, steal_pct=3.0,
        )
        assert vm.needs_more_vcpus()

    def test_no_hotplug_when_at_max(self):
        vm = VmCpuState(
            vmid=100, min_vcpus=2, max_vcpus=4,
            allocated_vcpus=4, cpu_usage_pct=90.0, steal_pct=3.0,
        )
        assert not vm.needs_more_vcpus()

    def test_needs_migration_when_steal_high(self):
        vm = VmCpuState(
            vmid=100, min_vcpus=2, max_vcpus=4,
            allocated_vcpus=4, cpu_usage_pct=90.0, steal_pct=15.0,
        )
        assert vm.needs_migration()

    def test_no_migration_when_steal_low(self):
        vm = VmCpuState(
            vmid=100, min_vcpus=2, max_vcpus=4,
            allocated_vcpus=4, cpu_usage_pct=90.0, steal_pct=5.0,
        )
        assert not vm.needs_migration()

    def test_registry_vms_needing_hotplug(self):
        reg = make_registry()
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=6))
        reg.update_metrics(100, cpu_usage_pct=85.0, steal_pct=2.0)
        assert 100 in reg.vms_needing_hotplug()

    def test_registry_vms_needing_migration(self):
        reg = make_registry()
        reg.admit(VmCpuSpec(vmid=100, min_vcpus=3, max_vcpus=3))
        reg.update_metrics(100, cpu_usage_pct=95.0, steal_pct=12.0)
        assert 100 in reg.vms_needing_migration()


# ─── Admission cluster (CpuAdmissionController) ───────────────────────────────

class TestClusterAdmission:
    def test_admit_on_best_node(self):
        ctrl = make_controller(pcpus_per_node=4, num_nodes=3)
        dec = ctrl.admit(VmCpuSpec(vmid=200, min_vcpus=2, max_vcpus=8))
        assert dec.admitted
        assert dec.node_id in {"node1", "node2", "node3"}

    def test_prefer_requested_node(self):
        ctrl = make_controller()
        dec = ctrl.admit(
            VmCpuSpec(vmid=200, min_vcpus=2, max_vcpus=4),
            preferred_node="node2",
        )
        assert dec.admitted
        assert dec.node_id == "node2"

    def test_fallback_when_preferred_full(self):
        nodes = {
            "small": NodeCpuCapacity(node_id="small", num_pcpus=1),  # 9 slots
            "big":   NodeCpuCapacity(node_id="big",   num_pcpus=4),  # 36 slots
        }
        ctrl = CpuAdmissionController(nodes)
        # Remplir "small" complètement
        for i in range(9):
            ctrl.admit(VmCpuSpec(vmid=i, min_vcpus=1, max_vcpus=1), preferred_node="small")
        # Demander depuis "small" → fallback vers "big"
        dec = ctrl.admit(
            VmCpuSpec(vmid=99, min_vcpus=1, max_vcpus=2),
            preferred_node="small",
        )
        assert dec.admitted
        assert dec.node_id == "big"

    def test_refuse_when_no_node_available(self):
        nodes = {"tiny": NodeCpuCapacity(node_id="tiny", num_pcpus=1)}
        ctrl = CpuAdmissionController(nodes)
        # Remplir les 9 slots
        for i in range(9):
            ctrl.admit(VmCpuSpec(vmid=i, min_vcpus=1, max_vcpus=1))
        dec = ctrl.admit(VmCpuSpec(vmid=99, min_vcpus=1, max_vcpus=2))
        assert not dec.admitted
        assert "aucun nœud" in dec.reason

    def test_overloaded_node_skipped(self):
        nodes = {
            "hot": NodeCpuCapacity(node_id="hot", num_pcpus=4, steal_pct=20.0),
            "ok":  NodeCpuCapacity(node_id="ok",  num_pcpus=4, steal_pct=1.0),
        }
        ctrl = CpuAdmissionController(nodes)
        dec = ctrl.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=4))
        assert dec.admitted
        assert dec.node_id == "ok"  # "hot" ignoré

    def test_cluster_snapshot(self):
        ctrl = make_controller()
        ctrl.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=4))
        snap = ctrl.cluster_snapshot()
        # Une VM dans l'une des nodes
        total_vms = sum(len(vms) for vms in snap.values())
        assert total_vms == 1

    def test_free_slots_per_node(self):
        ctrl = make_controller(pcpus_per_node=2, num_nodes=2)
        # node1: 18 slots, node2: 18 slots
        ctrl.admit(VmCpuSpec(vmid=100, min_vcpus=4, max_vcpus=8), preferred_node="node1")
        slots = ctrl.free_slots_per_node()
        assert slots["node1"] == 14
        assert slots["node2"] == 18


class TestClusterVcpuPoolPlanner:
    def test_smallest_deficit_first(self):
        planner = ClusterVcpuPoolPlanner()
        decisions = planner.plan(
            node_free_slots={"node1": 0, "node2": 6},
            vms=[
                VcpuPoolVm(vmid=100, node_id="node1", current_vcpus=1, min_vcpus=4, max_vcpus=4),
                VcpuPoolVm(vmid=101, node_id="node1", current_vcpus=1, min_vcpus=2, max_vcpus=2),
            ],
            now=100.0,
        )
        assert [d.vmid for d in decisions] == [101, 100]

    def test_prefers_local_resolution_when_pool_has_local_capacity(self):
        planner = ClusterVcpuPoolPlanner()
        decisions = planner.plan(
            node_free_slots={"node1": 2, "node2": 10},
            vms=[VcpuPoolVm(vmid=100, node_id="node1", current_vcpus=2, min_vcpus=4, max_vcpus=4)],
            now=100.0,
        )
        assert decisions[0].action == "apply_local"

    def test_uses_partial_migration_when_it_improves_pool(self):
        planner = ClusterVcpuPoolPlanner(VcpuPoolConfig(min_gain_vcpus=1))
        decisions = planner.plan(
            node_free_slots={"node1": 1, "node2": 3, "node3": 1},
            vms=[VcpuPoolVm(vmid=100, node_id="node1", current_vcpus=2, min_vcpus=4, max_vcpus=4)],
            now=100.0,
        )
        assert decisions[0].action == "migrate_partial"
        assert decisions[0].target_node == "node2"

    def test_prefers_full_fit_when_a_node_can_satisfy_the_minimum(self):
        planner = ClusterVcpuPoolPlanner(VcpuPoolConfig(min_gain_vcpus=1))
        decisions = planner.plan(
            node_free_slots={"node1": 1, "node2": 10, "node3": 4},
            vms=[VcpuPoolVm(vmid=100, node_id="node1", current_vcpus=2, min_vcpus=4, max_vcpus=4)],
            now=100.0,
        )
        assert decisions[0].action == "migrate_full"
        assert decisions[0].target_node == "node2"

    def test_waits_for_cooldown_before_migrating_again(self):
        planner = ClusterVcpuPoolPlanner(VcpuPoolConfig(migration_cooldown_secs=60.0))
        decisions = planner.plan(
            node_free_slots={"node1": 0, "node2": 6},
            vms=[VcpuPoolVm(vmid=100, node_id="node1", current_vcpus=2, min_vcpus=4, max_vcpus=4)],
            last_migration_at={100: 80.0},
            now=100.0,
        )
        assert decisions[0].action == "wait_cooldown"
        assert decisions[0].cooldown_remaining_secs == pytest.approx(40.0)

    def test_waits_when_no_node_can_improve_deficit(self):
        planner = ClusterVcpuPoolPlanner(VcpuPoolConfig(min_gain_vcpus=3))
        decisions = planner.plan(
            node_free_slots={"node1": 1, "node2": 2},
            vms=[VcpuPoolVm(vmid=100, node_id="node1", current_vcpus=2, min_vcpus=4, max_vcpus=4)],
            now=100.0,
        )
        assert decisions[0].action == "wait_deficit"

    def test_hotplug_via_controller(self):
        ctrl = make_controller()
        dec = ctrl.admit(
            VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=8),
            preferred_node="node1",
        )
        assert dec.admitted
        ok, msg = ctrl.try_hotplug(100, "node1", delta=3)
        assert ok

    def test_admission_payload_structure(self):
        ctrl = make_controller()
        dec = ctrl.admit(VmCpuSpec(vmid=100, min_vcpus=2, max_vcpus=6))
        payload = dec.admission_payload()
        assert payload["vmid"] == 100
        assert payload["allocated_vcpus"] == 2
        assert payload["max_vcpus"] == 6

    def test_release_then_readmit(self):
        nodes = {"solo": NodeCpuCapacity(node_id="solo", num_pcpus=1)}
        ctrl = CpuAdmissionController(nodes)
        dec = ctrl.admit(VmCpuSpec(vmid=100, min_vcpus=9, max_vcpus=9))
        assert dec.admitted
        # Plus de place
        dec2 = ctrl.admit(VmCpuSpec(vmid=200, min_vcpus=1, max_vcpus=1))
        assert not dec2.admitted
        # Libérer
        ctrl.release(100, "solo")
        dec3 = ctrl.admit(VmCpuSpec(vmid=200, min_vcpus=1, max_vcpus=1))
        assert dec3.admitted
