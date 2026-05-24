"""Tests du moteur de placement — PlacementEngine."""

import pytest
from controller.cluster import ClusterState, NodeInfo, VmEntry
from controller.placement import PlacementEngine, PlacementDecision, PlacementStrategy


# ─── Helpers ──────────────────────────────────────────────────────────────────

def make_node(
    node_id:          str,
    mem_total_mib:    int,
    mem_free_mib:     int,
    local_vms:        list[VmEntry] | None = None,
    reachable:        bool = True,
) -> NodeInfo:
    return NodeInfo(
        node_id          = node_id,
        store_addr       = f"{node_id}:9100",
        api_addr         = f"{node_id}:9200",
        mem_total_kb     = mem_total_mib * 1024,
        mem_available_kb = mem_free_mib * 1024,
        mem_usage_pct    = (1.0 - mem_free_mib / mem_total_mib) * 100 if mem_total_mib else 0,
        pages_stored     = 0,
        store_used_kb    = 0,
        local_vms        = local_vms or [],
        timestamp_secs   = 0,
        reachable        = reachable,
    )


def make_vm(
    vmid:          int,
    max_mem_mib:   int,
    remote_pages:  int,
    status:        str = "Running",
) -> VmEntry:
    return VmEntry(
        vmid           = vmid,
        max_mem_mib    = max_mem_mib,
        rss_kb         = max_mem_mib * 512,
        remote_pages   = remote_pages,
        remote_mem_mib = remote_pages * 4 // 1024,
        status         = status,
    )


def make_cluster(*nodes: NodeInfo) -> ClusterState:
    return ClusterState(nodes=list(nodes))


# ─── Fixtures de base ─────────────────────────────────────────────────────────

@pytest.fixture
def engine():
    """Moteur avec seuils par défaut."""
    return PlacementEngine(
        strategy         = PlacementStrategy.BEST_FIT,
        safety_margin    = 0.10,
        min_remote_pages = 100,
    )


@pytest.fixture
def three_node_cluster():
    """Cluster 3 nœuds : A saturé, B et C avec de la RAM libre."""
    vm = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
    node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=200, local_vms=[vm])
    node_b = make_node("pve-b", mem_total_mib=8192, mem_free_mib=4096)
    node_c = make_node("pve-c", mem_total_mib=8192, mem_free_mib=2048)
    return make_cluster(node_a, node_b, node_c)


# ─── required_free_mib ────────────────────────────────────────────────────────

class TestRequiredFreeMib:
    def test_exact_with_10pct_margin(self, engine):
        vm = make_vm(vmid=1, max_mem_mib=1024, remote_pages=0)
        # 1024 * 1.10 = 1126
        assert engine.required_free_mib(vm) == 1126

    def test_zero_memory_vm(self, engine):
        vm = make_vm(vmid=1, max_mem_mib=0, remote_pages=0)
        assert engine.required_free_mib(vm) == 0

    def test_custom_margin(self):
        eng = PlacementEngine(safety_margin=0.20)
        vm  = make_vm(vmid=1, max_mem_mib=512, remote_pages=0)
        assert eng.required_free_mib(vm) == int(512 * 1.20)


# ─── find_target ──────────────────────────────────────────────────────────────

class TestFindTarget:
    def test_finds_target_when_ram_available(self, engine, three_node_cluster):
        vm     = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
        target = engine.find_target(three_node_cluster, "pve-a", vm)
        assert target is not None
        assert target.node_id != "pve-a"

    def test_excludes_source_node(self, engine, three_node_cluster):
        vm     = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
        target = engine.find_target(three_node_cluster, "pve-a", vm)
        assert target.node_id != "pve-a"

    def test_returns_none_when_no_node_has_enough_ram(self, engine):
        vm      = make_vm(vmid=1, max_mem_mib=8192, remote_pages=500)
        node_a  = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm])
        node_b  = make_node("pve-b", mem_total_mib=8192, mem_free_mib=200)
        cluster = make_cluster(node_a, node_b)
        assert engine.find_target(cluster, "pve-a", vm) is None

    def test_excludes_unreachable_nodes(self, engine):
        vm      = make_vm(vmid=1, max_mem_mib=512, remote_pages=500)
        node_a  = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm])
        node_b  = make_node("pve-b", mem_total_mib=8192, mem_free_mib=4096, reachable=False)
        cluster = make_cluster(node_a, node_b)
        assert engine.find_target(cluster, "pve-a", vm) is None

    def test_single_node_cluster_returns_none(self, engine):
        vm      = make_vm(vmid=1, max_mem_mib=512, remote_pages=500)
        node_a  = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm])
        cluster = make_cluster(node_a)
        assert engine.find_target(cluster, "pve-a", vm) is None


