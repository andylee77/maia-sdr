#
# Fishball P25 -- LSM FIR HDL tests (LPF + RRC)
#
# Phase 6E.2 / 6E.3 of the LSM HDL port. Drives the generic
# `LsmFir` module with the frozen LPF and RRC tap arrays from the
# Rust reference, then verifies it against the corresponding
# golden vectors emitted by `lsm::golden_dump`.
#
# Tolerance model
# ---------------
# Unlike the decimator test, the FIR is not bit-exact against the
# Rust f32 pipeline -- the Rust reference computes in f32 while the
# HDL computes in fixed-point Q1.15 / Q1.17. The two will agree to
# within the quantisation noise floor but not exactly. The test
# uses an absolute tolerance of `2 ULPs of Q15` (i.e. <= 2 / 32768
# absolute = 6.1e-5) which is comfortably above the worst-case
# rounding error from quantising 18-bit coefficients and a 16-bit
# input through an 83-tap (LPF) or 105-tap (RRC) sum.
#
# A ramp-up region of `n_taps` samples at the start of the output
# is excluded from the comparison because the Rust reference
# `apply_real_fir_complex` and the HDL shift-register both treat
# pre-input history as zero, but the difference between
# `<short partial sum> @ f32` and `<short partial sum> @ Q15` can
# briefly exceed the steady-state tolerance during the convolution
# transient.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_fir import LsmFir, LPF_TAPS_31250, RRC_TAPS_31250

from .golden_vector_loader import load_iq_stage, to_fixed


# Q15 ULP -- one bit of the 16-bit signed output Q-format.
Q15_ULP = 1.0 / (1 << 15)
# Number of Q15 ULPs the FIR test will tolerate per output sample.
# Empirical: a clean fixed-point FIR with Q1.17 coeffs and Q1.15
# input over ~100 taps lands well within 1 ULP, so 4 gives plenty
# of safety while still catching meaningful regressions.
TOLERANCE_ULPS = 4


def _drive_fir_against_golden(test, dut, stage):
    """Helper: clock through ``dut`` driven by ``stage`` (an IQStage),
    collect output samples, and assert they agree with the golden
    output to within ``TOLERANCE_ULPS`` Q15 ULPs after the transient.
    """
    in_re_q = to_fixed(stage.input_re, frac_bits=15, width=16)
    in_im_q = to_fixed(stage.input_im, frac_bits=15, width=16)
    out_re_q_expected = to_fixed(stage.output_re, frac_bits=15, width=16)
    out_im_q_expected = to_fixed(stage.output_im, frac_bits=15, width=16)

    n_in = stage.n_input
    n_taps = dut.n_taps

    # Cycles needed per FIR output: 1 cycle for shift+kick, N cycles
    # of MAC, 1 cycle for the final saturate+latch. Add some slack.
    cycles_per_output = n_taps + 4

    hdl_outputs = []

    async def bench(ctx):
        for i in range(n_in):
            ctx.set(dut.re_in, in_re_q[i])
            ctx.set(dut.im_in, in_im_q[i])
            ctx.set(dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.strobe_in, 0)
            # Wait long enough for the MAC sequence to finish.
            for _ in range(cycles_per_output):
                await ctx.tick()
                if ctx.get(dut.strobe_out):
                    hdl_outputs.append((
                        ctx.get(dut.re_out),
                        ctx.get(dut.im_out),
                    ))

    sim = Simulator(dut)
    sim.add_clock(16e-9)  # 62.5 MHz
    sim.add_testbench(bench)
    sim.run()

    test.assertEqual(
        len(hdl_outputs), n_in,
        f"FIR produced {len(hdl_outputs)} outputs, expected {n_in}")

    # Skip the transient region: the first n_taps outputs see the
    # zero-padded history and are dominated by partial sums whose
    # Q15 vs f32 rounding can briefly exceed the steady-state
    # tolerance. After the transient, the comparison must hold.
    skip = n_taps
    max_err_re = 0
    max_err_im = 0
    fail_count = 0
    fail_first = None
    for i in range(skip, n_in):
        got_re, got_im = hdl_outputs[i]
        exp_re = out_re_q_expected[i]
        exp_im = out_im_q_expected[i]
        err_re = abs(got_re - exp_re)
        err_im = abs(got_im - exp_im)
        max_err_re = max(max_err_re, err_re)
        max_err_im = max(max_err_im, err_im)
        if err_re > TOLERANCE_ULPS or err_im > TOLERANCE_ULPS:
            fail_count += 1
            if fail_first is None:
                fail_first = (i, (got_re, got_im), (exp_re, exp_im),
                              (err_re, err_im))

    test.assertEqual(
        fail_count, 0,
        f"{fail_count} FIR outputs exceeded {TOLERANCE_ULPS}-ULP tolerance "
        f"(max err re={max_err_re}, im={max_err_im}); "
        f"first failure: {fail_first}")


