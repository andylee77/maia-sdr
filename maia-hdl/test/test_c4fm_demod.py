#
# Fishball P25 - C4FM Demodulator tests
#
# SPDX-License-Identifier: MIT
#

import unittest

import numpy as np

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.c4fm_demod import C4FMDemod


class TestC4FMDemod(unittest.TestCase):
    """Test C4FM FM discriminator with synthetic IQ signals."""

    def _simulate(self, bench, *, vcd=None):
        sim = Simulator(self.dut)
        sim.add_clock(12e-9)
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    def test_dc_input_zero_output(self):
        """Constant IQ -> zero frequency -> disc_out ~= 0."""
        self.dut = C4FMDemod()

        outputs = []

        async def bench(ctx):
            # Feed constant IQ (re=1000, im=0) for 20 cycles
            for i in range(30):
                ctx.set(self.dut.re_in, 1000)
                ctx.set(self.dut.im_in, 0)
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    outputs.append(ctx.get(self.dut.disc_out))

        self._simulate(bench)
        # After pipeline flush, all outputs should be 0 (no frequency)
        for val in outputs[2:]:
            self.assertEqual(val, 0, f"DC input should give zero output, got {val}")

    def test_positive_frequency(self):
        """Positive frequency rotation -> consistent positive disc output."""
        self.dut = C4FMDemod()

        # Generate IQ with known positive frequency
        # f = 0.05 cycles/sample (positive rotation)
        n_samples = 50
        freq = 0.05
        amplitude = 10000
        t = np.arange(n_samples)
        re = np.round(amplitude * np.cos(2 * np.pi * freq * t)).astype(int)
        im = np.round(amplitude * np.sin(2 * np.pi * freq * t)).astype(int)

        outputs = []

        async def bench(ctx):
            for i in range(n_samples):
                ctx.set(self.dut.re_in, int(re[i]))
                ctx.set(self.dut.im_in, int(im[i]))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    outputs.append(ctx.get(self.dut.disc_out))

        self._simulate(bench)
        # After pipeline warmup, outputs should be consistently positive
        steady = outputs[4:]
        self.assertGreater(len(steady), 10)
        for val in steady:
            self.assertGreater(val, 0,
                               "Positive frequency should give positive output")

    def test_negative_frequency(self):
        """Negative frequency rotation -> consistent negative disc output."""
        self.dut = C4FMDemod()

        n_samples = 50
        freq = -0.05
        amplitude = 10000
        t = np.arange(n_samples)
        re = np.round(amplitude * np.cos(2 * np.pi * freq * t)).astype(int)
        im = np.round(amplitude * np.sin(2 * np.pi * freq * t)).astype(int)

        outputs = []

        async def bench(ctx):
            for i in range(n_samples):
                ctx.set(self.dut.re_in, int(re[i]))
                ctx.set(self.dut.im_in, int(im[i]))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    outputs.append(ctx.get(self.dut.disc_out))

        self._simulate(bench)
        steady = outputs[4:]
        self.assertGreater(len(steady), 10)
        for val in steady:
            self.assertLess(val, 0,
                            "Negative frequency should give negative output")

    def test_four_levels(self):
        """Four C4FM deviation levels produce four distinct output levels."""
        self.dut = C4FMDemod()

        # P25 C4FM at 48 kSPS: deviations of ±600 Hz and ±1800 Hz
        # Normalized: ±600/48000 = ±0.0125, ±1800/48000 = ±0.0375
        fs = 48000
        deviations = [-1800, -600, 600, 1800]
        amplitude = 10000
        samples_per_level = 40  # enough to stabilize

        iq_re = []
        iq_im = []
        phase = 0.0
        for dev in deviations:
            freq_norm = dev / fs
            for _ in range(samples_per_level):
                phase += 2 * np.pi * freq_norm
                iq_re.append(int(round(amplitude * np.cos(phase))))
                iq_im.append(int(round(amplitude * np.sin(phase))))

        outputs = []
        level_outputs = {d: [] for d in deviations}

        async def bench(ctx):
            for i in range(len(iq_re)):
                ctx.set(self.dut.re_in, iq_re[i])
                ctx.set(self.dut.im_in, iq_im[i])
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    outputs.append((i, ctx.get(self.dut.disc_out)))

        self._simulate(bench)

        # Collect outputs for each deviation level (skip transitions)
        for idx, val in outputs:
            level_idx = idx // samples_per_level
            sample_in_level = idx % samples_per_level
            if level_idx < 4 and sample_in_level >= 10:  # skip transient
                level_outputs[deviations[level_idx]].append(val)

        # Verify ordering: -1800 < -600 < 600 < 1800
        means = {}
        for dev in deviations:
            if level_outputs[dev]:
                means[dev] = np.mean(level_outputs[dev])

        if len(means) == 4:
            self.assertLess(means[-1800], means[-600])
            self.assertLess(means[-600], means[600])
            self.assertLess(means[600], means[1800])

    def test_strobe_gating(self):
        """No output strobe when input strobe is deasserted."""
        self.dut = C4FMDemod()

        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for i in range(20):
                ctx.set(self.dut.re_in, 1000)
                ctx.set(self.dut.im_in, 500)
                ctx.set(self.dut.strobe_in, 0)  # strobe off
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    strobe_count += 1

        self._simulate(bench)
        self.assertEqual(strobe_count, 0, "No output when strobe_in is low")

    def test_diff_re_dc(self):
        """Constant IQ -> diff_re ~= |z|^2 (no rotation, full real part)."""
        self.dut = C4FMDemod()

        outputs = []

        async def bench(ctx):
            for i in range(30):
                ctx.set(self.dut.re_in, 10000)
                ctx.set(self.dut.im_in, 0)
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.strobe_out):
                    outputs.append((ctx.get(self.dut.diff_re_out),
                                    ctx.get(self.dut.diff_im_out)))

        self._simulate(bench)
        # After pipeline flush: diff_im should be 0 (no frequency)
        # and diff_re should be positive (= |z|^2 / 2^15)
        steady = outputs[3:]
        for diff_re, diff_im in steady:
            self.assertEqual(diff_im, 0, "DC -> diff_im should be 0")
            self.assertGreater(diff_re, 0,
                               "DC at non-zero magnitude -> diff_re > 0")

    def test_quadrant_mapping(self):
        """Verify each LSM-style quadrant produces the right (re,im) signs.

        Feed an IQ signal with constant phase change of +/-pi/4 or +/-3pi/4
        between samples. The differential product z[n]*conj(z[n-1]) should
        land in the quadrant matching the phase change.
        """
        self.dut = C4FMDemod()

        # Phase changes for the 4 P25 dibit positions
        # +pi/4 -> dibit 00 (+1) -> (re>0, im>0)
        # +3pi/4 -> dibit 01 (+3) -> (re<0, im>0)
        # -pi/4 -> dibit 10 (-1) -> (re>0, im<0)
        # -3pi/4 -> dibit 11 (-3) -> (re<0, im<0)
        phase_steps = [np.pi/4, 3*np.pi/4, -np.pi/4, -3*np.pi/4]
        expected_quadrants = [
            ('+', '+'),  # +pi/4
            ('-', '+'),  # +3pi/4
            ('+', '-'),  # -pi/4
            ('-', '-'),  # -3pi/4
        ]

        amplitude = 10000

        for step, (exp_re_sign, exp_im_sign) in zip(
                phase_steps, expected_quadrants):
            self.dut = C4FMDemod()
            n_samples = 30
            phase = 0.0
            re = []
            im = []
            for _ in range(n_samples):
                phase += step
                re.append(int(round(amplitude * np.cos(phase))))
                im.append(int(round(amplitude * np.sin(phase))))

            outputs = []

            async def bench(ctx):
                for i in range(n_samples):
                    ctx.set(self.dut.re_in, re[i])
                    ctx.set(self.dut.im_in, im[i])
                    ctx.set(self.dut.strobe_in, 1)
                    await ctx.tick()
                    if ctx.get(self.dut.strobe_out):
                        outputs.append((ctx.get(self.dut.diff_re_out),
                                        ctx.get(self.dut.diff_im_out)))

            self._simulate(bench)

            steady = outputs[5:]
            self.assertGreater(len(steady), 5)
            for diff_re, diff_im in steady:
                if exp_re_sign == '+':
                    self.assertGreater(
                        diff_re, 0,
                        f"step={step:.3f}: expected diff_re>0, got {diff_re}")
                else:
                    self.assertLess(
                        diff_re, 0,
                        f"step={step:.3f}: expected diff_re<0, got {diff_re}")
                if exp_im_sign == '+':
                    self.assertGreater(
                        diff_im, 0,
                        f"step={step:.3f}: expected diff_im>0, got {diff_im}")
                else:
                    self.assertLess(
                        diff_im, 0,
                        f"step={step:.3f}: expected diff_im<0, got {diff_im}")


if __name__ == '__main__':
    unittest.main()
