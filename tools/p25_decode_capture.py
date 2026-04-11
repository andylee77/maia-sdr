#!/usr/bin/env python3
"""
Phase 6F.2h: replay an aligned capture from the on-target Fishball P25
LSM software decoder and run the same TSBK decode pipeline in pure
Python with verbose intermediate state.

Use case: when the on-target dashboard reports "PS LSM TSBK CRC fails
on every block" but we can't tell whether the bug is in the
deinterleaver, the trellis, the byte packing, or the CRC, this script
takes a single captured frame and prints byte-by-byte diff vs the
on-target Rust output AND vs a Python reference encode that we cross-
check against SDRTrunk.

Usage:
    # Live capture from the radio:
    wget -qO capture.json http://fishball.local:8080/api/lsm_capture_aligned
    python tools/p25_decode_capture.py capture.json

    # Or pipe directly:
    wget -qO - http://fishball.local:8080/api/lsm_capture_aligned \\
        | python tools/p25_decode_capture.py -

What it prints:
    - Sync dibits (24) and Hamming distance to FRAME_SYNC_DIBIT_PATTERN
    - Raw NID dibits (33) with status dibit position highlighted
    - 64-bit nid_bits word (status dibit removed) compared against
      what we expect for NAC=0x8A1 DUID=0x7
    - BCH-decoded NAC/DUID (Rust on-target value vs Python reference)
    - Raw TSDU body dibits (122) with status positions highlighted
    - Trellis input dibits after deinterleave (98)
    - Trellis decode in Python -- 12 bytes -- compared against Rust
    - CRC-16 CCITT computed both conventions, compared against the
      message CRC field
    - TSBK opcode + interpreted fields (best effort)

The script is dependency-free except for `p25_nid_fec` (already in
this folder).
"""

from __future__ import annotations

import json
import sys
from typing import Any

# Local import (same dir).
sys.path.insert(0, __file__.rsplit("/", 1)[0] if "/" in __file__ else ".")
try:
    from p25_nid_fec import decode_nid as ref_bch_decode_nid
except ImportError:
    ref_bch_decode_nid = None  # graceful degradation


# ─── Constants matching the Rust decoder ───────────────────────────────

FRAME_SYNC_DIBIT_PATTERN = 0x5575_F5FF_77FF
FRAME_SYNC_BITS = 48
NID_TRANSMITTED_DIBITS = 33
NID_STATUS_DIBIT_INDEX = 11

# TIA-102 BAAA Table 7-2: P25 1/2 rate trellis transition matrix.
# transition_matrix[prev_input][curr_input] = transmitted 4-bit value.
TRANSITION_MATRIX = [
    [2, 12, 1, 15],
    [14, 0, 13, 3],
    [9, 7, 10, 4],
    [5, 11, 6, 8],
]

# TIA-102 BAAA Table 7-7 / SDRTrunk DATA_DEINTERLEAVE: bit permutation
# applied AFTER trellis decoding the 196-bit message. Encoder applies
# the inverse before transmission.
DATA_DEINTERLEAVE = [
    0, 1, 2, 3, 16, 17, 18, 19, 32, 33, 34, 35, 48, 49, 50, 51,
    64, 65, 66, 67, 80, 81, 82, 83, 96, 97, 98, 99, 112, 113, 114, 115,
    128, 129, 130, 131, 144, 145, 146, 147, 160, 161, 162, 163, 176, 177, 178, 179,
    192, 193, 194, 195, 4, 5, 6, 7, 20, 21, 22, 23, 36, 37, 38, 39,
    52, 53, 54, 55, 68, 69, 70, 71, 84, 85, 86, 87, 100, 101, 102, 103,
    116, 117, 118, 119, 132, 133, 134, 135, 148, 149, 150, 151, 164, 165, 166, 167,
    180, 181, 182, 183, 8, 9, 10, 11, 24, 25, 26, 27, 40, 41, 42, 43,
    56, 57, 58, 59, 72, 73, 74, 75, 88, 89, 90, 91, 104, 105, 106, 107,
    120, 121, 122, 123, 136, 137, 138, 139, 152, 153, 154, 155, 168, 169, 170, 171,
    184, 185, 186, 187, 12, 13, 14, 15, 28, 29, 30, 31, 44, 45, 46, 47,
    60, 61, 62, 63, 76, 77, 78, 79, 92, 93, 94, 95, 108, 109, 110, 111,
    124, 125, 126, 127, 140, 141, 142, 143, 156, 157, 158, 159, 172, 173, 174, 175,
    188, 189, 190, 191,
]
assert len(DATA_DEINTERLEAVE) == 196

