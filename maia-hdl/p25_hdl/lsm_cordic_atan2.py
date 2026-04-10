#
# Fishball P25 -- 4-iteration CORDIC vectoring atan2(y, x)
#
# Phase 6E.6e of the LSM HDL port. New block introduced to replace
# the small-angle linearised PLL update body in `lsm_pll_update.py`
# (the original Phase 6E.6b form documented in doc/changes/015).
#
# Why we needed this
# ------------------
# The Phase 6E.6b PLL update used a small-angle linearisation
# (`raw = (q +/- i)`, `pll -= raw * combined_gain`) to avoid putting
# a CORDIC atan2 in the symbol-rate critical path. On synthetic test
# vectors and clean RF the linearisation was good to ~1.5 % at the
# +/- 0.3 rad clamp boundary, which the integrator absorbs without
# affecting steady-state lock.
#
# On real-RF Clay County NAC 0x8A1 (2026-04-10 test cycle 3) the
# linearised loop locked cleanly from cold boot, ran ~35 valid NIDs
# in 18 seconds, and then drifted into a wrong 4-PSK Costas basin on
# a single transient -- pll integrator flipped sign from
# ~[-8500, -4500] to ~[+5000, +8500] within one second and stuck
# there forever. Same RF feed through the Phase 6D Rust LSM pipeline
# (which uses true f32 atan2) ran 9 minutes without slipping. The
# linearisation is the cause -- see doc/changes/024 for the full
# analysis. The fix is to replace the linearised body with a true
# atan2-based phase-error computation, which keeps the per-symbol
# update bounded by `|to_dibit(angle) - dibit_phase(h)| <= pi/4` by
# construction and matches the proven-good Rust algorithm bit-by-bit
# (modulo CORDIC residual quantisation, which is well below the
# loop's tolerance budget).
#
# CORDIC vectoring algorithm
# --------------------------
# Given an input vector (x, y), iteratively rotate it toward the
# +x axis by a sequence of fixed-angle rotations whose tangents are
# powers of two (so the rotation is a shift, not a multiply).
# Accumulate the rotation angle as we go; after N iterations the
# accumulator equals atan2(y, x) within the residual atan(2^-N).
#
# Per iteration i (vectoring mode, drives y -> 0):
#
#     if y >= 0:
#         x_new = x + (y >> i)
#         y_new = y - (x >> i)
#         z_new = z + atan(2^-i)
#     else:
#         x_new = x - (y >> i)
#         y_new = y + (x >> i)
#         z_new = z - atan(2^-i)
#
# We use 10 iterations. Empirical sweep over 2000 random inputs at
# N = 4, 6, 8, 10, 12 (max-magnitude integer inputs ~Q14):
#
#     N= 4 : max err =  124.3 mrad   rms =  70.3 mrad
#     N= 6 : max err =   31.3 mrad   rms =  18.0 mrad
#     N= 8 : max err =    8.4 mrad   rms =   4.6 mrad
#     N=10 : max err =    2.8 mrad   rms =   1.2 mrad
#     N=12 : max err =    1.4 mrad   rms =   0.3 mrad
#
# At N=10 the worst case is well below the per-step PLL clamp
# (0.3 rad), well below the original linearisation's 1.5 % error
# at the clamp boundary (~4.5 mrad), and below the gain*clamp
# product (0.1 * 0.3 = 30 mrad) so the integrator absorbs it
# instantly. N=12 buys an extra ~1.4 mrad of headroom for an extra
# 2 cycles of latency and ~16 LUT -- not worth it given there is
# no measurable accuracy benefit at the loop's tolerance budget.
#
# Quadrant pre-rotation
# ---------------------
# CORDIC vectoring as written natively handles inputs with x >= 0
# (right half-plane) -- the iteration only rotates by angles in
# (-pi/2, +pi/2). For x < 0 we pre-rotate the input by +/- pi/2 to
# bring it into the right half-plane and seed z with the
# corresponding offset:
#
#     if y >= 0:    # second quadrant
#         x' = +y, y' = -x, z = +pi/2     (rotate input by -pi/2)
#     else:         # third quadrant
#         x' = -y, y' = +x, z = -pi/2     (rotate input by +pi/2)
#
# After this, the iteration body operates on a positive-x vector
# and the final accumulator is the full-circle angle.
#
# CORDIC magnitude gain
# ---------------------
# Each iteration grows the vector magnitude by sqrt(1 + 4^-i). The
# total gain after 4 iterations is K_4 ~= 1.6468 (the Pythagorean
# product of the per-iteration gains). We don't care about the
# output magnitude (atan2 is invariant to uniform scaling), but the
# *internal* x and y registers must be wide enough to hold the
# grown values without wrapping. We allow input_width + 3 bits of
# headroom: 1 for the +/- pi/2 sign flip, 1 for the CORDIC gain
# (ceil(log2(1.6468)) = 1), and 1 for safety against per-iteration
# arithmetic worst-cases.
#
# Q-format
# --------
# Inputs:  signed `input_width` bits. The interpretation is up to
#          the caller -- atan2 is scale-invariant. The width must
#          be wide enough to hold the post-pre-rotation magnitudes
#          plus the CORDIC gain.
#
# Output:  signed 20-bit Q4.16, range +/- 8 rad. Comfortably covers
#          the +/- pi range any atan2 result can take.
#
# Pipeline
# --------
# Iterative FSM, 1 cycle per iteration:
#
#   IDLE   --strobe_in--> ITER0 : 1 cycle (latch + pre-rotate)
#   ITER0  --auto------>  ITER1
#   ...
#   ITER9  --auto------>  DONE
#   DONE   --auto------>  IDLE  : 1 cycle (latch output + raise strobe_out)
#
# Total latency: 12 sync cycles from `strobe_in` to `strobe_out`
# for 10 iterations. This block is downstream of the symbol_strobe
# in LsmPllUpdate, which fires once per symbol (~6500 sync cycles
# apart at the Fishball clock frequency vs the 4800 sym/s P25 baud
# rate), so the 12-cycle latency is invisible in the symbol-rate
# budget.
#
# Resource cost
# -------------
# 10 iterations + pre-rotate: ~4 LUT/iter + 60 LUT for FSM and
# pre-rotate scaffolding ~= 100 LUT. Approx 80 FF for the working
# registers (x, y, z, FSM state). NO DSP, NO BRAM. The shifters
# are constant-distance arithmetic shifts, which Amaranth
# synthesises directly to wire permutation + sign-extension.
#
# SPDX-License-Identifier: MIT
#

