#
# Fishball P25 -- LSM decision-directed PLL update
#
# Streaming Amaranth port of the decision-directed PLL update block
# in `p25-httpd/src/lsm/demod.rs` lines 265-279:
#
#     let h = to_dibit(soft_symbol)        # 4-PSK quadrant
#     let phase_error = soft_symbol - dibit_phase(h)
#     clamp phase_error to +/- PLL_MAX_ERROR     (0.3 rad)
#     state.pll -= phase_error * PLL_GAIN        (0.1)
#     clamp state.pll to +/- MAX_PLL_ABS         (PI/3)
#
# This file provides TWO implementations of the same interface:
#
#   - `LsmPllUpdate`            -- production CORDIC atan2 form
#                                  (Phase 6E.6e, doc/changes/024)
#   - `LsmPllUpdateLinearised`  -- legacy small-angle linearisation
#                                  (Phase 6E.6b, doc/changes/015)
#
# `LsmPllUpdate` is what `LsmDemodLoop` instantiates by default.
# `LsmPllUpdateLinearised` is kept alive for two reasons:
#   1. Sim-side A/B regression tests that prove the CORDIC form
#      tracks transients the linearised form drops onto the wrong
#      4-PSK Costas basin (test_lsm_demod_loop.py).
#   2. Historical reference -- the linearisation is documented in
#      doc/changes/015 and was the form actually flashed during the
#      first on-target test cycle. Keeping it in-tree means a future
#      reader can run both side-by-side without pulling git
#      archaeology.
#
# Why we replaced linearisation with CORDIC
# -----------------------------------------
# The linearised form approximates `phase_error = atan2(q, i) -
# dibit_phase(h)` as `(q +/- i) * sqrt(2)/2`, valid to first order
# for small phase errors. On synthetic clean RF and on the bench
# (Rust DSP loop comparison) this is good to ~1.5% at the +/- 0.3
# rad clamp boundary. On the 2026-04-10 on-target Clay County test
# (NAC 0x8A1, 860.9625 MHz), the linearised loop locked cleanly
# from cold boot, ran ~35 valid NIDs in 18 seconds, then drifted
# into a wrong 4-PSK Costas basin on a single transient and stuck
# there forever. The same RF feed through the Phase 6D Rust LSM
# pipeline (which uses true f32 atan2) ran 9 minutes without
# slipping. See doc/changes/024 for the full debug log + analysis;
# the short version is that the literal atan2 form has a
# self-correcting "phase error always points toward the nearest
# constellation point" property bounded by `|phase_error| <= pi/4`
# by construction, while the linearisation is bounded only by the
# raw clamp (a much weaker condition that lets transients sustain
# wrong-direction integration).
#
# CORDIC implementation details
# -----------------------------
# `LsmPllUpdate` uses `LsmCordicAtan2` (lsm_cordic_atan2.py) -- a
# 10-iteration vectoring CORDIC with quadrant pre-rotation. Pure
# shifts and adds, no DSP, no BRAM, ~100 LUT and 12-cycle pipeline
# latency. Worst-case angle error sweep at N=10: ~3 mrad max,
# ~1 mrad rms -- well below the per-step PLL gain * clamp budget
# of 30 mrad.
#
# Pre-rotation: instead of feeding the raw `(i, q)` into atan2 and
# then subtracting `dibit_phase(h)` (which would need an extra
# subtractor and modulo handling), we rotate the input by
# `-dibit_phase(h)` first. This puts the symbol near the +x axis
# when the dibit decision is correct, and the CORDIC then computes
# atan2 of the small residual angle directly. The pre-rotation is
# just adds and sign flips because the four `dibit_phase` values
# are all +/- pi/4 or +/- 3pi/4 (cos/sin both +/- sqrt(2)/2). The
# implicit sqrt(2)/2 factor is irrelevant -- atan2 is invariant to
# uniform input scaling.
#
# Pre-rotation table (let a = i + q, b = q - i):
#     dibit 00 (ideal +pi/4):     (x', y') = ( a,  b)
#     dibit 01 (ideal +3pi/4):    (x', y') = ( b, -a)
#     dibit 10 (ideal -pi/4):     (x', y') = (-b,  a)
#     dibit 11 (ideal -3pi/4):    (x', y') = (-a, -b)
#
# Fixed-point format
# ------------------
# Inputs:       signed 18-bit Q3.15 (LsmDiffDemodSlicer outputs).
# Pre-rotation: signed 20-bit (input_width + 2 for the negation
#               headroom on the +/- sign flip).
# CORDIC angle: signed 20-bit Q4.16 (range +/- 8 rad, well covers
#               the +/- pi atan2 output).
# Clamped angle: same Q4.16, clamped to +/- 0.3 rad = +/- 19661.
# Loop gain:    0.1 in Q1.16 = 6554, fits in signed 15-bit.
# Step:         angle * gain = Q5.32, shifted right by 19 to align
#               with the pll register's Q2.13.
# PLL register: signed 16-bit Q2.13, clamped to +/- pi/3 ~= +/-
#               8580 in Q2.13.
#
# Pipeline
# --------
# Symbol-strobe to pll-strobe latency for the CORDIC form:
#   1 cycle: pre-rotate (combinational, latched on symbol_strobe)
#   12 cycles: CORDIC vectoring (10 iters + IDLE/DONE bookkeeping)
#   1 cycle: clamp angle to +/- 0.3 rad
#   1 cycle: multiply clamped angle by 0.1 gain
#   1 cycle: subtract step from pll, clamp to +/- pi/3, latch out
# Total: 16 cycles. The symbol period at the Fishball clock
# (~62.5 MHz core / 4800 sym/s) is ~13000 cycles, so the latency
# is invisible in the symbol budget.
#
# Resource cost (CORDIC form)
# ---------------------------
# 1 DSP for the gain multiply (same as the linearised form),
# ~100 LUT and ~80 FF for the CORDIC vectoring block, ~30 LUT for
# the pre-rotation mux and post-CORDIC clamps. Total ~130 LUT
# above the linearised form. NO BRAM. Acceptable cost for the
# slip-resistance gain.
#
# SPDX-License-Identifier: MIT
#

