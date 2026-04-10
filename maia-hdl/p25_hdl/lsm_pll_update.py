#
# Fishball P25 -- LSM decision-directed PLL update
#
# Phase 6E.6b of the LSM HDL port. Streaming Amaranth port of the
# decision-directed PLL update block in
# `p25-httpd/src/lsm/demod.rs` lines 265-279:
#
#     let h = to_dibit(soft_symbol)        # 4-PSK quadrant
#     let phase_error = soft_symbol - dibit_phase(h)
#     clamp phase_error to +/- PLL_MAX_ERROR     (0.3 rad)
#     state.pll -= phase_error * PLL_GAIN        (0.1)
#     clamp state.pll to +/- MAX_PLL_ABS         (PI/3)
#
# Small-angle linearization (the trick that makes this fit in HDL
# without a CORDIC atan2)
# -----------------------------------------------------------------
# `soft_symbol = atan2(q_sym, i_sym)` is the angle of the
# differential-demodulated symbol. `dibit_phase(h)` is the ideal
# angle for that quadrant (+/- PI/4 or +/- 3*PI/4). For small
# `phase_error`, we have:
#
#     phase_error ~= sin(phase_error)
#                  = Im(z_sym * conj(z_ideal)) / (|z_sym| * |z_ideal|)
#
# For unit-magnitude `z_ideal` (which it is, by construction) and
# assuming `|z_sym| ~= 1` (the role of the AGC in the Rust loop --
# 6E.6 skips AGC, see the change doc for the resulting accuracy
# discussion), this collapses to:
#
#     phase_error ~= q_sym * cos(dibit_phase) - i_sym * sin(dibit_phase)
#
# Plugging in cos / sin of the four ideal phases (all are +/-
# sqrt(2)/2):
#
#     dibit 00 (+1, +PI/4):    +(sqrt(2)/2) * (q - i)
#     dibit 01 (+3, +3PI/4):   -(sqrt(2)/2) * (q + i)
#     dibit 10 (-1, -PI/4):    +(sqrt(2)/2) * (q + i)
#     dibit 11 (-3, -3PI/4):   +(sqrt(2)/2) * (i - q)
#
# We fold the constant sqrt(2)/2 ~= 0.707 into the loop gain so the
# datapath sees only `(q +/- i)` and `i - q`, then clamps the raw
# value at the equivalent un-scaled limit `0.3 / (sqrt(2)/2) ~=
# 0.4243`, then multiplies by the *combined* gain
# `(sqrt(2)/2) * PLL_GAIN ~= 0.0707`. This saves one multiply
# vs the literal small-angle form.
#
# Linearization accuracy
# ----------------------
# `sin(0.3) ~= 0.2955` vs `0.3` -- a 1.5% relative error at the
# clamp boundary. Inside the clamp the error is much smaller. The
# PLL is a feedback loop with proportional gain 0.1 -- this kind of
# small per-step bias is absorbed by the integrator without
# affecting steady-state lock.
#
# Fixed-point format
# ------------------
# Inputs: signed 18-bit Q3.15 (LsmDiffDemodSlicer outputs).
# `raw` = (q +/- i): signed 19-bit Q4.15.
# Combined gain `(sqrt(2)/2) * PLL_GAIN`: Q1.16, fits in 14-bit.
# Multiply: signed 33-bit Q5.31. Shift right by 18 bits to align
# the result with the PLL register's Q2.13 format.
# PLL register: signed 16-bit Q2.13. Range +/-2 (ULP 1.2e-4) which
# comfortably contains the clamp range +/- PI/3 ~= +/-1.047.
#
# DSP48E1 cost
# ------------
# 1 DSP for the gain multiply. The 4-way `q +/- i` mux is just LUT.
# Total ~1 DSP for the PLL update block.
#
# SPDX-License-Identifier: MIT
#

import math

from amaranth import *


# Loop constants from `lsm::demod`.
PLL_GAIN_FLOAT = 0.1
PLL_MAX_ERROR_FLOAT = 0.3      # rad
MAX_PLL_ABS_FLOAT = math.pi / 3.0   # ~1.047 rad

