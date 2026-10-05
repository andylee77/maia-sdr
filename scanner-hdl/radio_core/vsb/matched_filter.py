#
# Fishball radio core - the 8-VSB matched filter at any instant (pl/front.rs `MatchedFilter::at`).
#
# The AD9361's samples go into a buffer of the last 64. An evaluation at instant p (input samples,
# 32.32) takes the 40 samples i - 19 to i + 20 around i, the sample p rounds to at 1/64 of a sample,
# and the table's row for that sixty-fourth: real taps on I and on Q. Five DSPs share the 40 taps,
# DSP d taking taps 8d to 8d + 7: eight cycles on I, then eight on Q, so an evaluation takes 16
# cycles and they follow back to back. The five partial sums are added and rounded to 13 bits.
#
# The table (64 rows of 40 taps, 12 bits) depends on the true sample rate, so the PS writes it with
# the model's own code: word 8p + c holds the five DSPs' taps for row p, step c.
#
#   samples -> buffer (LUT RAM, a read port per DSP) --\
#   row p, step c -> table (block RAM) ----------------+-> 5 x (multiply, accumulate 8) -> sum -> round
#
# Everything here is in one domain (the parent's clk3x).
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

DSPS = 5
TAPS_PER_DSP = 8
TAPS = DSPS * TAPS_PER_DSP
HALF = TAPS // 2
PHASE_BITS = 6
TAP_WIDTH = 12
IN_WIDTH = 12
OUT_WIDTH = 13
# The output's rounding: taps are 1.0 at 2^11 and the output keeps the input's scale.
SHIFT = TAP_WIDTH - 1
BUFFER_LOG2 = 6
CYCLES = 2 * TAPS_PER_DSP
# Cycles from an issue taken to its output.
LATENCY = CYCLES + 11


def table_words(table):
    """The block RAM's words for a 64 x 40 table (row-major): word 8p + c holds DSP d's tap
    8d + c of row p at bits 12d."""
    words = []
    for p in range(1 << PHASE_BITS):
        for c in range(TAPS_PER_DSP):
            w = 0
            for d in range(DSPS):
                w |= (table[p * TAPS + TAPS_PER_DSP * d + c] & 0xFFF) << (TAP_WIDTH * d)
            words.append(w)
    return words


def requantize(v, shift, width):
    """pl/mod.rs `requantize`: rounded half up by `shift` bits, saturated to `width` bits."""
    if shift > 0:
        v = (v + (1 << (shift - 1))) >> shift
    elif shift < 0:
        v = v << -shift
    top = (1 << (width - 1)) - 1
    return max(-top - 1, min(top, v))


