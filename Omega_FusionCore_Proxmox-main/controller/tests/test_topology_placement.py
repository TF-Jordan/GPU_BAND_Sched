"""Tests du moteur de placement topology-aware (L8)."""

import pytest
from controller.cluster import ClusterState, NodeInfo, VmEntry
from controller.topology_placement import (
    NodeTopology, PlacementWeights,
    TopologyAwarePlacementEngine,
)
from controller.placement import PlacementStrategy


# ─── Helpers ──────────────────────────────────────────────────────────────────

def make_node(node_id, mem_total_mib, mem_free_mib, local_vms=None, reachable=True):
    return NodeInfo(
        node_id          = node_id,
        store_addr       = f"{node_id}:9100",
        api_addr         = f"{node_id}:9200",
        mem_total_kb     = mem_total_mib * 1024,
        mem_available_kb = mem_free_mib * 1024,
        mem_usage_pct    = (1 - mem_free_mib / mem_total_mib) * 100 if mem_total_mib else 0,
        pages_stored     = 0,
        store_used_kb    = 0,
        local_vms        = local_vms or [],
        timestamp_secs   = 0,
        reachable        = reachable,
    )


def make_vm(vmid, max_mem_mib, remote_pages, status="Running"):
    return VmEntry(
        vmid           = vmid,
        max_mem_mib    = max_mem_mib,
        rss_kb         = max_mem_mib * 512,
        remote_pages   = remote_pages,
        remote_mem_mib = remote_pages * 4 // 1024,
        status         = status,
    )


def make_cluster(*nodes):
    return ClusterState(nodes=list(nodes))


# ─── Topologies de test ───────────────────────────────────────────────────────

def three_node_topology():
    """A et B dans le même rack, C dans un autre rack (même zone)."""
    return {
        "pve-a": NodeTopology("pve-a", rack="rack-1", zone="zone-eu",
                              cpu_usage=0.8, active_migrations=0,
                              latency_ms={"pve-b": 0.5, "pve-c": 2.0}),
        "pve-b": NodeTopology("pve-b", rack="rack-1", zone="zone-eu",
                              cpu_usage=0.2, active_migrations=0,
                              latency_ms={"pve-a": 0.5, "pve-c": 2.0}),
        "pve-c": NodeTopology("pve-c", rack="rack-2", zone="zone-eu",
                              cpu_usage=0.1, active_migrations=0,
                              latency_ms={"pve-a": 2.0, "pve-b": 2.0}),
    }


def cross_zone_topology():
    """A dans zone-eu, B dans zone-us."""
    return {
        "pve-a": NodeTopology("pve-a", rack="rack-1", zone="zone-eu",
                              cpu_usage=0.7,
                              latency_ms={"pve-b": 80.0}),
        "pve-b": NodeTopology("pve-b", rack="rack-2", zone="zone-us",
                              cpu_usage=0.1,
                              latency_ms={"pve-a": 80.0}),
    }


# ─── Tests score_node ─────────────────────────────────────────────────────────

