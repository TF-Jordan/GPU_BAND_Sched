"""
Client Proxmox VE API — implémentation V4.

# Authentification

Deux méthodes supportées :

1. **Token API** (recommandé) :
   Authorization: PVEAPIToken=user@realm!tokenid=UUID

2. **Ticket** (session) :
   POST /access/ticket → ticket + CSRFPreventionToken

# Migration live (Ceph RBD)

POST /api2/json/nodes/{source_node}/qemu/{vmid}/migrate
Body:
  target  : nœud cible (ex: "pve-node2")
  online  : 1 (live migration sans arrêt) / 0 (cold)
  bwlimit : limite bande passante en Ko/s (0 = illimité)

Pas de `with-local-disks` — les disques sont sur Ceph, déjà accessibles
depuis le nœud cible.

# Suivi de tâche

La migration retourne un UPID (Unique Process ID Proxmox).
On peut surveiller la progression via :
GET /api2/json/nodes/{node}/tasks/{upid}/status
"""

from __future__ import annotations

import logging
import re
import time
from dataclasses import dataclass
from typing import Optional
from urllib.parse import urljoin

import requests
import urllib3

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

logger = logging.getLogger(__name__)


# ─── Structures ───────────────────────────────────────────────────────────────

@dataclass
class MigrationTask:
    """Tâche de migration en cours."""
    vmid:        int
    source_node: str
    target_node: str
    upid:        str      # Proxmox UPID : "UPID:pve-node1:..."
    started_at:  float    = 0.0
    status:      str      = "pending"  # pending | running | OK | ERROR

    @property
    def elapsed_secs(self) -> float:
        return time.time() - self.started_at if self.started_at else 0.0

    @property
    def completed(self) -> bool:
        return self.status in ("OK", "stopped", "ERROR")


@dataclass
class NodeSummary:
    name:       str
    status:     str    # "online" | "offline"
    mem_total:  int    # Ko
    mem_used:   int    # Ko
    cpu:        float  # 0.0 – 1.0

    @property
    def mem_free_mib(self) -> int:
        return (self.mem_total - self.mem_used) // 1024


# ─── Client ───────────────────────────────────────────────────────────────────

