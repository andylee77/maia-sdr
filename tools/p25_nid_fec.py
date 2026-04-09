#!/usr/bin/env python3
"""p25_nid_fec.py -- BCH(63,16,11) NID forward error correction.

Phase 6B of the Fishball P25 LSM development plan: port SDRTrunk's NID FEC
into our Python reference so the LSM prototype can correct the residual NID
bit errors that are leaking through as wrong-NAC sync events today.

Background
----------
The P25 NID is a 64-bit field protected by binary BCH(63,16,11):

    bit  0..11: NAC  (12 bits, MSB-first)
    bit 12..15: DUID (4 bits, MSB-first)
    bit 16..63: 48 BCH parity bits

The (63,16,d=23) code corrects up to t = (23-1)/2 = 11 bit errors.

Source of truth
---------------
This is a direct port of SDRTrunk's encoder, taken verbatim from:

    sdrtrunk/src/test/java/io/github/dsheirer/edac/bch/BCH_63_16_23_P25_Test.java

The 16-row generator matrix below was copy-pasted from
P25_NID_BCH_63_16_GENERATOR_MATRIX in the test file (octal literals
preserved). The encoder loop is the same 5-line construction:

    parity = 0
    for x in 0..15:
        if data_bit_x:
            parity ^= GENERATOR_MATRIX[x]
    nid = (data << 48) | parity

A canonical test vector is documented in that file too:
    NAC=1 (0x001), DUID=0 (HDU) -> 0x00103185B7E9E224

This matches what our encoder produces (verified by self-test).

Why ML decoder instead of Berlekamp-Massey + Chien search?
-----------------------------------------------------------
SDRTrunk's actual decoder is a port of the Linux kernel BCH driver
(edac/bch/BCH.java, ~600 lines of dense Galois-field code with
Berlekamp-Trace root factorisation). Porting that faithfully would be days
of bug-prone work for a code whose entire codebook is only 65,536 entries.

Instead, we use **maximum-likelihood decoding via exhaustive codebook
search**. For every received NID, compute the Hamming distance to each of
the 65,536 valid codewords and pick the closest. If the minimum distance is
<= 11, return that codeword; otherwise declare uncorrectable.

ML decoding is provably optimal for any binary code: it achieves the
(63,16,d=23) minimum-distance bound exactly, so it has IDENTICAL correction
strength to a properly-implemented BCH decoder. (For received words within
the unique-decoding sphere of radius t, the nearest codeword IS the BCH
decoder's output.)

Properties:
  - Verifiable against SDRTrunk's known-good encoder test vector with no
    Galois field math to debug.
  - Vectorised across all 65,536 codewords using numpy uint64 XOR + popcount;
    a single NID decode runs in ~100us on commodity hardware.
  - Trivial to port to Rust (~50 lines).
  - In hardware: 65,536 * 64 bits = 524 KB ROM. Fits comfortably in BRAM
    on Zynq-7020 (4.9 MB BRAM available); one 64-bit XOR + popcount tree
    per BRAM entry, fully pipelined, single-cycle nearest-neighbour search
    with a final reduction tree.

Phase ladder reminder:
  Phase 6A (done): Python LSM demod, validates ~91% NID accuracy raw
  Phase 6B (THIS): NID FEC, target >99.9% NID accuracy after correction
  Phase 6C: IQ DMA path in FPGA gateware
  Phase 6D: Rust on PS port (codebook stays as a static const array)
  Phase 6E: HDL/PL final implementation (codebook becomes BRAM)

Usage
-----
    # standalone self-test
    python tools/p25_nid_fec.py

    # as a library
    from p25_nid_fec import encode_nid, decode_nid, decode_nid_batch
    cw = encode_nid(nac=0x8A1, duid=7)
    nac, duid, n_errs = decode_nid(received_64bit)
"""
from __future__ import annotations

import numpy as np