import math

from amaranth import *

from .lsm_cordic_atan2 import (
    LsmCordicAtan2,
    ANGLE_FRAC_BITS as CORDIC_FRAC_BITS,
    ANGLE_WIDTH as CORDIC_ANGLE_WIDTH,
)


# Loop constants from `lsm::demod`.
PLL_GAIN_FLOAT = 0.1
PLL_MAX_ERROR_FLOAT = 0.3      # rad
MAX_PLL_ABS_FLOAT = math.pi / 3.0   # ~1.047 rad

# Linearization constants (sqrt(2)/2 absorbed into the gain).
# Used by `LsmPllUpdateLinearised` and the legacy Python reference
# in test_lsm_pll_update.py.
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

# CORDIC-form constants.
# Per-step PLL gain in Q1.16 -- shared with the linearised form's
# loop gain, just without the sqrt(2)/2 fold.
PLL_GAIN_Q16 = _q_round(PLL_GAIN_FLOAT, GAIN_FRAC_BITS)         # 6554
# Per-step phase error clamp expressed in CORDIC's Q4.16 angle
# format (the legacy linearised form's RAW_CLAMP_Q15 is in Q3.15
# *raw* (q +/- i) units, so it cannot be reused directly).
PLL_MAX_ERROR_Q16 = _q_round(PLL_MAX_ERROR_FLOAT, CORDIC_FRAC_BITS)  # 19661

# Sanity-check the constants.
assert 4500 < COMBINED_GAIN_Q16 < 4800        # 0.0707 * 65536 ~= 4634
assert 13000 < RAW_CLAMP_Q15 < 14500          # 0.4243 * 32768 ~= 13903
assert 8000 < MAX_PLL_ABS_Q13 < 9000          # 1.047 * 8192 ~= 8580
assert 6500 < PLL_GAIN_Q16 < 6600             # 0.1 * 65536 = 6554
assert 19500 < PLL_MAX_ERROR_Q16 < 19800      # 0.3 * 65536 ~= 19661


