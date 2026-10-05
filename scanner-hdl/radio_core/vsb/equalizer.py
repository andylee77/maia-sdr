#
# Fishball radio core - the 8-VSB equalizer applied (pl/equalizer.rs): out[m] = sum of w[i] y[m + 32 - i]
# over 128 taps, each output 32 symbols after its input.
#
# Eight DSPs share the taps, DSP d taking taps 16d to 16d + 15 over 16 cycles, so an output takes
# one symbol's worth of cycles (at least 16 apart). The window is eight 16-entry LUT RAMs: when a
# symbol comes in, each passes its oldest value to the next and segment 0 takes the new one. The
# products run oldest first, so the next symbol's update, which overwrites only the oldest entries,
# can come as this output's last products are read.
#
# Two tap sets in LUT RAM: the PS writes the idle one and arms it, and the equalizer swaps them at
# the next field's first symbol at its output. Until the first set the taps are 1 at tap 32, so the
# equalizer passes its input (halved and rounded, as the model's). The symbols' places come through
# a 32-symbol delay to meet the output.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

TAPS = 128
PRE = 32
DSPS = 8
TAPS_PER_DSP = TAPS // DSPS
Y_WIDTH = 12
TAP_WIDTH = 16
OUT_WIDTH = 12
# A tap of 1 is 2^12; the output's level is 2^6 against the input's 2^7.
UNIT = 1 << (TAP_WIDTH - 4)
SHIFT = TAP_WIDTH + Y_WIDTH - OUT_WIDTH - 3


