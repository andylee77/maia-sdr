#
# Fishball P25 -- LSM timing recovery + interp HDL tests
#
# Phase 6E.4 of the LSM HDL port. Drives `LsmTimingInterp` with
# synthetic input streams and verifies the four interpolated output
# values + the decision strobe rate against a Python reference that
# implements the same lerp + sample_point algorithm in floating
# point.
#
# Phase 6E.6e timing fix (doc/changes/022): `LsmTimingInterp` is
# now a 2-stage pipeline (was 1 cycle). decision_strobe fires 2
# cycles after the input strobe_in that triggered the decision,
# rather than 1. The polling pattern below adds an extra
# `await ctx.tick()` after the input strobe to drain the new
# stage 1 -> stage 2 latch and read decision_strobe at the cycle
# the lerp results are latched into the output flops.
#
# This sub-phase does NOT compare against the Phase 6D
# `demod_loop_synthetic` golden vector -- the Rust loop has AGC,
# differential demod, PLL rotation, and Gardner TED all wired in,
# none of which exist yet. The full-pipeline integration test
# lands in 6E.6 once those blocks are added.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_timing_interp import (
    LsmTimingInterp,
    P25_LSM_SAMPLE_RATE_HZ,
    P25_SYMBOL_RATE_HZ,
    SPS_Q12,
    HALF_SPS_Q12,
    ONE_Q12,
)


# Q15 ULP for tolerance
Q15_ULP = 1.0 / (1 << 15)


def _to_q15(x):
    """Saturate-clip a float in [-1, 1] to Q1.15."""
    q = int(round(x * (1 << 15)))
    return max(-(1 << 15), min((1 << 15) - 1, q))


def _python_reference(samples_re, samples_im, *, sps_float=None,
                      half_sps_float=None):
    """Pure-Python port of the timing+interp loop, used as the
    floating-point reference for the HDL test.

    Drives the same FIFO + sample_point logic as the HDL but in
    floats, so the test can compare HDL fixed-point output against
    a clean reference without depending on the Rust pipeline (which
    has additional AGC/PLL/Gardner stages mixed in).

    Returns a list of `(i_mid, q_mid, i_cur, q_cur)` tuples, one
    per symbol decision, in float -- the HDL test quantises and
    compares.
    """
    if sps_float is None:
        sps_float = P25_LSM_SAMPLE_RATE_HZ / P25_SYMBOL_RATE_HZ
    if half_sps_float is None:
        half_sps_float = sps_float / 2.0

    # 8-deep FIFO matching the HDL default. Index 0 = newest sample.
    fifo_depth = 8
    BP = fifo_depth - 3  # 5
    fifo_re = [0.0] * fifo_depth
    fifo_im = [0.0] * fifo_depth

    # Bump initial sample_point by (BP + 2) periods to match the
    # HDL's warmup-offset init. See the comment in
    # `LsmTimingInterp.elaborate` for the rationale -- this delays
    # the first decision until the pre-shift FIFO has accumulated
    # enough samples to match Rust's pre-loaded-buffer access.
    sample_point = sps_float + (BP + 2)
    out = []
    for re, im in zip(samples_re, samples_im):
        # Decrement first, then check.
        sp_dec = sample_point - 1.0
        if sp_dec >= 1.0:
            # Just shift and wait.
            for j in range(fifo_depth - 1, 0, -1):
                fifo_re[j] = fifo_re[j - 1]
                fifo_im[j] = fifo_im[j - 1]
            fifo_re[0] = re
            fifo_im[0] = im
            sample_point = sp_dec
            continue

        # Decision fires THIS step. The HDL computes lerps against
        # the *pre-shift* FIFO, so the Python reference must do the
        # same: lerp first, then shift, then add sps.
        mu_mid = sp_dec  # already in [0, 1)
        i_mid = fifo_re[BP] + (fifo_re[BP - 1] - fifo_re[BP]) * mu_mid
        q_mid = fifo_im[BP] + (fifo_im[BP - 1] - fifo_im[BP]) * mu_mid

        ptr = sp_dec + half_sps_float
        cur_int = int(ptr)
        cur_frac = ptr - cur_int
        a_re = fifo_re[BP - cur_int]
        b_re = fifo_re[BP - cur_int - 1]
        a_im = fifo_im[BP - cur_int]
        b_im = fifo_im[BP - cur_int - 1]
        i_cur = a_re + (b_re - a_re) * cur_frac
        q_cur = a_im + (b_im - a_im) * cur_frac

        out.append((i_mid, q_mid, i_cur, q_cur))

        # Now shift in the new sample.
        for j in range(fifo_depth - 1, 0, -1):
            fifo_re[j] = fifo_re[j - 1]
            fifo_im[j] = fifo_im[j - 1]
        fifo_re[0] = re
        fifo_im[0] = im
        sample_point = sp_dec + sps_float

    return out


