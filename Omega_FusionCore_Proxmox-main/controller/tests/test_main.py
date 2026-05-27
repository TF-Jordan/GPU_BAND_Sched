from controller import main
from controller.migration_policy import NodeState, VmState


class FakeResponse:
    def __init__(self, payload, status_code=200):
        self._payload = payload
        self.status_code = status_code

    def json(self):
        return self._payload

    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(f"http {self.status_code}")


def make_vm(vm_id: int = 101, gpu_vram_budget_mib: int = 0) -> VmState:
    return VmState(
        vm_id=vm_id,
        status="running",
        max_mem_mib=2048,
        rss_kb=0,
        remote_pages=0,
        avg_cpu_pct=0.0,
        throttle_ratio=0.0,
        gpu_vram_budget_mib=gpu_vram_budget_mib,
    )


def make_node(
    node_id: str,
    vcpu_free: int,
    gpu_total_vram_mib: int = 0,
    gpu_free_vram_mib: int = 0,
) -> NodeState:
    return NodeState(
        node_id=node_id,
        mem_total_kb=16 * 1024 * 1024,
        mem_available_kb=12 * 1024 * 1024,
        vcpu_total=12,
        vcpu_free=vcpu_free,
        gpu_total_vram_mib=gpu_total_vram_mib,
        gpu_free_vram_mib=gpu_free_vram_mib,
        local_vms=[],
    )


def test_reconcile_uses_real_proxmox_node_name(monkeypatch):
    proxmox_calls = []

    class FakeAdmission:
        pass

    class FakeProxmox:
        def get_vm_config(self, node, vmid):
            proxmox_calls.append((node, vmid))
            return {}

        def parse_omega_metadata(self, config):
            return config

    monkeypatch.setattr(main, "_fetch_vm_quota", lambda source_url, vm_id: {"quota": "ok"})
    monkeypatch.setattr(main, "_fetch_vcpu_status", lambda source_url: {"vm_states": []})
    monkeypatch.setattr(main, "_ensure_vm_gpu_budget", lambda **kwargs: None)
    monkeypatch.setattr(main, "_ensure_vm_vcpu_profile", lambda **kwargs: None)

    vm = make_vm()
    node = make_node("node-a", vcpu_free=2)
    node.proxmox_node_name = "pve"
    node.local_vms = [vm]

    main._reconcile_cluster_resources(
        node_urls={"node-a": "http://node-a:9300"},
        node_states={"node-a": node},
        admission=FakeAdmission(),
        proxmox=FakeProxmox(),
        vcpu_pool=main.ClusterVcpuPoolPlanner(),
        last_vcpu_migrations={},
        last_admission_migrations={},
        dry_run=False,
    )

    assert proxmox_calls == [("pve", 101)]


def test_reconcile_stops_after_reposition_request(monkeypatch):
    class FakeDecision:
        admitted = True
        placement_node = "node-c"
        reason = ""

        @staticmethod
        def quota_payload():
            return {"local_budget_mib": 1536, "remote_budget_mib": 512}

    class FakeAdmission:
        def admit(self, cluster, spec):
            return FakeDecision()

    class FakeProxmox:
        def get_vm_config(self, node, vmid):
            raise AssertionError("should not fetch config when relocation is pending")

        def parse_omega_metadata(self, config):
            return config

    posts = []

    def fake_post(url, json, timeout):
        posts.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main, "_fetch_vm_quota", lambda source_url, vm_id: None)
    monkeypatch.setattr(main.requests, "post", fake_post)

    vm = make_vm()
    source = make_node("node-a", vcpu_free=2)
    source.proxmox_node_name = "pve"
    source.local_vms = [vm]
    target = make_node("node-c", vcpu_free=8)
    target.proxmox_node_name = "pve3"

    main._reconcile_cluster_resources(
        node_urls={"node-a": "http://node-a:9300", "node-c": "http://node-c:9300"},
        node_states={"node-a": source, "node-c": target},
        admission=FakeAdmission(),
        proxmox=FakeProxmox(),
        vcpu_pool=main.ClusterVcpuPoolPlanner(),
        last_vcpu_migrations={},
        last_admission_migrations={},
        dry_run=False,
    )

    assert posts == [
        (
            "http://node-a:9300/control/migrate",
            {"vm_id": 101, "target": "pve3", "type": "live"},
        )
    ]


