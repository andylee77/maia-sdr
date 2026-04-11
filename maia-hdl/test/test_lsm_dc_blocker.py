#
# Fishball P25 -- LSM DC blocker HDL tests
#
# Phase 6G.1 of the LSM HDL port. Three deterministic tests for
# `LsmDcBlocker`:
#
#   1. Step response: feed a constant DC bias and verify the
#      output decays toward zero with the expected time constant.
#   2. Passband: feed a 1 kHz sinusoid and verify the steady-state
#      output amplitude is essentially the input amplitude (no
#      significant attenuation in the LSM signal band).
#   3. Bypass: assert `enable_in=0`, feed a DC bias, verify the
#      output passes through unchanged on every cycle.
#
# Plus a fourth sanity test that the strobe_out follows strobe_in
# by exactly one tick (the convention `LsmDemod` will rely on).
#
# All tests run against the production K=7 / signed-16 default,
# which is what `LsmDemod` instantiates. The Python reference
# inside the tests uses the same shift-and-add update so the
# comparison is bit-exact (no fixed-point tolerance needed for
# the step response; the passband test uses a small sinusoid
# tolerance because of the sat/truncate boundary).
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_dc_blocker import LsmDcBlocker, DEFAULT_ALPHA_SHIFT


# 31.25 kSPS, the LSM IQ rate post-decimator. Used by the passband
# sinusoid test to convert frequency to per-sample phase.
LSM_IQ_RATE_HZ = 31_250


def _python_dc_block_step(acc, x, K, width):
    """One step of the bit-exact Python reference matching the HDL.

    Mirrors the HDL's update on input strobes:
        diff = (x << K) - acc
        acc <- acc + (diff arith-shift K)
        dc  = acc arith-shift K          (truncate)
        y_wide = x - dc
        y = saturate(y_wide, signed `width`)
    Returns (acc_next, y).
    """
    x_ext = x << K
    diff = x_ext - acc
    # Python `>>` on negative ints is arithmetic by definition.
    acc_next = acc + (diff >> K)
    dc = acc >> K  # use the OLD acc, matching HDL combinational dc
    y_wide = x - dc
    sat_lo = -(1 << (width - 1))
    sat_hi = (1 << (width - 1)) - 1
    y = max(sat_lo, min(sat_hi, y_wide))
    return acc_next, y


