#
# Fishball radio core - the 8-VSB trellis decoders and the bytes they make (pl/decoder.rs
# `Viterbi`, `Decoder::place_pair`).
#
# Each data symbol, as a soft symbol (5 bits below a level, within +-12 levels), steps one of 12
# decoders, symbol k of data segment s stepping decoder (k + 4 s) mod 12. A step reads that
# decoder's state from LUT RAM (four path metrics, four survivors of 40 decisions, its last Z2),
# takes the four branch metrics (the squared distance to each coset's nearer level), adds,
# compares and selects with the model's tie rules (a tie keeps the first way in, the best state
# is the first smallest), and writes the state back. The same decoder comes back 12 data symbols
# later, so the steps pipeline freely. The decision is the best state's survivor 40 deep, with
# the precoder undone.
#
# The 12 symbols of a segment's group k = 12g .. 12g + 11 go one to each decoder, all as its
# symbol q = 69 s + g of the field, and their decisions all concern the symbols 39 groups back.
# So one counter of groups places every decision: decision p of a field (p = q - 39) is the pair at
# place p mod 4 of its decoder's byte j = p / 4, which is field byte 12 j + ((e - 4 n) mod 12)
# with n = 12 j / 207 rounded down, followed as j counts. Each decoder gathers its four pairs into
# the byte.
#
#   V0  the decoder, the group, the soft symbol; the decoder's state read
#   V1  the branch metrics
#   V2  add, compare, select
#   V3  normalize, decide, write the state back, gather the byte
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from .tables import ENCODERS, INTO, PER_FIELD

DEPTH = 40
# The costs are below 2^17 and two steps join any two states, so the metrics' spread stays below
# 2^18 and a metric plus a cost below 2^19.
METRIC_WIDTH = 19
SURVIVOR_WIDTH = 2 * DEPTH
SOFT_FRAC = 5
SOFT_LIMIT = 12 << SOFT_FRAC
UNIT = 1 << SOFT_FRAC
E_WIDTH = 12
STATE_WIDTH = 4 * METRIC_WIDTH + 4 * SURVIVOR_WIDTH + 1