class TestLsmFir(unittest.TestCase):

    def test_lpf_dc_gain_quantised(self):
        """The LPF DC gain (sum of taps) at Q1.17 should still measure ~0.99.

        Quick smoke test on the tap quantisation: a regression that
        flipped a sign bit or scaled wrong would shift the integer
        sum by orders of magnitude.
        """
        dut = LsmFir(LPF_TAPS_31250)
        # Sum of quantised coefficients, divided by 2^17, should be
        # very close to the float sum (0.9899).
        int_sum = sum(dut.taps_q)
        float_dc_gain = int_sum / (1 << dut.coeff_frac_bits)
        self.assertAlmostEqual(float_dc_gain, 0.9899, places=3)
        self.assertEqual(len(dut.taps_q), 83)

    def test_rrc_taps_quantised(self):
        """RRC tap count + center value sanity check after quantisation."""
        dut = LsmFir(RRC_TAPS_31250)
        self.assertEqual(len(dut.taps_q), 105)
        # Center tap should be the largest in absolute value.
        center = dut.taps_q[52]
        max_abs = max(abs(t) for t in dut.taps_q)
        self.assertEqual(abs(center), max_abs)

    def test_lpf_impulse_response_matches_quantised_taps(self):
        """Drive a Kronecker delta + zeros and confirm the output samples
        reproduce the quantised tap values one-by-one (the impulse response
        of any FIR == the tap array)."""
        dut = LsmFir(LPF_TAPS_31250)

        n_taps = dut.n_taps
        cycles_per_output = n_taps + 4
        out_samples = []

        async def bench(ctx):
            for i in range(2 * n_taps):
                ctx.set(dut.re_in, (1 << 15) - 1 if i == 0 else 0)
                ctx.set(dut.im_in, 0)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                for _ in range(cycles_per_output):
                    await ctx.tick()
                    if ctx.get(dut.strobe_out):
                        out_samples.append(ctx.get(dut.re_out))

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        # Outputs should match (input × tap) >> shift for each tap.
        # Input was Q15 max-positive (32767), so the product ladder is:
        #   product = tap_int * 32767  (signed 16x18 -> 33-bit)
        #   y[k]    = product >> (input_frac + coeff_frac - output_frac)
        # which for the default Q1.15 in / Q1.17 coeff / Q1.15 out is
        # `>> 17`. With a 32767 (≈ 1.0) input, this is ~`tap_int / 4`.
        # Allow ±2 ULP for rounding through the truncating shift.
        impulse_in = (1 << 15) - 1
        for k in range(n_taps):
            expected = (dut.taps_q[k] * impulse_in) >> dut.shift
            got = out_samples[k]
            self.assertLessEqual(
                abs(got - expected), 2,
                f"impulse response sample {k}: got {got}, expected ~{expected}")

    def test_lpf_golden_vector_31250(self):
        """End-to-end LPF test against the Rust f32 reference output.

        Loads `lpf_31250.json` (2048 sweep samples in/out at 31.25 kSPS),
        drives the HDL FIR with the LPF taps, and asserts every steady-
        state output sample lies within `TOLERANCE_ULPS` of the
        f32 reference.
        """
        stage = load_iq_stage('lpf_31250')
        self.assertEqual(stage.n_input, 2048)
        self.assertEqual(stage.n_output, 2048)
        dut = LsmFir(LPF_TAPS_31250)
        _drive_fir_against_golden(self, dut, stage)

    def test_rrc_impulse_response_matches_quantised_taps(self):
        """RRC impulse response: same shape test as the LPF, different
        kernel. Catches a regression in tap quantisation that only
        affects the larger / more numerous RRC coefficients.
        """
        dut = LsmFir(RRC_TAPS_31250)
        n_taps = dut.n_taps
        cycles_per_output = n_taps + 4
        out_samples = []

        async def bench(ctx):
            for i in range(2 * n_taps):
                ctx.set(dut.re_in, (1 << 15) - 1 if i == 0 else 0)
                ctx.set(dut.im_in, 0)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                for _ in range(cycles_per_output):
                    await ctx.tick()
                    if ctx.get(dut.strobe_out):
                        out_samples.append(ctx.get(dut.re_out))

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        impulse_in = (1 << 15) - 1
        for k in range(n_taps):
            expected = (dut.taps_q[k] * impulse_in) >> dut.shift
            got = out_samples[k]
            self.assertLessEqual(
                abs(got - expected), 2,
                f"RRC impulse response sample {k}: got {got}, expected ~{expected}")

    def test_rrc_golden_vector_31250(self):
        """End-to-end RRC test against the Rust f32 reference output.

        Loads `rrc_31250.json` (2048 sweep samples in/out at 31.25 kSPS),
        drives the HDL FIR with the 105-tap RRC matched filter taps,
        and asserts every steady-state output sample lies within
        `TOLERANCE_ULPS` of the f32 reference. Same fixture shape as
        the LPF test, but with 25% more taps and a non-monotone tap
        envelope -- a tap-quantisation bug specific to the RRC's
        large-magnitude lobes would slip past the LPF test.
        """
        stage = load_iq_stage('rrc_31250')
        self.assertEqual(stage.n_input, 2048)
        self.assertEqual(stage.n_output, 2048)
        dut = LsmFir(RRC_TAPS_31250)
        _drive_fir_against_golden(self, dut, stage)


if __name__ == '__main__':
    unittest.main()
