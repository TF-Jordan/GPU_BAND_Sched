from controller.gpu_global import GpuPlacementAction, choose_gpu_placement
from controller.migration_policy import NodeState, VmState


def vm(vmid=2370, gpu=1024):
    return VmState(
        vm_id=vmid,
        status="running",
        max_mem_mib=4096,
        gpu_vram_budget_mib=gpu,
    )


def node(
    node_id,
    *,
    ram=32768,
    vcpu=16,
    gpu_total=0,
    gpu_free=0,
    disk_pressure=0.0,
    local_vms=None,
):
    return NodeState(
        node_id=node_id,
        proxmox_node_name=node_id,
        mem_total_kb=65536 * 1024,
        mem_available_kb=ram * 1024,
        vcpu_total=32,
        vcpu_free=vcpu,
        gpu_total_vram_mib=gpu_total,
        gpu_free_vram_mib=gpu_free,
        disk_pressure_pct=disk_pressure,
        local_vms=local_vms or [],
    )


def test_keeps_vm_on_local_gpu_when_capacity_exists():
    states = {
        "ram": node("ram", gpu_total=8192, gpu_free=4096),
        "rem": node("rem", gpu_total=8192, gpu_free=8192),
    }

    decision = choose_gpu_placement(
        source_node="ram",
        vm=vm(),
        node_states=states,
        required_vcpus=1,
        gpu_budget_mib=1024,
    )

    assert decision.action == GpuPlacementAction.LOCAL_GPU
    assert decision.target_node == "ram"


def test_migrates_to_best_gpu_node_when_source_has_no_gpu():
    states = {
        "emilia": node("emilia", gpu_total=0, gpu_free=0),
        "ram": node("ram", gpu_total=8192, gpu_free=2048),
        "rem": node("rem", gpu_total=8192, gpu_free=4096),
    }

    decision = choose_gpu_placement(
        source_node="emilia",
        vm=vm(),
        node_states=states,
        required_vcpus=2,
        gpu_budget_mib=1024,
    )

    assert decision.action == GpuPlacementAction.MIGRATE_TO_GPU
    assert decision.target_node == "rem"
    assert decision.proxmox_target == "rem"


def test_uses_remote_proxy_when_gpu_node_cannot_host_vm():
    states = {
        "emilia": node("emilia", gpu_total=0, gpu_free=0),
        "ram": node("ram", ram=512, vcpu=0, gpu_total=8192, gpu_free=4096),
    }

    decision = choose_gpu_placement(
        source_node="emilia",
        vm=vm(),
        node_states=states,
        required_vcpus=4,
        gpu_budget_mib=1024,
        fallback_network=True,
    )

    assert decision.action == GpuPlacementAction.REMOTE_PROXY
    assert decision.target_node == "ram"
    assert decision.proxy_url == "http://ram:9400"


def test_rejects_without_gpu_and_without_fallback():
    states = {
        "emilia": node("emilia"),
        "ram": node("ram"),
    }

    decision = choose_gpu_placement(
        source_node="emilia",
        vm=vm(),
        node_states=states,
        required_vcpus=1,
        gpu_budget_mib=1024,
        fallback_network=False,
    )

    assert decision.action == GpuPlacementAction.REJECT


def test_gpu_target_penalizes_unhealthy_disk_pressure():
    states = {
        "emilia": node("emilia", gpu_total=0, gpu_free=0),
        "ram": node(
            "ram",
            ram=60000,
            vcpu=24,
            gpu_total=24576,
            gpu_free=20000,
            disk_pressure=95.0,
        ),
        "rem": node(
            "rem",
            ram=48000,
            vcpu=18,
            gpu_total=24576,
            gpu_free=18000,
            disk_pressure=1.0,
        ),
    }

    decision = choose_gpu_placement(
        source_node="emilia",
        vm=vm(),
        node_states=states,
        required_vcpus=2,
        gpu_budget_mib=1024,
    )

    assert decision.action == GpuPlacementAction.MIGRATE_TO_GPU
    assert decision.target_node == "rem"


def test_proxy_target_keeps_vram_headroom():
    states = {
        "emilia": node("emilia", gpu_total=0, gpu_free=0),
        "ram": node("ram", gpu_total=8192, gpu_free=1500, ram=64000, vcpu=30),
        "rem": node("rem", gpu_total=8192, gpu_free=4096, ram=32000, vcpu=16),
    }

    decision = choose_gpu_placement(
        source_node="emilia",
        vm=vm(),
        node_states=states,
        required_vcpus=64,
        gpu_budget_mib=1024,
        fallback_network=True,
    )

    assert decision.action == GpuPlacementAction.REMOTE_PROXY
    assert decision.target_node == "rem"