# Linearization constants (sqrt(2)/2 absorbed into the gain).
SQRT2_OVER_2 = math.sqrt(2.0) / 2.0
COMBINED_GAIN_FLOAT = SQRT2_OVER_2 * PLL_GAIN_FLOAT  # ~0.0707
RAW_CLAMP_FLOAT = PLL_MAX_ERROR_FLOAT / SQRT2_OVER_2  # ~0.4243

# Q-format
INPUT_FRAC_BITS = 15      # Q3.15 (matches LsmDiffDemodSlicer)
GAIN_FRAC_BITS = 16       # Q1.16
PLL_FRAC_BITS = 13        # Q2.13 (signed 16-bit pll register)


def _q_round(value, frac_bits):
    return int(round(value * (1 << frac_bits)))


COMBINED_GAIN_Q16 = _q_round(COMBINED_GAIN_FLOAT, GAIN_FRAC_BITS)
RAW_CLAMP_Q15 = _q_round(RAW_CLAMP_FLOAT, INPUT_FRAC_BITS)
MAX_PLL_ABS_Q13 = _q_round(MAX_PLL_ABS_FLOAT, PLL_FRAC_BITS)

# Sanity-check the constants.
assert 4500 < COMBINED_GAIN_Q16 < 4800        # 0.0707 * 65536 ~= 4634
assert 13000 < RAW_CLAMP_Q15 < 14500          # 0.4243 * 32768 ~= 13903
assert 8000 < MAX_PLL_ABS_Q13 < 9000          # 1.047 * 8192 ~= 8580