class MatchedFilter(Elaboratable):
    """The matched filter's buffer, table and DSPs.

    Attributes
    ----------
    wr_en, wr_re, wr_im : in
        A new AD9361 sample.
    in_count : Signal(32), out
        Samples written since the reset; sample k is in the buffer until sample k + 64 is written.
    issue : Signal(), in
        Start an evaluation at ``issue_pos`` (input samples, 32.32), with ``issue_tag``; taken
        when ``ready``.
    ready : Signal(), out
    tw_en, tw_addr, tw_data : in
        A table word.
    out_valid : Signal(), out
        One cycle: ``out_re``, ``out_im`` and ``out_tag`` of an evaluation, in issue order.
    """
    def __init__(self, tag_width):
        self.tag_width = tag_width
        self.wr_en = Signal()
        self.wr_re = Signal(signed(IN_WIDTH))
        self.wr_im = Signal(signed(IN_WIDTH))
        self.in_count = Signal(32)
        self.issue = Signal()
        self.issue_pos = Signal(64)
        self.issue_tag = Signal(tag_width)
        self.ready = Signal()
        self.tw_en = Signal()
        self.tw_addr = Signal(9)
        self.tw_data = Signal(DSPS * TAP_WIDTH)
        self.out_valid = Signal()
        self.out_re = Signal(signed(OUT_WIDTH))
        self.out_im = Signal(signed(OUT_WIDTH))
        self.out_tag = Signal(tag_width)

    def elaborate(self, platform):
        m = Module()

        # ── The input buffer: one read port per DSP ──────────────────────────────────────
        m.submodules.buffer = buf = Memory(shape=2 * IN_WIDTH, depth=1 << BUFFER_LOG2, init=[])
        wr = buf.write_port()
        m.d.comb += [wr.en.eq(self.wr_en), wr.addr.eq(self.in_count[:BUFFER_LOG2]),
                     wr.data.eq(Cat(self.wr_re, self.wr_im))]
        with m.If(self.wr_en):
            m.d.sync += self.in_count.eq(self.in_count + 1)
        buf_rd = [buf.read_port(domain='comb') for _ in range(DSPS)]

        m.submodules.table = table = Memory(shape=DSPS * TAP_WIDTH, depth=512, init=[])
        tw = table.write_port()
        m.d.comb += [tw.en.eq(self.tw_en), tw.addr.eq(self.tw_addr), tw.data.eq(self.tw_data)]
        tr = table.read_port(transparent_for=())

        # ── The evaluation in progress ───────────────────────────────────────────────────
        busy = Signal()
        c = Signal(4)
        start = Signal(BUFFER_LOG2)
        phase = Signal(PHASE_BITS)
        parity = Signal()
        tags = Array(Signal(self.tag_width, name=f'tag{i}') for i in range(2))
        m.d.comb += self.ready.eq(~busy | (c == CYCLES - 1))
        rounded = Signal(64)
        m.d.comb += rounded.eq(self.issue_pos + (1 << (32 - PHASE_BITS - 1)))
        with m.If(busy):
            m.d.sync += c.eq(c + 1)
            with m.If(c == CYCLES - 1):
                m.d.sync += busy.eq(0)
        with m.If(self.issue & self.ready):
            m.d.sync += [
                busy.eq(1), c.eq(0),
                # The window's first sample, i + 1 - HALF, only modulo the buffer.
                start.eq(rounded[32:32 + BUFFER_LOG2] + 1 - HALF),
                phase.eq(rounded[32 - PHASE_BITS:32]),
                parity.eq(~parity),
                tags[~parity].eq(self.issue_tag),
            ]

        # ── Stage A: addresses ────────────────────────────────────────────────────────────
        step = c[:3]
        a_valid = Signal()
        a_comp = Signal()
        a_first = Signal()
        a_last = Signal()
        a_parity = Signal()
        a_addr = [Signal(BUFFER_LOG2, name=f'a_addr{d}') for d in range(DSPS)]
        a_row = Signal(9)
        near = Signal(BUFFER_LOG2)
        m.d.comb += near.eq(start + step)
        m.d.sync += [
            a_valid.eq(busy), a_comp.eq(c[3]), a_first.eq(step == 0),
            a_last.eq(step == TAPS_PER_DSP - 1), a_parity.eq(parity),
            a_row.eq(Cat(step, phase)),
            *[a_addr[d].eq(near + TAPS_PER_DSP * d) for d in range(DSPS)],
        ]

        # ── Stage B: the samples, and the table's read ────────────────────────────────────
        b_valid, b_comp, b_first, b_last, b_parity = (Signal(name=f'b_{n}') for n in
                                                      ('valid', 'comp', 'first', 'last', 'parity'))
        b_x = [Signal(signed(IN_WIDTH), name=f'b_x{d}') for d in range(DSPS)]
        for d in range(DSPS):
            m.d.comb += buf_rd[d].addr.eq(a_addr[d])
            word = buf_rd[d].data
            m.d.sync += b_x[d].eq(Mux(a_comp, word[IN_WIDTH:], word[:IN_WIDTH]))
        m.d.comb += [tr.addr.eq(a_row), tr.en.eq(1)]
        m.d.sync += [b_valid.eq(a_valid), b_comp.eq(a_comp), b_first.eq(a_first),
                     b_last.eq(a_last), b_parity.eq(a_parity)]

        # ── Stage C: the table's output register ──────────────────────────────────────────
        # Kept in the fabric beside the block RAM: absorbed into the DSPs' B registers it would
        # leave the RAM's unregistered output a cycle to reach five DSPs.
        c_valid, c_comp, c_first, c_last, c_parity = (Signal(name=f'c_{n}') for n in
                                                      ('valid', 'comp', 'first', 'last', 'parity'))
        c_x = [Signal(signed(IN_WIDTH), name=f'c_x{d}') for d in range(DSPS)]
        c_w = [Signal(signed(TAP_WIDTH), name=f'c_w{d}', attrs={'keep': 'true'})
               for d in range(DSPS)]
        m.d.sync += [c_valid.eq(b_valid), c_comp.eq(b_comp), c_first.eq(b_first),
                     c_last.eq(b_last), c_parity.eq(b_parity)]
        for d in range(DSPS):
            m.d.sync += [c_x[d].eq(b_x[d]), c_w[d].eq(tr.data[TAP_WIDTH * d:TAP_WIDTH * (d + 1)])]

        # ── Stages D-F: the DSPs (A/B registers, M, P) ────────────────────────────────────
        def delay(sig, n, name):
            out = sig
            for k in range(n):
                q = Signal.like(sig, name=f'{name}_{k}')
                m.d.sync += q.eq(out)
                out = q
            return out

        p = []
        for d in range(DSPS):
            a = Signal(signed(IN_WIDTH), name=f'dsp{d}_a')
            b = Signal(signed(TAP_WIDTH), name=f'dsp{d}_b')
            mm = Signal(signed(IN_WIDTH + TAP_WIDTH), name=f'dsp{d}_m')
            pp = Signal(signed(32), name=f'dsp{d}_p')
            m.d.sync += [a.eq(c_x[d]), b.eq(c_w[d]), mm.eq(a * b)]
            first_e = delay(c_first, 2, f'first{d}')
            m.d.sync += pp.eq(mm + Mux(first_e, 0, pp))
            p.append(pp)
        # The partial sums are whole in stage G, after a last step.
        g_last = delay(c_last, 3, 'g_last')
        g_valid = delay(c_valid, 3, 'g_valid')
        g_comp = delay(c_comp, 3, 'g_comp')
        g_parity = delay(c_parity, 3, 'g_parity')

        # ── Stages G-J: the sum and the rounding ──────────────────────────────────────────
        # Carry chains: synthesis would otherwise take three more DSPs for these adds.
        no_dsp = {'use_dsp': 'no'}
        s01 = Signal(signed(32), attrs=no_dsp)
        s23 = Signal(signed(32), attrs=no_dsp)
        s4 = Signal(signed(32))
        s0123 = Signal(signed(32), attrs=no_dsp)
        s4b = Signal(signed(32))
        total = Signal(signed(32), attrs=no_dsp)
        h_take = Signal()
        i_take = Signal()
        j_take = Signal()
        h_comp = Signal()
        i_comp = Signal()
        j_comp = Signal()
        h_parity = Signal()
        i_parity = Signal()
        j_parity = Signal()
        m.d.sync += [
            s01.eq(p[0] + p[1]), s23.eq(p[2] + p[3]), s4.eq(p[4]),
            h_take.eq(g_valid & g_last), h_comp.eq(g_comp), h_parity.eq(g_parity),
            s0123.eq(s01 + s23), s4b.eq(s4),
            i_take.eq(h_take), i_comp.eq(h_comp), i_parity.eq(h_parity),
            total.eq(s0123 + s4b),
            j_take.eq(i_take), j_comp.eq(i_comp), j_parity.eq(i_parity),
        ]
        # Rounded, then saturated, a cycle each.
        rounded_sum = Signal(signed(32 - SHIFT))
        k_take = Signal()
        k_comp = Signal()
        k_parity = Signal()
        m.d.sync += [rounded_sum.eq((total + (1 << (SHIFT - 1))) >> SHIFT),
                     k_take.eq(j_take), k_comp.eq(j_comp), k_parity.eq(j_parity)]
        top = (1 << (OUT_WIDTH - 1)) - 1
        saturated = Signal(signed(OUT_WIDTH))
        m.d.comb += saturated.eq(Mux(rounded_sum > top, top,
                                     Mux(rounded_sum < -top - 1, -top - 1, rounded_sum)))
        m.d.sync += self.out_valid.eq(0)
        with m.If(k_take):
            with m.If(~k_comp):
                m.d.sync += self.out_re.eq(saturated)
            with m.Else():
                m.d.sync += [self.out_im.eq(saturated), self.out_valid.eq(1),
                             self.out_tag.eq(tags[k_parity])]
        return m
