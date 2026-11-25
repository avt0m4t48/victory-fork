#!/usr/bin/env python3
"""Build Victory-readable synthetic containers around original GFX10 .text."""
from pathlib import Path
import struct
import sys
import zlib

SLOT_OFFSETS = (0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x48, 0x60)


def signature(elements):
    """One row per semantic with a known raw DXBC register mask."""
    names = bytearray()
    start = 8 + len(elements) * 32
    part = bytearray(struct.pack("<II", len(elements), 8))
    for name, system, component, row, mask in elements:
        part += struct.pack("<IIIIII4BI", 0, start + len(names), 0, system,
                            component, row, mask, 0, 0, 0, 0)
        names += name.encode("ascii") + b"\0"
    return part + names


def native_part(code, mesh, flat):
    payload = bytearray(0x68)
    struct.pack_into("<Q", payload, 0, 6)
    for offset in SLOT_OFFSETS:
        struct.pack_into("<Q", payload, offset, 0xDEADBEEFDEADBEEF)
    stage_slot = 0x48 if mesh else 0x30
    struct.pack_into("<Q", payload, stage_slot, len(payload))
    record = bytearray(0x54)
    if mesh:
        stage_data = bytearray(0x45C)
        for param in range(16):
            row = param + 1
            flags = 0b11 if param == 15 else 0
            offset = 0xC8 + 6 * param
            stage_data[offset:offset + 6] = bytes([6, row, param, row, param, flags])
        struct.pack_into("<I", stage_data, 0x188, 16)
        struct.pack_into("<HHHBB", stage_data, 0x448, 128, 128, 1, 0, 2)
        vgprs = 16
    else:
        stage_data = bytearray(0x14C + 8 * 404)
        # attr3.x carries both guest halves. The signature element occupies z/w
        # of row 3. The native mask names the one physical interpolation dword.
        flags = 0b11000 | (1 if flat else 2)
        stage_data[0x24:0x2C] = bytes([6, 3, 0b0001, 3, 0, 3, 0, flags])
        struct.pack_into("<I", stage_data, 0x124, 1)
        struct.pack_into("<I", stage_data, 0x134, 0 if flat else (1 << 1))
        for mrt in range(8):
            struct.pack_into("<I", stage_data, 0x14C + 404 * mrt, 0xFFFFFFFF)
        struct.pack_into("<I", record, 0x14, 1 << 5)  # wave32
        vgprs = 4
    struct.pack_into("<QQ", record, 0, len(payload) + len(record) + len(stage_data), len(code))
    struct.pack_into("<I", record, 0x50, vgprs)
    payload += record + stage_data + code
    unpacked = b"FSSM" + struct.pack("<IQ", 0x101, len(payload)) + payload
    packed = zlib.compress(unpacked, level=6)
    blob = b"XBCH" + struct.pack("<IQQ", 3, len(packed), len(unpacked)) + packed
    return bytes(0x18) + b"XDAT" + struct.pack("<IIIII", 1, 0x24, 0x0D, 0x30, len(blob)) + blob


def container(parts):
    chunks = [tag + struct.pack("<I", len(data)) + data for tag, data in parts]
    offset = 0x20 + 4 * len(parts)
    offsets = bytearray()
    for chunk in chunks:
        offsets += struct.pack("<I", offset)
        offset += len(chunk)
    return b"DXBC" + bytes(16) + struct.pack("<HHII", 1, 0, offset, len(parts)) + offsets + b"".join(chunks)


def main():
    variant, source, destination = sys.argv[1:]
    mesh = variant in ("mesh", "mesh-control")
    code = bytes.fromhex(Path(source).read_text(encoding="ascii"))
    if mesh:
        outputs = [("SV_Position", 1, 3, 0, 0xF)]
        outputs.extend((f"BASE{n:02}", 0, 3, n + 1, 0xF) for n in range(14))
        outputs.append(("TAIL", 0, 3, 15, 0x3 if variant == "mesh-control" else 0x7))
        outputs.append(("ATTRPAIR", 0, 6, 16, 0x3))
        signatures = [(b"OSG1", signature(outputs))]
    else:
        assert variant in ("pixel-flat", "pixel-center")
        signatures = [
            (b"ISG1", signature([("ATTRPAIR", 0, 6, 3, 0xC)])),
            (b"OSG1", signature([("SV_Target", 64, 3, 0, 0xF)])),
        ]
    Path(destination).write_bytes(container(signatures + [(b"DXIL", native_part(code, mesh, variant == "pixel-flat"))]))


if __name__ == "__main__":
    main()
