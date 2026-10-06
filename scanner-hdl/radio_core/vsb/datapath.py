#
# Fishball radio core - the 8-VSB front end's per-symbol datapath and its loops (pl/front.rs `run`):
# the matched filter's output times the gain word, turned by e^(j theta), the pilot's DC taken out,
# saturated to 12 bits: y, the equalizer's input. Then the loops, each a symbol's recursion:
#
#   - carrier: theta steps a quarter turn a symbol plus the frequency (its sum shifted down 5) less
#     the turned imaginary part shifted up 9; the frequency's sum takes the imaginary part away;
#   - gain: the word is the sum shifted down 16, the sum gaining the target (4 levels) less |y|,
#     bounded to 1 .. 2^28 - 1;
#   - DC: the sum of y's difference before saturation, the DC the sum shifted down 16.
#
# A segment sync symbol's early and late evaluations pass through with theta turned back or on by
# half a symbol's turn, and update nothing.
#
# The schedule, in cycles from an input: the gain's two products on one DSP (1-4), the table (2-4),
# the turn's four products on another (5-11), each part shifted and saturated a cycle each, y
# (11), the loops in four steps (12-15). The state is the next input's from cycle 16, and inputs
# come at least 16 cycles apart (one matched-filter evaluation).
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from .tables import quarter_cosine

IN_WIDTH = 13
GAIN_WIDTH = 12
Y_WIDTH = 12
NCO_ADDR = 11
NCO_AMP = 10
# The model's loop gains and scales (pl/mod.rs `Loops::DEFAULT`, pl/front.rs).
CARRIER_KP = 9
CARRIER_KI = 5
QUARTER = 1 << 30
GAIN_SHIFT = 8           # gain word 12 bits (unity 2^8), y 12 bits, the filter's output 13
GAIN_SUM_ONE = 1 << 24
GAIN_SUM_MAX = (1 << 28) - 1
GAIN_SUM_SHIFT = 16
GAIN_TARGET = 4 * 128
DC_K = 16
TURN_SHIFT = NCO_AMP - 1
FREQ_SUM_WIDTH = 40
GAIN_SUM_WIDTH = 30
DC_SUM_WIDTH = 32
# Cycles from an input to its output, and the least spacing of inputs.
LATENCY = 14
SPACING = 16

KIND_MAIN = 0
KIND_EARLY = 1
KIND_LATE = 2


def _sat(m, value, width, name):
    top = (1 << (width - 1)) - 1
    out = Signal(signed(width), name=name)
    m.d.comb += out.eq(Mux(value > top, top, Mux(value < -top - 1, -top - 1, value)))
    return out


