#
# Fishball P25 -- LSM Gardner timing error detector
#
# Phase 6E.6a of the LSM HDL port. Streaming Amaranth port of the
# Gardner TED block in `p25-httpd/src/lsm/demod.rs` lines 254-263:
#
#     timing_adj = (prev_sym_i - i_sym) * i_mid_demod
#                + (prev_sym_q - q_sym) * q_mid_demod
#     clamp(timing_adj, +/- max_timing_adj)   # max = sps / 25 ~= 0.26
#     timing_adj *= ted_gain                  # ted_gain = sps / 4 ~= 1.63
#     sample_point += timing_adj
#     prev_sym_i, prev_sym_q <- i_sym, q_sym
#
# This is the standard Gardner timing error detector applied on the
# 2-D *demodulated* symbols (the differential demod output) rather
# than on the 1-D FM discriminator. The midpoint sample (halfway
# between two symbols) is the natural error indicator: when the
# midpoint amplitude is high, the previous-to-current symbol
# transition lined up well with our timing point; when it's low or
# its sign disagrees with the symbol transition, our timing is off.
#
# Why use the demodulated symbols and not the raw IQ
# ----------------------------------------------------
# The Rust loop and SDRTrunk's `P25P1DemodulatorLSM.process` both
# do this, and it has a critical property: the Gardner formulation
# above is invariant to a constant complex rotation. Even if the
# PLL hasn't locked yet, the timing recovery still works -- the
# rotation cancels in the dot product. That decouples the timing
# loop from the carrier loop, which is what makes joint
# timing+carrier recovery converge cleanly.
#
# Fixed-point format
# ------------------
# Inputs: signed 18-bit Q3.15 (matches LsmDiffDemodSlicer outputs).
# Internal accumulator: signed 38-bit Q8.30 (sum of two 18 x 18
#   products, +1 bit for the addition).
# Constants: signed 18-bit Q3.15 for max_timing_adj and Q1.17 for
#   ted_gain (Q1.17 max is 0.99996, ted_gain ~1.63 doesn't fit;
#   use Q2.16 instead -- max ~1.99996, ULP 1.5e-5, plenty for the
#   loop gain).
# Output timing_adj: signed 16-bit Q4.12 (matches the
#   LsmTimingInterp.sample_point format so it can be added directly).
#
# DSP48E1 cost
# ------------
# 2 DSPs for the (prev - sym) * mid_demod multiplies (Vivado packs
# the pre-subtract into the DSP48E1's pre-adder), 1 more for the
# ted_gain scale. Total ~3 DSPs.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


# Symbol rate constants -- copied from lsm_timing_interp for clarity.
P25_LSM_SAMPLE_RATE_HZ = 31_250
P25_SYMBOL_RATE_HZ = 4_800
SPS_FLOAT = P25_LSM_SAMPLE_RATE_HZ / P25_SYMBOL_RATE_HZ  # 6.5104...

# Gardner loop constants (verbatim from `demod_lsm_with_state` in
# `p25-httpd/src/lsm/demod.rs`):
MAX_TIMING_ADJ_FLOAT = SPS_FLOAT / 25.0   # 0.260...  pre-scale clamp
TED_GAIN_FLOAT = SPS_FLOAT / 4.0          # 1.628...  post-clamp scale

# Initial value for prev_sym_i / prev_sym_q (Rust default 0.7).
PREV_SYM_INIT_FLOAT = 0.7


def _q_round(value, frac_bits):
    """Round-half-up quantisation of a float to signed Qx.frac_bits."""
    return int(round(value * (1 << frac_bits)))


