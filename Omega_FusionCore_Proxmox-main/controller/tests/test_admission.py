"""
Tests du contrôleur d'admission — garantie de non-dépassement mémoire.

Invariant vérifié : local_budget + remote_budget = max_mem (exactement).
"""

import pytest
from controller.cluster import ClusterState, NodeInfo, VmEntry
from controller.admission import AdmissionController, VmSpec, AdmissionDecision


# ─── Helpers ──────────────────────────────────────────────────────────────────

def make_node(node_id, mem_total_mib, mem_free_mib, reachable=True):
    return NodeInfo(
        node_id          = node_id,
        store_addr       = f"{node_id}:9100",
        api_addr         = f"{node_id}:9200",
        mem_total_kb     = mem_total_mib * 1024,
        mem_available_kb = mem_free_mib * 1024,
        mem_usage_pct    = (1 - mem_free_mib / mem_total_mib) * 100 if mem_total_mib else 0,
        pages_stored     = 0,
        store_used_kb    = 0,
        local_vms        = [],
        timestamp_secs   = 0,
        reachable        = reachable,
    )


def make_vm(vmid, max_mem_mib, preferred_node=None, forbidden_nodes=None):
    return VmSpec(
        vmid            = vmid,
        max_mem_mib     = max_mem_mib,
        preferred_node  = preferred_node,
        forbidden_nodes = forbidden_nodes or [],
    )


def make_cluster(*nodes):
    return ClusterState(nodes=list(nodes))


def controller(safety_margin=0.0, prefer_local=True):
    """Contrôleur sans marge de sécurité pour simplifier les calculs dans les tests."""
    return AdmissionController(
        safety_margin          = safety_margin,
        prefer_local           = prefer_local,
        min_remote_node_free   = 256,
    )


# ─── Invariant de base ────────────────────────────────────────────────────────