class LsmPllUpdateLinearised(Elaboratable):
    """Decision-directed PLL update -- small-angle linearisation.

    LEGACY form documented in doc/changes/015. Kept in-tree as the
    "before" side of the slip-resistance regression test in
    test_lsm_demod_loop.py and as a historical reference for the
    Phase 6E.6b form. Production builds should use `LsmPllUpdate`
    (the CORDIC form) -- this class is NOT instantiated by
    `LsmDemodLoop` unless explicitly requested with `pll_mode=
    'linearised'`.

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
        # Phase 8A: runtime reset. One-cycle pulse clears the PLL
        # accumulator + pipeline registers back to init. Wired in
        # `p25_top.py` from `lsm_control.lsm_reset` (W1P). See
        # `doc/changes/038_phase8_runtime_reset.md`.
        self.reset_in = Signal()

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

        # ── Phase 8A runtime reset override ─────────────────────
        # A 1-cycle `reset_in` pulse clears the PLL accumulator and
        # the pipeline registers. Last-assignment-wins in m.d.sync
        # makes this an override of any update in flight this cycle.
        # The PS protocol (p25-httpd retune path) disables the chain
        # a few sync cycles before pulsing reset, so no new
        # symbol_strobe is propagating through the pipeline when
        # this fires — we only need to clear the persistent state.
        with m.If(self.reset_in):
            m.d.sync += [
                pll_reg.eq(0),
                self.pll_out.eq(0),
                self.pll_strobe.eq(0),
                raw_clamped_q.eq(0),
                stage1_strobe.eq(0),
                product.eq(0),
                stage2_strobe.eq(0),
            ]

        return m


class LsmPllUpdate(Elaboratable):
    """Decision-directed PLL update with true CORDIC atan2 phase
    error.

    Production form (Phase 6E.6e). Replaces the small-angle
    linearisation in `LsmPllUpdateLinearised` with a 10-iteration
    CORDIC vectoring `LsmCordicAtan2` so the per-symbol phase
    error is computed bit-faithfully against `atan2(q, i) -
    dibit_phase(h)` instead of the first-order linear approximation
    `(q +/- i) * sqrt(2)/2`. The atan2 form has a self-correcting
    `|phase_error| <= pi/4` bound by construction (because
    `to_dibit` picks the closest 4-PSK quadrant before the
    subtraction), which prevents the cycle-slip behaviour observed
    on the linearised form during the 2026-04-10 Clay County
    on-target test.

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
        pll_strobe  : Signal()  one cycle, 16 sync clocks after the
            input symbol_strobe pulse, when pll_out is valid

    Debug taps (sync domain):
        angle_dbg  : signed CORDIC_ANGLE_WIDTH (Q4.16) -- the
            clamped CORDIC atan2 output for the most-recent symbol
        step_dbg   : signed pll_width + 2 -- the shifted+rounded
            per-step delta added to the pll register
    """

    def __init__(self, *, demod_width=18, pll_width=16):
        self.dw = demod_width
        self.pw = pll_width

        # ── Inputs ──────────────────────────────────────────────
        self.i_sym_in = Signal(signed(demod_width))
        self.q_sym_in = Signal(signed(demod_width))
        self.dibit_in = Signal(2)
        self.symbol_strobe = Signal()
        # Phase 8A: runtime reset. See LsmPllUpdateLinearised above.
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.pll_out = Signal(signed(pll_width), reset_less=True)
        self.pll_strobe = Signal()

        # ── Debug taps ──────────────────────────────────────────
        # Q4.16 clamped phase-error angle (post-CORDIC, post-clamp).
        self.angle_dbg = Signal(
            signed(CORDIC_ANGLE_WIDTH), reset_less=True)
        # Q2.13 shifted/rounded delta (matches the linearised form's
        # step_dbg signature so test infrastructure can compare).
        self.step_dbg = Signal(signed(pll_width + 2), reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # ── CORDIC submodule ────────────────────────────────────
        # Input width: dw + 2 to hold the post-pre-rotation values
        # (dw+1 for the i+q sum and one extra bit for the negation
        # in the +/- sign flip).
        m.submodules.cordic = cordic = LsmCordicAtan2(
            input_width=self.dw + 2)

        # ── Persistent PLL accumulator (Q2.13, init 0) ──────────
        pll_reg = Signal(signed(self.pw), init=0, reset_less=True)

        # ── Stage 0: pre-rotation by -dibit_phase(h) ────────────
        # Compute a = i + q and b = q - i once, then permute via a
        # 4-way mux on the dibit. The implicit sqrt(2)/2 factor on
        # the rotation is irrelevant -- atan2 is invariant to
        # uniform input scaling.
        sum_width = self.dw + 1
        a = Signal(signed(sum_width))
        b = Signal(signed(sum_width))
        m.d.comb += [
            a.eq(self.i_sym_in + self.q_sym_in),
            b.eq(self.q_sym_in - self.i_sym_in),
        ]

        rot_width = self.dw + 2  # +1 for sum, +1 for sign-flip
        x_pre = Signal(signed(rot_width))
        y_pre = Signal(signed(rot_width))
        with m.Switch(self.dibit_in):
            with m.Case(0b00):              # ideal +pi/4
                m.d.comb += [x_pre.eq(a), y_pre.eq(b)]
            with m.Case(0b01):              # ideal +3pi/4
                m.d.comb += [x_pre.eq(b), y_pre.eq(-a)]
            with m.Case(0b10):              # ideal -pi/4
                m.d.comb += [x_pre.eq(-b), y_pre.eq(a)]
            with m.Case(0b11):              # ideal -3pi/4
                m.d.comb += [x_pre.eq(-a), y_pre.eq(-b)]

        # Drive CORDIC. We feed strobe_in directly from the upstream
        # symbol_strobe so the CORDIC starts on the same cycle the
        # rotated symbol becomes valid. x/y are combinationally
        # latched into the CORDIC's IDLE-state pre-rotate registers.
        m.d.comb += [
            cordic.x_in.eq(x_pre),
            cordic.y_in.eq(y_pre),
            cordic.strobe_in.eq(self.symbol_strobe),
        ]

        # ── Single-shot zero-input gate ─────────────────────────
        # The Rust loop skips the PLL update entirely when
        # `soft_symbol == 0.0` (i.e. atan2 returns exactly 0).
        # We approximate that with `(i_sym_in == 0) AND (q_sym_in
        # == 0)` -- the only input that produces an undefined
        # atan2. The CORDIC FSM happily processes (0, 0) and emits
        # the algebraic sum of every CORDIC angle constant
        # (~1.74 rad), which would clamp to 0.3 and *do something*
        # if we let it through. So we latch a `pending_skip` flag
        # at symbol_strobe and use it to suppress the final
        # subtract.
        #
        # Single-shot is safe because symbol_strobe events are
        # ~13000 sync cycles apart (4800 sym/s on a ~62.5 MHz
        # core) and the CORDIC + post-stages finish in ~16 cycles
        # -- only one update is ever in flight.
        pending_skip = Signal()
        with m.If(self.symbol_strobe):
            m.d.sync += pending_skip.eq(
                (self.i_sym_in == 0) & (self.q_sym_in == 0))

        # ── Stage 1: clamp CORDIC angle to +/- 0.3 rad (Q4.16) ──
        clamp_pos = Const(
            PLL_MAX_ERROR_Q16, signed(CORDIC_ANGLE_WIDTH))
        clamp_neg = Const(
            -PLL_MAX_ERROR_Q16, signed(CORDIC_ANGLE_WIDTH))
        clamped_angle = Signal(signed(CORDIC_ANGLE_WIDTH))
        with m.If(cordic.angle_out > clamp_pos):
            m.d.comb += clamped_angle.eq(clamp_pos)
        with m.Elif(cordic.angle_out < clamp_neg):
            m.d.comb += clamped_angle.eq(clamp_neg)
        with m.Else():
            m.d.comb += clamped_angle.eq(cordic.angle_out)

        # Latch into stage 1 register on cordic.strobe_out.
        clamped_angle_q = Signal(
            signed(CORDIC_ANGLE_WIDTH), reset_less=True)
        stage1_strobe = Signal()
        stage1_skip = Signal()
        m.d.sync += stage1_strobe.eq(0)
        with m.If(cordic.strobe_out):
            m.d.sync += [
                clamped_angle_q.eq(clamped_angle),
                stage1_strobe.eq(1),
                # Carry the skip flag forward through the post-CORDIC
                # stages so the final subtract honours it.
                stage1_skip.eq(pending_skip),
            ]

        m.d.comb += self.angle_dbg.eq(clamped_angle_q)

        # ── Stage 2: multiply clamped angle by gain (Q1.16) ─────
        # Gain 0.1 in Q1.16 = 6554, fits in signed 15-bit.
        # Product: signed (CORDIC_ANGLE_WIDTH + 15) bits, Q5.32.
        gain_const = Const(PLL_GAIN_Q16, signed(15))
        product_width = CORDIC_ANGLE_WIDTH + 15
        product = Signal(signed(product_width), reset_less=True)
        stage2_strobe = Signal()
        stage2_skip = Signal()
        m.d.sync += stage2_strobe.eq(0)
        with m.If(stage1_strobe):
            m.d.sync += [
                product.eq(clamped_angle_q * gain_const),
                stage2_strobe.eq(1),
                stage2_skip.eq(stage1_skip),
            ]

        # ── Stage 3: shift to Q2.13, subtract from pll, clamp ──
        # product is Q?.{16+16} = Q?.32. The pll register is Q2.13.
        # Shift right by (32 - 13) = 19. Round-to-nearest via the
        # +half-ULP bias before the arithmetic shift -- same trick
        # as the linearised form, same rationale (zero-mean error
        # over the four dibit quadrants, no integrator drift).
        step_shift = CORDIC_FRAC_BITS + GAIN_FRAC_BITS - PLL_FRAC_BITS
        round_bias = Const(
            1 << (step_shift - 1), signed(product_width))
        step = (product + round_bias) >> step_shift

        delta_width = self.pw + 2
        delta = Signal(signed(delta_width))
        m.d.comb += delta.eq(step)
        m.d.comb += self.step_dbg.eq(delta)

        # If the input was exactly (0, 0), zero out the delta so
        # the pll register is left untouched (matches Rust's
        # `if soft_symbol != 0.0` skip path).
        effective_delta = Signal(signed(delta_width))
        with m.If(stage2_skip):
            m.d.comb += effective_delta.eq(0)
        with m.Else():
            m.d.comb += effective_delta.eq(delta)

        new_pll = Signal(signed(delta_width + 1))
        m.d.comb += new_pll.eq(pll_reg - effective_delta)

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

        # ── Phase 8A runtime reset override ─────────────────────
        # Clear the persistent PLL accumulator + all post-CORDIC
        # pipeline registers. The CORDIC submodule itself has no
        # reset pin (yet) but its 16-cycle pipeline drains naturally
        # once upstream symbol_strobe stops firing, and the PS
        # protocol guarantees that drain before it pulses reset.
        with m.If(self.reset_in):
            m.d.sync += [
                pll_reg.eq(0),
                self.pll_out.eq(0),
                self.pll_strobe.eq(0),
                pending_skip.eq(0),
                clamped_angle_q.eq(0),
                stage1_strobe.eq(0),
                stage1_skip.eq(0),
                product.eq(0),
                stage2_strobe.eq(0),
                stage2_skip.eq(0),
            ]

        return m