class TestLsmTimingInterp(unittest.TestCase):

    def _simulate(self, dut, bench, *, vcd=None):
        sim = Simulator(dut)
        sim.add_clock(16e-9)  # 62.5 MHz
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    def test_constants(self):
        """Q4.12 constants match the float SPS = 31250/4800.

        Tolerance places=3 (1e-3 absolute) is comfortably above the
        Q4.12 ULP of 2.4e-4 -- a tighter check would be sub-ULP and
        would fail on harmless rounding.
        """
        sps_float = P25_LSM_SAMPLE_RATE_HZ / P25_SYMBOL_RATE_HZ
        self.assertAlmostEqual(SPS_Q12 / ONE_Q12, sps_float, places=3)
        self.assertAlmostEqual(HALF_SPS_Q12 / ONE_Q12, sps_float / 2.0, places=3)

    def test_decision_rate(self):
        """Decisions fire at ~1 per sps input strobes (4800 / 31250 ≈ 0.154).

        Drive a long stream of constant inputs (lerp values are
        irrelevant for this test, only the decision count matters).

        With the Phase 6E.6e 2-stage pipeline, decision_strobe
        fires 2 cycles after the input strobe that triggered the
        decision. We add an extra `await ctx.tick()` after the
        input strobe to drain the stage 1 -> stage 2 latch and
        catch decision_strobe at the cycle the lerp result is
        latched into the output flop.
        """
        dut = LsmTimingInterp()
        n_in = 1000
        decision_count = 0

        async def bench(ctx):
            nonlocal decision_count
            for i in range(n_in):
                ctx.set(dut.re_in, 1000)
                ctx.set(dut.im_in, -500)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                # Stage 2 fires the cycle AFTER strobe_in for a
                # decision input -- drain one extra cycle.
                await ctx.tick()
                if ctx.get(dut.decision_strobe):
                    decision_count += 1

        self._simulate(dut, bench)

        # Expected: ~ n_in * 4800 / 31250 = ~153.6 decisions
        # Allow ±2 since the very first few samples are absorbed
        # by sample_point initialisation.
        sps_float = P25_LSM_SAMPLE_RATE_HZ / P25_SYMBOL_RATE_HZ
        expected = int(round(n_in / sps_float))
        self.assertGreaterEqual(decision_count, expected - 2)
        self.assertLessEqual(decision_count, expected + 2)

    def test_constant_input_returns_constant(self):
        """Constant DC input -> all four lerps emit the same constant.

        The lerp `a + (b - a) * mu` reduces to `a` when a == b, so
        a constant DC stream MUST produce identical output samples.
        Catches a regression that swaps mu polarity or wires the
        lerp inputs incorrectly.
        """
        dut = LsmTimingInterp()
        const_re = 5000
        const_im = -3000
        decisions = []

        async def bench(ctx):
            for i in range(200):
                ctx.set(dut.re_in, const_re)
                ctx.set(dut.im_in, const_im)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                # Drain the 2-stage pipeline (Phase 6E.6e fix).
                await ctx.tick()
                if ctx.get(dut.decision_strobe):
                    decisions.append((
                        ctx.get(dut.i_mid_out),
                        ctx.get(dut.q_mid_out),
                        ctx.get(dut.i_cur_out),
                        ctx.get(dut.q_cur_out),
                    ))

        self._simulate(dut, bench)

        # Allow a transient region where the FIFO has not yet been
        # filled with the constant -- the early decisions see some
        # zero-padded history.
        # FIFO depth 8, so after ~8 input strobes the FIFO is fully
        # constant. Decision rate is ~1/6.5, so by decision index
        # 2 the FIFO is settled.
        for i, (im_, qm, ic, qc) in enumerate(decisions[3:]):
            self.assertEqual(
                (im_, qm, ic, qc),
                (const_re, const_im, const_re, const_im),
                f"decision {i + 3}: lerp of constant DC must equal the constant")

    def test_lerp_against_python_reference_ramp(self):
        """Drive an integer ramp and compare HDL output against the
        pure-Python lerp reference, sample-by-sample.

        Tolerance: 2 Q15 ULPs per output to absorb the round-toward-
        zero of the HDL's truncating right shift vs Python's
        floating-point lerp. A clean port lands well within 1 ULP.
        """
        dut = LsmTimingInterp()

        n_in = 200
        # Use a ramp scaled into Q15 so the lerps stay inside the
        # representable range. Step 50 keeps |re| <= ~10000 over
        # the full 200-sample stream.
        in_re = [(i - n_in // 2) * 50 for i in range(n_in)]
        in_im = [(n_in // 2 - i) * 30 for i in range(n_in)]

        # Float reference (treats inputs as the same integer values
        # so the lerp produces float results that we then quantise
        # the same way the HDL does at the end of its lerp).
        ref = _python_reference(
            [float(x) for x in in_re],
            [float(x) for x in in_im],
        )

        decisions = []

        async def bench(ctx):
            for i in range(n_in):
                ctx.set(dut.re_in, in_re[i])
                ctx.set(dut.im_in, in_im[i])
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                # Drain the 2-stage pipeline (Phase 6E.6e fix).
                await ctx.tick()
                if ctx.get(dut.decision_strobe):
                    decisions.append((
                        ctx.get(dut.i_mid_out),
                        ctx.get(dut.q_mid_out),
                        ctx.get(dut.i_cur_out),
                        ctx.get(dut.q_cur_out),
                    ))

        self._simulate(dut, bench)

        # Skip the first 2 decisions for FIFO transient (zero history).
        skip = 2
        self.assertEqual(
            len(decisions), len(ref),
            f"HDL fired {len(decisions)} decisions, Python expected {len(ref)}")
        for i in range(skip, len(decisions)):
            hdl = decisions[i]
            py = ref[i]
            for j, (got, want) in enumerate(zip(hdl, py)):
                # Round Python ref through Q15 truncation to match
                # HDL `>> 12` (truncate-toward-negative-infinity for
                # signed values, just like Verilog `>>>`).
                # Python's `int()` truncates toward zero, so for the
                # comparison we'll use a 2-ULP slop instead of trying
                # to mimic the exact rounding mode -- we just want
                # to catch off-by-many-ULPs bugs.
                self.assertLessEqual(
                    abs(got - int(round(want))), 2,
                    f"decision {i} field {j}: HDL {got} vs ref {want:.4f}")


if __name__ == '__main__':
    unittest.main()
