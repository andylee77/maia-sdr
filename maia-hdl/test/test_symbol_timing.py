#
# Fishball P25 - Symbol Timing Recovery tests
#
# SPDX-License-Identifier: MIT
#

import unittest

import numpy as np

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.symbol_timing import SymbolTimingRecovery


class TestSymbolTimingRecovery(unittest.TestCase):
    """Test Gardner-based symbol timing recovery."""

    def _simulate(self, bench, *, vcd=None):
        sim = Simulator(self.dut)
        sim.add_clock(12e-9)
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    def _generate_diff_samples(self, dibits, samples_per_symbol=10,
                               timing_offset=0):
        """Generate (diff_re, diff_im) samples for a dibit sequence.

        Maps dibits to 4 quadrants of the differential product plane,
        matching the SDRTrunk LSM/C4FM unified slicer:
          0b00 (+1): (diff_re > 0, diff_im > 0)  -> +pi/4
          0b01 (+3): (diff_re < 0, diff_im > 0)  -> +3pi/4
          0b10 (-1): (diff_re > 0, diff_im < 0)  -> -pi/4
          0b11 (-3): (diff_re < 0, diff_im < 0)  -> -3pi/4

        Returns two lists (re, im) of integer samples.
        """
        # 4 unit-vector points on the differential plane (scaled to 18-bit)
        amp = 50000
        quad_map = {
            0b00: ( amp,  amp),  # quadrant 1, dibit value 0
            0b01: (-amp,  amp),  # quadrant 2, dibit value 1
            0b10: ( amp, -amp),  # quadrant 4, dibit value 2
            0b11: (-amp, -amp),  # quadrant 3, dibit value 3
        }
        re = []
        im = []
        for _ in range(timing_offset):
            re.append(0)
            im.append(0)
        for dibit in dibits:
            r, i = quad_map[dibit]
            for _ in range(samples_per_symbol):
                re.append(r)
                im.append(i)
        return re, im

    def test_known_dibit_sequence(self):
        """Feed a clean dibit pattern and verify output matches."""
        self.dut = SymbolTimingRecovery(samples_per_symbol=10)

        # Known dibit sequence
        dibits = [0b01, 0b00, 0b10, 0b11] * 10  # 40 symbols
        re, im = self._generate_diff_samples(dibits)

        recovered = []

        async def bench(ctx):
            for i in range(len(re)):
                ctx.set(self.dut.diff_re_in,
                        max(-131071, min(131071, re[i])))
                ctx.set(self.dut.diff_im_in,
                        max(-131071, min(131071, im[i])))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    recovered.append(ctx.get(self.dut.dibit_out))

        self._simulate(bench)

        # Approximately the right number of symbols
        if len(recovered) > 10:
            self.assertAlmostEqual(len(recovered), len(dibits), delta=5)

    def test_symbol_rate(self):
        """Verify symbol strobes occur at approximately 1/10 of input rate."""
        self.dut = SymbolTimingRecovery(samples_per_symbol=10)

        dibits = [0b01, 0b11] * 10  # 20 symbols = 200 samples
        re, im = self._generate_diff_samples(dibits)
        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for i in range(len(re)):
                ctx.set(self.dut.diff_re_in,
                        max(-131071, min(131071, re[i])))
                ctx.set(self.dut.diff_im_in,
                        max(-131071, min(131071, im[i])))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    strobe_count += 1

        self._simulate(bench)

        # Should get roughly 20 symbol strobes for 200 input samples
        self.assertGreater(strobe_count, 15, f"Too few strobes: {strobe_count}")
        self.assertLess(strobe_count, 25, f"Too many strobes: {strobe_count}")

    def test_slicer_levels(self):
        """Verify the sign-bit slicer maps each quadrant to its dibit."""
        # Each test case: (expected_dibit, diff_re_value, diff_im_value)
        # Mapping: dibit_lsb = (re < 0), dibit_msb = (im < 0)
        test_cases = [
            (0b00,  50000,  50000),   # quadrant 1: +1
            (0b01, -50000,  50000),   # quadrant 2: +3
            (0b10,  50000, -50000),   # quadrant 4: -1
            (0b11, -50000, -50000),   # quadrant 3: -3
        ]

        for expected_dibit, re_val, im_val in test_cases:
            sps = 10
            n_samples = 15 * sps
            recovered = []

            self.dut = SymbolTimingRecovery(samples_per_symbol=sps)

            async def bench(ctx):
                for _ in range(n_samples):
                    ctx.set(self.dut.diff_re_in, re_val)
                    ctx.set(self.dut.diff_im_in, im_val)
                    ctx.set(self.dut.strobe_in, 1)
                    await ctx.tick()
                    if ctx.get(self.dut.symbol_strobe):
                        recovered.append(ctx.get(self.dut.dibit_out))

            self._simulate(bench)

            if len(recovered) > 5:
                steady = recovered[3:]
                correct = sum(1 for d in steady if d == expected_dibit)
                ratio = correct / len(steady)
                self.assertGreater(
                    ratio, 0.8,
                    f"Quadrant ({re_val},{im_val}): expected dibit "
                    f"{expected_dibit:02b}, got {ratio:.0%} correct of "
                    f"{len(steady)} symbols")

    def test_strobe_gating(self):
        """No symbol strobe when input strobe is deasserted."""
        self.dut = SymbolTimingRecovery()

        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for _ in range(100):
                ctx.set(self.dut.diff_re_in, 10000)
                ctx.set(self.dut.diff_im_in, 10000)
                ctx.set(self.dut.strobe_in, 0)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    strobe_count += 1

        self._simulate(bench)
        self.assertEqual(strobe_count, 0)


if __name__ == '__main__':
    unittest.main()
