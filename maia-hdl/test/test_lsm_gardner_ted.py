#
# Fishball P25 -- LSM Gardner TED HDL tests
#
# Phase 6E.6a of the LSM HDL port. Drives `LsmGardnerTed` with a
# pure-Python reference of the same algorithm and verifies the
# fixed-point HDL output is within tolerance.
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_gardner_ted import (
    LsmGardnerTed,
    SPS_FLOAT,
    MAX_TIMING_ADJ_FLOAT,
    TED_GAIN_FLOAT,
    PREV_SYM_INIT_FLOAT,
)


def _q(x, frac_bits, width):
    """Saturate-clip a float to signed Q*.frac_bits."""
    q = int(round(x * (1 << frac_bits)))
    lo = -(1 << (width - 1))
    hi = (1 << (width - 1)) - 1
    return max(lo, min(hi, q))


class _PythonGardner:
    """Pure-Python clone of the Gardner TED algorithm.

    Mirrors `lsm::demod::demod_lsm_with_state` lines 254-263 in
    floating point. The HDL test compares the HDL fixed-point output
    against the float output from this reference, sample by sample.
    """

    def __init__(self):
        self.prev_sym_i = PREV_SYM_INIT_FLOAT
        self.prev_sym_q = PREV_SYM_INIT_FLOAT

    def step(self, i_sym, q_sym, i_mid_demod, q_mid_demod):
        timing_adj = ((self.prev_sym_i - i_sym) * i_mid_demod
                      + (self.prev_sym_q - q_sym) * q_mid_demod)
        if timing_adj > MAX_TIMING_ADJ_FLOAT:
            timing_adj = MAX_TIMING_ADJ_FLOAT
        elif timing_adj < -MAX_TIMING_ADJ_FLOAT:
            timing_adj = -MAX_TIMING_ADJ_FLOAT
        timing_adj *= TED_GAIN_FLOAT
        # Update prev for the next call.
        self.prev_sym_i = i_sym
        self.prev_sym_q = q_sym
        return timing_adj