# ─── Stratégies de placement ──────────────────────────────────────────────────

class TestPlacementStrategies:
    """Vérifie que chaque stratégie choisit le bon nœud."""

    def _cluster_with_two_targets(self) -> tuple[ClusterState, VmEntry]:
        vm     = make_vm(vmid=1, max_mem_mib=512, remote_pages=500)
        node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm])
        # B a 2 Gio libre, C a 4 Gio libre
        node_b = make_node("pve-b", mem_total_mib=8192, mem_free_mib=2048)
        node_c = make_node("pve-c", mem_total_mib=8192, mem_free_mib=4096)
        return make_cluster(node_a, node_b, node_c), vm

    def test_best_fit_picks_tightest(self):
        """BEST_FIT → nœud avec le moins de RAM libre suffisante (pve-b, 2 Gio)."""
        cluster, vm = self._cluster_with_two_targets()
        eng    = PlacementEngine(strategy=PlacementStrategy.BEST_FIT, safety_margin=0.10)
        target = eng.find_target(cluster, "pve-a", vm)
        assert target.node_id == "pve-b"

    def test_most_free_picks_largest(self):
        """MOST_FREE → nœud avec le plus de RAM libre (pve-c, 4 Gio)."""
        cluster, vm = self._cluster_with_two_targets()
        eng    = PlacementEngine(strategy=PlacementStrategy.MOST_FREE, safety_margin=0.10)
        target = eng.find_target(cluster, "pve-a", vm)
        assert target.node_id == "pve-c"

    def test_first_fit_picks_first_candidate(self):
        """FIRST_FIT → premier nœud dans la liste avec assez de RAM."""
        cluster, vm = self._cluster_with_two_targets()
        eng    = PlacementEngine(strategy=PlacementStrategy.FIRST_FIT, safety_margin=0.10)
        target = eng.find_target(cluster, "pve-a", vm)
        # Les candidats sont [pve-b, pve-c] dans l'ordre du cluster → pve-b en premier
        assert target.node_id == "pve-b"


# ─── evaluate_migration ───────────────────────────────────────────────────────

class TestEvaluateMigration:
    def test_feasible_decision_has_target(self, engine, three_node_cluster):
        vm       = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
        decision = engine.evaluate_migration(three_node_cluster, "pve-a", vm)
        assert decision.feasible
        assert decision.target_node != ""
        assert decision.vmid == 101
        assert decision.source_node == "pve-a"

    def test_confidence_in_range(self, engine, three_node_cluster):
        vm       = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
        decision = engine.evaluate_migration(three_node_cluster, "pve-a", vm)
        assert 0.0 <= decision.confidence <= 1.0

    def test_reason_is_populated(self, engine, three_node_cluster):
        vm       = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
        decision = engine.evaluate_migration(three_node_cluster, "pve-a", vm)
        assert decision.reason
        assert len(decision.reason) > 5

    def test_below_min_pages_not_feasible(self, engine, three_node_cluster):
        vm       = make_vm(vmid=101, max_mem_mib=512, remote_pages=50)  # < 100
        decision = engine.evaluate_migration(three_node_cluster, "pve-a", vm)
        assert not decision.feasible
        assert decision.confidence == 0.0

    def test_no_room_not_feasible(self, engine):
        vm      = make_vm(vmid=1, max_mem_mib=8192, remote_pages=500)
        node_a  = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm])
        node_b  = make_node("pve-b", mem_total_mib=8192, mem_free_mib=200)
        cluster = make_cluster(node_a, node_b)
        decision = engine.evaluate_migration(cluster, "pve-a", vm)
        assert not decision.feasible

    def test_stopped_vm_not_migrated(self, engine, three_node_cluster):
        """Une VM à l'arrêt ne doit pas être candidate à la migration."""
        vm_stopped = make_vm(vmid=200, max_mem_mib=512, remote_pages=500, status="Stopped")
        node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm_stopped])
        node_b = make_node("pve-b", mem_total_mib=8192, mem_free_mib=4096)
        cluster = make_cluster(node_a, node_b)
        # vms_with_remote_pages filtre les VMs non Running → find_all_migrations retourne []
        decisions = engine.find_all_migrations(cluster)
        assert all(d.vmid != 200 for d in decisions)

    def test_to_dict_has_required_keys(self, engine, three_node_cluster):
        vm       = make_vm(vmid=101, max_mem_mib=512, remote_pages=500)
        decision = engine.evaluate_migration(three_node_cluster, "pve-a", vm)
        d        = decision.to_dict()
        for key in ("vmid", "source_node", "target_node", "confidence", "strategy", "reason"):
            assert key in d, f"clé manquante : {key}"


