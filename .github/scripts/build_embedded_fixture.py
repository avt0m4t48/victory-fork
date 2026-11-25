#!/usr/bin/env python3
"""Build the source-only reduction in victory-shader emit/wave64_tests.rs."""
from pathlib import Path
from struct import pack, pack_into
from zlib import compress

def create_fixture(deep, per_lane=False):
    words = [
        0xbe850380, 0xf4080202, 0xfa000000,  # load the UAV descriptor
        0x7e0a0281, 0x2c0c0081, 0x340e0b06,  # contribution: 1 << (thread / 2)
    ]
    if per_lane:
        words.extend([0x34100082, 0x7e1202ff, 0x55555555])  # v8 = 4 * thread, v9 = sentinel
    if deep:
        words.extend([0xbe860301, 0xbe940306])  # initialize s6 from s1 and save it to s20
    words.extend([0x7d880000, 0xbe8c246a])  # compare then narrow EXEC
    branch = len(words)
    words.append(0xbf880000 if deep else 0xbf840000)
    if deep:
        words.append(0xbe860385)  # set s6 = 5
    words.extend([
        0xbe8425c1, 0xd5010001, 0x00120e80, 0x380202e9, 0xde54c101,
        0x380202e9, 0x93601201, 0x380202e9, 0x00092401, 0x380202fa,
        0xff094001, 0xd7780002, 0x02010101, 0x38020302, 0xd7600007,
        0x00014101, 0x38020207, 0xd7600007, 0x00010101, 0xbefe0404,
    ])
    if per_lane:
        words.append(0x7e120207)  # v9 = reduction for EXEC lanes only
    else:
        words.extend([0x7e000207, 0xe0700000, 0x80020000])  # store OR reduction
    if deep:
        words.append(0xbe860314)  # restore s6 from s20
    words[branch] |= len(words) - branch - 1
    words.append(0xbefe040c)  # restore EXEC
    if deep:
        words.append(0x7e040206)  # read s6 after the join
    if per_lane:
        words.extend([0xe0701000, 0x80020908])  # buffer_store_dword v9, v8, s[8:11], 0 offen
    words.append(0xbf810000)
    program = pack(f"<{len(words)}I", *words)
    record = 0x68
    stage_data = record + 0x54
    layout = stage_data + 0x34
    used = layout + 40
    code = used + 20
    payload = bytearray(code)
    pack_into("<Q", payload, 0, 6)
    for slot in (0x8, 0x10, 0x18, 0x20, 0x28, 0x30, 0x48):
        pack_into("<Q", payload, slot, 0xdeadbeefdeadbeef)
    pack_into("<Q", payload, 0x60, record)
    pack_into("<QQ", payload, record, code, len(program))
    pack_into("<QQII", payload, record + 0x28, layout, used, 2, 1)
    pack_into("<I", payload, record + 0x50, 16)
    pack_into("<IIII", payload, stage_data + 0x24, 5 << 1, 64, 1, 1)
    pack_into("<IIIIBBH", payload, layout, 0, 0, 4, 0, 0, 0, 0)
    pack_into("<IIIIBBH", payload, layout + 20, 3, 0, 1, 0, 4, 1, 0)
    pack_into("<BBHIIIBBBB", payload, used, 3, 3, 0, 0, 0, 0, 4, 4, 1, 0)
    payload.extend(program)
    fssm = pack("<4sIQ", b"FSSM", 0x101, len(payload)) + payload
    stream = compress(fssm)
    xbch = pack("<4sIQQ", b"XBCH", 3, len(stream), len(fssm)) + stream
    dxil = bytes(0x18) + pack("<4sIIIII", b"XDAT", 1, 0x24, 0x0d, 0x30, len(xbch)) + xbch
    return pack("<4s16sIIII4sI", b"DXBC", bytes(16), 1, 44 + len(dxil), 1, 36, b"DXIL", len(dxil)) + dxil

for name, deep in (("approx-only", False), ("deep", True)):
    # The header is 36 bytes including the part-offset table, followed by the
    # 8-byte DXIL part header. The fixture is generated, never an input asset.
    Path(f"{name}.dxbc").write_bytes(create_fixture(deep))

Path("dispatch-oracle.dxbc").write_bytes(create_fixture(deep=True, per_lane=True))