class Equalizer(Elaboratable):
    """The equalizer.

    Parameters
    ----------
    tag_width : int
        Bits of a symbol's place carried from its input to its output. The tag's low bits are
        ``fields`` (1), ``segment`` (9) and ``place`` (10): the swap happens at a symbol with
        fields counted at segment 0, place 0.

    Attributes
    ----------
    in_valid, in_y, in_tag : in
        A symbol; at least 16 cycles apart.
    tap_wen, tap_addr, tap_data : in
        A tap into the idle set.
    arm : in
        The idle set takes effect at the next field's first symbol at the output.
    out_valid, out_e, out_tag, out_loaded : out
        An output (for the symbol 32 inputs back) with its place and the tap sets loaded by then.
    armed, loaded : out
    """
    def __init__(self, tag_width):
        self.tag_width = tag_width
        self.in_valid = Signal()
        self.in_y = Signal(signed(Y_WIDTH))
        self.in_tag = Signal(tag_width)
        self.tap_wen = Signal()
        self.tap_addr = Signal(7)
        self.tap_data = Signal(signed(TAP_WIDTH))
        self.arm = Signal()
        self.out_valid = Signal()
        self.out_e = Signal(signed(OUT_WIDTH))
        self.out_tag = Signal(tag_width)
        self.out_loaded = Signal(16)
        self.armed = Signal()
        self.loaded = Signal(16)

    def elaborate(self, platform):
        m = Module()

        # ── The window ───────────────────────────────────────────────────────────────────
        wp = Signal(4)
        wp_next = Signal(4)
        m.d.comb += wp_next.eq(wp + 1)
        window_rd = []
        for d in range(DSPS):
            mem = Memory(shape=signed(Y_WIDTH), depth=TAPS_PER_DSP, init=[])
            m.submodules[f'window{d}'] = mem
            wr = mem.write_port()
            evict = mem.read_port(domain='comb')
            rd = mem.read_port(domain='comb')
            m.d.comb += [evict.addr.eq(wp_next), wr.addr.eq(wp_next), wr.en.eq(self.in_valid)]
            window_rd.append((wr, evict, rd))
        m.d.comb += window_rd[0][0].data.eq(self.in_y)
        for d in range(1, DSPS):
            m.d.comb += window_rd[d][0].data.eq(window_rd[d - 1][1].data)
        with m.If(self.in_valid):
            m.d.sync += wp.eq(wp_next)

        # ── The taps: two sets ───────────────────────────────────────────────────────────
        bank = Signal()
        passing = Signal(init=1)
        tap_rd = []
        for d in range(DSPS):
            mem = Memory(shape=signed(TAP_WIDTH), depth=2 * TAPS_PER_DSP, init=[])
            m.submodules[f'taps{d}'] = mem
            wr = mem.write_port()
            rd = mem.read_port(domain='comb')
            m.d.comb += [
                wr.en.eq(self.tap_wen & (self.tap_addr[4:] == d)),
                wr.addr.eq(Cat(self.tap_addr[:4], ~bank)),
                wr.data.eq(self.tap_data),
            ]
            tap_rd.append(rd)

        # ── The places, 32 symbols back ──────────────────────────────────────────────────
        m.submodules.places = places = Memory(shape=self.tag_width, depth=PRE, init=[])
        pw = places.write_port()
        pr = places.read_port(domain='comb')
        pn = places.read_port(domain='comb')
        pw_addr = Signal(5)
        seen = Signal(range(PRE + 1))
        m.d.comb += [pw.en.eq(self.in_valid), pw.addr.eq(pw_addr), pw.data.eq(self.in_tag),
                     pr.addr.eq(pw_addr), pn.addr.eq(pw_addr + 1)]
        with m.If(self.in_valid):
            m.d.sync += pw_addr.eq(pw_addr + 1)
            with m.If(seen < PRE):
                m.d.sync += seen.eq(seen + 1)
        out_place = pr.data
        # Whether the next output starts a field, known a symbol ahead (its place was written 31
        # symbols back).
        nxt = pn.data
        next_starts = Signal()
        with m.If(self.in_valid):
            m.d.sync += next_starts.eq(nxt[0] & (nxt[1:10] == 0) & (nxt[10:20] == 0))

        # ── The swap, at the start of an output ──────────────────────────────────────────
        with m.If(self.arm):
            m.d.sync += self.armed.eq(1)
        swap = self.in_valid & (seen == PRE) & next_starts & self.armed
        with m.If(swap):
            m.d.sync += [bank.eq(~bank), passing.eq(0), self.loaded.eq(self.loaded + 1),
                         self.armed.eq(0)]

        # The output's place and tap count, two outputs in flight.
        parity = Signal()
        tags = Array(Signal(self.tag_width, name=f'tag{i}') for i in range(2))
        counts = Array(Signal(16, name=f'count{i}') for i in range(2))
        valids = Array(Signal(name=f'valid{i}') for i in range(2))
        with m.If(self.in_valid):
            m.d.sync += [parity.eq(~parity), tags[~parity].eq(out_place),
                         counts[~parity].eq(self.loaded + swap), valids[~parity].eq(seen == PRE)]

        # ── The products, oldest first ───────────────────────────────────────────────────
        busy = Signal()
        j = Signal(4)
        with m.If(busy):
            m.d.sync += j.eq(j + 1)
            with m.If(j == TAPS_PER_DSP - 1):
                m.d.sync += busy.eq(0)
        with m.If(self.in_valid):
            m.d.sync += [busy.eq(1), j.eq(0)]

        c = Signal(4)
        m.d.comb += c.eq(TAPS_PER_DSP - 1 - j)
        a_valid = Signal()
        a_first = Signal()
        a_last = Signal()
        a_parity = Signal()
        a_win = Signal(4)
        a_tap = Signal(5)
        a_pass = Signal()
        m.d.sync += [
            a_valid.eq(busy), a_first.eq(j == 0), a_last.eq(j == TAPS_PER_DSP - 1),
            a_parity.eq(parity), a_win.eq(wp - c), a_tap.eq(Cat(c, bank)),
            a_pass.eq(passing),
        ]
        a_c = Signal(4)
        m.d.sync += a_c.eq(c)
        b_x = [Signal(signed(Y_WIDTH), name=f'b_x{d}') for d in range(DSPS)]
        b_w = [Signal(signed(TAP_WIDTH), name=f'b_w{d}') for d in range(DSPS)]
        for d in range(DSPS):
            m.d.comb += [window_rd[d][2].addr.eq(a_win), tap_rd[d].addr.eq(a_tap)]
            unit = UNIT if d == PRE // TAPS_PER_DSP else 0
            m.d.sync += [
                b_x[d].eq(window_rd[d][2].data),
                b_w[d].eq(Mux(a_pass, Mux(a_c == PRE % TAPS_PER_DSP, unit, 0), tap_rd[d].data)),
            ]

        def delay(sig, n, name):
            out = sig
            for k in range(n):
                q = Signal.like(sig, name=f'{name}_{k}')
                m.d.sync += q.eq(out)
                out = q
            return out

        b_first = delay(a_first, 1, 'b_first')
        p = []
        for d in range(DSPS):
            a = Signal(signed(Y_WIDTH), name=f'dsp{d}_a')
            b = Signal(signed(TAP_WIDTH), name=f'dsp{d}_b')
            mm = Signal(signed(Y_WIDTH + TAP_WIDTH), name=f'dsp{d}_m')
            pp = Signal(signed(36), name=f'dsp{d}_p')
            m.d.sync += [a.eq(b_x[d]), b.eq(b_w[d]), mm.eq(a * b)]
            first_e = delay(b_first, 2, f'first{d}')
            m.d.sync += pp.eq(mm + Mux(first_e, 0, pp))
            p.append(pp)
        e_last = delay(a_last, 4, 'e_last')
        e_valid = delay(a_valid, 4, 'e_valid')
        e_parity = delay(a_parity, 4, 'e_parity')

        # ── The sum and the rounding ─────────────────────────────────────────────────────
        level1 = [Signal(signed(37), name=f'sum1_{i}') for i in range(4)]
        level2 = [Signal(signed(38), name=f'sum2_{i}') for i in range(2)]
        total = Signal(signed(39))
        m.d.sync += [level1[i].eq(p[2 * i] + p[2 * i + 1]) for i in range(4)]
        m.d.sync += [level2[i].eq(level1[2 * i] + level1[2 * i + 1]) for i in range(2)]
        m.d.sync += total.eq(level2[0] + level2[1])
        # Rounded, then saturated, a cycle each.
        take = delay(e_valid & e_last, 4, 'take')
        take_parity = delay(e_parity, 4, 'take_parity')
        rounded = Signal(signed(39 - SHIFT))
        m.d.sync += rounded.eq((total + (1 << (SHIFT - 1))) >> SHIFT)
        top = (1 << (OUT_WIDTH - 1)) - 1
        m.d.sync += self.out_valid.eq(take & valids[take_parity])
        with m.If(take):
            m.d.sync += [
                self.out_e.eq(Mux(rounded > top, top, Mux(rounded < -top - 1, -top - 1, rounded))),
                self.out_tag.eq(tags[take_parity]),
                self.out_loaded.eq(counts[take_parity]),
            ]
        return m
