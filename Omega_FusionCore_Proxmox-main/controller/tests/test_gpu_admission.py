"""Tests du contrôleur d'admission GPU."""

import pytest
from controller.gpu_admission import (
    GpuAdmissionController,
    GpuAdmissionDecision,
    NodeGpuCapacity,
    VmGpuSpec,
    VRAM_OVERCOMMIT_RATIO,
)


# ─── Fixtures ─────────────────────────────────────────────────────────────────

def make_controller(vram_per_node=8192, num_nodes=3):
    nodes = {
        f"node{i}": NodeGpuCapacity(
            node_id=f"node{i}",
            vram_total_mib=vram_per_node,
            gpu_name="RTX 3080",
        )
        for i in range(1, num_nodes + 1)
    }
    return GpuAdmissionController(nodes)


# ─── NodeGpuCapacity ─────────────────────────────────────────────────────────

class TestNodeGpuCapacity:
    def test_free_vram_empty_node(self):
        cap = NodeGpuCapacity(node_id="n1", vram_total_mib=8192)
        assert cap.vram_free_mib == 8192

    def test_free_vram_after_usage(self):
        cap = NodeGpuCapacity(node_id="n1", vram_total_mib=8192, vram_used_mib=2048)
        assert cap.vram_free_mib == 6144

    def test_can_host_vm(self):
        cap  = NodeGpuCapacity(node_id="n1", vram_total_mib=8192)
        spec = VmGpuSpec(vmid=100, vram_budget_mib=4096)
        assert cap.can_host_vm(spec)

    def test_cannot_host_vm_too_large(self):
        cap  = NodeGpuCapacity(node_id="n1", vram_total_mib=4096)
        spec = VmGpuSpec(vmid=100, vram_budget_mib=8192)
        assert not cap.can_host_vm(spec)

    def test_vram_budget_applies_overcommit_ratio(self):
        cap = NodeGpuCapacity(node_id="n1", vram_total_mib=8192)
        # Avec ratio 1.0 (pas de surcommit), budget = total
        assert cap.vram_budget_mib == int(8192 * VRAM_OVERCOMMIT_RATIO)


# ─── Admission sans GPU ───────────────────────────────────────────────────────

class TestNoGpuRequired:
    def test_zero_budget_always_admitted(self):
        ctrl = make_controller()
        dec  = ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=0))
        assert dec.admitted
        assert dec.vram_budget_mib == 0
        assert "pas de GPU requis" in dec.reason

    def test_zero_budget_doesnt_consume_vram(self):
        ctrl = make_controller(vram_per_node=100)
        ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=0))
        ctrl.admit(VmGpuSpec(vmid=101, vram_budget_mib=0))
        snap = ctrl.cluster_snapshot()
        total_used = sum(n["vram_used_mib"] for n in snap)
        assert total_used == 0

    def test_admission_payload_zero_budget(self):
        ctrl = make_controller()
        dec  = ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=0))
        payload = dec.admission_payload()
        assert payload["vram_budget_mib"] == 0


# ─── Admission avec GPU ───────────────────────────────────────────────────────