def test_ensure_vm_vcpu_profile_posts_profile_update(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    assert main._ensure_vm_vcpu_profile(
        source_node="node-a",
        source_url="http://node-a:9300",
        vm=make_vm(),
        metadata={"min_vcpus": 2, "max_vcpus": 4},
        node_states={
            "node-a": make_node("node-a", vcpu_free=2),
            "node-b": make_node("node-b", vcpu_free=4),
        },
        gpu_budget_mib=0,
        vcpu_status={
            "vm_states": [
                {"vm_id": 101, "current_vcpus": 1, "min_vcpus": 1, "max_vcpus": 2}
            ]
        },
        pool_decision=None,
        last_vcpu_migrations={},
        now=0.0,
        dry_run=False,
    ) is False

    assert calls == [
        (
            "http://node-a:9300/control/vm/101/vcpu",
            {"min_vcpus": 2, "max_vcpus": 4},
        )
    ]


def test_ensure_vm_vcpu_profile_migrates_when_source_is_too_full(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    source = make_node("node-a", vcpu_free=1)
    source.proxmox_node_name = "pve1"
    target = make_node("node-b", vcpu_free=4)
    target.proxmox_node_name = "pve2"

    assert main._ensure_vm_vcpu_profile(
        source_node="node-a",
        source_url="http://node-a:9300",
        vm=make_vm(),
        metadata={"min_vcpus": 3, "max_vcpus": 4},
        node_states={
            "node-a": source,
            "node-b": target,
        },
        gpu_budget_mib=0,
        vcpu_status={
            "vm_states": [
                {"vm_id": 101, "current_vcpus": 1, "min_vcpus": 1, "max_vcpus": 2}
            ]
        },
        pool_decision=None,
        last_vcpu_migrations={},
        now=0.0,
        dry_run=False,
    ) is True

    assert calls == [
        (
            "http://node-a:9300/control/migrate",
            {"vm_id": 101, "target": "pve2", "type": "live"},
        )
    ]


def test_ensure_vm_vcpu_profile_migrates_partial_fit_when_full_fit_is_unavailable(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    source = make_node("node-a", vcpu_free=1)
    source.proxmox_node_name = "pve1"
    partial_target = make_node("node-b", vcpu_free=3)
    partial_target.proxmox_node_name = "pve2"
    weak_target = make_node("node-c", vcpu_free=1)
    weak_target.proxmox_node_name = "pve3"

    assert main._ensure_vm_vcpu_profile(
        source_node="node-a",
        source_url="http://node-a:9300",
        vm=make_vm(),
        metadata={"min_vcpus": 4, "max_vcpus": 4},
        node_states={
            "node-a": source,
            "node-b": partial_target,
            "node-c": weak_target,
        },
        gpu_budget_mib=0,
        vcpu_status={
            "vm_states": [
                {"vm_id": 101, "current_vcpus": 2, "min_vcpus": 2, "max_vcpus": 4}
            ]
        },
        pool_decision=None,
        last_vcpu_migrations={},
        now=0.0,
        dry_run=False,
    ) is True

    assert calls == [
        (
            "http://node-a:9300/control/migrate",
            {"vm_id": 101, "target": "pve2", "type": "live"},
        )
    ]


def test_ensure_vm_vcpu_profile_keeps_vm_alive_when_no_better_target_exists(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    source = make_node("node-a", vcpu_free=1)
    source.proxmox_node_name = "pve1"
    weak_target = make_node("node-b", vcpu_free=1)
    weak_target.proxmox_node_name = "pve2"

    assert main._ensure_vm_vcpu_profile(
        source_node="node-a",
        source_url="http://node-a:9300",
        vm=make_vm(),
        metadata={"min_vcpus": 4, "max_vcpus": 4},
        node_states={
            "node-a": source,
            "node-b": weak_target,
        },
        gpu_budget_mib=0,
        vcpu_status={
            "vm_states": [
                {"vm_id": 101, "current_vcpus": 2, "min_vcpus": 2, "max_vcpus": 4}
            ]
        },
        pool_decision=None,
        last_vcpu_migrations={},
        now=0.0,
        dry_run=False,
    ) is False

    assert calls == []


def test_reconcile_processes_smallest_vcpu_deficit_first(monkeypatch):
    order = []

    class FakeAdmission:
        pass

    class FakeProxmox:
        def get_vm_config(self, node, vmid):
            return {"vmid": vmid}

        def parse_omega_metadata(self, config):
            if config["vmid"] == 101:
                return {"min_vcpus": 3, "max_vcpus": 3}
            return {"min_vcpus": 2, "max_vcpus": 2}

    def fake_ensure_vcpu(**kwargs):
        order.append(kwargs["vm"].vm_id)
        return False

    monkeypatch.setattr(main, "_fetch_vm_quota", lambda source_url, vm_id: {"quota": "ok"})
    monkeypatch.setattr(
        main,
        "_fetch_vcpu_status",
        lambda source_url: {
            "vm_states": [
                {"vm_id": 101, "current_vcpus": 1, "min_vcpus": 1, "max_vcpus": 3},
                {"vm_id": 102, "current_vcpus": 1, "min_vcpus": 1, "max_vcpus": 2},
            ]
        },
    )
    monkeypatch.setattr(main, "_ensure_vm_vcpu_profile", fake_ensure_vcpu)
    monkeypatch.setattr(main, "_ensure_vm_gpu_budget", lambda **kwargs: None)

    node = make_node("node-a", vcpu_free=2)
    node.proxmox_node_name = "pve"
    node.local_vms = [make_vm(101), make_vm(102)]

    main._reconcile_cluster_resources(
        node_urls={"node-a": "http://node-a:9300"},
        node_states={"node-a": node},
        admission=FakeAdmission(),
        proxmox=FakeProxmox(),
        vcpu_pool=main.ClusterVcpuPoolPlanner(),
        last_vcpu_migrations={},
        last_admission_migrations={},
        dry_run=False,
    )

    assert order == [102, 101]


def test_reconcile_skips_gpu_update_when_vcpu_migration_is_pending(monkeypatch):
    gpu_calls = []

    class FakeAdmission:
        pass

    class FakeProxmox:
        def get_vm_config(self, node, vmid):
            return {"gpu_vram_mib": 512, "min_vcpus": 4, "max_vcpus": 4}

        def parse_omega_metadata(self, config):
            return config

    monkeypatch.setattr(main, "_fetch_vm_quota", lambda source_url, vm_id: {"quota": "ok"})
    monkeypatch.setattr(
        main,
        "_fetch_vcpu_status",
        lambda source_url: {"vm_states": [{"vm_id": 101, "current_vcpus": 1, "min_vcpus": 1, "max_vcpus": 4}]},
    )
    monkeypatch.setattr(main, "_ensure_vm_vcpu_profile", lambda **kwargs: True)
    monkeypatch.setattr(main, "_ensure_vm_gpu_budget", lambda **kwargs: gpu_calls.append(kwargs))

    node = make_node("node-a", vcpu_free=1)
    node.proxmox_node_name = "pve"
    node.local_vms = [make_vm(101, gpu_vram_budget_mib=0)]

    main._reconcile_cluster_resources(
        node_urls={"node-a": "http://node-a:9300"},
        node_states={"node-a": node},
        admission=FakeAdmission(),
        proxmox=FakeProxmox(),
        vcpu_pool=main.ClusterVcpuPoolPlanner(),
        last_vcpu_migrations={},
        last_admission_migrations={},
        dry_run=False,
    )

    assert gpu_calls == []


def test_ensure_vm_admitted_uses_cooldown_for_reposition(monkeypatch):
    class FakeDecision:
        admitted = True
        placement_node = "node-c"
        reason = ""

        @staticmethod
        def quota_payload():
            return {"local_budget_mib": 1536, "remote_budget_mib": 512}

    class FakeAdmission:
        def admit(self, cluster, spec):
            return FakeDecision()

    posts = []

    def fake_post(url, json, timeout):
        posts.append((url, json))
        return FakeResponse({"status": "migration_started", "task_id": 42})

    monkeypatch.setattr(main.requests, "post", fake_post)

    vm = make_vm()
    source = make_node("node-a", vcpu_free=2)
    source.proxmox_node_name = "pve1"
    target = make_node("node-c", vcpu_free=8)
    target.proxmox_node_name = "pve3"
    last_attempts = {}

    assert main._ensure_vm_admitted(
        source_node="node-a",
        source_url="http://node-a:9300",
        vm=vm,
        cluster=main.ClusterState(nodes=[]),
        admission=FakeAdmission(),
        node_states={"node-a": source, "node-c": target},
        last_admission_migrations=last_attempts,
        now=100.0,
        dry_run=False,
    ) is True
    assert posts == [
        (
            "http://node-a:9300/control/migrate",
            {"vm_id": 101, "target": "pve3", "type": "live"},
        )
    ]

    assert main._ensure_vm_admitted(
        source_node="node-a",
        source_url="http://node-a:9300",
        vm=vm,
        cluster=main.ClusterState(nodes=[]),
        admission=FakeAdmission(),
        node_states={"node-a": source, "node-c": target},
        last_admission_migrations=last_attempts,
        now=120.0,
        dry_run=False,
    ) is True
    assert len(posts) == 1


def test_auto_migration_type_prefers_live_unless_explicitly_stopped():
    assert main._auto_migration_type(make_vm(101)) == "live"

    unknown = make_vm(102)
    unknown.status = "unknown"
    assert main._auto_migration_type(unknown) == "live"

    stopped = make_vm(103)
    stopped.status = "stopped"
    assert main._auto_migration_type(stopped) == "cold"


def test_ensure_vm_gpu_placement_migrates_to_less_loaded_gpu_node(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    vm = make_vm(101, gpu_vram_budget_mib=512)
    source = make_node("node-a", vcpu_free=4, gpu_total_vram_mib=8192, gpu_free_vram_mib=512)
    source.proxmox_node_name = "pve1"
    source.local_vms = [vm]
    target = make_node("node-b", vcpu_free=6, gpu_total_vram_mib=8192, gpu_free_vram_mib=4096)
    target.proxmox_node_name = "pve2"

    assert main._ensure_vm_gpu_placement(
        source_node="node-a",
        source_url="http://node-a:9300",
        node_urls={"node-a": "http://node-a:9300", "node-b": "http://node-b:9300"},
        vm=vm,
        node_states={"node-a": source, "node-b": target},
        required_vcpus=2,
        gpu_budget_mib=2048,
        last_gpu_migrations={},
        now=0.0,
        dry_run=False,
    ) is True

    assert calls == [
        (
            "http://node-a:9300/control/migrate",
            {"vm_id": 101, "target": "pve2", "type": "live"},
        )
    ]


def test_ensure_vm_gpu_placement_creates_space_by_evicting_resident_vm(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    requester = make_vm(101, gpu_vram_budget_mib=512)
    resident = make_vm(202, gpu_vram_budget_mib=2048)

    source = make_node("node-a", vcpu_free=4, gpu_total_vram_mib=8192, gpu_free_vram_mib=512)
    source.proxmox_node_name = "pve1"
    source.local_vms = [requester]

    target = make_node("node-b", vcpu_free=6, gpu_total_vram_mib=8192, gpu_free_vram_mib=2048)
    target.proxmox_node_name = "pve2"
    target.local_vms = [resident]

    alternate = make_node("node-c", vcpu_free=6, gpu_total_vram_mib=8192, gpu_free_vram_mib=2048)
    alternate.proxmox_node_name = "pve3"

    assert main._ensure_vm_gpu_placement(
        source_node="node-a",
        source_url="http://node-a:9300",
        node_urls={
            "node-a": "http://node-a:9300",
            "node-b": "http://node-b:9300",
            "node-c": "http://node-c:9300",
        },
        vm=requester,
        node_states={"node-a": source, "node-b": target, "node-c": alternate},
        required_vcpus=2,
        gpu_budget_mib=4096,
        last_gpu_migrations={},
        now=0.0,
        dry_run=False,
    ) is True

    assert calls == [
        (
            "http://node-b:9300/control/migrate",
            {"vm_id": 202, "target": "pve3", "type": "live"},
        )
    ]


def test_plan_node_drain_reserves_target_capacity_across_multiple_vms():
    vm_a = make_vm(101)
    vm_a.max_mem_mib = 8192
    vm_b = make_vm(102)
    vm_b.max_mem_mib = 4096

    source = make_node("node-a", vcpu_free=2)
    source.local_vms = [vm_a, vm_b]

    target_b = make_node("node-b", vcpu_free=2)
    target_b.mem_available_kb = 10 * 1024 * 1024
    target_b.proxmox_node_name = "pve2"

    target_c = make_node("node-c", vcpu_free=2)
    target_c.mem_available_kb = 6 * 1024 * 1024
    target_c.proxmox_node_name = "pve3"

    plan = main._plan_node_drain(
        "node-a",
        {
            "node-a": source,
            "node-b": target_b,
            "node-c": target_c,
        },
    )

    assert [(c.vm.vm_id, c.target, c.reason.value) for c in plan] == [
        (101, "node-b", "maintenance_drain"),
        (102, "node-c", "maintenance_drain"),
    ]


def test_start_auto_migration_includes_maintenance_reason(monkeypatch):
    calls = []

    def fake_post(url, json, timeout):
        calls.append((url, json))
        return FakeResponse({"status": "ok"})

    monkeypatch.setattr(main.requests, "post", fake_post)

    source = make_node("node-a", vcpu_free=2)
    source.proxmox_node_name = "pve1"
    target = make_node("node-b", vcpu_free=8)
    target.proxmox_node_name = "pve2"

    launched = main._start_auto_migration(
        source_url="http://node-a:9300",
        node_states={"node-a": source, "node-b": target},
        source_node="node-a",
        vm=make_vm(101),
        target="node-b",
        detail="maintenance drain test",
        tracker={},
        now=0.0,
        dry_run=False,
        reason="maintenance",
    )

    assert launched is True
    assert calls == [
        (
            "http://node-a:9300/control/migrate",
            {"vm_id": 101, "target": "pve2", "type": "live", "reason": "maintenance"},
        )
    ]