# SDRTrunk CRCP25.CCITT_80_CHECKSUMS: per-bit XOR table for the
# CRC-16/CCITT used to protect P25 80-bit (10-byte) TSBK payloads.
CCITT_80_CHECKSUMS = [
    0x1BCB, 0x8DE5, 0xC6F2, 0x6B69, 0xB5B4, 0x52CA, 0x2175, 0x90BA, 0x404D,
    0xA026, 0x5803, 0xAC01, 0xD600, 0x6310, 0x3998, 0x14DC, 0x027E, 0x092F,
    0x8497, 0xC24B, 0xE125, 0xF092, 0x7059, 0xB82C, 0x5406, 0x2213, 0x9109,
    0xC884, 0x6C52, 0x3E39, 0x9F1C, 0x479E, 0x2BDF, 0x95EF, 0xCAF7, 0xE57B,
    0xF2BD, 0xF95E, 0x74BF, 0xBA5F, 0xDD2F, 0xEE97, 0xF74B, 0xFBA5, 0xFDD2,
    0x76F9, 0xBB7C, 0x55AE, 0x22C7, 0x9163, 0xC8B1, 0xE458, 0x7A3C, 0x350E,
    0x1297, 0x894B, 0xC4A5, 0xE252, 0x7939, 0xBC9C, 0x565E, 0x233F, 0x919F,
    0xC8CF, 0xE467, 0xF233, 0xF919, 0xFC8C, 0x7656, 0x333B, 0x999D, 0xCCCE,
    0x6E77, 0xB73B, 0xDB9D, 0xEDCE, 0x7EF7, 0xBF7B, 0xDFBD, 0xEFDE, 0x0001,
    0x0002, 0x0004, 0x0008, 0x0010, 0x0020, 0x0040, 0x0080, 0x0100, 0x0200,
    0x0400, 0x0800, 0x1000, 0x2000, 0x4000, 0x8000,
]


def hamming4(a: int, b: int) -> int:
    return bin((a ^ b) & 0x0F).count("1")


# ─── Loaders ───────────────────────────────────────────────────────────


def load_capture(path: str) -> dict[str, Any]:
    if path == "-":
        return json.load(sys.stdin)
    with open(path) as f:
        return json.load(f)


def hex_to_dibits(hex_str: str) -> list[int]:
    return [int(c, 16) for c in hex_str]


def bytes_hex_to_list(hex_str: str) -> list[int]:
    return [int(hex_str[i : i + 2], 16) for i in range(0, len(hex_str), 2)]


# ─── Stage 1: sync ─────────────────────────────────────────────────────


def check_sync(sync_dibits: list[int]) -> tuple[int, int]:
    """Return (computed register, hamming distance to canonical pattern)."""
    reg = 0
    for d in sync_dibits:
        reg = ((reg << 2) | (d & 0x3)) & ((1 << FRAME_SYNC_BITS) - 1)
    dist = bin(reg ^ FRAME_SYNC_DIBIT_PATTERN).count("1")
    return reg, dist


# ─── Stage 2: NID ──────────────────────────────────────────────────────