class TestScoreNode:
    def test_same_rack_scores_higher_than_different_rack(self):
        """Un nœud dans le même rack doit avoir un meilleur score topologique."""
        topo   = three_node_topology()
        vm     = make_vm(1, 512, 500)
        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)   # même rack que A
        node_c = make_node("pve-c", 8192, 4096)   # rack différent

        engine = TopologyAwarePlacementEngine(topo)
        all_nodes = [node_a, node_b, node_c]

        score_b = engine.score_node(node_b, "pve-a", vm, all_nodes)
        score_c = engine.score_node(node_c, "pve-a", vm, all_nodes)

        assert score_b > score_c, (
            f"pve-b (même rack) devrait scorer plus haut que pve-c (rack diff) : "
            f"{score_b:.3f} vs {score_c:.3f}"
        )

    def test_low_cpu_scores_higher(self):
        """Un nœud moins chargé CPU doit scorer plus haut."""
        topo = {
            "pve-a": NodeTopology("pve-a", rack="rack-1", zone="z"),
            "pve-b": NodeTopology("pve-b", rack="rack-1", zone="z", cpu_usage=0.1),
            "pve-c": NodeTopology("pve-c", rack="rack-1", zone="z", cpu_usage=0.9),
        }
        vm = make_vm(1, 512, 500)
        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)  # CPU 10%
        node_c = make_node("pve-c", 8192, 4096)  # CPU 90%

        engine = TopologyAwarePlacementEngine(topo)
        all_nodes = [node_a, node_b, node_c]

        score_b = engine.score_node(node_b, "pve-a", vm, all_nodes)
        score_c = engine.score_node(node_c, "pve-a", vm, all_nodes)

        assert score_b > score_c

    def test_fewer_migrations_scores_higher(self):
        """Un nœud avec moins de migrations en cours doit scorer plus haut."""
        topo = {
            "pve-a": NodeTopology("pve-a", rack="r", zone="z"),
            "pve-b": NodeTopology("pve-b", rack="r", zone="z", active_migrations=0),
            "pve-c": NodeTopology("pve-c", rack="r", zone="z", active_migrations=5),
        }
        vm = make_vm(1, 512, 500)
        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)
        node_c = make_node("pve-c", 8192, 4096)

        engine = TopologyAwarePlacementEngine(topo)
        all_nodes = [node_a, node_b, node_c]

        score_b = engine.score_node(node_b, "pve-a", vm, all_nodes)
        score_c = engine.score_node(node_c, "pve-a", vm, all_nodes)

        assert score_b > score_c

    def test_score_always_between_0_and_1(self):
        """Le score doit toujours être dans [0, 1]."""
        topo = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo)

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)
        node_c = make_node("pve-c", 8192, 4096)
        vm     = make_vm(1, 512, 500)

        for candidate in [node_b, node_c]:
            s = engine.score_node(candidate, "pve-a", vm, [node_a, node_b, node_c])
            assert 0.0 <= s <= 1.0, f"score hors plage : {s}"

    def test_high_latency_penalizes_score(self):
        """Une latence trop élevée doit pénaliser le score."""
        topo = cross_zone_topology()  # pve-b à 80ms de pve-a
        engine = TopologyAwarePlacementEngine(topo, max_latency_ms=50.0)

        vm     = make_vm(1, 512, 500)
        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)

        score_b = engine.score_node(node_b, "pve-a", vm, [node_a, node_b])

        # Le score doit être plus bas qu'avec une latence acceptable
        engine_low_lat = TopologyAwarePlacementEngine(topo, max_latency_ms=200.0)
        score_b_ok = engine_low_lat.score_node(node_b, "pve-a", vm, [node_a, node_b])

        assert score_b < score_b_ok, "haute latence doit pénaliser le score"


# ─── Tests find_target ────────────────────────────────────────────────────────

class TestFindTarget:
    def test_prefers_same_rack(self):
        """Avec RAM égale, doit choisir le nœud du même rack."""
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo)
        vm     = make_vm(1, 512, 500)

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)  # même rack, CPU 20%
        node_c = make_node("pve-c", 8192, 4096)  # rack diff, CPU 10%
        cluster = make_cluster(node_a, node_b, node_c)

        result = engine.find_target(cluster, "pve-a", vm)
        assert result is not None
        target, score = result
        # pve-b et pve-c ont la même RAM; pve-b gagne grâce au même rack
        assert target.node_id == "pve-b"

    def test_returns_none_when_no_ram(self):
        """Retourne None si aucun nœud n'a assez de RAM."""
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo)
        vm     = make_vm(1, 8192, 500)   # VM énorme

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 200)
        cluster = make_cluster(node_a, node_b)

        assert engine.find_target(cluster, "pve-a", vm) is None

    def test_score_returned_with_target(self):
        """Le score retourné doit être valide."""
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo)
        vm     = make_vm(1, 512, 500)

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)

        result = engine.find_target(cluster, "pve-a", vm)
        assert result is not None
        _, score = result
        assert 0.0 <= score <= 1.0


# ─── Tests evaluate_migration ─────────────────────────────────────────────────

class TestEvaluateMigration:
    def test_decision_includes_distance_in_reason(self):
        """La raison doit mentionner la distance topologique."""
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo)
        vm     = make_vm(1, 512, 500)

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)

        decision = engine.evaluate_migration(cluster, "pve-a", vm)
        assert decision.feasible
        assert "distance" in decision.reason or "same_rack" in decision.reason

    def test_below_min_pages_not_feasible(self):
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo, min_remote_pages=1000)
        vm     = make_vm(1, 512, 100)   # 100 < 1000

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)

        decision = engine.evaluate_migration(cluster, "pve-a", vm)
        assert not decision.feasible
        assert decision.confidence == 0.0

    def test_confidence_reflects_score(self):
        """La confidence doit être le score calculé (0.0 – 1.0)."""
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo)
        vm     = make_vm(1, 512, 500)

        node_a = make_node("pve-a", 8192, 100)
        node_b = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)

        decision = engine.evaluate_migration(cluster, "pve-a", vm)
        assert 0.0 < decision.confidence <= 1.0