import math

from amaranth import *


# CORDIC iteration count and angle accumulator format. These are
# module-scope so the test bench can pull them in for the bit-exact
# Python reference.
N_ITERS = 10
ANGLE_FRAC_BITS = 16
ANGLE_INT_BITS = 4              # signed; range +/- 8 rad
ANGLE_WIDTH = ANGLE_INT_BITS + ANGLE_FRAC_BITS   # 20 bits


def _cordic_angle_q(i):
    """`atan(2 ** -i)` quantised to Q4.16 (rounded to nearest)."""
    return int(round(math.atan(2.0 ** (-i)) * (1 << ANGLE_FRAC_BITS)))


# Pre-rotation constant.
PI_OVER_2_Q = int(round(math.pi / 2.0 * (1 << ANGLE_FRAC_BITS)))


# Sanity-check the constants at module load. The exact integer
# values come from rounding `atan(2^-i) * 2^16` to nearest; if any
# of these drift, the bit-exact reference no longer matches the
# HDL and tests will fail loudly with a clear pointer.
assert PI_OVER_2_Q == 102944, f"PI_OVER_2_Q = {PI_OVER_2_Q}"
_EXPECTED_CORDIC_ANGLES = [
    51472, 30386, 16055, 8150, 4091, 2047, 1024, 512, 256, 128,
]
_ACTUAL_CORDIC_ANGLES = [_cordic_angle_q(i) for i in range(N_ITERS)]
assert _ACTUAL_CORDIC_ANGLES == _EXPECTED_CORDIC_ANGLES, (
    f"CORDIC angles drifted: expected {_EXPECTED_CORDIC_ANGLES} "
    f"got {_ACTUAL_CORDIC_ANGLES}"
)


def cordic_vectoring_reference(x, y, n_iters=N_ITERS):
    """Bit-exact Python reference for `LsmCordicAtan2`.

    Implements the same fixed-point quadrant pre-rotation, the same
    `n_iters` CORDIC iterations, and the same Q4.16 angle accumulator
    as the HDL block. Returns the final z value as a signed integer
    in Q4.16. Used by the unit tests to compare against the HDL
    output ULP-by-ULP.
    """
    # Quadrant pre-rotation.
    if x >= 0:
        z = 0
    elif y >= 0:
        x, y = y, -x
        z = PI_OVER_2_Q
    else:
        x, y = -y, x
        z = -PI_OVER_2_Q

    # CORDIC iterations.
    for i in range(n_iters):
        # Arithmetic right shift; Python's >> on negative ints is
        # already arithmetic (floor division by 2).
        x_shifted = x >> i
        y_shifted = y >> i
        if y >= 0:
            x, y = x + y_shifted, y - x_shifted
            z += _cordic_angle_q(i)
        else:
            x, y = x - y_shifted, y + x_shifted
            z -= _cordic_angle_q(i)

    return z