class Datapath(Elaboratable):
    """Gain, turn, DC and the loops.

    Attributes
    ----------
    carrier0 : Signal(signed(32)), in
        The carrier loop's starting frequency (theta a symbol beyond the quarter turn), taken at
        the first cycle after the reset.
    in_valid, in_re, in_im, in_kind, in_tag : in
        A matched-filter output; inputs at least ``SPACING`` cycles apart.
    out_valid : out
        One cycle, ``LATENCY`` cycles after an input: its y (for an early or late evaluation, its
        value), its stages for the tests, and the loops' state it was made with.
    """
    def __init__(self, tag_width):
        self.tag_width = tag_width
        self.carrier0 = Signal(signed(32))
        self.in_valid = Signal()
        self.in_re = Signal(signed(IN_WIDTH))
        self.in_im = Signal(signed(IN_WIDTH))
        self.in_kind = Signal(2)
        self.in_tag = Signal(tag_width)

        self.out_valid = Signal()
        self.out_kind = Signal(2)
        self.out_tag = Signal(tag_width)
        self.out_y = Signal(signed(Y_WIDTH))
        self.out_re_in = Signal(signed(IN_WIDTH))
        self.out_im_in = Signal(signed(IN_WIDTH))
        self.out_gain = Signal(GAIN_WIDTH)
        self.out_gained_re = Signal(signed(Y_WIDTH))
        self.out_gained_im = Signal(signed(Y_WIDTH))
        self.out_theta = Signal(32)
        self.out_turned_re = Signal(signed(Y_WIDTH))
        self.out_turned_im = Signal(signed(Y_WIDTH))
        self.out_dc = Signal(signed(DC_SUM_WIDTH - DC_K))
        self.out_freq = Signal(32)

    def elaborate(self, platform):
        m = Module()

        # ── The loops' state ─────────────────────────────────────────────────────────────
        theta = Signal(32)
        freq_sum = Signal(signed(FREQ_SUM_WIDTH))
        gain_sum = Signal(GAIN_SUM_WIDTH - 2, init=GAIN_SUM_ONE)
        dc_sum = Signal(signed(DC_SUM_WIDTH))
        # Derived from them after each update, for the next input.
        freq = Signal(signed(FREQ_SUM_WIDTH - CARRIER_KI))
        half_turn = Signal(32)
        gain = Signal(GAIN_WIDTH, init=GAIN_SUM_ONE >> GAIN_SUM_SHIFT)
        dc = Signal(signed(DC_SUM_WIDTH - DC_K))
        loaded = Signal()
        with m.If(~loaded):
            # The starting frequency, in the sum and in what the first symbol's step reads.
            m.d.sync += [loaded.eq(1), freq_sum.eq(self.carrier0 << CARRIER_KI), freq.eq(self.carrier0)]

        # ── Cycle 0: the input ───────────────────────────────────────────────────────────
        s1_valid = Signal()
        s1_kind = Signal(2)
        s1_tag = Signal(self.tag_width)
        s1_re = Signal(signed(IN_WIDTH))
        s1_im = Signal(signed(IN_WIDTH))
        s1_theta = Signal(32)
        s1_gain = Signal(GAIN_WIDTH)
        s1_dc = Signal.like(dc)
        s1_freq = Signal(32)
        turn = Mux(self.in_kind == KIND_EARLY, -half_turn,
                   Mux(self.in_kind == KIND_LATE, half_turn, 0))
        m.d.sync += s1_valid.eq(self.in_valid)
        with m.If(self.in_valid):
            m.d.sync += [
                s1_kind.eq(self.in_kind), s1_tag.eq(self.in_tag),
                s1_re.eq(self.in_re), s1_im.eq(self.in_im),
                s1_theta.eq(theta + turn),
                s1_gain.eq(gain), s1_dc.eq(dc), s1_freq.eq(freq),
            ]
        # The pipeline's position: bit k set k + 1 cycles after an input.
        t = Signal(15)
        m.d.sync += t.eq(Cat(self.in_valid, t[:-1]))

        # ── The gain: I then Q on one DSP (A/B, M, P) ────────────────────────────────────
        ga = Signal(signed(IN_WIDTH))
        gb = Signal(signed(GAIN_WIDTH + 1))
        gm = Signal(signed(IN_WIDTH + GAIN_WIDTH + 1))
        gp = Signal(signed(IN_WIDTH + GAIN_WIDTH + 2))
        with m.If(self.in_valid):
            m.d.sync += [ga.eq(self.in_re), gb.eq(gain)]
        with m.Elif(t[0]):
            m.d.sync += [ga.eq(s1_im), gb.eq(s1_gain)]
        m.d.sync += [gm.eq(ga * gb), gp.eq(gm + (1 << (GAIN_SHIFT - 1)))]
        gained = _sat(m, gp >> GAIN_SHIFT, Y_WIDTH, 'gained')
        gz_re = Signal(signed(Y_WIDTH))
        gz_im = Signal(signed(Y_WIDTH))
        with m.If(t[2]):
            m.d.sync += gz_re.eq(gained)
        with m.If(t[3]):
            m.d.sync += gz_im.eq(gained)

        # ── The table: cos and sin from a quarter wave ───────────────────────────────────
        quarter = quarter_cosine(NCO_ADDR, NCO_AMP)
        q4 = 1 << (NCO_ADDR - 2)
        m.submodules.rom = rom = Memory(shape=NCO_AMP - 1, depth=len(quarter), init=quarter)
        rom_c = rom.read_port(domain='comb')
        rom_s = rom.read_port(domain='comb')
        addr = Signal(NCO_ADDR)
        m.d.sync += addr.eq((s1_theta + (1 << (31 - NCO_ADDR)))[32 - NCO_ADDR:32])
        quad = addr[NCO_ADDR - 2:]
        r = addr[:NCO_ADDR - 2]
        reflected = Signal(range(q4 + 1))
        m.d.comb += [
            reflected.eq(q4 - r),
            rom_c.addr.eq(Mux(quad[0], reflected, r)),
            rom_s.addr.eq(Mux(quad[0], r, reflected)),
        ]
        c_mag = Signal(NCO_AMP - 1)
        s_mag = Signal(NCO_AMP - 1)
        c_neg = Signal()
        s_neg = Signal()
        m.d.sync += [c_mag.eq(rom_c.data), s_mag.eq(rom_s.data),
                     c_neg.eq(quad[0] ^ quad[1]), s_neg.eq(quad[1])]
        cos = Signal(signed(NCO_AMP))
        sin = Signal(signed(NCO_AMP))
        m.d.sync += [cos.eq(Mux(c_neg, -c_mag, c_mag)), sin.eq(Mux(s_neg, -s_mag, s_mag))]

        # ── The turn: four products on one DSP ───────────────────────────────────────────
        ta = Signal(signed(Y_WIDTH))
        tb = Signal(signed(NCO_AMP + 1))
        tm = Signal(signed(Y_WIDTH + NCO_AMP + 1))
        tp = Signal(signed(Y_WIDTH + NCO_AMP + 3))
        with m.If(t[4]):
            m.d.sync += [ta.eq(gz_re), tb.eq(cos)]
        with m.Elif(t[5]):
            m.d.sync += [ta.eq(gz_im), tb.eq(-sin)]
        with m.Elif(t[6]):
            m.d.sync += [ta.eq(gz_re), tb.eq(sin)]
        with m.Elif(t[7]):
            m.d.sync += [ta.eq(gz_im), tb.eq(cos)]
        m.d.sync += tm.eq(ta * tb)
        # Products land in P at cycles 7 (re, first), 8 (re), 9 (im, first) and 10 (im); each
        # sum is shifted, then saturated, a cycle each.
        m.d.sync += tp.eq(tm + Mux(t[7] | t[9], tp, 1 << (TURN_SHIFT - 1)))
        shifted = Signal(signed(Y_WIDTH + NCO_AMP + 3 - TURN_SHIFT))
        re = Signal(signed(Y_WIDTH))
        im = Signal(signed(Y_WIDTH))
        with m.If(t[8] | t[10]):
            m.d.sync += shifted.eq(tp >> TURN_SHIFT)
        turned = _sat(m, shifted, Y_WIDTH, 'turned')
        with m.If(t[9]):
            m.d.sync += re.eq(turned)
        with m.If(t[11]):
            m.d.sync += im.eq(turned)

        # ── y, and the loops ─────────────────────────────────────────────────────────────
        diff = Signal(signed(DC_SUM_WIDTH - DC_K + 2))
        m.d.comb += diff.eq(re - s1_dc)
        y = Signal(signed(Y_WIDTH))
        d10 = Signal.like(diff)
        with m.If(t[10]):
            m.d.sync += [y.eq(_sat(m, diff, Y_WIDTH, 'y_sat')), d10.eq(diff)]
        main = s1_kind == KIND_MAIN
        absy = Signal(Y_WIDTH)
        g_next = Signal(signed(GAIN_SUM_WIDTH))
        step = Signal(32)
        with m.If(t[11] & main):
            m.d.sync += [absy.eq(abs(y)), dc_sum.eq(dc_sum + d10)]
        with m.If(t[12] & main):
            m.d.sync += [
                g_next.eq(gain_sum + GAIN_TARGET - absy),
                dc.eq(dc_sum >> DC_K),
                freq_sum.eq(freq_sum - im),
                # theta's step: from the frequency before this symbol's update.
                step.eq(QUARTER + freq - (im << CARRIER_KP)),
            ]
        with m.If(t[13] & main):
            m.d.sync += [
                gain_sum.eq(Mux(g_next < 1, 1, Mux(g_next > GAIN_SUM_MAX, GAIN_SUM_MAX, g_next))),
                theta.eq(theta + step),
                freq.eq(freq_sum >> CARRIER_KI),
            ]
        with m.If(t[14] & main):
            g_word = gain_sum >> GAIN_SUM_SHIFT
            v = Signal(signed(FREQ_SUM_WIDTH - CARRIER_KI + 2))
            m.d.comb += v.eq(QUARTER + freq)
            m.d.sync += [
                gain.eq(Mux(g_word < 1, 1, g_word)),
                # Halved towards zero, as the model's integer division.
                half_turn.eq((v + (v < 0)) >> 1),
            ]

        # ── The output ───────────────────────────────────────────────────────────────────
        m.d.sync += self.out_valid.eq(t[12])
        with m.If(t[12]):
            m.d.sync += [
                self.out_kind.eq(s1_kind), self.out_tag.eq(s1_tag),
                self.out_y.eq(y),
                self.out_re_in.eq(s1_re), self.out_im_in.eq(s1_im),
                self.out_gain.eq(s1_gain),
                self.out_gained_re.eq(gz_re), self.out_gained_im.eq(gz_im),
                self.out_theta.eq(s1_theta),
                self.out_turned_re.eq(re), self.out_turned_im.eq(im),
                self.out_dc.eq(s1_dc), self.out_freq.eq(s1_freq),
            ]
        return m