class ProxmoxClient:
    """
    Client REST Proxmox VE API.

    Paramètres d'environnement recommandés :
        PROXMOX_URL    : ex "https://192.168.1.1:8006"
        PROXMOX_TOKEN  : ex "root@pam!omega-token=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
        PROXMOX_NODE   : nœud par défaut pour les requêtes
    """

    def __init__(
        self,
        base_url:          str   = "https://127.0.0.1:8006",
        api_token:         str   = "",
        verify_ssl:        bool  = False,
        timeout:           float = 10.0,
        with_local_disks:  bool  = False,
    ):
        self.base_url          = base_url.rstrip("/")
        self.api_token         = api_token
        self.verify_ssl        = verify_ssl
        self.timeout           = timeout
        self.with_local_disks  = with_local_disks
        self._stub_mode        = not bool(api_token)

        self._session = requests.Session()
        self._session.verify = verify_ssl

        if api_token:
            self._session.headers["Authorization"] = f"PVEAPIToken={api_token}"
            logger.info(
                "ProxmoxClient configuré — URL=%s with_local_disks=%s",
                base_url, with_local_disks,
            )
        else:
            logger.warning(
                "=" * 60 + "\n"
                "  ATTENTION : ProxmoxClient en MODE STUB\n"
                "  Aucune migration ne sera réellement exécutée.\n"
                "  Fournir --proxmox-token pour activer les migrations réelles.\n"
                + "=" * 60
            )

    def _url(self, path: str) -> str:
        return f"{self.base_url}/api2/json/{path.lstrip('/')}"

    def _get(self, path: str) -> Optional[dict]:
        if self._stub_mode:
            return None
        try:
            resp = self._session.get(self._url(path), timeout=self.timeout)
            resp.raise_for_status()
            return resp.json().get("data")
        except Exception as e:
            logger.error("GET %s échoué : %s", path, e)
            return None

    def _post(self, path: str, data: dict) -> Optional[str]:
        """Retourne le UPID de la tâche si succès, None sinon."""
        if self._stub_mode:
            logger.warning("STUB POST %s body=%s", path, data)
            return f"UPID:stub-node:STUB:{int(time.time())}:0:stub"
        try:
            resp = self._session.post(self._url(path), data=data, timeout=self.timeout)
            resp.raise_for_status()
            upid = resp.json().get("data")
            logger.info("POST %s → UPID=%s", path, upid)
            return upid
        except Exception as e:
            logger.error("POST %s échoué : %s", path, e)
            return None

    # ─── Nœuds ────────────────────────────────────────────────────────────

    def get_nodes(self) -> list[NodeSummary]:
        """Retourne la liste des nœuds du cluster."""
        if self._stub_mode:
            return self._stub_nodes()

        data = self._get("nodes") or []
        return [
            NodeSummary(
                name      = n["node"],
                status    = n.get("status", "unknown"),
                mem_total = n.get("maxmem", 0) // 1024,  # API retourne en octets
                mem_used  = n.get("mem", 0) // 1024,
                cpu       = n.get("cpu", 0.0),
            )
            for n in data
        ]

    def get_node_with_most_free_ram(self, exclude: Optional[str] = None) -> Optional[str]:
        """Retourne le nœud ayant le plus de RAM libre (hors `exclude`)."""
        nodes = [n for n in self.get_nodes()
                 if n.status == "online" and n.name != exclude]
        if not nodes:
            return None
        return max(nodes, key=lambda n: n.mem_free_mib).name

    # ─── VMs ──────────────────────────────────────────────────────────────

    def get_vm_node(self, vmid: int) -> Optional[str]:
        """Retourne le nœud hébergeant actuellement la VM."""
        if self._stub_mode:
            return "pve-node1"  # stub

        # Interroge tous les nœuds
        for node in self.get_nodes():
            data = self._get(f"nodes/{node.name}/qemu/{vmid}/status/current")
            if data:
                return node.name
        return None

    def get_vm_config(self, node: str, vmid: int) -> dict:
        """Retourne la configuration Proxmox brute d'une VM."""
        if self._stub_mode:
            return {}
        return self._get(f"nodes/{node}/qemu/{vmid}/config") or {}

    @staticmethod
    def parse_omega_metadata(config: dict) -> dict:
        """
        Extrait les métadonnées omega depuis la config Proxmox.

        Sources supportées :
        - description multiline :
            omega.gpu_vram_mib=2048
            omega.min_vcpus=2
            omega.max_vcpus=8
        - tags Proxmox :
            omega-gpu-2048;omega-min-vcpus-2;omega-max-vcpus-8

        Valeurs par défaut :
        - max_vcpus = sockets × cores si omega.max_vcpus n'est pas déclaré
        - min_vcpus = max_vcpus // 2 si omega.min_vcpus n'est pas déclaré
        """
        result = {
            "gpu_vram_mib": 0,
            "min_vcpus": None,
            "max_vcpus": None,
        }

        description = config.get("description", "") or ""
        for line in description.splitlines():
            if "=" not in line:
                continue
            key, value = [part.strip() for part in line.split("=", 1)]
            if key == "omega.gpu_vram_mib":
                result["gpu_vram_mib"] = int(value or 0)
            elif key == "omega.min_vcpus":
                result["min_vcpus"] = int(value or 0)
            elif key == "omega.max_vcpus":
                result["max_vcpus"] = int(value or 0)

        tags = config.get("tags", "") or ""
        for tag in re.split(r"[;, ]+", tags):
            if not tag:
                continue
            if match := re.fullmatch(r"omega-gpu-(\d+)", tag):
                result["gpu_vram_mib"] = int(match.group(1))
            elif match := re.fullmatch(r"omega-min-vcpus-(\d+)", tag):
                result["min_vcpus"] = int(match.group(1))
            elif match := re.fullmatch(r"omega-max-vcpus-(\d+)", tag):
                result["max_vcpus"] = int(match.group(1))

        sockets = int(config.get("sockets", 1) or 1)
        cores = int(config.get("cores", 1) or 1)
        inferred_max_vcpus = max(1, sockets * cores)

        if result["max_vcpus"] is None:
            result["max_vcpus"] = inferred_max_vcpus

        if result["min_vcpus"] is None and result["max_vcpus"] is not None:
            result["min_vcpus"] = max(1, result["max_vcpus"] // 2)

        max_vcpus = result["max_vcpus"]
        min_vcpus = result["min_vcpus"]
        if min_vcpus is not None and max_vcpus is not None and min_vcpus > max_vcpus:
            result["min_vcpus"] = max_vcpus

        return result

    # ─── Migration ────────────────────────────────────────────────────────

    def migrate_vm(
        self,
        vmid:        int,
        source_node: str,
        target_node: str,
        online:      bool = True,
        bwlimit:     int  = 0,
    ) -> Optional[MigrationTask]:
        """
        Déclenche une migration d'une VM vers un autre nœud (Ceph RBD).

        Tente d'abord une migration live (online=True). Si elle échoue,
        bascule en migration offline (VM arrêtée momentanément).

        Returns:
            MigrationTask avec le UPID si la migration a démarré, None sinon.
        """
        logger.info(
            "migration VM %d : %s → %s (online=%s)",
            vmid, source_node, target_node, online,
        )

        upid = self._try_migrate(vmid, source_node, target_node,
                                  online=online, bwlimit=bwlimit)

        if not upid and online:
            logger.warning(
                "migration LIVE échouée pour vmid=%d — bascule en mode OFFLINE",
                vmid,
            )
            logger.warning(
                "ATTENTION : migration offline = arrêt momentané de la VM %d", vmid
            )
            upid = self._try_migrate(vmid, source_node, target_node,
                                      online=False, bwlimit=bwlimit)

        if not upid:
            logger.error("migration vmid=%d : échec live ET offline", vmid)
            return None

        task = MigrationTask(
            vmid        = vmid,
            source_node = source_node,
            target_node = target_node,
            upid        = upid,
            started_at  = time.time(),
            status      = "running",
        )
        logger.info("migration démarrée : UPID=%s", upid)
        return task

    def _try_migrate(
        self,
        vmid:        int,
        source_node: str,
        target_node: str,
        online:      bool,
        bwlimit:     int,
    ) -> Optional[str]:
        """Tente une migration et retourne le UPID, ou None en cas d'échec."""
        body: dict = {
            "target": target_node,
            "online": int(online),
        }
        if bwlimit > 0:
            body["bwlimit"] = bwlimit
        # Stockage local : les disques ne sont pas accessibles depuis le nœud cible,
        # il faut les copier (migration plus lente, requiert de l'espace disque cible).
        if self.with_local_disks:
            body["with-local-disks"] = 1

        return self._post(f"nodes/{source_node}/qemu/{vmid}/migrate", body)

    def get_task_status(self, node: str, upid: str) -> dict:
        """
        Retourne l'état d'une tâche Proxmox.

        Returns: {"status": "running"|"stopped", "exitstatus": "OK"|"ERROR:...", ...}
        """
        if self._stub_mode or upid.startswith("UPID:stub"):
            # Simulation : la migration prend 10s
            return {"status": "stopped", "exitstatus": "OK"}

        data = self._get(f"nodes/{node}/tasks/{upid}/status") or {}
        return data

    def wait_for_migration(
        self,
        task:           MigrationTask,
        poll_interval:  float = 5.0,
        max_wait_secs:  float = 600.0,
    ) -> bool:
        """
        Attend la fin d'une migration et retourne True si succès.

        Poll l'API Proxmox toutes les `poll_interval` secondes.
        """
        deadline = time.time() + max_wait_secs

        while time.time() < deadline:
            status_data = self.get_task_status(task.source_node, task.upid)
            pve_status  = status_data.get("status", "running")
            exit_status = status_data.get("exitstatus", "")

            logger.debug(
                "migration vmid=%d : status=%s exit=%s elapsed=%.0fs",
                task.vmid, pve_status, exit_status, task.elapsed_secs,
            )

            if pve_status == "stopped":
                success = exit_status == "OK"
                task.status = "OK" if success else "ERROR"

                if success:
                    logger.info(
                        "migration réussie : vmid=%d → %s en %.0fs",
                        task.vmid, task.target_node, task.elapsed_secs,
                    )
                else:
                    logger.error(
                        "migration échouée : vmid=%d, exitstatus=%s",
                        task.vmid, exit_status,
                    )
                return success

            time.sleep(poll_interval)

        logger.error(
            "migration timeout : vmid=%d, UPID=%s (%.0fs écoulés)",
            task.vmid, task.upid, max_wait_secs,
        )
        task.status = "ERROR"
        return False

    # ─── Stubs ────────────────────────────────────────────────────────────

    def _stub_nodes(self) -> list[NodeSummary]:
        """Nœuds simulés pour les tests sans Proxmox réel."""
        return [
            NodeSummary("pve-node1", "online", 16*1024*1024, 13*1024*1024, 0.70),
            NodeSummary("pve-node2", "online", 32*1024*1024,  4*1024*1024, 0.10),
            NodeSummary("pve-node3", "online", 32*1024*1024,  8*1024*1024, 0.20),
        ]

    def cluster_memory_summary(self) -> dict:
        nodes = self.get_nodes()
        total = sum(n.mem_total for n in nodes)
        used  = sum(n.mem_used  for n in nodes)
        return {
            "nodes": [
                {
                    "name":         n.name,
                    "mem_total_mib": n.mem_total // 1024,
                    "mem_free_mib":  n.mem_free_mib,
                    "usage_pct":     round((n.mem_used / n.mem_total) * 100, 1) if n.mem_total else 0,
                }
                for n in nodes
            ],
            "total_mem_mib": total // 1024,
            "used_mem_mib":  used  // 1024,
        }