class TestQuotaInvariant:
    """L'invariant central : local_budget + remote_budget == max_mem."""

    def test_invariant_when_node_has_full_capacity(self):
        """Si le nœud a toute la RAM → remote_budget = 0."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 12288)
        cluster = make_cluster(node_a)
        vm      = make_vm(1, 8192)

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.local_budget_mib + d.remote_budget_mib == vm.max_mem_mib

    def test_invariant_when_node_has_partial_capacity(self):
        """Si le nœud a seulement 8 Go sur 10 Go → remote = 2 Go."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)  # 10 Gio

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.local_budget_mib + d.remote_budget_mib == vm.max_mem_mib, (
            f"local={d.local_budget_mib} + remote={d.remote_budget_mib} "
            f"≠ max_mem={vm.max_mem_mib}"
        )

    def test_invariant_for_multiple_vms(self):
        """L'invariant tient pour toutes les VMs d'un batch."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 16384, 8192)
        cluster = make_cluster(node_a, node_b)

        vms = [
            make_vm(1, 4096),
            make_vm(2, 6144),
            make_vm(3, 2048),
        ]

        decisions = ctrl.admit_batch(cluster, vms)
        for vm, d in zip(vms, decisions):
            if d.admitted:
                assert d.local_budget_mib + d.remote_budget_mib == vm.max_mem_mib, (
                    f"VM {vm.vmid}: local={d.local_budget_mib} + remote={d.remote_budget_mib} "
                    f"≠ max_mem={vm.max_mem_mib}"
                )

    def test_remote_budget_never_negative(self):
        """Même si un nœud a plus de RAM que demandé, remote_budget ≥ 0."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 16384)  # 16 Gio libres
        cluster = make_cluster(node_a)
        vm      = make_vm(1, 4096)  # VM de seulement 4 Gio

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.remote_budget_mib == 0
        assert d.local_budget_mib  == vm.max_mem_mib

    def test_quota_payload_sums_to_max_mem(self):
        """Le payload envoyé au daemon doit respecter l'invariant."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)

        d = ctrl.admit(cluster, vm)
        payload = d.quota_payload()
        assert payload["local_budget_mib"] + payload["remote_budget_mib"] == vm.max_mem_mib


# ─── Refus d'admission ────────────────────────────────────────────────────────

class TestAdmissionRefusal:
    def test_refused_when_cluster_has_no_nodes(self):
        ctrl    = controller()
        cluster = make_cluster()
        vm      = make_vm(1, 4096)

        d = ctrl.admit(cluster, vm)
        assert not d.admitted
        assert "aucun nœud" in d.reason

    def test_refused_when_cluster_ram_insufficient(self):
        """4 Gio total cluster, VM demande 8 Gio → refus."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 8192, 2048)
        node_b  = make_node("pve-b", 8192, 2048)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 8192)

        d = ctrl.admit(cluster, vm)
        assert not d.admitted
        assert d.cluster_free_mib == 4096
        assert "insuffisante" in d.reason

    def test_refused_when_unreachable_nodes_only(self):
        """Tous les nœuds injoignables → refus."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192, reachable=False)
        cluster = make_cluster(node_a)
        vm      = make_vm(1, 4096)

        d = ctrl.admit(cluster, vm)
        assert not d.admitted

    def test_refused_when_all_nodes_forbidden(self):
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        cluster = make_cluster(node_a)
        vm      = make_vm(1, 4096, forbidden_nodes=["pve-a"])

        d = ctrl.admit(cluster, vm)
        assert not d.admitted
        assert "forbidden" in d.reason

    def test_refused_when_remote_not_coverable(self):
        """
        Cluster free >= max_mem, mais aucun nœud secondaire ne dépasse
        le seuil min_remote_node_free → le remote_budget est non couvrable.

        Setup : node_a=7000 Mio, node_b=3500 Mio.  VM=10 Gio.
        cluster_free = 10500 ≥ 10240 → passe la vérif globale.
        node_a choisi (plus de RAM) : local=7000, remote=3240.
        min_remote_node_free=5000 → node_b (3500 < 5000) ne peut pas contribuer.
        """
        ctrl = AdmissionController(
            safety_margin        = 0.0,
            min_remote_node_free = 5000,   # seuil très élevé
        )
        node_a  = make_node("pve-a", 16384, 7000)
        node_b  = make_node("pve-b", 8192,  3500)   # < 5000 → ne contribue pas au remote
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)

        d = ctrl.admit(cluster, vm)
        assert not d.admitted
        assert "remote" in d.reason.lower() or "distante" in d.reason.lower()


# ─── Décisions d'admission valides ───────────────────────────────────────────

class TestAdmissionSuccess:
    def test_vm_fully_local_when_node_has_capacity(self):
        """Si le nœud peut tout accueillir, remote_budget doit être 0."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 12288)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 8192)

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.remote_budget_mib == 0
        assert not d.requires_remote

    def test_vm_split_across_nodes(self):
        """VM de 10 Gio sur cluster 8+4 Gio → local=8, remote=2."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.requires_remote
        assert d.local_budget_mib == 8192
        assert d.remote_budget_mib == 2048

    def test_placement_on_node_with_most_ram(self):
        """Sans topologie, place sur le nœud avec le plus de RAM."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 8192, 2048)
        node_b  = make_node("pve-b", 16384, 8192)  # plus de RAM
        node_c  = make_node("pve-c", 8192, 4096)
        cluster = make_cluster(node_a, node_b, node_c)
        vm      = make_vm(1, 6144)

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.placement_node == "pve-b"

    def test_preferred_node_honored_when_has_capacity(self):
        """Le nœud préféré est choisi s'il a assez de RAM."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 16384, 12288)   # plus de RAM
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 4096, preferred_node="pve-a")

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.placement_node == "pve-a"   # respecte la préférence

    def test_remote_nodes_listed_in_decision(self):
        """Les nœuds qui hébergent le remote doivent être listés."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 6144)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 8192)  # 8 Gio, node_a a 6 Gio → 2 Gio remote

        d = ctrl.admit(cluster, vm)
        assert d.admitted
        assert "pve-b" in d.remote_nodes

    def test_admits_vm_exactly_equal_to_cluster_capacity(self):
        """VM de 10 Gio sur exactement 10 Gio cluster → admise."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 8192, 6144)
        node_b  = make_node("pve-b", 8192, 4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)

        d = ctrl.admit(cluster, vm)
        assert d.admitted


# ─── Batch admission ──────────────────────────────────────────────────────────

class TestBatchAdmission:
    def test_second_vm_accounts_for_first_reservation(self):
        """Admettre 2 VMs de 6 Gio sur un cluster de 8 Gio : 1ère admise, 2ème refusée."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 8192, 8192)
        cluster = make_cluster(node_a)
        vms     = [make_vm(1, 6144), make_vm(2, 6144)]

        decisions = ctrl.admit_batch(cluster, vms)
        assert decisions[0].admitted
        assert not decisions[1].admitted  # plus assez après la 1ère réservation

    def test_batch_admits_multiple_small_vms(self):
        """3 VMs de 2 Gio sur cluster de 8 Gio → toutes admises."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 8192, 8192)
        cluster = make_cluster(node_a)
        vms     = [make_vm(i, 2048) for i in range(1, 4)]

        decisions = ctrl.admit_batch(cluster, vms)
        assert all(d.admitted for d in decisions)

    def test_batch_invariant_for_all_admitted(self):
        """L'invariant local+remote=max_mem tient pour toutes les VMs admises."""
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 10240)
        node_b  = make_node("pve-b", 8192,  8192)
        cluster = make_cluster(node_a, node_b)
        vms     = [make_vm(i, 4096 * i) for i in range(1, 5)]

        decisions = ctrl.admit_batch(cluster, vms)
        for vm, d in zip(vms, decisions):
            if d.admitted:
                total = d.local_budget_mib + d.remote_budget_mib
                assert total == vm.max_mem_mib, (
                    f"VM {vm.vmid}: {total} ≠ {vm.max_mem_mib}"
                )

    def test_empty_batch_returns_empty(self):
        ctrl    = controller()
        cluster = make_cluster(make_node("pve-a", 8192, 8192))
        assert ctrl.admit_batch(cluster, []) == []


