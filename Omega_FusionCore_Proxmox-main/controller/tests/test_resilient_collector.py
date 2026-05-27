"""Tests du CollecteurRésilient (circuit-breaker + retry + cache stale) — L4."""

import time
import threading
import pytest
from unittest.mock import patch, MagicMock

from controller.resilient_collector import (
    CircuitBreaker, CircuitState, RetryPolicy, ResilientCollector,
)


# ─── CircuitBreaker ───────────────────────────────────────────────────────────

class TestCircuitBreaker:
    def test_starts_closed(self):
        cb = CircuitBreaker(addr="pve-a:9200", failure_threshold=3)
        assert cb.state == CircuitState.CLOSED
        assert cb.allow_request()

    def test_opens_after_threshold_failures(self):
        cb = CircuitBreaker(addr="pve-a:9200", failure_threshold=3)
        for _ in range(3):
            cb.record_failure()
        assert cb.state == CircuitState.OPEN
        assert not cb.allow_request()

    def test_does_not_open_before_threshold(self):
        cb = CircuitBreaker(addr="pve-a:9200", failure_threshold=3)
        cb.record_failure()
        cb.record_failure()
        assert cb.state == CircuitState.CLOSED
        assert cb.allow_request()

    def test_success_resets_to_closed(self):
        cb = CircuitBreaker(addr="pve-a:9200", failure_threshold=3)
        for _ in range(3):
            cb.record_failure()
        assert cb.state == CircuitState.OPEN
        cb.record_success()
        assert cb.state == CircuitState.CLOSED
        assert cb.allow_request()

    def test_transitions_to_half_open_after_delay(self):
        cb = CircuitBreaker(addr="pve-a:9200", failure_threshold=2,
                            open_duration_secs=0.05)
        cb.record_failure()
        cb.record_failure()
        assert cb.state == CircuitState.OPEN

        time.sleep(0.1)  # Attendre la fin du délai d'ouverture
        assert cb.allow_request()
        assert cb.state == CircuitState.HALF_OPEN

    def test_half_open_to_closed_on_success(self):
        cb = CircuitBreaker(addr="x", failure_threshold=1, open_duration_secs=0.05)
        cb.record_failure()
        time.sleep(0.1)
        cb.allow_request()  # Passe en HALF_OPEN
        cb.record_success()
        assert cb.state == CircuitState.CLOSED

    def test_half_open_to_open_on_failure(self):
        cb = CircuitBreaker(addr="x", failure_threshold=1, open_duration_secs=0.05)
        cb.record_failure()
        time.sleep(0.1)
        cb.allow_request()  # Passe en HALF_OPEN
        cb.record_failure()
        assert cb.state == CircuitState.OPEN

    def test_thread_safe(self):
        """Le circuit-breaker doit être thread-safe sous charge concurrente."""
        cb = CircuitBreaker(addr="x", failure_threshold=10)
        errors = []

        def hammer():
            try:
                for _ in range(50):
                    cb.record_failure()
                    cb.record_success()
                    cb.allow_request()
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=hammer) for _ in range(8)]
        for t in threads: t.start()
        for t in threads: t.join()

        assert not errors, f"Erreurs concurrentes : {errors}"


# ─── RetryPolicy ──────────────────────────────────────────────────────────────

class TestRetryPolicy:
    def test_generates_correct_number_of_delays(self):
        policy = RetryPolicy(max_retries=3)
        delays = list(policy.delays())
        assert len(delays) == 3

    def test_delays_are_non_negative(self):
        policy = RetryPolicy(max_retries=5, base_delay=1.0, jitter=0.0)
        for d in policy.delays():
            assert d >= 0.0

    def test_delays_increase_exponentially(self):
        policy = RetryPolicy(max_retries=4, base_delay=1.0, jitter=0.0)
        delays = list(policy.delays())
        # 1, 2, 4, 8 (sans jitter)
        for i in range(1, len(delays)):
            assert delays[i] >= delays[i - 1]

    def test_delay_capped_at_max(self):
        policy = RetryPolicy(max_retries=10, base_delay=1.0, max_delay=5.0, jitter=0.0)
        delays = list(policy.delays())
        assert all(d <= 5.0 + 1e-9 for d in delays)

    def test_zero_retries_produces_no_delays(self):
        policy = RetryPolicy(max_retries=0)
        assert list(policy.delays()) == []