# ─── Tests find_all_migrations ────────────────────────────────────────────────

class TestFindAllMigrations:
    def test_sorted_by_confidence_desc(self):
        topo   = three_node_topology()
        engine = TopologyAwarePlacementEngine(topo, min_remote_pages=100)

        vm1 = make_vm(1, 512,  500)
        vm2 = make_vm(2, 2048, 800)
        node_a = make_node("pve-a", 8192, 100, local_vms=[vm1, vm2])
        node_b = make_node("pve-b", 8192, 4096)
        node_c = make_node("pve-c", 8192, 8192)
        cluster = make_cluster(node_a, node_b, node_c)

        decisions = engine.find_all_migrations(cluster)
        confidences = [d.confidence for d in decisions]
        assert confidences == sorted(confidences, reverse=True)

    def test_empty_cluster_returns_empty(self):
        engine = TopologyAwarePlacementEngine({})
        cluster = make_cluster()
        assert engine.find_all_migrations(cluster) == []

    def test_cross_zone_migration_still_works(self):
        """Même en cross-zone, si c'est le seul candidat, la migration doit se faire."""
        topo   = cross_zone_topology()
        engine = TopologyAwarePlacementEngine(topo, min_remote_pages=100,
                                              max_latency_ms=200.0)  # tolérant

        vm     = make_vm(1, 512, 500)
        node_a = make_node("pve-a", 8192, 100, local_vms=[vm])
        node_b = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)

        decisions = engine.find_all_migrations(cluster)
        assert len(decisions) == 1
        assert decisions[0].target_node == "pve-b"

    def test_topology_unknown_uses_defaults(self):
        """Si la topologie d'un nœud est inconnue, doit quand même fonctionner."""
        engine = TopologyAwarePlacementEngine(topology={})  # aucune topologie connue
        vm     = make_vm(1, 512, 500)
        node_a = make_node("pve-a", 8192, 100, local_vms=[vm])
        node_b = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)

        decisions = engine.find_all_migrations(cluster)
        assert len(decisions) == 1


# ─── Tests PlacementWeights ───────────────────────────────────────────────────

class TestPlacementWeights:
    def test_default_weights_sum_to_one(self):
        w = PlacementWeights()
        total = w.ram_weight + w.topology_weight + w.cpu_weight + w.migration_weight
        assert abs(total - 1.0) < 1e-9

    def test_invalid_weights_raise(self):
        w = PlacementWeights(ram_weight=0.9, topology_weight=0.5)
        with pytest.raises(ValueError, match="somme des poids"):
            w.validate()

    def test_custom_ram_heavy_weights(self):
        """Des poids 100% RAM doivent fonctionner (topologie ignorée)."""
        w = PlacementWeights(
            ram_weight=1.0, topology_weight=0.0,
            cpu_weight=0.0, migration_weight=0.0,
        )
        w.validate()  # Ne doit pas lever d'exception


# ─── Test NodeTopology ────────────────────────────────────────────────────────

class TestNodeTopology:
    def test_same_rack(self):
        a = NodeTopology("a", rack="rack-1", zone="zone-eu")
        b = NodeTopology("b", rack="rack-1", zone="zone-eu")
        assert a.distance_to(b) == "same_rack"

    def test_same_zone_different_rack(self):
        a = NodeTopology("a", rack="rack-1", zone="zone-eu")
        b = NodeTopology("b", rack="rack-2", zone="zone-eu")
        assert a.distance_to(b) == "same_zone"

    def test_cross_zone(self):
        a = NodeTopology("a", rack="rack-1", zone="zone-eu")
        b = NodeTopology("b", rack="rack-2", zone="zone-us")
        assert a.distance_to(b) == "cross_zone"

    def test_latency_default(self):
        a = NodeTopology("a")
        assert a.latency_to("unknown-node") == 10.0

    def test_latency_measured(self):
        a = NodeTopology("a", latency_ms={"pve-b": 0.5})
        assert a.latency_to("pve-b") == 0.5