def extract_nid_skipping_status(raw_nid_dibits: list[int]) -> int:
    nid_bits = 0
    for j in range(NID_TRANSMITTED_DIBITS):
        if j == NID_STATUS_DIBIT_INDEX:
            continue
        nid_bits = (nid_bits << 2) | (raw_nid_dibits[j] & 0x3)
    return nid_bits


# ─── Stage 3: TSDU body deinterleaver ──────────────────────────────────

STATUS_POSITIONS = (13, 49, 85, 121)
NULL_DIBITS = 21
TRELLIS_DATA_DIBITS = 98


def deinterleave_tsdu(body_dibits: list[int]) -> list[int]:
    after_status = [
        d for i, d in enumerate(body_dibits) if i not in STATUS_POSITIONS
    ]
    if len(after_status) >= NULL_DIBITS:
        after_status = after_status[: len(after_status) - NULL_DIBITS]
    return after_status[:TRELLIS_DATA_DIBITS]


# ─── Stage 4: Viterbi (P25 1/2 rate) ───────────────────────────────────


def viterbi_decode(trellis_dibits: list[int]) -> tuple[bytes, int]:
    """Return (12 bytes, total_path_error).

    Phase 6F.2j: applies the DATA_DEINTERLEAVE bit permutation BEFORE
    decoding, mirroring SDRTrunk's TSBKMessageFactory.
    """
    if len(trellis_dibits) < 98:
        raise ValueError(f"need 98 trellis dibits, got {len(trellis_dibits)}")

    # Step 1: convert dibits to 196 raw bits (interleaved order).
    interleaved_bits = []
    for d in trellis_dibits[:98]:
        interleaved_bits.append((d >> 1) & 1)
        interleaved_bits.append(d & 1)

    # Step 2: apply DATA_DEINTERLEAVE permutation.
    de_bits = [0] * 196
    for i in range(196):
        de_bits[DATA_DEINTERLEAVE[i]] = interleaved_bits[i]

    # Step 3: pack 196 deinterleaved bits into 49 nibbles (4 bits each, MSB first).
    nibbles = []
    for n in range(49):
        nib = 0
        for k in range(4):
            nib = (nib << 1) | de_bits[n * 4 + k]
        nibbles.append(nib)

    INF = 1 << 30
    metrics = [INF] * 4
    metrics[0] = 0
    traceback: list[list[int]] = [[0, 0, 0, 0] for _ in range(49)]

    for t, recv in enumerate(nibbles):
        new_metrics = [INF] * 4
        for prev in range(4):
            if metrics[prev] >= INF:
                continue
            for curr in range(4):
                expected = TRANSITION_MATRIX[prev][curr]
                err = hamming4(expected, recv)
                cand = metrics[prev] + err
                if cand < new_metrics[curr]:
                    new_metrics[curr] = cand
                    traceback[t][curr] = prev
        metrics = new_metrics

    # Encoder flushes with input 0; final state is 0.
    state = 0
    inputs = [0] * 49
    for t in range(48, -1, -1):
        inputs[t] = state
        state = traceback[t][state]

    # Drop the trailing flush input (inputs[48]); 48 data inputs = 96 bits.
    bits: list[int] = []
    for n in range(48):
        two = inputs[n] & 0x3
        bits.append((two >> 1) & 1)
        bits.append(two & 1)

    out = bytearray(12)
    for byte_idx in range(12):
        b = 0
        for k in range(8):
            b |= bits[byte_idx * 8 + k] << (7 - k)
        out[byte_idx] = b

    total_err = sum(metrics)  # rough indicator
    return bytes(out), metrics[0]


# ─── Stage 5: CRC-16 CCITT ─────────────────────────────────────────────


def crc16_ccitt(data: bytes) -> int:
    crc = 0xFFFF
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            if crc & 0x8000:
                crc = ((crc << 1) ^ 0x1021) & 0xFFFF
            else:
                crc = (crc << 1) & 0xFFFF
    return crc ^ 0xFFFF  # final XOR (CCITT-FALSE inverted convention)