# ─── find_all_migrations ──────────────────────────────────────────────────────

class TestFindAllMigrations:
    def test_returns_empty_when_no_vm_qualifies(self, engine):
        vm      = make_vm(vmid=1, max_mem_mib=512, remote_pages=50)  # sous le seuil
        node_a  = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm])
        node_b  = make_node("pve-b", mem_total_mib=8192, mem_free_mib=4096)
        cluster = make_cluster(node_a, node_b)
        assert engine.find_all_migrations(cluster) == []

    def test_returns_one_decision_per_migratable_vm(self, engine):
        vm1    = make_vm(vmid=1, max_mem_mib=512, remote_pages=500)
        vm2    = make_vm(vmid=2, max_mem_mib=512, remote_pages=300)
        node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm1, vm2])
        node_b = make_node("pve-b", mem_total_mib=8192, mem_free_mib=4096)
        cluster = make_cluster(node_a, node_b)
        decisions = engine.find_all_migrations(cluster)
        assert len(decisions) == 2
        vmids = {d.vmid for d in decisions}
        assert vmids == {1, 2}

    def test_sorted_by_confidence_desc(self, engine):
        """Décisions triées par confiance décroissante."""
        vm1    = make_vm(vmid=1, max_mem_mib=512, remote_pages=500)
        vm2    = make_vm(vmid=2, max_mem_mib=4096, remote_pages=300)
        node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100, local_vms=[vm1, vm2])
        node_b = make_node("pve-b", mem_total_mib=16384, mem_free_mib=8192)
        cluster = make_cluster(node_a, node_b)
        decisions = engine.find_all_migrations(cluster)
        confidences = [d.confidence for d in decisions]
        assert confidences == sorted(confidences, reverse=True)

    def test_empty_cluster(self, engine):
        cluster = make_cluster()
        assert engine.find_all_migrations(cluster) == []

    def test_all_nodes_unreachable(self, engine):
        vm     = make_vm(vmid=1, max_mem_mib=512, remote_pages=500)
        node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=100,
                           local_vms=[vm], reachable=False)
        cluster = make_cluster(node_a)
        assert engine.find_all_migrations(cluster) == []

    def test_multiple_vms_multiple_nodes(self, engine):
        """Scénario complet 3 nœuds, plusieurs VMs sous pression."""
        vm_a1  = make_vm(vmid=101, max_mem_mib=1024, remote_pages=1000)
        vm_a2  = make_vm(vmid=102, max_mem_mib=512,  remote_pages=600)
        vm_b1  = make_vm(vmid=201, max_mem_mib=2048, remote_pages=2000)
        node_a = make_node("pve-a", mem_total_mib=8192, mem_free_mib=300, local_vms=[vm_a1, vm_a2])
        node_b = make_node("pve-b", mem_total_mib=8192, mem_free_mib=400, local_vms=[vm_b1])
        node_c = make_node("pve-c", mem_total_mib=16384, mem_free_mib=10000)
        cluster = make_cluster(node_a, node_b, node_c)
        decisions = engine.find_all_migrations(cluster)
        # Toutes les VMs qualifient et pve-c a assez de RAM
        assert len(decisions) == 3
        assert all(d.target_node == "pve-c" for d in decisions)