class LsmGardnerTed(Elaboratable):
    """Gardner timing-error detector + clamp + scale.

    Parameters
    ----------
    demod_width : int
        Width of the input demod samples (Q3.15). Default 18.
    out_width : int
        Width of the timing_adj output (Q4.12, matches
        `LsmTimingInterp.sample_point`). Default 16.

    Inputs (sync domain):
        i_sym_in,    q_sym_in        : signed demod_width
        i_mid_demod_in, q_mid_demod_in : signed demod_width
        symbol_strobe : Signal()  one cycle per new symbol decision
            (driven by `LsmDiffDemodSlicer.symbol_strobe`)

    Outputs (sync domain):
        timing_adj_out  : signed out_width Q4.12
        timing_adj_strobe : Signal()  one cycle when timing_adj_out
            is valid; 2 sync clocks after the input symbol_strobe.
    """

    # Q-format precomputes
    INPUT_FRAC_BITS = 15        # Q3.15
    OUTPUT_FRAC_BITS = 12       # Q4.12
    GAIN_FRAC_BITS = 16         # Q2.16

    # Q3.15 representation of the clamp limit MAX_TIMING_ADJ.
    MAX_TIMING_ADJ_Q15 = _q_round(MAX_TIMING_ADJ_FLOAT, INPUT_FRAC_BITS)
    # Q2.16 representation of the loop gain TED_GAIN.
    TED_GAIN_Q16 = _q_round(TED_GAIN_FLOAT, GAIN_FRAC_BITS)
    # Q3.15 init for prev_sym_i / prev_sym_q.
    PREV_SYM_INIT_Q15 = _q_round(PREV_SYM_INIT_FLOAT, INPUT_FRAC_BITS)

    # Sanity-check the constants at import time.
    assert 8000 < MAX_TIMING_ADJ_Q15 < 9000   # 0.26 * 32768 ~= 8519
    assert 100000 < TED_GAIN_Q16 < 110000     # 1.628 * 65536 ~= 106721
    assert 22000 < PREV_SYM_INIT_Q15 < 24000  # 0.7 * 32768 ~= 22938

    def __init__(self, *, demod_width=18, out_width=16):
        self.dw = demod_width
        self.ow = out_width

        # ── Inputs ──────────────────────────────────────────────
        self.i_sym_in = Signal(signed(demod_width))
        self.q_sym_in = Signal(signed(demod_width))
        self.i_mid_demod_in = Signal(signed(demod_width))
        self.q_mid_demod_in = Signal(signed(demod_width))
        self.symbol_strobe = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.timing_adj_out = Signal(signed(out_width), reset_less=True)
        self.timing_adj_strobe = Signal()

        # ── Debug taps ──────────────────────────────────────────
        # Expose the unclamped sum + the clamped+scaled value so
        # tests can sanity-check the intermediate stages without
        # peering into private signals.
        self.unclamped_dbg = Signal(signed(2 * demod_width + 2),
                                    reset_less=True)
        self.scaled_dbg = Signal(signed(2 * demod_width + 2 + 17),
                                 reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # ── prev_sym registers (init 0.7 in Q3.15) ──────────────
        prev_sym_i = Signal(signed(self.dw),
                            init=self.PREV_SYM_INIT_Q15,
                            reset_less=True)
        prev_sym_q = Signal(signed(self.dw),
                            init=self.PREV_SYM_INIT_Q15,
                            reset_less=True)

        # ── Stage 1: differences * mid_demod, sum into accumulator
        # diff_i = prev_sym_i - i_sym  (signed dw+1)
        # prod_i = diff_i * i_mid_demod (signed 2*dw+1)
        # similarly for q
        # sum    = prod_i + prod_q     (signed 2*dw+2)
        # Vivado will pack each diff+mult into a single DSP48E1
        # using the pre-adder.
        sum_width = 2 * self.dw + 2
        unclamped = Signal(signed(sum_width), reset_less=True)
        stage1_strobe = Signal()

        with m.If(self.symbol_strobe):
            diff_i = (prev_sym_i - self.i_sym_in)
            diff_q = (prev_sym_q - self.q_sym_in)
            m.d.sync += [
                unclamped.eq(
                    diff_i * self.i_mid_demod_in
                    + diff_q * self.q_mid_demod_in),
                stage1_strobe.eq(1),
                # Update prev_sym now -- the multiplies above read
                # the OLD value because Amaranth m.d.sync only
                # commits at the next edge.
                prev_sym_i.eq(self.i_sym_in),
                prev_sym_q.eq(self.q_sym_in),
            ]
        with m.Else():
            m.d.sync += stage1_strobe.eq(0)

        m.d.comb += self.unclamped_dbg.eq(unclamped)

        # ── Stage 2: clamp to +/- (max_timing_adj * 2^15), then
        # multiply by TED_GAIN_Q16, then shift to Q4.12 output.
        #
        # Sample_point is Q4.12, the input multiply produced a
        # Q?.30 value (15 frac bits + 15 frac bits). The clamp
        # limit (Q3.15 from the constant table) needs to be
        # rescaled to Q?.30 first -- shift it left by 15.
        clamp_limit_q30 = Const(
            self.MAX_TIMING_ADJ_Q15 << self.INPUT_FRAC_BITS,
            signed(sum_width))
        neg_clamp_limit_q30 = Const(
            -(self.MAX_TIMING_ADJ_Q15 << self.INPUT_FRAC_BITS),
            signed(sum_width))

        clamped = Signal(signed(sum_width), reset_less=True)
        m.d.comb += clamped.eq(unclamped)
        # Note: only one of the with branches needs to fire; we use
        # combinational saturation since clamping is a pure
        # function of unclamped.
        clamped_comb = Signal(signed(sum_width))
        with m.If(unclamped > clamp_limit_q30):
            m.d.comb += clamped_comb.eq(clamp_limit_q30)
        with m.Elif(unclamped < neg_clamp_limit_q30):
            m.d.comb += clamped_comb.eq(neg_clamp_limit_q30)
        with m.Else():
            m.d.comb += clamped_comb.eq(unclamped)

        # ted_gain is Q2.16, signed 17-bit max (it's positive 1.628).
        # Use 18-bit signed for safety on the multiply input port.
        ted_gain_const = Const(self.TED_GAIN_Q16, signed(18))

        # scaled = clamped (Q?.30) * ted_gain (Q?.16)  ->  Q?.46
        # signed sum_width + 18 = 38 + 18 = 56 bits
        scaled_width = sum_width + 18
        scaled = Signal(signed(scaled_width), reset_less=True)
        stage2_strobe = Signal()

        with m.If(stage1_strobe):
            m.d.sync += [
                scaled.eq(clamped_comb * ted_gain_const),
                stage2_strobe.eq(1),
            ]
        with m.Else():
            m.d.sync += stage2_strobe.eq(0)

        m.d.comb += self.scaled_dbg.eq(scaled)

        # ── Stage 3: shift to Q4.12, saturate to out_width.
        # scaled is Q?.{30+16} = Q?.46.  Output is Q4.12.
        # Shift right by (46 - 12) = 34.
        out_shift = (2 * self.INPUT_FRAC_BITS + self.GAIN_FRAC_BITS
                     - self.OUTPUT_FRAC_BITS)

        scaled_shifted = scaled >> out_shift

        out_max = (1 << (self.ow - 1)) - 1
        out_min = -(1 << (self.ow - 1))

        m.d.sync += self.timing_adj_strobe.eq(0)
        with m.If(stage2_strobe):
            with m.If(scaled_shifted > out_max):
                m.d.sync += self.timing_adj_out.eq(out_max)
            with m.Elif(scaled_shifted < out_min):
                m.d.sync += self.timing_adj_out.eq(out_min)
            with m.Else():
                m.d.sync += self.timing_adj_out.eq(scaled_shifted)
            m.d.sync += self.timing_adj_strobe.eq(1)

        return m
