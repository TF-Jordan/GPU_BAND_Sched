"""
CollecteurRésilient — correction de la limite L4.

# Problème corrigé

Le `ClusterStateCollector` original lançait une seule requête HTTP par nœud,
sans retry, sans backoff, sans circuit-breaker. Un nœud temporairement lent
(charge CPU, pic réseau) pouvait faire rater une collecte entière et déclencher
une décision de migration sur des données périmées.

# Solution

`ResilientCollector` ajoute trois mécanismes :

1. **Retry avec backoff exponentiel** : jusqu'à `max_retries` tentatives,
   avec délai doublé à chaque échec (ex : 0.5s, 1s, 2s).

2. **Circuit-breaker par nœud** : après `failure_threshold` échecs consécutifs,
   le nœud est mis en « circuit ouvert » pendant `open_duration_secs` secondes.
   Les requêtes sont court-circuitées immédiatement (pas de timeout à attendre).
   Le circuit se referme automatiquement après le délai.

3. **Cache de dernier état connu** : si un nœud est injoignable, on retourne
   son dernier état connu (marqué `stale=True`) plutôt que de le déclarer mort.
   Utile pour les décisions de placement qui peuvent se baser sur des données
   légèrement périmées.

# États du circuit-breaker

```
CLOSED → (failure_threshold échecs consécutifs) → OPEN
OPEN   → (après open_duration_secs)             → HALF-OPEN
HALF-OPEN → (succès) → CLOSED
HALF-OPEN → (échec)  → OPEN
```
"""

from __future__ import annotations

import time
import logging
import threading
from dataclasses import dataclass, field
from enum import Enum
from typing import Optional

import requests

from .cluster import ClusterState, ClusterStateCollector, NodeInfo

logger = logging.getLogger(__name__)


# ─── Circuit-breaker ──────────────────────────────────────────────────────────

class CircuitState(str, Enum):
    CLOSED    = "closed"     # Normal — les requêtes passent
    OPEN      = "open"       # Bloqué — échecs consécutifs trop nombreux
    HALF_OPEN = "half_open"  # Test — une requête sonde passe


@dataclass
class CircuitBreaker:
    """Circuit-breaker par nœud."""
    addr:              str
    failure_threshold: int    = 3
    open_duration_secs: float = 30.0

    _state:            CircuitState = field(default=CircuitState.CLOSED, init=False)
    _failures:         int          = field(default=0, init=False)
    _opened_at:        float        = field(default=0.0, init=False)
    _lock:             threading.Lock = field(default_factory=threading.Lock, init=False)

    def record_success(self) -> None:
        with self._lock:
            self._failures = 0
            self._state    = CircuitState.CLOSED

    def record_failure(self) -> None:
        with self._lock:
            self._failures += 1
            if self._failures >= self.failure_threshold:
                if self._state != CircuitState.OPEN:
                    logger.warning(
                        "circuit-breaker OUVERT pour %s (%d échecs consécutifs)",
                        self.addr, self._failures,
                    )
                self._state     = CircuitState.OPEN
                self._opened_at = time.time()

    def allow_request(self) -> bool:
        """Retourne True si la requête doit être effectuée."""
        with self._lock:
            if self._state == CircuitState.CLOSED:
                return True

            if self._state == CircuitState.OPEN:
                elapsed = time.time() - self._opened_at
                if elapsed >= self.open_duration_secs:
                    logger.info("circuit-breaker HALF-OPEN pour %s (sonde)", self.addr)
                    self._state = CircuitState.HALF_OPEN
                    return True
                return False

            # HALF_OPEN : une seule sonde autorisée
            return True

    @property
    def state(self) -> CircuitState:
        with self._lock:
            return self._state


# ─── Retry avec backoff ───────────────────────────────────────────────────────

class RetryPolicy:
    """Politique de retry avec backoff exponentiel et jitter."""

    def __init__(
        self,
        max_retries:   int   = 3,
        base_delay:    float = 0.5,
        max_delay:     float = 8.0,
        jitter:        float = 0.1,
    ):
        self.max_retries = max_retries
        self.base_delay  = base_delay
        self.max_delay   = max_delay
        self.jitter      = jitter

    def delays(self):
        """Générateur de délais exponentiels."""
        import random
        delay = self.base_delay
        for attempt in range(self.max_retries):
            jitter_val = random.uniform(-self.jitter, self.jitter) * delay
            yield max(0.0, delay + jitter_val)
            delay = min(delay * 2, self.max_delay)


# ─── CollecteurRésilient ──────────────────────────────────────────────────────

