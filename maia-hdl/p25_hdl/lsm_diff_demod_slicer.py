#
# Fishball P25 -- LSM differential demod + 4-PSK quadrant slicer
#
# Phase 6E.5 of the LSM HDL port. Streaming Amaranth equivalent of
# the differential demod and slicer block of the Rust LSM demod
# loop in `p25-httpd/src/lsm/demod.rs` lines 233-283. The PLL
# rotation step is hard-coded to identity (cos(pll)=1, sin(pll)=0)
# in this sub-phase -- the actual PLL update + sin/cos generator
# lands in 6E.6 alongside Gardner TED and AGC, where they all
# share the same loop-update timing.
#
# Algorithm
# ---------
# Per symbol decision (one decision_strobe pulse from
# `LsmTimingInterp` in 6E.4), compute:
#
#     i_mid_demod = i_mid * prev_mid_i + q_mid * prev_mid_q
#     q_mid_demod = q_mid * prev_mid_i - i_mid * prev_mid_q
#     i_sym       = i_cur * prev_cur_i + q_cur * prev_cur_q
#     q_sym       = q_cur * prev_cur_i - i_cur * prev_cur_q
#
# These are the real and imaginary parts of `z_curr * conj(z_prev)`,
# the same as the existing `C4FMDemod` block (but applied at the
# symbol decision rate, not at every input sample, and to two
# separate sample sets -- the midpoint sample and the current
# symbol sample).
#
# Then update prev:
#     prev_mid_i, prev_mid_q  <- i_mid, q_mid
#     prev_cur_i, prev_cur_q  <- i_cur, q_cur
#
# And slice the symbol-rate output `(i_sym, q_sym)` into a 4-PSK
# dibit by quadrant:
#
#     i>=0, q>=0  ->  Q1, +1, dibit 00
#     i< 0, q>=0  ->  Q2, +3, dibit 01
#     i>=0, q< 0  ->  Q4, -1, dibit 10
#     i< 0, q< 0  ->  Q3, -3, dibit 11
#
# In bit form: `dibit = Cat(i_sign, q_sign)` -- LSB = sign of i_sym,
# MSB = sign of q_sym, matching `Dibit.toDibit()` in SDRTrunk and the
# `to_dibit` helper in `p25-httpd/src/lsm/demod.rs`. The sign bits
# are taken directly from the 33-bit full product (no truncation
# needed for the slicer -- shifting can only ever zero out the
# magnitude, not flip the sign).
#
# Pipeline
# --------
# Two registered stages from `decision_strobe` in to `symbol_strobe`
# out:
#
#   stage 1 (1 cycle): multiplies -> 33-bit sums latched into the
#       per-decision *_full registers. Vivado packs the 8
#       multiplies into 8 DSP48E1s with 1-cycle internal pipeline.
#       Prev state is updated in the same cycle (the ".eq(input)"
#       path runs in parallel with the multiplies).
#
#   stage 2 (1 cycle): truncate the 33-bit sums to demod_width
#       (default 18, matching `C4FMDemod`) via arithmetic shift,
#       extract sign bits for the dibit slicer, latch outputs and
#       symbol_strobe.
#
# Total latency: 2 sync cycles from `decision_strobe` to
# `symbol_strobe`.
#
# DSP48E1 cost
# ------------
# 8 DSPs (4 multiplies for `i_mid_demod` and `q_mid_demod`, 4 for
# `i_sym` and `q_sym`). The PLL rotation in 6E.6 will add another
# 8, so the diff-demod / PLL section ends up around 16 DSP48 in
# the final design -- still well within the Z7020's 220-DSP budget
# even with the existing C4FM chain present.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class LsmDiffDemodSlicer(Elaboratable):
    """Differential demodulator + 4-PSK quadrant slicer for LSM.

    Parameters
    ----------
    iq_width : int
        Width of input IQ samples (Q1.{iw-1}). Default 16, matches
        the upstream `LsmTimingInterp` outputs in 6E.4.
    demod_width : int
        Width of the soft demod outputs `i_mid_demod_out`,
        `q_mid_demod_out`, `i_sym_out`, `q_sym_out`. Default 18,
        matches the existing `C4FMDemod.diff_*_out` width and
        gives Gardner TED in 6E.6 enough headroom over Q1.15.

    Inputs (sync domain):
        i_mid_in, q_mid_in : signed iq_width  midpoint sample
        i_cur_in, q_cur_in : signed iq_width  current-symbol sample
        decision_strobe    : Signal()         one cycle per symbol
            decision (driven by `LsmTimingInterp.decision_strobe`)

    Outputs (sync domain):
        i_mid_demod_out, q_mid_demod_out : signed demod_width
            differentially demodulated midpoint sample, used by
            Gardner TED in 6E.6
        i_sym_out, q_sym_out             : signed demod_width
            differentially demodulated current-symbol sample, used
            by Gardner TED and the soft sync detector
        dibit_out         : Signal(2)
            4-PSK hard-sliced dibit, valid on `symbol_strobe`
        symbol_strobe     : Signal()
            one cycle per emitted dibit, 2 sync clocks after the
            corresponding `decision_strobe` pulse
    """

    def __init__(self, *, iq_width=16, demod_width=18):
        self.iw = iq_width
        self.dw = demod_width
        # The full product of two iq_width signed values is
        # 2*iq_width bits; sum/difference of two such products
        # needs one extra integer bit. For iq_width=16, that's a
        # 33-bit signed accumulator. Round up to a multiple of 8
        # for clarity.
        self.full_width = ((2 * iq_width + 1) + 7) // 8 * 8

        # Shift to convert the 33-bit full product back into the
        # demod output Q-format. With Q1.15 inputs and Q*.15
        # outputs (default demod_width=18 -> Q3.15), shift = 15.
        # The same shift the existing C4FMDemod uses.
        self.shift = iq_width - 1

        # ── Inputs ──────────────────────────────────────────────
        self.i_mid_in = Signal(signed(iq_width))
        self.q_mid_in = Signal(signed(iq_width))
        self.i_cur_in = Signal(signed(iq_width))
        self.q_cur_in = Signal(signed(iq_width))
        self.decision_strobe = Signal()
        # Phase 8A: runtime reset. Clears the per-decision prev_*
        # history registers so the first post-reset decision uses a
        # clean (0, 0) reference instead of the phase picked up on
        # the previous carrier.
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.i_mid_demod_out = Signal(signed(demod_width), reset_less=True)
        self.q_mid_demod_out = Signal(signed(demod_width), reset_less=True)
        self.i_sym_out = Signal(signed(demod_width), reset_less=True)
        self.q_sym_out = Signal(signed(demod_width), reset_less=True)
        self.dibit_out = Signal(2, reset_less=True)
        self.symbol_strobe = Signal()

    def elaborate(self, platform):
        m = Module()

        # ── Per-decision history registers ──────────────────────
        # Initialised to 0 -- matches the Rust reference, which
        # starts `prev_middle_i = prev_middle_q = prev_current_i =
        # prev_current_q = 0.0` in `DemodState::new`. (The Rust
        # reference uses `prev_sym_i = prev_sym_q = 0.7` for the
        # Gardner TED's *previous symbol* state, which is a
        # different field that lives in 6E.6.)
        prev_middle_i = Signal(signed(self.iw), reset_less=True)
        prev_middle_q = Signal(signed(self.iw), reset_less=True)
        prev_current_i = Signal(signed(self.iw), reset_less=True)
        prev_current_q = Signal(signed(self.iw), reset_less=True)

        # ── Stage 1: per-decision multiply + sum, latch into the
        #    33-bit *_full registers. Update prev state in parallel.
        i_mid_demod_full = Signal(signed(self.full_width), reset_less=True)
        q_mid_demod_full = Signal(signed(self.full_width), reset_less=True)
        i_sym_full = Signal(signed(self.full_width), reset_less=True)
        q_sym_full = Signal(signed(self.full_width), reset_less=True)
        decision_q = Signal()

        with m.If(self.decision_strobe):
            m.d.sync += [
                # z_curr * conj(z_prev), midpoint sample.
                #   real = i_mid * prev_mid_i + q_mid * prev_mid_q
                #   imag = q_mid * prev_mid_i - i_mid * prev_mid_q
                i_mid_demod_full.eq(
                    self.i_mid_in * prev_middle_i
                    + self.q_mid_in * prev_middle_q),
                q_mid_demod_full.eq(
                    self.q_mid_in * prev_middle_i
                    - self.i_mid_in * prev_middle_q),
                # z_curr * conj(z_prev), current-symbol sample.
                i_sym_full.eq(
                    self.i_cur_in * prev_current_i
                    + self.q_cur_in * prev_current_q),
                q_sym_full.eq(
                    self.q_cur_in * prev_current_i
                    - self.i_cur_in * prev_current_q),
                # Update prev state for the NEXT decision. The
                # multiplies above use the PRE-update prev values
                # because Amaranth's m.d.sync assignments take
                # effect at the next clock edge -- both the prev
                # update and the multiply read happen in the same
                # cycle, but the multiply sees the old value.
                prev_middle_i.eq(self.i_mid_in),
                prev_middle_q.eq(self.q_mid_in),
                prev_current_i.eq(self.i_cur_in),
                prev_current_q.eq(self.q_cur_in),
                decision_q.eq(1),
            ]
        with m.Else():
            m.d.sync += decision_q.eq(0)

        # ── Stage 2: truncate to demod_width, extract sign bits
        #    for the slicer, latch outputs and symbol_strobe.
        m.d.sync += self.symbol_strobe.eq(0)
        with m.If(decision_q):
            m.d.sync += [
                self.i_mid_demod_out.eq(i_mid_demod_full >> self.shift),
                self.q_mid_demod_out.eq(q_mid_demod_full >> self.shift),
                self.i_sym_out.eq(i_sym_full >> self.shift),
                self.q_sym_out.eq(q_sym_full >> self.shift),
                # Slicer: dibit = (q_sym sign << 1) | (i_sym sign).
                # `Cat(a, b)` puts `a` at LSB and `b` at MSB, so
                # Cat(i_sym sign, q_sym sign) gives the right
                # ordering. Sign bits taken from the 33-bit full
                # product -- truncation can only zero out
                # magnitude, never flip the sign, so this is
                # equivalent to slicing the truncated value but
                # avoids one round-trip through the shift.
                self.dibit_out.eq(
                    Cat(i_sym_full[-1], q_sym_full[-1])),
                self.symbol_strobe.eq(1),
            ]

        # ── Phase 8A runtime reset override ─────────────────────
        # Clear the per-decision prev_* history registers, both
        # pipeline accumulators, and the output latches. After a
        # reset the first post-reset decision runs against a
        # (0, 0) reference which matches cold-boot semantics.
        with m.If(self.reset_in):
            m.d.sync += [
                prev_middle_i.eq(0),
                prev_middle_q.eq(0),
                prev_current_i.eq(0),
                prev_current_q.eq(0),
                i_mid_demod_full.eq(0),
                q_mid_demod_full.eq(0),
                i_sym_full.eq(0),
                q_sym_full.eq(0),
                decision_q.eq(0),
                self.i_mid_demod_out.eq(0),
                self.q_mid_demod_out.eq(0),
                self.i_sym_out.eq(0),
                self.q_sym_out.eq(0),
                self.dibit_out.eq(0),
                self.symbol_strobe.eq(0),
            ]

        return m
