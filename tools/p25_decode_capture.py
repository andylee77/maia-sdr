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

STATUS_POSITIONS = (14, 50, 86)
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
    """Return (12 bytes, total_path_error)."""
    if len(trellis_dibits) < 98:
        raise ValueError(f"need 98 trellis dibits, got {len(trellis_dibits)}")

    # Pack 98 dibits into 49 nibbles (high bits = first dibit).
    nibbles = []
    for n in range(49):
        a = trellis_dibits[n * 2] & 0x3
        b = trellis_dibits[n * 2 + 1] & 0x3
        nibbles.append((a << 2) | b)

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


def crc_check(tsbk_bytes: bytes) -> tuple[bool, bool, int, int]:
    """Return (plain_match, xored_match, calc_plain, msg_crc)."""
    calc = crc16_ccitt(tsbk_bytes[:10])
    msg = (tsbk_bytes[10] << 8) | tsbk_bytes[11]
    return calc == msg, (calc ^ 0xFFFF) == msg, calc, msg


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
            ref_nac, ref_duid = ref
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
