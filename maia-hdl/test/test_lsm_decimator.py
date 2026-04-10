#
# Fishball P25 -- LSM /2 decimator HDL tests
#
# Phase 6E.1 of the LSM HDL port. Drives `LsmDecimator2` with the
# `decimator_62k5_to_31k25` golden vector emitted by the Rust port
# (`p25-httpd/src/lsm/golden_dump.rs`) and asserts the HDL output
# matches the Rust output sample-for-sample.
#
# The decimator is a no-DSP block (just a 1-bit phase counter), so
# the comparison is exact -- no fixed-point tolerance needed. The
# input/output values are quantised to Q15 (16-bit signed) for HDL
# drive, with the same quantiser applied to the expected output, so
# both sides see the same rounded integers.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_decimator import LsmDecimator2

from .golden_vector_loader import load_iq_stage, to_fixed


class TestLsmDecimator2(unittest.TestCase):
    """Drive the /2 decimator HDL with golden-vector IQ from the Rust port."""

    def _simulate(self, bench, *, vcd=None):
        sim = Simulator(self.dut)
        sim.add_clock(16e-9)  # 62.5 MHz sync clock, matches p25_top
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    def test_basic_alternating_pattern(self):
        """Sanity: feed [0,1,2,3,4,5,...] and confirm we emit [0,2,4,...]."""
        self.dut = LsmDecimator2(width=16)

        n_in = 20
        outputs = []

        async def bench(ctx):
            for i in range(n_in):
                ctx.set(self.dut.re_in, i)
                ctx.set(self.dut.im_in, -i)
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(self.dut.strobe_in, 0)
                # Strobe_out is registered, so it appears one cycle
                # *after* the strobe_in cycle (latched on the same
                # tick as re_out/im_out).
                if ctx.get(self.dut.strobe_out):
                    outputs.append((
                        ctx.get(self.dut.re_out),
                        ctx.get(self.dut.im_out),
                    ))

        self._simulate(bench)

        expected = [(2 * k, -(2 * k)) for k in range(n_in // 2)]
        self.assertEqual(outputs, expected)

    def test_no_strobe_no_output(self):
        """No output strobe when input strobe is held low."""
        self.dut = LsmDecimator2(width=16)
        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            ctx.set(self.dut.strobe_in, 0)
            for _ in range(40):
                ctx.set(self.dut.re_in, 12345)
                ctx.set(self.dut.im_in, -6789)
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    strobe_count += 1

        self._simulate(bench)
        self.assertEqual(strobe_count, 0)

    def test_first_sample_emitted(self):
        """First strobe after reset must emit the first input sample.

        Matches the Rust default ``StreamingDecimator2::new() {skip:0}``
        and the batch helper ``decimate_by_2`` which takes
        ``input[0::2]``. A regression that swapped the phase polarity
        would silently shift the entire downstream sample stream by
        one input sample -- catching that here is the whole point of
        this test.
        """
        self.dut = LsmDecimator2(width=16)
        outputs = []

        async def bench(ctx):
            for i, val in enumerate([1000, 2000, 3000, 4000]):
                ctx.set(self.dut.re_in, val)
                ctx.set(self.dut.im_in, val + 1)
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(self.dut.strobe_in, 0)
                if ctx.get(self.dut.strobe_out):
                    outputs.append((
                        ctx.get(self.dut.re_out),
                        ctx.get(self.dut.im_out),
                    ))

        self._simulate(bench)
        # Inputs were 1000, 2000, 3000, 4000 -- evens (0,2) -> 1000, 3000.
        self.assertEqual(outputs, [(1000, 1001), (3000, 3001)])

    def test_golden_vector_decimator_62k5_to_31k25(self):
        """End-to-end golden vector test against the Rust reference.

        Loads `decimator_62k5_to_31k25.json` (4096 sweep samples in,
        2048 out at 31.25 kSPS), quantises to Q15, drives the HDL,
        and asserts the HDL output equals the (quantised) Rust output
        bit-for-bit.

        Q15 is enough headroom for the sweep input which lives in
        [-1, 1] by construction; the conversion is the same on both
        sides so any rounding decision is identical.
        """
        stage = load_iq_stage('decimator_62k5_to_31k25')
        self.assertEqual(stage.n_input, 4096)
        self.assertEqual(stage.n_output, 2048)

        in_re_q = to_fixed(stage.input_re, frac_bits=15, width=16)
        in_im_q = to_fixed(stage.input_im, frac_bits=15, width=16)
        out_re_q = to_fixed(stage.output_re, frac_bits=15, width=16)
        out_im_q = to_fixed(stage.output_im, frac_bits=15, width=16)

        self.dut = LsmDecimator2(width=16)

        hdl_outputs = []

        async def bench(ctx):
            for i in range(stage.n_input):
                ctx.set(self.dut.re_in, in_re_q[i])
                ctx.set(self.dut.im_in, in_im_q[i])
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(self.dut.strobe_in, 0)
                if ctx.get(self.dut.strobe_out):
                    hdl_outputs.append((
                        ctx.get(self.dut.re_out),
                        ctx.get(self.dut.im_out),
                    ))
            # Drain one extra cycle in case the last strobe_out is
            # still on its registered edge.
            await ctx.tick()
            if ctx.get(self.dut.strobe_out):
                hdl_outputs.append((
                    ctx.get(self.dut.re_out),
                    ctx.get(self.dut.im_out),
                ))

        self._simulate(bench)

        self.assertEqual(
            len(hdl_outputs), stage.n_output,
            f"HDL produced {len(hdl_outputs)} samples, expected {stage.n_output}")

        # Bit-exact comparison: the quantised goldens go through a
        # block that does no arithmetic, so equality is the right
        # check (a tolerance here would mask a phase bug).
        for i, (got, expected) in enumerate(zip(
                hdl_outputs,
                list(zip(out_re_q, out_im_q)))):
            self.assertEqual(
                got, expected,
                f"sample {i}: HDL {got} != golden {expected}")


if __name__ == '__main__':
    unittest.main()