# ============================================================================
# Generator matrix from BCH_63_16_23_P25_Test.java (verbatim, octal literals)
# ============================================================================
# These are the SAME 16 rows used by SDRTrunk's encoder. Each row is a
# 48-bit parity contribution selected by data bit x in 0..15. The systematic
# encoding is parity = XOR over all rows whose data bit is set.

_GENERATOR_OCTAL = [
    "6331141367235452",  # row  0
    "5265521614723276",  # row  1
    "4603711461164164",  # row  2
    "2301744630472072",  # row  3
    "7271623073000466",  # row  4
    "5605650752635660",  # row  5
    "2702724365316730",  # row  6
    "1341352172547354",  # row  7
    "0560565075263566",  # row  8
    "6141333751704220",  # row  9
    "3060555764742110",  # row 10
    "1430266772361044",  # row 11
    "0614133375170422",  # row 12
    "6037114611641642",  # row 13
    "5326507063515373",  # row 14
    "4662302756473127",  # row 15
]

P25_NID_GENERATOR_MATRIX: list[int] = [int(o, 8) for o in _GENERATOR_OCTAL]


# ============================================================================
# Bit layout constants
# ============================================================================
# SDRTrunk's CorrectedBinaryMessage uses bit 0 as the MSB of the 64-bit word.

NAC_BITS = 12
DUID_BITS = 4
DATA_BITS = NAC_BITS + DUID_BITS    # 16
PARITY_BITS = 48
CODE_BITS = DATA_BITS + PARITY_BITS  # 64

# Per the BCH (63,16,d=23) bound: t = (d-1)/2 = 11.
# Any received word with Hamming distance <= 11 to a valid codeword has a
# UNIQUE nearest codeword, so ML decoding is identical to BCH decoding within
# the unique-decoding sphere.
T_MAX_ERRORS = 11


# ============================================================================
# Encoder
# ============================================================================
# Verbatim port of BCH_63_16_23_P25_Test.create():
#
#   CorrectedBinaryMessage cbm = new CorrectedBinaryMessage(64);
#   cbm.setInt(nac,  NAC_FIELD);   // bits 0..11
#   cbm.setInt(duid, DUID_FIELD);  // bits 12..15
#
#   long parity = 0;
#   for(int x = 0; x < 16; x++) {
#       if(cbm.get(x)) {                            // get bit x (MSB-first)
#           parity ^= P25_NID_BCH_63_16_GENERATOR_MATRIX[x];
#       }
#   }
#   cbm.load(16, 48, parity);                       // load 48 parity bits

def encode_nid(nac: int, duid: int) -> int:
    """Encode (NAC, DUID) into a 64-bit BCH(63,16,11) codeword."""
    if not (0 <= nac < (1 << NAC_BITS)):
        raise ValueError(f"NAC out of range: {nac}")
    if not (0 <= duid < (1 << DUID_BITS)):
        raise ValueError(f"DUID out of range: {duid}")

    data_word = (nac << DUID_BITS) | duid       # 16 bits, NAC then DUID
    parity = 0
    # Iterate bit positions 0..15 in MSB-first order, matching cbm.get(x).
    # Bit 0 of cbm is the MSB of data_word (bit DATA_BITS-1 in numeric form).
    for bit_idx in range(DATA_BITS):
        if data_word & (1 << (DATA_BITS - 1 - bit_idx)):
            parity ^= P25_NID_GENERATOR_MATRIX[bit_idx]
    return (data_word << PARITY_BITS) | parity


# ============================================================================
# Decoder: maximum-likelihood via exhaustive codebook search
# ============================================================================
# Build the 65,536-entry codebook lazily (1 MB of uint64 + 128 KB of uint16).
# After first build it's cached for the lifetime of the process.

_CODEBOOK: np.ndarray | None = None         # uint64[65536], the codewords
_CODEBOOK_DATA: np.ndarray | None = None    # uint16[65536], the data words