class LsmPllUpdate(Elaboratable):
    """Decision-directed PLL update with small-angle linearization.

    Parameters
    ----------
    demod_width : int
        Width of input i_sym_in / q_sym_in. Default 18 (matches
        LsmDiffDemodSlicer).
    pll_width : int
        Width of the pll register / output. Default 16 (Q2.13,
        range +/- 2 with ULP 1.2e-4).

    Inputs (sync domain):
        i_sym_in, q_sym_in : signed demod_width
        dibit_in           : Signal(2)
        symbol_strobe      : Signal()

    Outputs (sync domain):
        pll_out     : signed pll_width  Q2.13 -- updated pll value
        pll_strobe  : Signal()  one cycle, 3 sync clocks after the
            input symbol_strobe pulse, when pll_out is valid
    """

    def __init__(self, *, demod_width=18, pll_width=16):
        self.dw = demod_width
        self.pw = pll_width

        # ── Inputs ──────────────────────────────────────────────
        self.i_sym_in = Signal(signed(demod_width))
        self.q_sym_in = Signal(signed(demod_width))
        self.dibit_in = Signal(2)
        self.symbol_strobe = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.pll_out = Signal(signed(pll_width), reset_less=True)
        self.pll_strobe = Signal()

        # ── Debug taps ──────────────────────────────────────────
        # Expose the raw (pre-clamp) phase-error proxy and the
        # post-gain step value so tests can sanity-check
        # intermediate stages without poking at private signals.
        self.raw_dbg = Signal(signed(demod_width + 1), reset_less=True)
        self.step_dbg = Signal(signed(pll_width + 2), reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # ── PLL accumulator (Q2.13, init 0) ─────────────────────
        pll_reg = Signal(signed(self.pw), init=0, reset_less=True)

        # ── Stage 1: 4-way mux + clamp on raw (q +/- i) ────────
        # raw is one bit wider than the inputs to hold the worst-
        # case |q| + |i| ~= 2 in Q3.15.
        raw_width = self.dw + 1
        raw_dibit = Signal(signed(raw_width))

        with m.Switch(self.dibit_in):
            with m.Case(0b00):
                # +(q - i)
                m.d.comb += raw_dibit.eq(self.q_sym_in - self.i_sym_in)
            with m.Case(0b01):
                # -(q + i)
                m.d.comb += raw_dibit.eq(-(self.q_sym_in + self.i_sym_in))
            with m.Case(0b10):
                # +(q + i)
                m.d.comb += raw_dibit.eq(self.q_sym_in + self.i_sym_in)
            with m.Case(0b11):
                # +(i - q)
                m.d.comb += raw_dibit.eq(self.i_sym_in - self.q_sym_in)

        # Combinational clamp to +/- RAW_CLAMP_Q15.
        clamp_pos = Const(RAW_CLAMP_Q15, signed(raw_width))
        clamp_neg = Const(-RAW_CLAMP_Q15, signed(raw_width))
        raw_clamped = Signal(signed(raw_width))
        with m.If(raw_dibit > clamp_pos):
            m.d.comb += raw_clamped.eq(clamp_pos)
        with m.Elif(raw_dibit < clamp_neg):
            m.d.comb += raw_clamped.eq(clamp_neg)
        with m.Else():
            m.d.comb += raw_clamped.eq(raw_dibit)

        # Latch the clamped value into a stage-1 register.
        raw_clamped_q = Signal(signed(raw_width), reset_less=True)
        stage1_strobe = Signal()
        with m.If(self.symbol_strobe):
            m.d.sync += [
                raw_clamped_q.eq(raw_clamped),
                stage1_strobe.eq(1),
            ]
        with m.Else():
            m.d.sync += stage1_strobe.eq(0)

        m.d.comb += self.raw_dbg.eq(raw_clamped_q)

        # ── Stage 2: multiply by combined gain ────────────────
        # combined_gain is Q1.16, fits in signed 18-bit safely.
        # Output: signed (raw_width + 18) bits, Q?.31 (15 + 16).
        gain_const = Const(COMBINED_GAIN_Q16, signed(18))
        product_width = raw_width + 18
        product = Signal(signed(product_width), reset_less=True)
        stage2_strobe = Signal()

        with m.If(stage1_strobe):
            m.d.sync += [
                product.eq(raw_clamped_q * gain_const),
                stage2_strobe.eq(1),
            ]
        with m.Else():
            m.d.sync += stage2_strobe.eq(0)

        # ── Stage 3: shift to Q2.13, subtract from pll, clamp ──
        # product is Q?.{15+16} = Q?.31. The pll register is Q2.13.
        # Shift right by (31 - 13) = 18.
        #
        # Round-to-nearest (instead of truncate-toward-zero): add a
        # half-ULP bias before the shift. The Amaranth `>>` is an
        # arithmetic right shift, which floors for negatives -- this
        # introduces a steady downward drift in a feedback loop. The
        # +half-ULP bias is the standard fix and gives round-half-up,
        # which is unbiased for symmetric input distributions like
        # ours (the dibit error term is zero-mean over the four
        # quadrants).
        step_shift = INPUT_FRAC_BITS + GAIN_FRAC_BITS - PLL_FRAC_BITS
        round_bias = Const(1 << (step_shift - 1), signed(product_width))
        step = (product + round_bias) >> step_shift

        # Sign-extend step into a wide-enough signed value to
        # subtract from pll without overflow concerns.
        # pll is signed pll_width; step here can be wider, so use
        # max(pll_width + 2, ...).
        delta_width = self.pw + 2
        delta = Signal(signed(delta_width))
        m.d.comb += delta.eq(step)
        m.d.comb += self.step_dbg.eq(delta)

        # new_pll = pll - delta
        new_pll = Signal(signed(delta_width + 1))
        m.d.comb += new_pll.eq(pll_reg - delta)

        # Clamp new_pll to +/- MAX_PLL_ABS_Q13.
        pll_max = Const(MAX_PLL_ABS_Q13, signed(self.pw))
        pll_min = Const(-MAX_PLL_ABS_Q13, signed(self.pw))
        new_pll_clamped = Signal(signed(self.pw))
        with m.If(new_pll > pll_max):
            m.d.comb += new_pll_clamped.eq(pll_max)
        with m.Elif(new_pll < pll_min):
            m.d.comb += new_pll_clamped.eq(pll_min)
        with m.Else():
            m.d.comb += new_pll_clamped.eq(new_pll)

        m.d.sync += self.pll_strobe.eq(0)
        with m.If(stage2_strobe):
            m.d.sync += [
                pll_reg.eq(new_pll_clamped),
                self.pll_out.eq(new_pll_clamped),
                self.pll_strobe.eq(1),
            ]

        return m