class LsmCordicAtan2(Elaboratable):
    """4-iteration CORDIC vectoring atan2 with quadrant pre-rotate.

    No DSP, no BRAM, no multiplies -- pure shifts and adds.

    Parameters
    ----------
    input_width : int
        Width of `x_in` / `y_in` (signed). Default 20 -- enough for
        the post-pre-rotation symbol values from `LsmPllUpdate`
        (max ~|i|+|q| ~= 2 * 2^17 with the dw=18 inputs).

    Inputs (sync domain):
        x_in, y_in   : signed input_width
        strobe_in    : Signal()  one cycle when (x_in, y_in) is valid

    Outputs (sync domain):
        angle_out  : signed 20  Q4.16  -- atan2(y_in, x_in)
        strobe_out : Signal()  one cycle, 6 sync clocks after strobe_in

    Pipeline latency: 6 cycles from strobe_in to strobe_out.
    """

    def __init__(self, *, input_width=20):
        self.iw = input_width
        # Working width holds the (potentially-grown) x and y across
        # all CORDIC iterations. See "CORDIC magnitude gain" above
        # for the +3 bits of headroom rationale.
        self.ww = input_width + 3

        # Q-format constants exposed for the tests.
        self.aw = ANGLE_WIDTH
        self.afb = ANGLE_FRAC_BITS
        self.n_iters = N_ITERS

        # Inputs
        self.x_in = Signal(signed(self.iw))
        self.y_in = Signal(signed(self.iw))
        self.strobe_in = Signal()

        # Outputs
        self.angle_out = Signal(signed(self.aw), reset_less=True)
        self.strobe_out = Signal()

    def elaborate(self, platform):
        m = Module()

        # Working registers. Pre-rotation extends these from the
        # raw input width to `ww` bits via Amaranth's automatic
        # sign-extension on `Signal.eq()`.
        x = Signal(signed(self.ww))
        y = Signal(signed(self.ww))
        z = Signal(signed(self.aw))

        # Default: strobe_out low.
        m.d.sync += self.strobe_out.eq(0)

        # CORDIC angle constants.
        cordic_angles = [
            Const(_cordic_angle_q(i), signed(self.aw))
            for i in range(self.n_iters)
        ]
        pi_over_2 = Const(PI_OVER_2_Q, signed(self.aw))
        neg_pi_over_2 = Const(-PI_OVER_2_Q, signed(self.aw))

        with m.FSM():
            with m.State("IDLE"):
                with m.If(self.strobe_in):
                    # Quadrant pre-rotation; ensure x is non-negative
                    # before entering the CORDIC iterations.
                    with m.If(self.x_in >= 0):
                        m.d.sync += [
                            x.eq(self.x_in),
                            y.eq(self.y_in),
                            z.eq(0),
                        ]
                    with m.Elif(self.y_in >= 0):
                        # Second quadrant: rotate input by -pi/2
                        # so x = +y_in, y = -x_in, seed z = +pi/2.
                        m.d.sync += [
                            x.eq(self.y_in),
                            y.eq(-self.x_in),
                            z.eq(pi_over_2),
                        ]
                    with m.Else():
                        # Third quadrant: rotate input by +pi/2
                        # so x = -y_in, y = +x_in, seed z = -pi/2.
                        m.d.sync += [
                            x.eq(-self.y_in),
                            y.eq(self.x_in),
                            z.eq(neg_pi_over_2),
                        ]
                    m.next = "ITER0"

            for i in range(self.n_iters):
                with m.State(f"ITER{i}"):
                    # Constant-distance arithmetic right shift.
                    # Amaranth's >> on a signed signal is arithmetic.
                    x_shifted = x >> i
                    y_shifted = y >> i
                    with m.If(y >= 0):
                        m.d.sync += [
                            x.eq(x + y_shifted),
                            y.eq(y - x_shifted),
                            z.eq(z + cordic_angles[i]),
                        ]
                    with m.Else():
                        m.d.sync += [
                            x.eq(x - y_shifted),
                            y.eq(y + x_shifted),
                            z.eq(z - cordic_angles[i]),
                        ]
                    if i < self.n_iters - 1:
                        m.next = f"ITER{i + 1}"
                    else:
                        m.next = "DONE"

            with m.State("DONE"):
                m.d.sync += [
                    self.angle_out.eq(z),
                    self.strobe_out.eq(1),
                ]
                m.next = "IDLE"

        return m
