#
# Fishball radio core - the 8-VSB receiver's constants and tables, computed as the model computes
# them (scanner/src/protocol/atsc: vsb.rs, fec/, pl/): the framing, the field sync's sequences, the
# turn's quarter-wave table, GF(256)'s constant multipliers and the transmitter's byte map.
#
# SPDX-License-Identifier: MIT
#

import math

# Framing (A/53 Part 2).
SEGMENT = 832
SYNC_SYMBOLS = 4
DATA_SYMBOLS = SEGMENT - SYNC_SYMBOLS
FIELD_SEGMENTS = 313
DATA_SEGMENTS = FIELD_SEGMENTS - 1
FIELD = FIELD_SEGMENTS * SEGMENT
# The segment sync's signs.
SEGMENT_SYNC = (1, -1, -1, 1)
# Where the middle PN63 starts in a field sync segment.
MIDDLE_PN63 = 4 + 511 + 63

# The trellis code: 12 encoders, each decoder's symbols in a field.
ENCODERS = 12
PER_FIELD = DATA_SEGMENTS * DATA_SYMBOLS // ENCODERS
# Next state 2a + b comes from states 2b + D2 (D2 = 0, 1), sending coset 2 X1 + b
# (fec/trellis.rs INTO): per next state, per D2, the previous state and the coset.
INTO = (((0, 0), (1, 2)), ((2, 1), (3, 3)), ((0, 2), (1, 0)), ((2, 3), (3, 1)))

# Reed-Solomon (207, 187) and the deinterleaver.
RS_N = 207
RS_K = 187
RS_PARITY = RS_N - RS_K
FIELD_BYTES = RS_N * DATA_SEGMENTS
BRANCHES = 52
DEINTERLEAVER_DELAY = 4 * BRANCHES * (BRANCHES - 1)


def _lfsr(state, out, mask, n):
    bits = []
    for _ in range(n):
        bits.append((state >> out) & 1)
        state = ((state << 1) | (bin(state & mask).count('1') & 1)) & 0xFFFF_FFFF
    return bits


def pn511():
    """The field sync's PN511 as signs (vsb.rs `pn511`)."""
    return [1 if b else -1 for b in _lfsr(0b10, 8, 0b1_1011_0110, 511)]


def pn63():
    """The PN63 as signs (vsb.rs `pn63`)."""
    return [1 if b else -1 for b in _lfsr(0b11_1001, 5, 0b11_0000, 63)]


def round_half_away(x):
    """Rust's f64::round."""
    return math.floor(x + 0.5) if x >= 0 else -math.floor(-x + 0.5)


def quarter_cosine(addr_bits=11, amp_bits=10):
    """cos(2 pi r / 2^addr_bits) for r up to a quarter turn, inclusive, as the model's full
    table holds it (pl/front.rs `Turn::new`): the full table is this one's reflections exactly."""
    n = 1 << addr_bits
    one = (1 << (amp_bits - 1)) - 1
    return [round_half_away(math.cos(2 * math.pi * r / n) * one) for r in range(n // 4 + 1)]


def cos_sin(k, quarter, addr_bits=11):
    """The model's table entry k (cos, sin) from the quarter table."""
    q4 = 1 << (addr_bits - 2)
    q, r = divmod(k, q4)
    c = (quarter[r], -quarter[q4 - r], -quarter[r], quarter[q4 - r])[q]
    s = (quarter[q4 - r], quarter[r], -quarter[q4 - r], -quarter[r])[q]
    return c, s


def gf_exp():
    """GF(256) over x^8 + x^4 + x^3 + x^2 + 1: alpha^i for i < 255."""
    out, v = [], 1
    for _ in range(255):
        out.append(v)
        v <<= 1
        if v & 0x100:
            v ^= 0x11D
    return out


def gf_times_matrix(j):
    """Multiplication by alpha^j as 8 masks: output bit o is the parity of the input bits in
    mask o (fec/rs.rs `times[j]`)."""
    exp = gf_exp()
    log = {v: i for i, v in enumerate(exp)}
    def times(v):
        return 0 if v == 0 else exp[(log[v] + j) % 255]
    cols = [times(1 << i) for i in range(8)]
    return [sum(((cols[i] >> o) & 1) << i for i in range(8)) for o in range(8)]


def syndromes(cw):
    """Reed-Solomon's 20 syndromes of a codeword, Horner's way (fec/rs.rs `syndromes_with`)."""
    exp = gf_exp()
    log = {v: i for i, v in enumerate(exp)}
    syn = [0] * RS_PARITY
    for c in cw:
        for j in range(RS_PARITY):
            s = syn[j]
            syn[j] = (0 if s == 0 else exp[(log[s] + j) % 255]) ^ c
    return syn


def byte_of(e, j):
    """Field byte b that encoder e takes as its byte j (fec/mod.rs `byte_map`): the bytes go to the
    encoders in groups of 12, one each, the turn moving on by 4 at each data segment's start
    rounded up to a group."""
    return 12 * j + ((e - 4 * ((12 * j) // 207)) % 12)


def deinterleaver_bases():
    """Each branch's first byte in the deinterleaver's memory, and the bytes it holds."""
    lengths = [4 * (BRANCHES - 1 - j) for j in range(BRANCHES)]
    bases = [sum(lengths[:j]) for j in range(BRANCHES)]
    return bases, lengths