class TrellisDecoders(Elaboratable):
    """The 12 decoders and the bytes.

    Attributes
    ----------
    restart : in
        Starts a run: every decoder from zero, the next data symbol the first of a field.
    in_valid, in_e, in_k : in
        A data symbol: the equalizer's output and its place among its segment's data symbols
        (0-827). The symbols of a run come in order, every one.
    dec_valid, dec_enc, dec_soft, dec_decided, dec_pair : out
        Each symbol's decoder, soft symbol and decision (when ``dec_decided``), for the tests.
    byte_valid, byte, byte_index, byte_field : out
        A byte: its place in its field (0-64583) and the field's number in the run, mod 8.
    """
    def __init__(self):
        self.restart = Signal()
        self.in_valid = Signal()
        self.in_e = Signal(signed(E_WIDTH))
        self.in_k = Signal(10)
        self.dec_valid = Signal()
        self.dec_enc = Signal(4)
        self.dec_soft = Signal(signed(10))
        self.dec_decided = Signal()
        self.dec_pair = Signal(2)
        self.byte_valid = Signal()
        self.byte = Signal(8)
        self.byte_index = Signal(17)
        self.byte_field = Signal(3)

    def elaborate(self, platform):
        m = Module()

        # ── V0: the symbol's decoder and group ───────────────────────────────────────────
        first = Signal(init=1)       # the next data symbol is the run's first
        enc_last = Signal(4)
        seg_enc = Signal(4)
        kmod = Signal(4)
        groups = Signal(range(DEPTH))
        deciding = Signal()
        p = Signal(range(PER_FIELD))
        j = Signal(13)
        r = Signal(8)                # 12 j mod 207
        n3 = Signal(2)               # 12 j / 207, mod 3
        field = Signal(3)

        enc = Signal(4)
        new_group = Signal()
        k0 = self.in_k == 0
        m.d.comb += [
            enc.eq(Mux(first, 0,
                       Mux(k0, Mux(seg_enc >= 8, seg_enc - 8, seg_enc + 4),
                           Mux(enc_last == ENCODERS - 1, 0, enc_last + 1)))),
            new_group.eq(first | k0 | (kmod == 11)),
        ]
        # The group's decisions, after this symbol's step of the counter.
        g_deciding = Signal()
        g_p = Signal.like(p)
        g_j = Signal.like(j)
        g_r = Signal.like(r)
        g_n3 = Signal.like(n3)
        g_field = Signal.like(field)
        m.d.comb += [g_deciding.eq(deciding), g_p.eq(p), g_j.eq(j), g_r.eq(r), g_n3.eq(n3),
                     g_field.eq(field)]
        with m.If(new_group & ~first):
            with m.If(~deciding):
                with m.If(groups == DEPTH - 2):
                    m.d.comb += [g_deciding.eq(1), g_p.eq(0), g_j.eq(0), g_r.eq(0), g_n3.eq(0),
                                 g_field.eq(0)]
            with m.Elif(p == PER_FIELD - 1):
                m.d.comb += [g_p.eq(0), g_j.eq(0), g_r.eq(0), g_n3.eq(0), g_field.eq(field + 1)]
            with m.Else():
                m.d.comb += g_p.eq(p + 1)
                with m.If(p[:2] == 3):
                    m.d.comb += g_j.eq(j + 1)
                    with m.If(r + 12 >= 207):
                        m.d.comb += [g_r.eq(r + 12 - 207), g_n3.eq(Mux(n3 == 2, 0, n3 + 1))]
                    with m.Else():
                        m.d.comb += g_r.eq(r + 12)

        with m.If(self.in_valid):
            m.d.sync += [
                first.eq(0), enc_last.eq(enc),
                kmod.eq(Mux(new_group, 0, kmod + 1)),
                deciding.eq(g_deciding), p.eq(g_p), j.eq(g_j), r.eq(g_r), n3.eq(g_n3),
                field.eq(g_field),
            ]
            with m.If(first | k0):
                m.d.sync += seg_enc.eq(enc)
            with m.If(first):
                m.d.sync += groups.eq(0)
            with m.Elif(new_group & (groups < DEPTH - 1)):
                m.d.sync += groups.eq(groups + 1)
        with m.If(self.restart):
            m.d.sync += [first.eq(1), deciding.eq(0), groups.eq(0)]

        # The soft symbol: rounded to 5 bits below a level, within +-12 levels.
        half = Signal(signed(E_WIDTH))
        m.d.comb += half.eq((self.in_e + 1) >> 1)
        soft = Signal(signed(10))
        m.d.comb += soft.eq(Mux(half > SOFT_LIMIT, SOFT_LIMIT, Mux(half < -SOFT_LIMIT, -SOFT_LIMIT, half)))

        # The decoder's state; a decoder not yet stepped in this run starts from zero. In LUT RAM:
        # as block RAM its 397 bits would take six RAMB36 for 12 words.
        m.submodules.state = state = Memory(shape=STATE_WIDTH, depth=16, init=[],
                                            attrs={'ram_style': 'distributed'})
        srd = state.read_port(domain='comb')
        swr = state.write_port()
        fresh = Signal(ENCODERS, init=(1 << ENCODERS) - 1)
        m.d.comb += srd.addr.eq(enc)

        v1_valid = Signal()
        v1_enc = Signal(4)
        v1_soft = Signal(signed(10))
        v1_state = Signal(STATE_WIDTH)
        v1_deciding = Signal()
        v1_pos = Signal(2)
        v1_index = Signal(17)
        v1_field = Signal(3)
        place = Signal(signed(5))
        m.d.comb += place.eq(enc - (g_n3 << 2))
        m.d.sync += v1_valid.eq(self.in_valid)
        with m.If(self.in_valid):
            m.d.sync += [
                v1_enc.eq(enc), v1_soft.eq(soft),
                v1_state.eq(Mux(fresh.bit_select(enc, 1), 0, srd.data)),
                v1_deciding.eq(g_deciding), v1_pos.eq(g_p[:2]),
                v1_index.eq(g_j * 12 + Mux(place < 0, place + 12, place)),
                v1_field.eq(g_field),
            ]

        # ── V1: the branch metrics ───────────────────────────────────────────────────────
        # The squares of 11 bits in LUTs: at 62.5 MHz they need no DSP.
        cost = [Signal(17, name=f'cost{c}', attrs={'use_dsp': 'no'}) for c in range(4)]
        z2 = [Signal(name=f'z2_{c}') for c in range(4)]
        v2 = dict(valid=Signal(), enc=Signal(4), soft=Signal(signed(10)), state=Signal(STATE_WIDTH),
                  deciding=Signal(), pos=Signal(2), index=Signal(17), field=Signal(3))
        for c in range(4):
            lo = (2 * c - 7) * UNIT
            high = v1_soft > lo + 4 * UNIT
            d = Signal(signed(11), name=f'd{c}')
            m.d.comb += d.eq(v1_soft - lo - Mux(high, 8 * UNIT, 0))
            m.d.sync += [cost[c].eq(d * d), z2[c].eq(high)]
        m.d.sync += [v2['valid'].eq(v1_valid), v2['enc'].eq(v1_enc), v2['soft'].eq(v1_soft),
                     v2['state'].eq(v1_state), v2['deciding'].eq(v1_deciding), v2['pos'].eq(v1_pos),
                     v2['index'].eq(v1_index), v2['field'].eq(v1_field)]

        # ── V2: add, compare, select ─────────────────────────────────────────────────────
        def metric(st, s):
            return st[METRIC_WIDTH * s:METRIC_WIDTH * (s + 1)]

        def survivor(st, s):
            base = 4 * METRIC_WIDTH
            return st[base + SURVIVOR_WIDTH * s:base + SURVIVOR_WIDTH * (s + 1)]

        nxt = [Signal(METRIC_WIDTH, name=f'next{s}') for s in range(4)]
        surv = [Signal(SURVIVOR_WIDTH, name=f'surv{s}') for s in range(4)]
        v3 = dict(valid=Signal(), enc=Signal(4), soft=Signal(signed(10)), last_z2=Signal(),
                  deciding=Signal(), pos=Signal(2), index=Signal(17), field=Signal(3))
        st = v2['state']
        for ns, ((s0, c0), (s1, c1)) in enumerate(INTO):
            m0 = metric(st, s0) + cost[c0]
            m1 = metric(st, s1) + cost[c1]
            second = m1 < m0
            x1 = (ns >> 1) ^ second
            z = Mux(second, z2[c1], z2[c0])
            prev = Mux(second, survivor(st, s1), survivor(st, s0))
            m.d.sync += [nxt[ns].eq(Mux(second, m1, m0)),
                         surv[ns].eq(Cat(x1, z, prev[:SURVIVOR_WIDTH - 2]))]
        m.d.sync += [v3['valid'].eq(v2['valid']), v3['enc'].eq(v2['enc']), v3['soft'].eq(v2['soft']),
                     v3['last_z2'].eq(st[-1]), v3['deciding'].eq(v2['deciding']),
                     v3['pos'].eq(v2['pos']), v3['index'].eq(v2['index']), v3['field'].eq(v2['field'])]

        # ── V3: normalize, decide, write back, gather ────────────────────────────────────
        low01 = Mux(nxt[1] < nxt[0], nxt[1], nxt[0])
        low23 = Mux(nxt[3] < nxt[2], nxt[3], nxt[2])
        low = Signal(METRIC_WIDTH)
        m.d.comb += low.eq(Mux(low23 < low01, low23, low01))
        b01 = Mux(nxt[1] < nxt[0], 1, 0)
        b23 = Mux(nxt[3] < nxt[2], 3, 2)
        best = Signal(2)
        m.d.comb += best.eq(Mux(low23 < low01, b23, b01))
        pair = Array(s[SURVIVOR_WIDTH - 2:] for s in surv)[best]
        zd = pair[1]
        x1d = pair[0]
        decision = Signal(2)
        m.d.comb += decision.eq(Cat(x1d, zd ^ v3['last_z2']))
        new_last = Mux(v3['deciding'], zd, v3['last_z2'])
        m.d.comb += [
            swr.en.eq(v3['valid']), swr.addr.eq(v3['enc']),
            swr.data.eq(Cat(*[(nxt[s] - low)[:METRIC_WIDTH] for s in range(4)], *surv, new_last)),
        ]
        with m.If(v3['valid']):
            m.d.sync += fresh.bit_select(v3['enc'], 1).eq(0)
        with m.If(self.restart):
            m.d.sync += fresh.eq((1 << ENCODERS) - 1)

        acc = Array(Signal(6, name=f'acc{e}') for e in range(ENCODERS))
        m.d.sync += [self.dec_valid.eq(v3['valid']), self.byte_valid.eq(0)]
        with m.If(v3['valid']):
            m.d.sync += [self.dec_enc.eq(v3['enc']), self.dec_soft.eq(v3['soft']),
                         self.dec_decided.eq(v3['deciding']), self.dec_pair.eq(decision)]
            with m.If(v3['deciding']):
                with m.If(v3['pos'] == 3):
                    m.d.sync += [self.byte_valid.eq(1), self.byte.eq(Cat(decision, acc[v3['enc']])),
                                 self.byte_index.eq(v3['index']), self.byte_field.eq(v3['field'])]
                with m.Else():
                    m.d.sync += acc[v3['enc']].eq(Cat(decision, acc[v3['enc']][:4]))
        return m
