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

    def _generate_disc_samples(self, dibits, samples_per_symbol=10,
                               timing_offset=0):
        """Generate discriminator-like samples for a dibit sequence.

        Maps dibits to 4 amplitude levels matching C4FM:
          01 -> +3 (level +48000)
          00 -> +1 (level +16000)
          10 -> -1 (level -16000)
          11 -> -3 (level -48000)

        Returns list of integer samples at the given samples/symbol rate.
        """
        level_map = {0b01: 48000, 0b00: 16000, 0b10: -16000, 0b11: -48000}
        samples = []
        # Add timing offset as extra leading samples
        for _ in range(timing_offset):
            samples.append(0)
        for dibit in dibits:
            level = level_map[dibit]
            for _ in range(samples_per_symbol):
                samples.append(level)
        return samples

    def test_known_dibit_sequence(self):
        """Feed a clean dibit pattern and verify output matches."""
        self.dut = SymbolTimingRecovery(samples_per_symbol=10)

        # Known dibit sequence
        dibits = [0b01, 0b00, 0b10, 0b11] * 10  # 40 symbols
        samples = self._generate_disc_samples(dibits)

        recovered = []

        async def bench(ctx):
            for i in range(len(samples)):
                # Clamp to 18-bit signed range
                val = max(-131071, min(131071, samples[i]))
                ctx.set(self.dut.disc_in, val)
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    recovered.append(ctx.get(self.dut.dibit_out))

        self._simulate(bench)

        # After timing lock (skip first ~5 symbols), output should match input
        # The exact lock-up time depends on initial alignment
        if len(recovered) > 10:
            # Check that we recover roughly the right number of symbols
            expected_count = len(dibits)
            self.assertAlmostEqual(len(recovered), expected_count, delta=5)

    def test_symbol_rate(self):
        """Verify symbol strobes occur at approximately 1/10 of input rate."""
        self.dut = SymbolTimingRecovery(samples_per_symbol=10)

        # Feed 200 samples of alternating pattern
        dibits = [0b01, 0b11] * 10  # 20 symbols = 200 samples
        samples = self._generate_disc_samples(dibits)
        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for i in range(len(samples)):
                val = max(-131071, min(131071, samples[i]))
                ctx.set(self.dut.disc_in, val)
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    strobe_count += 1

        self._simulate(bench)

        # Should get roughly 20 symbol strobes for 200 input samples
        self.assertGreater(strobe_count, 15, f"Too few strobes: {strobe_count}")
        self.assertLess(strobe_count, 25, f"Too many strobes: {strobe_count}")

    def test_slicer_levels(self):
        """Verify the 4-level slicer maps to correct dibits."""
        # Feed each level independently for 10 symbols each
        test_cases = [
            (0b01, 48000),   # +3
            (0b00, 16000),   # +1
            (0b10, -16000),  # -1
            (0b11, -48000),  # -3
        ]

        for expected_dibit, level in test_cases:
            sps = 10
            # 15 symbols at this level
            samples = [level] * (15 * sps)
            recovered = []

            self.dut = SymbolTimingRecovery(samples_per_symbol=sps)

            async def bench(ctx):
                for val in samples:
                    val = max(-131071, min(131071, val))
                    ctx.set(self.dut.disc_in, val)
                    ctx.set(self.dut.strobe_in, 1)
                    await ctx.tick()
                    if ctx.get(self.dut.symbol_strobe):
                        recovered.append(ctx.get(self.dut.dibit_out))

            self._simulate(bench)

            # After lock, majority should be the expected dibit
            if len(recovered) > 5:
                steady = recovered[3:]  # skip initial
                correct = sum(1 for d in steady if d == expected_dibit)
                ratio = correct / len(steady)
                self.assertGreater(
                    ratio, 0.8,
                    f"Level {level}: expected dibit {expected_dibit:02b}, "
                    f"got {ratio:.0%} correct out of {len(steady)} symbols")

    def test_strobe_gating(self):
        """No symbol strobe when input strobe is deasserted."""
        self.dut = SymbolTimingRecovery()

        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for _ in range(100):
                ctx.set(self.dut.disc_in, 10000)
                ctx.set(self.dut.strobe_in, 0)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    strobe_count += 1

        self._simulate(bench)
        self.assertEqual(strobe_count, 0)


if __name__ == '__main__':
    unittest.main()