def ccitt80_crc(data: bytes) -> int:
    """SDRTrunk-style CCITT_80 CRC over the first 80 bits of data."""
    calc = 0xFFFF
    for byte_idx in range(10):
        b = data[byte_idx]
        for bit_idx in range(8):
            if (b >> (7 - bit_idx)) & 1:
                calc ^= CCITT_80_CHECKSUMS[byte_idx * 8 + bit_idx]
    return calc


def crc_check(tsbk_bytes: bytes) -> tuple[bool, bool, int, int]:
    """Return (plain_match, xored_match, calc, msg_crc).

    Phase 6F.2j: switched to SDRTrunk's table-based CCITT_80 CRC.
    Validates if residual (calc XOR msg) is 0 or 0xFFFF.
    """
    calc = ccitt80_crc(tsbk_bytes)
    msg = (tsbk_bytes[10] << 8) | tsbk_bytes[11]
    residual = calc ^ msg
    return residual == 0, residual == 0xFFFF, calc, msg


# ─── Pretty printing ───────────────────────────────────────────────────


def fmt_dibits(dibits: list[int], status_positions: tuple = ()) -> str:
    out = []
    for i, d in enumerate(dibits):
        s = f"{d:1X}"
        if i in status_positions:
            s = f"\033[33m{s}\033[0m"  # yellow = status
        out.append(s)
    return "".join(out)