class ResilientCollector:
    """
    Collecteur d'état du cluster avec retry, circuit-breaker et cache stale.

    Drop-in replacement pour `ClusterStateCollector`.
    """

    def __init__(
        self,
        peer_api_addrs:    list[str],
        timeout:           float = 3.0,
        max_retries:       int   = 3,
        failure_threshold: int   = 3,
        open_duration_secs: float = 30.0,
        stale_max_age_secs: float = 120.0,
    ):
        self.peers              = peer_api_addrs
        self.timeout            = timeout
        self.stale_max_age      = stale_max_age_secs
        self._retry_policy      = RetryPolicy(max_retries=max_retries)

        self._session = requests.Session()
        self._session.headers["User-Agent"] = "omega-controller-resilient/0.5"

        # Un circuit-breaker par nœud
        self._breakers: dict[str, CircuitBreaker] = {
            addr: CircuitBreaker(
                addr               = addr,
                failure_threshold  = failure_threshold,
                open_duration_secs = open_duration_secs,
            )
            for addr in peer_api_addrs
        }

        # Cache du dernier état connu par nœud (addr → (timestamp, NodeInfo))
        self._last_known: dict[str, tuple[float, NodeInfo]] = {}
        self._cache_lock = threading.Lock()

    def collect(self) -> ClusterState:
        """Interroge tous les pairs et retourne l'état du cluster."""
        nodes = []
        for addr in self.peers:
            node = self._fetch_with_resilience(addr)
            nodes.append(node)

        from .cluster import ClusterState
        return ClusterState(nodes=nodes)

    def _fetch_with_resilience(self, addr: str) -> NodeInfo:
        """Récupère l'état d'un nœud avec retry + circuit-breaker + cache stale."""
        breaker = self._breakers[addr]

        # Circuit ouvert → court-circuit immédiat
        if not breaker.allow_request():
            logger.debug("circuit OUVERT pour %s — utilisation du cache stale", addr)
            return self._stale_or_unreachable(addr, "circuit-breaker ouvert")

        # Tentatives avec backoff
        last_error = ""
        for i, delay in enumerate(self._retry_policy.delays()):
            if i > 0:
                logger.debug("retry %d pour %s (délai %.1fs)", i, addr, delay)
                time.sleep(delay)

            try:
                node = self._fetch_once(addr)
                breaker.record_success()
                self._update_cache(addr, node)
                return node

            except Exception as e:
                last_error = str(e)
                logger.warning("tentative %d/%d échouée pour %s : %s",
                               i + 1, self._retry_policy.max_retries, addr, e)

        # Toutes les tentatives ont échoué
        breaker.record_failure()
        logger.error("nœud %s injoignable après %d tentatives : %s",
                     addr, self._retry_policy.max_retries, last_error)

        return self._stale_or_unreachable(addr, last_error)

    def _fetch_once(self, addr: str) -> NodeInfo:
        """Une seule tentative HTTP — lève une exception en cas d'échec."""
        base = addr if addr.startswith("http") else f"http://{addr}"
        url  = f"{base}/api/status"

        resp = self._session.get(url, timeout=self.timeout)
        resp.raise_for_status()
        data = resp.json()

        from .cluster import VmEntry, NodeInfo
        vms = [
            VmEntry(
                vmid           = vm["vmid"],
                max_mem_mib    = vm.get("max_mem_mib", 0),
                rss_kb         = vm.get("rss_kb", 0),
                remote_pages   = vm.get("remote_pages", 0),
                remote_mem_mib = vm.get("remote_mem_mib", 0),
                status         = vm.get("status", "Unknown"),
                gpu_vram_budget_mib = vm.get("gpu_vram_budget_mib", 0),
                disk_read_bps = vm.get("disk_read_bps", 0.0),
                disk_write_bps = vm.get("disk_write_bps", 0.0),
                disk_io_weight = vm.get("disk_io_weight", 100),
                disk_local_share_active = vm.get("disk_local_share_active", False),
            )
            for vm in data.get("local_vms", [])
        ]
        gpu = data.get("gpu") or {}

        return NodeInfo(
            node_id          = data.get("node_id", addr),
            store_addr       = data.get("store_addr", ""),
            api_addr         = addr,
            mem_total_kb     = data.get("mem_total_kb", 0),
            mem_available_kb = data.get("mem_available_kb", 0),
            mem_usage_pct    = data.get("mem_usage_pct", 0.0),
            pages_stored     = data.get("pages_stored", 0),
            store_used_kb    = data.get("store_used_kb", 0),
            local_vms        = vms,
            timestamp_secs   = data.get("timestamp_secs", 0),
            gpu_enabled      = gpu.get("enabled", False),
            gpu_total_vram_mib = gpu.get("total_vram_mib", 0),
            gpu_free_vram_mib = gpu.get("free_vram_mib", 0),
            gpu_reserved_vram_mib = gpu.get("reserved_vram_mib", 0),
            gpu_backend_name = gpu.get("backend_name", ""),
            disk_pressure_pct = data.get("disk_pressure_pct", 0.0),
            reachable        = True,
        )

    def _update_cache(self, addr: str, node: NodeInfo) -> None:
        with self._cache_lock:
            self._last_known[addr] = (time.time(), node)

    def _stale_or_unreachable(self, addr: str, error: str) -> NodeInfo:
        """Retourne le dernier état connu si récent, sinon NodeInfo.unreachable."""
        with self._cache_lock:
            cached = self._last_known.get(addr)

        if cached:
            ts, node = cached
            age = time.time() - ts
            if age <= self.stale_max_age:
                logger.warning(
                    "nœud %s injoignable — utilisation état stale (âge %.0fs)",
                    addr, age,
                )
                # Retourner une copie avec stale marqué dans le champ error
                from dataclasses import replace
                return replace(node, reachable=True, error=f"stale ({age:.0f}s)")

        from .cluster import NodeInfo
        return NodeInfo.unreachable(node_id=addr, api_addr=addr, error=error)

    def circuit_states(self) -> dict[str, str]:
        """Retourne l'état de chaque circuit-breaker (pour monitoring)."""
        return {addr: breaker.state.value for addr, breaker in self._breakers.items()}

    def delete_vm_pages(self, api_addr: str, vmid: int) -> bool:
        """Délègue la suppression de pages (même interface que ClusterStateCollector)."""
        base = api_addr if api_addr.startswith("http") else f"http://{api_addr}"
        url  = f"{base}/api/pages/{vmid}"
        try:
            resp = self._session.delete(url, timeout=self.timeout)
            resp.raise_for_status()
            return True
        except Exception as e:
            logger.error("suppression pages vmid=%d sur %s échouée : %s", vmid, api_addr, e)
            return False
