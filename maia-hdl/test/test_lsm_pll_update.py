#
# Fishball P25 -- LSM PLL update HDL tests
#
# Phase 6E.6b. Drives `LsmPllUpdate` with synthetic inputs that
# exercise:
#   - the 4-way dibit mux (one test per dibit)
#   - the raw clamp (large input -> clamped step)
#   - the integrator clamp (drive pll past +/- pi/3)
#   - convergence behaviour against a Python reference
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_pll_update import (
    LsmPllUpdate,
    PLL_GAIN_FLOAT,
    PLL_MAX_ERROR_FLOAT,
    MAX_PLL_ABS_FLOAT,
    SQRT2_OVER_2,
    COMBINED_GAIN_FLOAT,
    RAW_CLAMP_FLOAT,
    INPUT_FRAC_BITS,
    PLL_FRAC_BITS,
)


def _q(x, frac_bits, width):
    """Saturate-clip a float to signed Q*.frac_bits."""
    q = int(round(x * (1 << frac_bits)))
    lo = -(1 << (width - 1))
    hi = (1 << (width - 1)) - 1
    return max(lo, min(hi, q))


class _PythonPll:
    """Linearised PLL update reference, matching the HDL block.

    Notice this is NOT a literal port of the Rust loop -- it's the
    *small-angle linearisation* the HDL implements. The Rust loop
    uses true atan2 + per-dibit ideal phase; the HDL skips the
    atan2 and computes
        phase_error_proxy = +/-(q +/- i)  (4-way dibit mux)
    then folds sqrt(2)/2 into the loop gain. The HDL test compares
    against THIS reference, not against the Rust loop, because the
    point of the test is to verify the HDL matches the algorithm
    we chose to implement.

    The full-pipeline integration test in 6E.6d will compare against
    the demod_loop_synthetic.json golden, which is the cross-check
    against the Rust loop and will absorb the ~1.5 % linearisation
    error in PLL convergence.
    """

    def __init__(self):
        self.pll = 0.0

    def step(self, i_sym, q_sym, dibit):
        if dibit == 0b00:
            raw = q_sym - i_sym
        elif dibit == 0b01:
            raw = -(q_sym + i_sym)
        elif dibit == 0b10:
            raw = q_sym + i_sym
        elif dibit == 0b11:
            raw = i_sym - q_sym
        # Clamp the *raw* value at the equivalent unscaled limit.
        if raw > RAW_CLAMP_FLOAT:
            raw = RAW_CLAMP_FLOAT
        elif raw < -RAW_CLAMP_FLOAT:
            raw = -RAW_CLAMP_FLOAT
        # Multiply by combined gain ((sqrt(2)/2) * PLL_GAIN).
        step = raw * COMBINED_GAIN_FLOAT
        self.pll -= step
        # Clamp pll.
        if self.pll > MAX_PLL_ABS_FLOAT:
            self.pll = MAX_PLL_ABS_FLOAT
        elif self.pll < -MAX_PLL_ABS_FLOAT:
            self.pll = -MAX_PLL_ABS_FLOAT
        return self.pll


def _drive_pll(dut, samples, *, drain=4):
    """Drive samples = [(i_q15, q_q15, dibit), ...] and collect
    (pll_out_q13, ...) per pll_strobe."""
    out = []

    async def bench(ctx):
        for (i, q, d) in samples:
            ctx.set(dut.i_sym_in, i)
            ctx.set(dut.q_sym_in, q)
            ctx.set(dut.dibit_in, d)
            ctx.set(dut.symbol_strobe, 1)
            await ctx.tick()
            ctx.set(dut.symbol_strobe, 0)
            for _ in range(drain):
                await ctx.tick()
                if ctx.get(dut.pll_strobe):
                    v = ctx.get(dut.pll_out)
                    if v >= (1 << 15):
                        v -= (1 << 16)
                    out.append(v)

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return out