def fmt_bytes(b: bytes) -> str:
    return " ".join(f"{x:02X}" for x in b)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 1

    cap = load_capture(sys.argv[1])
    if cap.get("status") == "timeout":
        print("CAPTURE TIMED OUT -- no sync hits in 2s window.")
        return 2
    if cap.get("status") != "captured":
        print(f"Unknown status: {cap.get('status')}")
        return 2

    print("=" * 72)
    print("Aligned LSM capture replay (Phase 6F.2h)")
    print("=" * 72)
    print(f"total_dibits at capture: {cap['total_dibits_at_capture']}")
    print()

    # ── Sync ──
    sync_dibits = hex_to_dibits(cap["sync_dibits_hex"])
    print(f"Stage 1: SYNC ({len(sync_dibits)} dibits)")
    print(f"  hex:    {fmt_dibits(sync_dibits)}")
    py_reg, py_dist = check_sync(sync_dibits)
    print(f"  python computed reg:  0x{py_reg:012X}")
    print(f"  expected sync pattern: 0x{FRAME_SYNC_DIBIT_PATTERN:012X}")
    print(f"  python distance: {py_dist}  (rust reported: {cap['sync_distance']})")
    print()

    # ── NID ──
    raw_nid = hex_to_dibits(cap["raw_nid_dibits_hex"])
    print(f"Stage 2: NID ({len(raw_nid)} raw dibits, status at index {NID_STATUS_DIBIT_INDEX})")
    print(f"  raw:    {fmt_dibits(raw_nid, (NID_STATUS_DIBIT_INDEX,))}")
    py_nid_bits = extract_nid_skipping_status(raw_nid)
    rust_nid_bits = int(cap["nid_bits_hex"], 16)
    print(f"  python  nid_bits: 0x{py_nid_bits:016X}")
    print(f"  rust    nid_bits: 0x{rust_nid_bits:016X}")
    if py_nid_bits != rust_nid_bits:
        print("  ⚠ MISMATCH between python and rust nid_bits packing")
    py_nac = (py_nid_bits >> 52) & 0xFFF
    py_duid_raw = (py_nid_bits >> 48) & 0xF
    print(f"  raw NAC: 0x{py_nac:03X}  raw DUID: 0x{py_duid_raw:1X}")
    print(f"  rust BCH NAC: {cap.get('bch_nac')}  DUID: {cap.get('bch_duid')}")
    if ref_bch_decode_nid is not None:
        ref = ref_bch_decode_nid(py_nid_bits)
        if ref is not None:
            # p25_nid_fec.decode_nid returns either (nac, duid) or
            # (nac, duid, n_corrected); accept either shape.
            ref_nac, ref_duid = ref[0], ref[1]
            print(f"  python BCH ref: NAC 0x{ref_nac:03X}  DUID 0x{ref_duid:1X}")
        else:
            print("  python BCH ref: REJECTED (>11 bit errors)")
    print()

    # ── TSDU body ──
    raw_body = hex_to_dibits(cap["raw_body_dibits_hex"])
    if not raw_body:
        print("No body dibits captured (NID failed BCH or non-TSDU DUID).")
        print(f"crc_result: {cap['crc_result']}")
        return 0

    print(f"Stage 3: TSDU body ({len(raw_body)} raw dibits, status at {STATUS_POSITIONS})")
    print(f"  raw[0..40]:   {fmt_dibits(raw_body[:40], STATUS_POSITIONS)}")
    print(f"  raw[40..80]:  {fmt_dibits(raw_body[40:80], tuple(p - 40 for p in STATUS_POSITIONS if 40 <= p < 80))}")
    print(f"  raw[80..122]: {fmt_dibits(raw_body[80:], tuple(p - 80 for p in STATUS_POSITIONS if 80 <= p < 122))}")
    print()

    # ── Deinterleave ──
    py_trellis = deinterleave_tsdu(raw_body)
    rust_trellis = hex_to_dibits(cap["trellis_dibits_hex"])
    print(f"Stage 4: deinterleave -> {len(py_trellis)} trellis dibits")
    print(f"  python: {fmt_dibits(py_trellis)}")
    print(f"  rust:   {fmt_dibits(rust_trellis)}")
    if py_trellis == rust_trellis:
        print("  ✓ python and rust deinterleave outputs match")
    else:
        diffs = [
            (i, p, r) for i, (p, r) in enumerate(zip(py_trellis, rust_trellis)) if p != r
        ]
        print(f"  ⚠ {len(diffs)} differing positions: {diffs[:10]}")
    print()

    # ── Viterbi ──
    py_bytes, py_err = viterbi_decode(py_trellis)
    rust_bytes = bytes(bytes_hex_to_list(cap["tsbk_bytes_hex"]))
    print(f"Stage 5: Viterbi decode (12 bytes)")
    print(f"  python: {fmt_bytes(py_bytes)}  (final state-0 metric: {py_err})")
    print(f"  rust:   {fmt_bytes(rust_bytes)}")
    if py_bytes == rust_bytes:
        print("  ✓ python and rust trellis outputs match -- bug is NOT in trellis")
    else:
        diffs = [
            (i, p, r) for i, (p, r) in enumerate(zip(py_bytes, rust_bytes)) if p != r
        ]
        print(f"  ⚠ {len(diffs)} differing bytes: {diffs}")
    print()

    # ── CRC ──
    py_plain, py_xored, py_calc, py_msg = crc_check(py_bytes)
    print(f"Stage 6: CRC")
    print(f"  python crc16(bytes[0..10]):       0x{py_calc:04X}")
    print(f"  message CRC field (bytes[10..12]): 0x{py_msg:04X}")
    print(f"  plain match (calc == msg):        {py_plain}")
    print(f"  xored match (calc^0xFFFF == msg): {py_xored}")
    print(f"  rust said: {cap['crc_result']}")
    print()

    if py_plain or py_xored:
        print("✓ DECODED A VALID TSBK -- the byte stream is correct, the bug")
        print("  must be in the on-target rust trellis or CRC if rust said fail.")
    else:
        print("✗ Python ALSO fails CRC. The bug is upstream of CRC -- in the")
        print("  trellis input dibits or in the deinterleave alignment.")
        print("  Compare the raw_body_dibits hex against an SDRTrunk capture")
        print("  of the same site to see what the real on-air bits are.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