class TestLsmDcBlocker(unittest.TestCase):
    """LsmDcBlocker -- step / passband / bypass / strobe."""

    def _simulate(self, dut, bench, *, vcd=None):
        sim = Simulator(dut)
        sim.add_clock(16e-9)  # 62.5 MHz sync clock, matches p25_top
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    # ──────────────────────────────────────────────────────────────
    # Test 1: step response (constant DC -> output decays to ~0)
    # ──────────────────────────────────────────────────────────────
    def test_step_response_decays_and_matches_reference(self):
        """Feed a constant DC level; HDL must match the Python reference
        bit-exactly and the residual must decay to a tiny fraction of the
        input within a handful of time constants."""
        K = DEFAULT_ALPHA_SHIFT
        W = 16
        dut = LsmDcBlocker(width=W, alpha_shift=K)

        # ~10% of full scale, well clear of saturation.
        dc_level = 3000
        # Time constant is 2^K = 128 samples. 16 tau = 2048 samples
        # is far past convergence; the integer truncation residual
        # locks well below 0.1% of dc_level by then.
        n_samples = 2048

        # Build the bit-exact Python reference trajectory in
        # parallel for the HDL comparison. This mirrors the HDL's
        # registered y_out: y_out[n] reflects the y_next computed
        # from the (acc, x) values active *during* sample n's
        # input cycle.
        ref_outputs = []
        acc = 0
        for _ in range(n_samples):
            acc, y = _python_dc_block_step(acc, dc_level, K, W)
            ref_outputs.append(y)

        outputs = []

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            for _ in range(n_samples):
                ctx.set(dut.x_in, dc_level)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                outputs.append(ctx.get(dut.y_out))
                await ctx.tick()

        self._simulate(dut, bench)

        # 1) HDL is bit-exact against the Python reference --
        #    catches any sign-extension / shift / saturator bug.
        self.assertEqual(
            outputs, ref_outputs,
            f"HDL diverged from reference: "
            f"first mismatch at sample "
            f"{next((i for i, (a, b) in enumerate(zip(outputs, ref_outputs)) if a != b), None)}")

        # 2) First sample is the full DC level (dc estimate is
        #    still 0 on the first strobe).
        self.assertEqual(outputs[0], dc_level)

        # 3) Decay is monotone-ish: midpoint is well below dc_level
        #    but not yet at the floor.
        midpoint = outputs[n_samples // 4]
        self.assertLess(abs(midpoint), dc_level // 2)
        self.assertGreater(abs(midpoint), 0)

        # 4) Final residual is a tiny fraction of the input -- the
        #    integer truncation floor for K=7 sits within a handful
        #    of LSBs, *much* smaller than the input level. We
        #    require <0.5% of dc_level (= 15 for dc_level=3000)
        #    which gives plenty of slack over the actual ~1-2 LSB
        #    floor while still proving the DC has been removed.
        residual = outputs[-1]
        self.assertLess(
            abs(residual), max(4, dc_level // 200),
            f"Final residual {residual} too large; "
            f"expected << dc_level={dc_level}")

    # ──────────────────────────────────────────────────────────────
    # Test 2: passband (1 kHz sinusoid passes through ~unattenuated)
    # ──────────────────────────────────────────────────────────────
    def test_passband_1khz_sinusoid_unattenuated(self):
        """A 1 kHz tone should be ~unaffected at fs = 31.25 kSPS, K=7."""
        dut = LsmDcBlocker(width=16, alpha_shift=DEFAULT_ALPHA_SHIFT)

        freq_hz = 1000.0
        amp = 8000  # ~25% of full scale, well clear of saturation
        # Run long enough to let the integrator settle past the
        # initial transient (8 tau ~= 1024 samples) and then
        # measure the next ~4 cycles of the sinusoid for amplitude.
        n_warmup = 1024
        n_measure = int(round(LSM_IQ_RATE_HZ / freq_hz)) * 4
        n_total = n_warmup + n_measure

        outputs = []

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            for n in range(n_total):
                phase = 2.0 * math.pi * freq_hz * n / LSM_IQ_RATE_HZ
                x = int(round(amp * math.sin(phase)))
                ctx.set(dut.x_in, x)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                outputs.append(ctx.get(dut.y_out))
                await ctx.tick()

        self._simulate(dut, bench)

        measured = outputs[n_warmup:]
        peak = max(abs(s) for s in measured)
        # Theoretical magnitude response of the one-pole leaky
        # integrator at 1 kHz / 31.25 kSPS / alpha = 1 - 2^-7 is
        # extremely close to unity (within ~0.5%). The IIR has a
        # mild peaking shoulder just above the cutoff that can
        # push the gain very slightly above 1; on top of that the
        # fixed-point and the non-integer samples-per-period add
        # a few LSBs of jitter on the peak measurement. We assert
        # the gain is between 0.95 and 1.05 of the input -- tight
        # enough to catch a real attenuation or runaway-amp bug,
        # loose enough to ride out the IIR shoulder + integer
        # arithmetic.
        self.assertGreater(
            peak, int(0.95 * amp),
            f"Peak {peak} fell too far below input amplitude {amp}")
        self.assertLess(
            peak, int(1.05 * amp),
            f"Peak {peak} ran too far above input amplitude {amp}")

    # ──────────────────────────────────────────────────────────────
    # Test 3: bypass mode (output == input on every cycle)
    # ──────────────────────────────────────────────────────────────
    def test_bypass_passes_dc_through(self):
        """With enable_in=0, output must equal input even with DC bias."""
        dut = LsmDcBlocker(width=16, alpha_shift=DEFAULT_ALPHA_SHIFT)

        dc_level = 5000
        n_samples = 256

        outputs = []

        async def bench(ctx):
            ctx.set(dut.enable_in, 0)  # bypass
            for _ in range(n_samples):
                ctx.set(dut.x_in, dc_level)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                outputs.append(ctx.get(dut.y_out))
                await ctx.tick()

        self._simulate(dut, bench)

        # Every single sample must be the input verbatim.
        self.assertTrue(
            all(s == dc_level for s in outputs),
            f"Bypass leaked: outputs[:8]={outputs[:8]}, "
            f"expected all == {dc_level}")

    # ──────────────────────────────────────────────────────────────
    # Test 4: strobe convention (registered, +1 tick)
    # ──────────────────────────────────────────────────────────────
    def test_strobe_out_lockstep_with_strobe_in(self):
        """strobe_out and strobe_in arrive together, registered (matches LsmDecimator2)."""
        dut = LsmDcBlocker(width=16, alpha_shift=DEFAULT_ALPHA_SHIFT)

        # Pattern: strobe high for 3 cycles, low for 5 cycles.
        pattern = [1, 1, 1, 0, 0, 0, 0, 0]
        observed_strobe_outs = []

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.x_in, 0)
            for s in pattern:
                ctx.set(dut.strobe_in, s)
                await ctx.tick()
                observed_strobe_outs.append(ctx.get(dut.strobe_out))

        self._simulate(dut, bench)

        # Lockstep convention (matches LsmDecimator2): the
        # registered strobe_out reflects the strobe_in present
        # before the tick. Reading after `await ctx.tick()` sees
        # exactly the input pattern.
        self.assertEqual(observed_strobe_outs, pattern)


if __name__ == '__main__':
    unittest.main()