def _build_codebook() -> tuple[np.ndarray, np.ndarray]:
    global _CODEBOOK, _CODEBOOK_DATA
    if _CODEBOOK is not None and _CODEBOOK_DATA is not None:
        return _CODEBOOK, _CODEBOOK_DATA
    n_codewords = 1 << DATA_BITS
    cb = np.zeros(n_codewords, dtype=np.uint64)
    cd = np.zeros(n_codewords, dtype=np.uint16)
    for nac in range(1 << NAC_BITS):
        for duid in range(1 << DUID_BITS):
            idx = (nac << DUID_BITS) | duid
            cb[idx] = encode_nid(nac, duid)
            cd[idx] = idx
    _CODEBOOK = cb
    _CODEBOOK_DATA = cd
    return cb, cd


def _popcount64(x: np.ndarray) -> np.ndarray:
    """Count set bits in each element of a uint64 array. Bit-twiddling popcount."""
    m1 = np.uint64(0x5555_5555_5555_5555)
    m2 = np.uint64(0x3333_3333_3333_3333)
    m4 = np.uint64(0x0F0F_0F0F_0F0F_0F0F)
    h01 = np.uint64(0x0101_0101_0101_0101)
    x = x - ((x >> np.uint64(1)) & m1)
    x = (x & m2) + ((x >> np.uint64(2)) & m2)
    x = (x + (x >> np.uint64(4))) & m4
    return (x * h01) >> np.uint64(56)


def decode_nid(received_nid: int) -> tuple[int, int, int] | None:
    """Decode a received 64-bit NID via maximum-likelihood.

    Returns (nac, duid, n_corrected_bits) on success, or None if the closest
    codeword has more than T_MAX_ERRORS bit differences from the received word.
    """
    cb, cd = _build_codebook()
    received = np.uint64(received_nid & ((1 << CODE_BITS) - 1))
    diff = cb ^ received
    distances = _popcount64(diff)
    best_idx = int(np.argmin(distances))
    best_dist = int(distances[best_idx])
    if best_dist > T_MAX_ERRORS:
        return None
    data = int(cd[best_idx])
    nac = (data >> DUID_BITS) & ((1 << NAC_BITS) - 1)
    duid = data & ((1 << DUID_BITS) - 1)
    return nac, duid, best_dist