# ─── ResilientCollector ───────────────────────────────────────────────────────

class TestResilientCollector:
    """Tests du collecteur résilient avec nœuds simulés."""

    def _make_collector(self, peers=None, failure_threshold=3, open_duration=0.1):
        peers = peers or ["pve-a:9200", "pve-b:9200"]
        return ResilientCollector(
            peer_api_addrs     = peers,
            timeout            = 1.0,
            max_retries        = 2,
            failure_threshold  = failure_threshold,
            open_duration_secs = open_duration,
            stale_max_age_secs = 60.0,
        )

    def _mock_good_response(self):
        """Réponse HTTP valide simulant un nœud omega-daemon."""
        resp = MagicMock()
        resp.raise_for_status.return_value = None
        resp.json.return_value = {
            "node_id":          "pve-a",
            "store_addr":       "pve-a:9100",
            "mem_total_kb":     8388608,
            "mem_available_kb": 4194304,
            "mem_usage_pct":    50.0,
            "pages_stored":     100,
            "store_used_kb":    409600,
            "local_vms":        [],
            "timestamp_secs":   1700000000,
        }
        return resp

    def test_successful_collect(self):
        collector = self._make_collector(peers=["pve-a:9200"])

        with patch.object(collector._session, "get",
                          return_value=self._mock_good_response()):
            cluster = collector.collect()

        assert len(cluster.nodes) == 1
        assert cluster.nodes[0].reachable
        assert cluster.nodes[0].node_id == "pve-a"

    def test_unreachable_node_returns_unreachable_nodeinfo(self):
        collector = self._make_collector(peers=["dead-node:9200"])

        with patch.object(collector._session, "get",
                          side_effect=ConnectionError("host down")):
            cluster = collector.collect()

        assert len(cluster.nodes) == 1
        # Pas de cache — doit retourner unreachable
        assert not cluster.nodes[0].reachable

    def test_stale_cache_used_when_node_unreachable(self):
        """Après un succès, si le nœud tombe, le cache stale est retourné."""
        collector = self._make_collector(peers=["pve-a:9200"])

        # Premier appel : succès → remplit le cache
        with patch.object(collector._session, "get",
                          return_value=self._mock_good_response()):
            collector.collect()

        # Deuxième appel : échec → doit retourner le cache stale
        with patch.object(collector._session, "get",
                          side_effect=ConnectionError("host down")):
            cluster = collector.collect()

        # Le nœud est retourné depuis le cache (reachable=True avec error stale)
        assert len(cluster.nodes) == 1
        assert cluster.nodes[0].node_id == "pve-a"
        assert "stale" in cluster.nodes[0].error

    def test_circuit_breaker_opens_after_repeated_failures(self):
        collector = self._make_collector(peers=["bad:9200"],
                                         failure_threshold=2)

        with patch.object(collector._session, "get",
                          side_effect=ConnectionError("down")):
            # Plusieurs collectes pour déclencher le circuit-breaker
            for _ in range(4):
                collector.collect()

        states = collector.circuit_states()
        assert states["bad:9200"] == "open"

    def test_circuit_states_returns_all_peers(self):
        collector = self._make_collector(peers=["pve-a:9200", "pve-b:9200"])
        states = collector.circuit_states()
        assert "pve-a:9200" in states
        assert "pve-b:9200" in states

    def test_retry_called_on_transient_failure(self):
        """Doit retenter après un échec transitoire."""
        collector = self._make_collector(peers=["pve-a:9200"])
        call_count = 0

        def side_effect(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            if call_count < 2:
                raise ConnectionError("transient")
            return self._mock_good_response()

        with patch.object(collector._session, "get", side_effect=side_effect):
            # max_retries=2 donc 2 délais → 2 tentatives max (1 échec + 1 succès)
            cluster = collector.collect()

        assert cluster.nodes[0].reachable
        assert call_count >= 2