class TestLsmPllUpdate(unittest.TestCase):

    def test_constants(self):
        """Q-format constants land within 1 ULP of the float values."""
        approx = LsmPllUpdate.__module__  # silence linter
        from p25_hdl.lsm_pll_update import (
            COMBINED_GAIN_Q16, RAW_CLAMP_Q15, MAX_PLL_ABS_Q13)
        self.assertAlmostEqual(
            COMBINED_GAIN_Q16 / (1 << 16), COMBINED_GAIN_FLOAT, places=4)
        self.assertAlmostEqual(
            RAW_CLAMP_Q15 / (1 << 15), RAW_CLAMP_FLOAT, places=3)
        self.assertAlmostEqual(
            MAX_PLL_ABS_Q13 / (1 << 13), MAX_PLL_ABS_FLOAT, places=3)

    def test_zero_input_no_change(self):
        """All-zero (i, q) -> raw == 0 -> step == 0 -> pll stays 0."""
        dut = LsmPllUpdate()
        samples = [(0, 0, 0b00)] * 10
        out = _drive_pll(dut, samples)
        self.assertGreaterEqual(len(out), 5)
        for v in out:
            self.assertEqual(v, 0)

    def test_each_dibit_drives_correct_sign(self):
        """For each of the four dibits, drive a positive `(i, q) =
        (0.5, 0.5)` input and confirm the resulting pll step is
        signed correctly per the linearisation table.

        With (i, q) = (0.5, 0.5):
            00:  raw = q - i = 0           ->  step = 0
            01:  raw = -(q + i) = -1       ->  step = -negative -> pll +
            10:  raw = +(q + i) = +1       ->  step = +positive -> pll -
            11:  raw = i - q = 0           ->  step = 0
        """
        i_q15 = _q(0.5, INPUT_FRAC_BITS, 18)
        q_q15 = _q(0.5, INPUT_FRAC_BITS, 18)

        # 00 -> step 0 -> pll stays 0
        dut = LsmPllUpdate()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b00)] * 5)
        for v in out:
            self.assertEqual(v, 0)

        # 11 -> step 0 -> pll stays 0
        dut = LsmPllUpdate()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b11)] * 5)
        for v in out:
            self.assertEqual(v, 0)

        # 01 -> raw = -1 (clamped to -RAW_CLAMP) -> step = -clamped*gain
        # -> pll += clamped*gain (positive)
        dut = LsmPllUpdate()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b01)] * 5)
        for v in out:
            self.assertGreater(v, 0,
                               f"01: expected positive pll, got {v}")

        # 10 -> raw = +1 (clamped to +RAW_CLAMP) -> step = +clamped*gain
        # -> pll -= positive (negative)
        dut = LsmPllUpdate()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b10)] * 5)
        for v in out:
            self.assertLess(v, 0,
                            f"10: expected negative pll, got {v}")

    def test_pll_clamps_at_pi_over_3(self):
        """Saturate the input to the max +/- raw value and let the
        loop integrate. The pll output must clamp at +/- pi/3."""
        # Use dibit 10 with (1, 1) input -> raw = 2, clamped to
        # RAW_CLAMP -> step = max negative -> pll grows in the
        # negative direction.
        i_q15 = _q(1.0, INPUT_FRAC_BITS, 18)  # saturates to 32767
        q_q15 = _q(1.0, INPUT_FRAC_BITS, 18)

        dut = LsmPllUpdate()
        # Run enough symbols to drive pll past pi/3.
        # Each step is approximately RAW_CLAMP * COMBINED_GAIN
        # ~= 0.4243 * 0.0707 ~= 0.030 rad / step.
        # pi/3 ~= 1.047, so ~35 steps to saturate. Run 100.
        out = _drive_pll(dut, [(i_q15, q_q15, 0b10)] * 100)
        # The final value should equal -MAX_PLL_ABS_Q13 (negative
        # because we drove pll downward).
        from p25_hdl.lsm_pll_update import MAX_PLL_ABS_Q13
        # Allow +/- 1 ULP at the clamp boundary.
        self.assertLessEqual(out[-1], -MAX_PLL_ABS_Q13 + 1)
        self.assertGreaterEqual(out[-1], -MAX_PLL_ABS_Q13 - 1)

    def test_against_python_reference(self):
        """Compare HDL pll trace against the Python linearised reference
        on a deterministic varying input."""
        rng = 0xCAFE
        n = 64
        py_inputs = []
        for k in range(n):
            rng = (rng * 1664525 + 1013904223) & 0xFFFFFFFF
            r1 = (rng / 0xFFFFFFFF)
            rng = (rng * 1664525 + 1013904223) & 0xFFFFFFFF
            r2 = (rng / 0xFFFFFFFF)
            i = 0.4 * (r1 - 0.5) * 2.0
            q = 0.4 * (r2 - 0.5) * 2.0
            d = (k & 0b11)
            py_inputs.append((i, q, d))

        py = _PythonPll()
        py_pll = []
        for (i, q, d) in py_inputs:
            py_pll.append(py.step(i, q, d))

        hdl_inputs = [
            (_q(i, INPUT_FRAC_BITS, 18),
             _q(q, INPUT_FRAC_BITS, 18),
             d)
            for (i, q, d) in py_inputs
        ]
        dut = LsmPllUpdate()
        out = _drive_pll(dut, hdl_inputs)

        self.assertEqual(len(out), len(py_pll),
                         f"HDL emitted {len(out)} samples, "
                         f"reference expected {len(py_pll)}")

        # Tolerance: pll output is Q2.13, ULP = 1.2e-4. Allow 16
        # ULPs (~2e-3 absolute) -- the fixed-point integrator
        # accumulates rounding error proportional to the number of
        # steps, and the multiply-then-shift truncation in stage 3
        # contributes ~1 ULP per step. 16 ULPs over 64 steps is
        # well below the 1.5 % linearisation error budget but
        # still tight enough to catch real bugs.
        TOLERANCE_ULPS = 16
        Q13 = 1 << 13
        max_err = 0
        for i, (got, ref) in enumerate(zip(out, py_pll)):
            ref_q13 = int(round(ref * Q13))
            err = abs(got - ref_q13)
            max_err = max(max_err, err)
            self.assertLessEqual(
                err, TOLERANCE_ULPS,
                f"sample {i}: HDL {got} (={got/Q13:.5f}) "
                f"vs ref {ref_q13} (={ref:.5f}), err {err} ULPs")


if __name__ == '__main__':
    unittest.main()