def decode_nid_batch(
    received_nids: np.ndarray,
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Vectorised batch decode of N received NIDs.

    Parameters
    ----------
    received_nids : uint64 array of shape (N,)

    Returns
    -------
    nacs       : uint16 array (N,) -- 0 if uncorrectable
    duids      : uint8  array (N,) -- 0 if uncorrectable
    distances  : uint8  array (N,) -- 255 if uncorrectable
    """
    cb, cd = _build_codebook()
    n = len(received_nids)
    nacs = np.zeros(n, dtype=np.uint16)
    duids = np.zeros(n, dtype=np.uint8)
    dists = np.full(n, 255, dtype=np.uint8)
    for i in range(n):
        recv = np.uint64(int(received_nids[i]))
        diff = cb ^ recv
        d = _popcount64(diff)
        bi = int(np.argmin(d))
        bd = int(d[bi])
        if bd <= T_MAX_ERRORS:
            data = int(cd[bi])
            nacs[i] = (data >> DUID_BITS) & ((1 << NAC_BITS) - 1)
            duids[i] = data & ((1 << DUID_BITS) - 1)
            dists[i] = bd
    return nacs, duids, dists


# ============================================================================
# Self-test
# ============================================================================

def _self_test() -> int:
    print("# p25_nid_fec.py self-test")
    print("# " + "=" * 60)

    # ----- Test 1: encoder against SDRTrunk's known-good vector -----
    # From BCH_63_16_23_P25_Test.java line 35-40:
    #   NAC=1, DUID=0 (HDU) -> 0x00103185B7E9E224
    expected = 0x00103185B7E9E224
    actual = encode_nid(nac=1, duid=0)
    ok = actual == expected
    print(
        f"  encoder NAC=1 DUID=0   : 0x{actual:016X}  expected 0x{expected:016X}  "
        f"[{'PASS' if ok else 'FAIL'}]"
    )
    if not ok:
        print("  encoder is BROKEN -- bit ordering or generator matrix wrong")
        return 1

    # ----- Test 2: encoder/decoder roundtrip on a few patterns -----
    print("  encoder/decoder roundtrip (clean codewords):")
    test_pairs = [(0, 0), (1, 0), (0x8A1, 7), (0xFFF, 0xF), (0x534, 2), (0x123, 5)]
    for nac, duid in test_pairs:
        cw = encode_nid(nac, duid)
        result = decode_nid(cw)
        ok = result == (nac, duid, 0)
        marker = "PASS" if ok else "FAIL"
        print(
            f"    NAC=0x{nac:03X} DUID=0x{duid:X}: cw=0x{cw:016X} "
            f"-> decode={result}  [{marker}]"
        )
        if not ok:
            return 1

    # ----- Test 3: error correction sweep up to t=11 -----
    print("  error correction sweep on NAC=0x8A1 DUID=7 (Clay County target):")
    base = encode_nid(0x8A1, 7)
    rng = np.random.default_rng(42)
    for n_errors in range(1, 12):
        n_trials = 100
        n_correct = 0
        for _ in range(n_trials):
            # Flip n_errors random bits in positions 0..62 (matching SDRTrunk's
            # test methodology which excludes the unused parity bit at index 63)
            positions = rng.choice(63, size=n_errors, replace=False)
            corrupted = base
            for pos in positions:
                corrupted ^= 1 << (CODE_BITS - 1 - int(pos))
            result = decode_nid(corrupted)
            if result == (0x8A1, 7, n_errors):
                n_correct += 1
        pct = 100.0 * n_correct / n_trials
        marker = "PASS" if n_correct == n_trials else "FAIL"
        print(
            f"    {n_errors:2d} bit errors: {n_correct:3d}/{n_trials} corrected "
            f"({pct:.0f}%)  [{marker}]"
        )
        if n_correct != n_trials:
            return 1

    # ----- Test 4: 12+ errors fall outside the unique-decoding sphere -----
    # We expect the decoder to either declare uncorrectable OR pick a
    # different codeword (because at distance 12, multiple codewords may be
    # equidistant or closer to the corrupted word than the original).
    n_trials = 200
    n_uncorrectable_or_wrong = 0
    for _ in range(n_trials):
        positions = rng.choice(63, size=12, replace=False)
        corrupted = base
        for pos in positions:
            corrupted ^= 1 << (CODE_BITS - 1 - int(pos))
        result = decode_nid(corrupted)
        if result is None or result[:2] != (0x8A1, 7):
            n_uncorrectable_or_wrong += 1
    pct = 100.0 * n_uncorrectable_or_wrong / n_trials
    print(
        f"  12 bit errors: {n_uncorrectable_or_wrong}/{n_trials} flagged "
        f"uncorrectable or wrong  ({pct:.0f}%)  "
        f"[INFO -- expected behaviour beyond t=11]"
    )

    # ----- Test 5: codebook size + decoder timing -----
    cb, cd = _build_codebook()
    print(f"  codebook size: {cb.nbytes + cd.nbytes} bytes "
          f"({cb.shape[0]} codewords; {cb.nbytes} cw + {cd.nbytes} idx)")

    import time
    n_decodes = 1000
    test_cw = encode_nid(0x8A1, 7)
    t0 = time.perf_counter()
    for _ in range(n_decodes):
        decode_nid(test_cw)
    elapsed = time.perf_counter() - t0
    print(
        f"  decode timing: {n_decodes} clean decodes in {elapsed*1000:.1f} ms "
        f"({elapsed*1000/n_decodes:.3f} ms each)"
    )

    print("# self-test PASSED")
    return 0


if __name__ == "__main__":
    import sys
    sys.exit(_self_test())
