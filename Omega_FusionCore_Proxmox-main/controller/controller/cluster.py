"""
État global du cluster — agrégation depuis tous les omega-daemons.

Chaque omega-daemon expose son état via GET /api/status.
Ce module interroge tous les nœuds connus et construit une vue
cohérente du cluster utilisée par le PlacementEngine.
"""

from __future__ import annotations

import time
import asyncio
import logging
from dataclasses import dataclass, field
from typing import Optional

import requests

logger = logging.getLogger(__name__)


# ─── Structures de données ────────────────────────────────────────────────────

@dataclass
class VmEntry:
    """Résumé d'une VM sur un nœud."""
    vmid:           int
    max_mem_mib:    int
    rss_kb:         int
    remote_pages:   int
    remote_mem_mib: int
    status:         str   # "Running" | "Stopped" | "Unknown"
    gpu_vram_budget_mib: int = 0
    disk_read_bps: float = 0.0
    disk_write_bps: float = 0.0
    disk_io_weight: int = 100
    disk_local_share_active: bool = False

    @property
    def remote_mem_kb(self) -> int:
        return self.remote_pages * 4

    @property
    def max_mem_kb(self) -> int:
        return self.max_mem_mib * 1024


@dataclass
class NodeInfo:
    """État complet d'un nœud du cluster."""
    node_id:          str
    store_addr:       str
    api_addr:         str
    mem_total_kb:     int
    mem_available_kb: int
    mem_usage_pct:    float
    pages_stored:     int
    store_used_kb:    int
    local_vms:        list[VmEntry]
    timestamp_secs:   int
    gpu_enabled:      bool = False
    gpu_total_vram_mib: int = 0
    gpu_free_vram_mib: int = 0
    gpu_reserved_vram_mib: int = 0
    gpu_backend_name: str = ""
    disk_pressure_pct: float = 0.0
    reachable:        bool = True
    error:            str  = ""

    @property
    def mem_free_kb(self) -> int:
        return self.mem_available_kb

    @property
    def mem_free_mib(self) -> int:
        return self.mem_available_kb // 1024

    @property
    def mem_total_mib(self) -> int:
        return self.mem_total_kb // 1024

    @classmethod
    def unreachable(cls, node_id: str, api_addr: str, error: str) -> "NodeInfo":
        return cls(
            node_id=node_id, store_addr="", api_addr=api_addr,
            mem_total_kb=0, mem_available_kb=0, mem_usage_pct=0.0,
            pages_stored=0, store_used_kb=0, local_vms=[],
            timestamp_secs=0, gpu_enabled=False, gpu_total_vram_mib=0,
            gpu_free_vram_mib=0, gpu_reserved_vram_mib=0, gpu_backend_name="",
            disk_pressure_pct=0.0,
            reachable=False, error=error,
        )


@dataclass
class ClusterState:
    """Vue globale du cluster à un instant t."""
    nodes:       list[NodeInfo]
    timestamp:   float = field(default_factory=time.time)

    @property
    def reachable_nodes(self) -> list[NodeInfo]:
        return [n for n in self.nodes if n.reachable]

    @property
    def total_mem_free_mib(self) -> int:
        return sum(n.mem_free_mib for n in self.reachable_nodes)

    @property
    def total_mem_mib(self) -> int:
        return sum(n.mem_total_mib for n in self.reachable_nodes)

    def all_vms(self) -> list[tuple[str, VmEntry]]:
        """Retourne toutes les VMs du cluster avec le nœud hôte."""
        result = []
        for node in self.reachable_nodes:
            for vm in node.local_vms:
                result.append((node.node_id, vm))
        return result

    def vms_with_remote_pages(self, min_pages: int = 100) -> list[tuple[str, VmEntry]]:
        """Retourne les VMs ayant au moins `min_pages` pages distantes."""
        return [
            (node_id, vm)
            for node_id, vm in self.all_vms()
            if vm.status == "Running" and vm.remote_pages >= min_pages
        ]

    def node_by_id(self, node_id: str) -> Optional[NodeInfo]:
        return next((n for n in self.nodes if n.node_id == node_id), None)

    def summary(self) -> dict:
        return {
            "total_nodes":       len(self.nodes),
            "reachable_nodes":   len(self.reachable_nodes),
            "total_mem_mib":     self.total_mem_mib,
            "total_free_mib":    self.total_mem_free_mib,
            "total_vms":         sum(len(n.local_vms) for n in self.reachable_nodes),
            "vms_with_remote":   len(self.vms_with_remote_pages(1)),
            "timestamp":         self.timestamp,
        }


# ─── Collecteur d'état ────────────────────────────────────────────────────────

class ClusterStateCollector:
    """
    Collecte l'état de tous les omega-daemons du cluster via HTTP.

    Utilise requests avec un timeout court. En V5 : aiohttp pour async pur.
    """

    def __init__(self, peer_api_addrs: list[str], timeout: float = 3.0):
        """
        Args:
            peer_api_addrs: Adresses HTTP des API des pairs, ex: ["192.168.1.1:9200", ...]
        """
        self.peers   = peer_api_addrs
        self.timeout = timeout
        self._session = requests.Session()
        self._session.headers["User-Agent"] = "omega-controller/0.4"

    def _fetch_node_status(self, addr: str) -> NodeInfo:
        """Interroge un seul nœud. Retourne un NodeInfo.unreachable en cas d'erreur."""
        # addr peut être "host:port" ou "http://host:port"
        base = addr if addr.startswith("http") else f"http://{addr}"
        url  = f"{base}/api/status"

        try:
            resp = self._session.get(url, timeout=self.timeout)
            resp.raise_for_status()
            data = resp.json()

            vms = [
                VmEntry(
                    vmid         = vm["vmid"],
                    max_mem_mib  = vm.get("max_mem_mib", 0),
                    rss_kb       = vm.get("rss_kb", 0),
                    remote_pages = vm.get("remote_pages", 0),
                    remote_mem_mib = vm.get("remote_mem_mib", 0),
                    status       = vm.get("status", "Unknown"),
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

        except Exception as e:
            logger.debug("nœud %s injoignable : %s", addr, e)
            return NodeInfo.unreachable(node_id=addr, api_addr=addr, error=str(e))

    def collect(self) -> ClusterState:
        """Interroge tous les pairs et retourne l'état du cluster."""
        nodes = []
        for addr in self.peers:
            node = self._fetch_node_status(addr)
            nodes.append(node)
            if node.reachable:
                logger.debug(
                    "nœud %s : RAM %d/%d Mio, %d pages, %d VMs",
                    node.node_id, node.mem_free_mib, node.mem_total_mib,
                    node.pages_stored, len(node.local_vms),
                )
            else:
                logger.warning("nœud %s injoignable : %s", addr, node.error)

        return ClusterState(nodes=nodes)

    def delete_vm_pages(self, api_addr: str, vmid: int) -> bool:
        """
        Supprime toutes les pages d'une VM sur un nœud donné (post-migration).

        Appelé après une migration réussie pour libérer le store distant.
        """
        base = api_addr if api_addr.startswith("http") else f"http://{api_addr}"
        url  = f"{base}/api/pages/{vmid}"
        try:
            resp = self._session.delete(url, timeout=self.timeout)
            resp.raise_for_status()
            data = resp.json()
            logger.info(
                "pages supprimées sur %s : vmid=%d, deleted=%d",
                api_addr, vmid, data.get("deleted", 0),
            )
            return True
        except Exception as e:
            logger.error("suppression pages vmid=%d sur %s échouée : %s", vmid, api_addr, e)
            return False