class TestGpuAdmission:
    def test_admit_success(self):
        ctrl = make_controller()
        dec  = ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=2048))
        assert dec.admitted
        assert dec.vram_budget_mib == 2048
        assert dec.node_id in {"node1", "node2", "node3"}

    def test_vram_consumed_after_admit(self):
        nodes = {"solo": NodeGpuCapacity(node_id="solo", vram_total_mib=8192)}
        ctrl  = GpuAdmissionController(nodes)
        ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=2048))
        snap = ctrl.cluster_snapshot()
        assert snap[0]["vram_used_mib"] == 2048

    def test_prefer_requested_node(self):
        ctrl = make_controller()
        dec  = ctrl.admit(
            VmGpuSpec(vmid=100, vram_budget_mib=1024),
            preferred_node="node2",
        )
        assert dec.admitted
        assert dec.node_id == "node2"

    def test_fallback_when_preferred_full(self):
        nodes = {
            "full": NodeGpuCapacity(node_id="full", vram_total_mib=1024),
            "free": NodeGpuCapacity(node_id="free", vram_total_mib=8192),
        }
        ctrl = GpuAdmissionController(nodes)
        # Remplir "full"
        ctrl.admit(VmGpuSpec(vmid=1, vram_budget_mib=1024), preferred_node="full")
        # Nouvelle VM → fallback vers "free"
        dec = ctrl.admit(
            VmGpuSpec(vmid=2, vram_budget_mib=512),
            preferred_node="full",
        )
        assert dec.admitted
        assert dec.node_id == "free"

    def test_refuse_when_all_full(self):
        nodes = {"tiny": NodeGpuCapacity(node_id="tiny", vram_total_mib=1024)}
        ctrl  = GpuAdmissionController(nodes)
        ctrl.admit(VmGpuSpec(vmid=1, vram_budget_mib=1024))
        dec = ctrl.admit(VmGpuSpec(vmid=2, vram_budget_mib=512))
        assert not dec.admitted
        assert "VRAM insuffisante" in dec.reason

    def test_refuse_duplicate_vmid(self):
        ctrl = make_controller()
        ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=1024))
        dec = ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=512))
        assert not dec.admitted
        assert "déjà un GPU" in dec.reason

    def test_admit_fills_largest_node_first(self):
        nodes = {
            "small": NodeGpuCapacity(node_id="small", vram_total_mib=4096),
            "large": NodeGpuCapacity(node_id="large", vram_total_mib=16384),
        }
        ctrl = GpuAdmissionController(nodes)
        dec  = ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=2048))
        assert dec.node_id == "large"  # le plus de VRAM libre


# ─── Release ──────────────────────────────────────────────────────────────────

class TestGpuRelease:
    def test_release_frees_vram(self):
        nodes = {"solo": NodeGpuCapacity(node_id="solo", vram_total_mib=4096)}
        ctrl  = GpuAdmissionController(nodes)
        ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=2048))
        ctrl.release(100)
        snap = ctrl.cluster_snapshot()
        assert snap[0]["vram_used_mib"] == 0

    def test_release_allows_readmit(self):
        nodes = {"solo": NodeGpuCapacity(node_id="solo", vram_total_mib=2048)}
        ctrl  = GpuAdmissionController(nodes)
        ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=2048))
        dec2 = ctrl.admit(VmGpuSpec(vmid=200, vram_budget_mib=1024))
        assert not dec2.admitted
        ctrl.release(100)
        dec3 = ctrl.admit(VmGpuSpec(vmid=200, vram_budget_mib=1024))
        assert dec3.admitted

    def test_release_unknown_vm_noop(self):
        ctrl = make_controller()
        ctrl.release(999)  # ne doit pas lever d'exception

    def test_allocations_cleared_after_release(self):
        ctrl = make_controller()
        ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=1024))
        ctrl.release(100)
        assert 100 not in ctrl.allocations_snapshot()


# ─── Snapshot & métriques ─────────────────────────────────────────────────────

class TestSnapshot:
    def test_cluster_snapshot_structure(self):
        ctrl = make_controller()
        snap = ctrl.cluster_snapshot()
        assert len(snap) == 3
        for n in snap:
            assert "node_id"        in n
            assert "vram_total_mib" in n
            assert "vram_free_mib"  in n

    def test_admission_payload_structure(self):
        ctrl = make_controller()
        dec  = ctrl.admit(VmGpuSpec(vmid=100, vram_budget_mib=2048))
        p    = dec.admission_payload()
        assert p["vmid"]            == 100
        assert p["vram_budget_mib"] == 2048

    def test_update_node_capacity(self):
        ctrl = make_controller()
        ctrl.update_node_capacity(
            "node1",
            NodeGpuCapacity(node_id="node1", vram_total_mib=24576, gpu_name="A100"),
        )
        snap = ctrl.cluster_snapshot()
        node1 = next(n for n in snap if n["node_id"] == "node1")
        assert node1["vram_total_mib"] == 24576