class TestLsmGardnerTed(unittest.TestCase):

    def _simulate(self, dut, bench):
        sim = Simulator(dut)
        sim.add_clock(16e-9)  # 62.5 MHz
        sim.add_testbench(bench)
        sim.run()

    def test_constants(self):
        """Q-format constants land within 1 ULP of the float values."""
        # MAX_TIMING_ADJ_Q15 / 2^15 ~= MAX_TIMING_ADJ_FLOAT
        approx = LsmGardnerTed.MAX_TIMING_ADJ_Q15 / (1 << 15)
        self.assertAlmostEqual(approx, MAX_TIMING_ADJ_FLOAT, places=4)
        # TED_GAIN_Q16 / 2^16 ~= TED_GAIN_FLOAT
        approx = LsmGardnerTed.TED_GAIN_Q16 / (1 << 16)
        self.assertAlmostEqual(approx, TED_GAIN_FLOAT, places=4)
        # PREV_SYM_INIT_Q15 / 2^15 ~= 0.7
        approx = LsmGardnerTed.PREV_SYM_INIT_Q15 / (1 << 15)
        self.assertAlmostEqual(approx, PREV_SYM_INIT_FLOAT, places=4)

    def test_zero_input_no_adjustment(self):
        """All-zero inputs after a few cycles -> timing_adj == 0.

        With i_sym = q_sym = 0, after one symbol prev_sym = (0, 0)
        and the dot product is identically zero. The Gardner output
        must also be zero.
        """
        dut = LsmGardnerTed()
        outs = []

        async def bench(ctx):
            for _ in range(20):
                ctx.set(dut.i_sym_in, 0)
                ctx.set(dut.q_sym_in, 0)
                ctx.set(dut.i_mid_demod_in, 0)
                ctx.set(dut.q_mid_demod_in, 0)
                ctx.set(dut.symbol_strobe, 1)
                await ctx.tick()
                ctx.set(dut.symbol_strobe, 0)
                # Pipeline drain
                for _ in range(3):
                    await ctx.tick()
                    if ctx.get(dut.timing_adj_strobe):
                        outs.append(ctx.get(dut.timing_adj_out))

        self._simulate(dut, bench)
        # Skip the first output (uses init prev_sym = 0.7, so dot
        # product isn't zero). After the second symbol, prev_sym
        # has been updated to 0 and the output is 0.
        for v in outs[2:]:
            self.assertEqual(v, 0, f"expected 0, got {v}")

    def test_constant_phase_input_zero_adjustment(self):
        """Constant input -> after first cycle, prev == sym so the
        dot product is (0)*mid + (0)*mid = 0, regardless of the
        midpoint values."""
        dut = LsmGardnerTed()
        outs = []
        const_i = _q(0.5, 15, 18)
        const_q = _q(0.5, 15, 18)
        const_im = _q(0.3, 15, 18)
        const_qm = _q(0.3, 15, 18)

        async def bench(ctx):
            for _ in range(20):
                ctx.set(dut.i_sym_in, const_i)
                ctx.set(dut.q_sym_in, const_q)
                ctx.set(dut.i_mid_demod_in, const_im)
                ctx.set(dut.q_mid_demod_in, const_qm)
                ctx.set(dut.symbol_strobe, 1)
                await ctx.tick()
                ctx.set(dut.symbol_strobe, 0)
                for _ in range(3):
                    await ctx.tick()
                    if ctx.get(dut.timing_adj_strobe):
                        outs.append(ctx.get(dut.timing_adj_out))

        self._simulate(dut, bench)
        # Skip first output (still settling against the 0.7 init
        # of prev_sym).
        for v in outs[2:]:
            self.assertEqual(v, 0)

    def test_against_python_reference(self):
        """Drive a deterministic varying input and compare per-symbol
        timing_adj against the Python reference."""
        dut = LsmGardnerTed()

        # Build a sequence of (i_sym, q_sym, i_mid, q_mid) in
        # floating point.
        n = 32
        py_inputs = []
        rng_state = 12345
        for k in range(n):
            # Deterministic pseudo-random generator: linear-congruential.
            rng_state = (rng_state * 1103515245 + 12345) & 0x7FFFFFFF
            r = rng_state / 0x7FFFFFFF  # in [0, 1)
            phase_sym = 2.0 * math.pi * r
            phase_mid = 2.0 * math.pi * (r * 0.5 + 0.25)
            mag_sym = 0.6 + 0.2 * (r - 0.5)  # ~0.5..0.7
            mag_mid = 0.4 + 0.2 * (r - 0.5)
            i_sym = mag_sym * math.cos(phase_sym)
            q_sym = mag_sym * math.sin(phase_sym)
            i_mid = mag_mid * math.cos(phase_mid)
            q_mid = mag_mid * math.sin(phase_mid)
            py_inputs.append((i_sym, q_sym, i_mid, q_mid))

        # Run Python reference.
        gardner = _PythonGardner()
        py_out = []
        for inp in py_inputs:
            adj = gardner.step(*inp)
            py_out.append(adj)

        # Convert to fixed-point Q3.15 for HDL drive.
        hdl_inputs = [
            (_q(i_s, 15, 18), _q(q_s, 15, 18),
             _q(i_m, 15, 18), _q(q_m, 15, 18))
            for (i_s, q_s, i_m, q_m) in py_inputs
        ]

        outs = []

        async def bench(ctx):
            for (i_s, q_s, i_m, q_m) in hdl_inputs:
                ctx.set(dut.i_sym_in, i_s)
                ctx.set(dut.q_sym_in, q_s)
                ctx.set(dut.i_mid_demod_in, i_m)
                ctx.set(dut.q_mid_demod_in, q_m)
                ctx.set(dut.symbol_strobe, 1)
                await ctx.tick()
                ctx.set(dut.symbol_strobe, 0)
                for _ in range(3):
                    await ctx.tick()
                    if ctx.get(dut.timing_adj_strobe):
                        # sign-extend the 16-bit signed result
                        v = ctx.get(dut.timing_adj_out)
                        if v >= (1 << 15):
                            v -= (1 << 16)
                        outs.append(v)
            # Drain any remaining
            for _ in range(5):
                await ctx.tick()
                if ctx.get(dut.timing_adj_strobe):
                    v = ctx.get(dut.timing_adj_out)
                    if v >= (1 << 15):
                        v -= (1 << 16)
                    outs.append(v)

        self._simulate(dut, bench)

        self.assertEqual(len(outs), len(py_out),
                         f"HDL emitted {len(outs)} samples, "
                         f"reference expected {len(py_out)}")

        # Tolerance: timing_adj_out is Q4.12, ULP = 1/4096 ~= 2.4e-4.
        # Allow ~6 ULPs (roughly 1.5e-3 absolute) -- the fixed-point
        # pipeline has a couple of rounding stages (multiply,
        # gain mult, shift), each contributing ~1 ULP of noise.
        TOLERANCE_ULPS = 6
        Q12_ULP = 1.0 / (1 << 12)
        max_err = 0
        for i, (got, ref) in enumerate(zip(outs, py_out)):
            ref_q12 = int(round(ref * (1 << 12)))
            err = abs(got - ref_q12)
            max_err = max(max_err, err)
            self.assertLessEqual(
                err, TOLERANCE_ULPS,
                f"sample {i}: HDL {got} (={got * Q12_ULP:.5f}) "
                f"vs ref {ref_q12} (={ref:.5f}), err {err} ULPs")


if __name__ == '__main__':
    unittest.main()
