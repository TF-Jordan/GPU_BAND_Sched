"""
Client Python simplifié vers les stores node-bc-store.

Utilisé par le controller pour interroger les métriques des stores B et C.
Implémente le même protocole binaire que le client Rust (node-a-agent/remote.rs).
"""

from __future__ import annotations

import socket
import struct
import json
from dataclasses import dataclass
from typing import Optional

# ─── Constantes protocole (doivent correspondre à node-bc-store/src/protocol.rs) ───

MAGIC       = 0x524D
PAGE_SIZE   = 4096
HEADER_SIZE = 20

class Opcode:
    PING          = 0x01
    PONG          = 0x02
    PUT_PAGE      = 0x10
    GET_PAGE      = 0x11
    DELETE_PAGE   = 0x12
    STATS_REQUEST = 0x20
    STATS_RESPONSE= 0x21
    OK            = 0x80
    NOT_FOUND     = 0x81
    ERROR         = 0x82


@dataclass
class RawMessage:
    opcode:  int
    flags:   int
    vm_id:   int
    page_id: int
    payload: bytes


def _pack_header(opcode: int, flags: int, vm_id: int, page_id: int, payload_len: int) -> bytes:
    return struct.pack(">HBBIQII"[:], MAGIC, opcode, flags, vm_id, page_id, payload_len)[:]


def _build_frame(opcode: int, vm_id: int = 0, page_id: int = 0, payload: bytes = b"") -> bytes:
    """Construit une trame complète (header + payload)."""
    header = struct.pack(">HBBIQI",
        MAGIC,
        opcode,
        0,           # flags
        vm_id,
        page_id,
        len(payload),
    )
    return header + payload


def _parse_header(data: bytes) -> tuple[int, int, int, int, int]:
    """Parse les 20 octets d'en-tête. Retourne (opcode, flags, vm_id, page_id, payload_len)."""
    magic, opcode, flags, vm_id, page_id, payload_len = struct.unpack(">HBBIQI", data)
    if magic != MAGIC:
        raise ValueError(f"magic incorrect : 0x{magic:04x}")
    return opcode, flags, vm_id, page_id, payload_len


class StoreClient:
    """
    Client synchrone TCP vers un store node-bc-store.

    Utilisé par le controller (pas par l'agent — l'agent utilise le client Rust async).
    """

    def __init__(self, host: str, port: int, timeout: float = 3.0):
        self.host    = host
        self.port    = port
        self.timeout = timeout
        self._sock: Optional[socket.socket] = None

    def connect(self) -> None:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.connect((self.host, self.port))
        self._sock = sock

    def disconnect(self) -> None:
        if self._sock:
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None

    def __enter__(self):
        self.connect()
        return self

    def __exit__(self, *_):
        self.disconnect()

    def _send_recv(self, frame: bytes) -> RawMessage:
        """Envoie une trame et lit la réponse."""
        if not self._sock:
            self.connect()

        assert self._sock is not None
        self._sock.sendall(frame)

        # Lecture de l'en-tête
        header = self._recv_exact(HEADER_SIZE)
        opcode, flags, vm_id, page_id, payload_len = _parse_header(header)

        # Lecture du payload
        payload = self._recv_exact(payload_len) if payload_len > 0 else b""

        return RawMessage(opcode=opcode, flags=flags, vm_id=vm_id,
                          page_id=page_id, payload=payload)

    def _recv_exact(self, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            chunk = self._sock.recv(n - len(buf))  # type: ignore[union-attr]
            if not chunk:
                raise ConnectionError("connexion fermée par le store")
            buf.extend(chunk)
        return bytes(buf)

    def ping(self) -> bool:
        """Retourne True si le store répond à PONG."""
        try:
            resp = self._send_recv(_build_frame(Opcode.PING))
            return resp.opcode == Opcode.PONG
        except Exception:
            return False

    def get_stats(self) -> Optional[dict]:
        """Récupère les statistiques du store (réponse JSON)."""
        try:
            resp = self._send_recv(_build_frame(Opcode.STATS_REQUEST))
            if resp.opcode == Opcode.STATS_RESPONSE and resp.payload:
                return json.loads(resp.payload.decode("utf-8"))
        except Exception:
            pass
        return None


@dataclass
class StoreStatus:
    addr:        str
    reachable:   bool
    stats:       Optional[dict]


def poll_all_stores(store_addrs: list[str], timeout: float = 2.0) -> list[StoreStatus]:
    """
    Interroge tous les stores **en parallèle** et retourne leur statut.

    Chaque store est interrogé dans un thread dédié via ThreadPoolExecutor.
    L'ordre de la liste retournée correspond à l'ordre de `store_addrs`.
    """
    from concurrent.futures import ThreadPoolExecutor

    def poll_one(addr: str) -> StoreStatus:
        host, port_str = addr.rsplit(":", 1)
        port = int(port_str)
        try:
            with StoreClient(host, port, timeout) as client:
                ok    = client.ping()
                stats = client.get_stats() if ok else None
                return StoreStatus(addr=addr, reachable=ok, stats=stats)
        except Exception:
            return StoreStatus(addr=addr, reachable=False, stats=None)

    if not store_addrs:
        return []

    with ThreadPoolExecutor(max_workers=len(store_addrs)) as ex:
        # map() préserve l'ordre de store_addrs
        return list(ex.map(poll_one, store_addrs))
