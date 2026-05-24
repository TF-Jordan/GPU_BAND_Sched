"""
Tests unitaires du protocole binaire (côté Python).

Valide que le client Python (store_client.py) produit et parse
des trames conformes à la spec du protocole (docs/protocol.md).
"""

import struct
import pytest
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "controller"))

from controller.store_client import (
    _build_frame, _parse_header,
    Opcode, MAGIC, PAGE_SIZE, HEADER_SIZE,
)


class TestHeaderFormat:
    def test_header_size_is_20_bytes(self):
        frame = _build_frame(Opcode.PING)
        assert len(frame) == HEADER_SIZE

    def test_magic_is_correct(self):
        frame = _build_frame(Opcode.PING)
        magic = struct.unpack(">H", frame[:2])[0]
        assert magic == MAGIC, f"magic incorrect : 0x{magic:04x}"

    def test_opcode_position(self):
        frame = _build_frame(Opcode.PING)
        assert frame[2] == Opcode.PING

    def test_vm_id_big_endian(self):
        vm_id = 0xDEADBEEF
        frame = _build_frame(Opcode.GET_PAGE, vm_id=vm_id)
        parsed_vm_id = struct.unpack(">I", frame[4:8])[0]
        assert parsed_vm_id == vm_id

    def test_page_id_big_endian(self):
        page_id = 0x0102030405060708
        frame = _build_frame(Opcode.GET_PAGE, page_id=page_id)
        parsed_page_id = struct.unpack(">Q", frame[8:16])[0]
        assert parsed_page_id == page_id

    def test_payload_len_big_endian(self):
        payload = b"\xAB" * PAGE_SIZE
        frame = _build_frame(Opcode.PUT_PAGE, payload=payload)
        payload_len = struct.unpack(">I", frame[16:20])[0]
        assert payload_len == PAGE_SIZE

    def test_total_frame_size_with_payload(self):
        payload = b"\x00" * PAGE_SIZE
        frame = _build_frame(Opcode.PUT_PAGE, payload=payload)
        assert len(frame) == HEADER_SIZE + PAGE_SIZE


class TestHeaderParsing:
    def test_roundtrip_ping(self):
        frame = _build_frame(Opcode.PING)
        opcode, flags, vm_id, page_id, payload_len = _parse_header(frame[:HEADER_SIZE])
        assert opcode      == Opcode.PING
        assert flags       == 0
        assert vm_id       == 0
        assert page_id     == 0
        assert payload_len == 0

    def test_roundtrip_put_page(self):
        payload = bytes(range(256)) * 16
        frame   = _build_frame(Opcode.PUT_PAGE, vm_id=42, page_id=1234, payload=payload)
        opcode, flags, vm_id, page_id, payload_len = _parse_header(frame[:HEADER_SIZE])
        assert opcode      == Opcode.PUT_PAGE
        assert vm_id       == 42
        assert page_id     == 1234
        assert payload_len == PAGE_SIZE

    def test_bad_magic_raises(self):
        frame = bytearray(_build_frame(Opcode.PING))
        frame[0] = 0xDE
        frame[1] = 0xAD
        with pytest.raises(ValueError, match="magic"):
            _parse_header(bytes(frame))

    def test_large_vm_id(self):
        vm_id = 2**32 - 1  # u32 max
        frame = _build_frame(Opcode.GET_PAGE, vm_id=vm_id)
        _, _, parsed_vm_id, _, _ = _parse_header(frame[:HEADER_SIZE])
        assert parsed_vm_id == vm_id

    def test_large_page_id(self):
        page_id = 2**64 - 1  # u64 max
        frame = _build_frame(Opcode.GET_PAGE, page_id=page_id)
        _, _, _, parsed_page_id, _ = _parse_header(frame[:HEADER_SIZE])
        assert parsed_page_id == page_id


class TestOpcodes:
    """Vérifie que tous les opcodes ont des valeurs distinctes et cohérentes avec le protocole."""

    def test_all_opcodes_distinct(self):
        opcodes = [
            Opcode.PING, Opcode.PONG,
            Opcode.PUT_PAGE, Opcode.GET_PAGE, Opcode.DELETE_PAGE,
            Opcode.STATS_REQUEST, Opcode.STATS_RESPONSE,
            Opcode.OK, Opcode.NOT_FOUND, Opcode.ERROR,
        ]
        assert len(opcodes) == len(set(opcodes)), "des opcodes ont des valeurs dupliquées"

    def test_ping_is_0x01(self):
        assert Opcode.PING == 0x01

    def test_put_page_is_0x10(self):
        assert Opcode.PUT_PAGE == 0x10

    def test_ok_is_0x80(self):
        assert Opcode.OK == 0x80

    def test_not_found_is_0x81(self):
        assert Opcode.NOT_FOUND == 0x81


class TestFrameConstruction:
    def test_empty_payload_produces_correct_frame(self):
        frame = _build_frame(Opcode.PING)
        assert len(frame) == HEADER_SIZE
        assert frame[16:20] == b"\x00\x00\x00\x00"  # payload_len = 0

    def test_page_payload_appended_after_header(self):
        payload = b"\xCC" * PAGE_SIZE
        frame   = _build_frame(Opcode.PUT_PAGE, payload=payload)
        assert frame[HEADER_SIZE:] == payload