# ─── Safety margin ────────────────────────────────────────────────────────────

class TestSafetyMargin:
    def test_safety_margin_reduces_effective_free(self):
        """Avec 10% de marge, un nœud à 8 Gio n'offre que 7.2 Gio effectifs."""
        ctrl   = AdmissionController(safety_margin=0.10)
        node_a = make_node("pve-a", 8192, 8192)
        node_b = make_node("pve-b", 8192, 8192)
        cluster = make_cluster(node_a, node_b)

        # VM de 8 Gio sur un cluster de 8+8 Gio avec 10% marge = 7.2+7.2=14.4 Gio effectif
        vm_ok = make_vm(1, 8192)
        d_ok  = ctrl.admit(cluster, vm_ok)
        assert d_ok.admitted

        # VM de 15 Gio → dépasse les 14.4 Gio effectifs
        vm_big = make_vm(2, 15360)
        d_big  = ctrl.admit(cluster, vm_big)
        assert not d_big.admitted

    def test_no_margin_uses_full_capacity(self):
        """Sans marge (0%), toute la RAM est utilisable."""
        ctrl   = AdmissionController(safety_margin=0.0)
        node_a = make_node("pve-a", 8192, 8192)
        cluster = make_cluster(node_a)

        vm = make_vm(1, 8192)
        d  = ctrl.admit(cluster, vm)
        assert d.admitted
        assert d.local_budget_mib == 8192


# ─── AdmissionDecision helpers ───────────────────────────────────────────────

class TestAdmissionDecision:
    def test_to_dict_contains_all_keys(self):
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)

        d = ctrl.admit(cluster, vm)
        result = d.to_dict()

        for key in ("admitted", "vmid", "max_mem_mib", "placement_node",
                    "local_budget_mib", "remote_budget_mib", "requires_remote",
                    "cluster_free_mib", "reason"):
            assert key in result, f"clé manquante : {key}"

    def test_requires_remote_false_when_local_covers_all(self):
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 16384)
        cluster = make_cluster(node_a)
        vm      = make_vm(1, 4096)

        d = ctrl.admit(cluster, vm)
        assert not d.requires_remote

    def test_quota_payload_structure(self):
        ctrl    = controller()
        node_a  = make_node("pve-a", 16384, 8192)
        node_b  = make_node("pve-b", 8192,  4096)
        cluster = make_cluster(node_a, node_b)
        vm      = make_vm(1, 10240)

        d = ctrl.admit(cluster, vm)
        payload = d.quota_payload()

        assert payload["vm_id"]            == vm.vmid
        assert payload["max_mem_mib"]      == vm.max_mem_mib
        assert "local_budget_mib"  in payload
        assert "remote_budget_mib" in payload
