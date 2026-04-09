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

    def _generate_iq_samples(self, dibits, samples_per_symbol=10,
                             timing_offset=0):
        """Generate raw IQ samples + diff_im for a dibit sequence.

        Walks a phase accumulator forward by the per-symbol phase step
        for each dibit, holding the phase steady within a symbol so the
        sample at the decision point lands on the symbol's IQ vector.

        Phase steps (matching P25 C4FM/LSM):
          0b00 (+1): +pi/4    (sym_diff in quadrant 1: re>0, im>0)
          0b01 (+3): +3pi/4   (quadrant 2: re<0, im>0)
          0b10 (-1): -pi/4    (quadrant 4: re>0, im<0)
          0b11 (-3): -3pi/4   (quadrant 3: re<0, im<0)

        Returns (re, im, diff_im):
          re, im: 16-bit signed IQ samples (post-DDC stand-in)
          diff_im: per-sample FM cross-product im(z[n] * conj(z[n-1]))
                   used to drive the Gardner TED in tests
        """
        amp = 10000
        phase_step = {
            0b00:  np.pi / 4,
            0b01:  3 * np.pi / 4,
            0b10: -np.pi / 4,
            0b11: -3 * np.pi / 4,
        }
        re = []
        im = []
        for _ in range(timing_offset):
            re.append(0)
            im.append(0)
        phase = 0.0
        for dibit in dibits:
            phase += phase_step[dibit]
            r = int(round(amp * np.cos(phase)))
            i = int(round(amp * np.sin(phase)))
            for _ in range(samples_per_symbol):
                re.append(r)
                im.append(i)

        # Per-sample diff_im = im[n]*re[n-1] - re[n]*im[n-1], scaled to
        # ~18-bit like the C4FMDemod output (>>15 of 32-bit product)
        diff_im = []
        re_p = 0
        im_p = 0
        for r, i in zip(re, im):
            d = (i * re_p - r * im_p) >> 15
            d = max(-131071, min(131071, d))
            diff_im.append(d)
            re_p, im_p = r, i
        return re, im, diff_im

    def test_known_dibit_sequence(self):
        """Feed a clean dibit pattern and verify output matches."""
        self.dut = SymbolTimingRecovery(samples_per_symbol=10)

        # Known dibit sequence
        dibits = [0b01, 0b00, 0b10, 0b11] * 10  # 40 symbols
        re, im, diff_im = self._generate_iq_samples(dibits)

        recovered = []

        async def bench(ctx):
            for i in range(len(re)):
                ctx.set(self.dut.re_in, re[i])
                ctx.set(self.dut.im_in, im[i])
                ctx.set(self.dut.diff_im_in, diff_im[i])
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
        re, im, diff_im = self._generate_iq_samples(dibits)
        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for i in range(len(re)):
                ctx.set(self.dut.re_in, re[i])
                ctx.set(self.dut.im_in, im[i])
                ctx.set(self.dut.diff_im_in, diff_im[i])
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    strobe_count += 1

        self._simulate(bench)

        # Should get roughly 20 symbol strobes for 200 input samples
        self.assertGreater(strobe_count, 15, f"Too few strobes: {strobe_count}")
        self.assertLess(strobe_count, 25, f"Too many strobes: {strobe_count}")

    def test_slicer_levels(self):
        """Each P25 phase step lands in the correct symbol-rate quadrant.

        Feeds raw IQ that walks the phase by exactly the per-dibit
        phase step every symbol. The symbol-rate differential then
        lands in the matching quadrant for the entire run.
        """
        # Each test: (expected_dibit, single dibit repeated)
        test_cases = [
            (0b00, [0b00] * 20),  # +pi/4 every symbol -> quadrant 1
            (0b01, [0b01] * 20),  # +3pi/4 every symbol -> quadrant 2
            (0b10, [0b10] * 20),  # -pi/4 every symbol -> quadrant 4
            (0b11, [0b11] * 20),  # -3pi/4 every symbol -> quadrant 3
        ]

        for expected_dibit, dibits in test_cases:
            sps = 10
            self.dut = SymbolTimingRecovery(samples_per_symbol=sps)
            re, im, diff_im = self._generate_iq_samples(
                dibits, samples_per_symbol=sps)

            recovered = []

            async def bench(ctx):
                for i in range(len(re)):
                    ctx.set(self.dut.re_in, re[i])
                    ctx.set(self.dut.im_in, im[i])
                    ctx.set(self.dut.diff_im_in, diff_im[i])
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
                    f"Dibit {expected_dibit:02b}: got {ratio:.0%} correct "
                    f"of {len(steady)} symbols (recovered={recovered})")

    def test_all_four_dibits_appear(self):
        """Regression: all 4 dibit values must appear, not just 0 and 2.

        This is the bug we hit on hardware: a sample-rate differential
        slicer produces ~99.6% dibits 0/2 because cos(small_angle) ≈ +1
        always, so diff_re never crosses zero. The symbol-rate slicer
        should produce ~25% of each value for a uniform input.
        """
        self.dut = SymbolTimingRecovery(samples_per_symbol=10)
        # Repeating 4-dibit pattern -> uniform distribution
        dibits = [0b00, 0b01, 0b10, 0b11] * 12
        re, im, diff_im = self._generate_iq_samples(dibits)

        recovered = []

        async def bench(ctx):
            for i in range(len(re)):
                ctx.set(self.dut.re_in, re[i])
                ctx.set(self.dut.im_in, im[i])
                ctx.set(self.dut.diff_im_in, diff_im[i])
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    recovered.append(ctx.get(self.dut.dibit_out))

        self._simulate(bench)

        if len(recovered) >= 16:
            seen = set(recovered[2:])  # skip startup transient
            self.assertEqual(
                seen, {0, 1, 2, 3},
                f"Expected all 4 dibit values, got {sorted(seen)} "
                f"from {recovered[2:]}")

    def test_strobe_gating(self):
        """No symbol strobe when input strobe is deasserted."""
        self.dut = SymbolTimingRecovery()

        strobe_count = 0

        async def bench(ctx):
            nonlocal strobe_count
            for _ in range(100):
                ctx.set(self.dut.re_in, 1000)
                ctx.set(self.dut.im_in, 500)
                ctx.set(self.dut.diff_im_in, 0)
                ctx.set(self.dut.strobe_in, 0)
                await ctx.tick()
                if ctx.get(self.dut.symbol_strobe):
                    strobe_count += 1

        self._simulate(bench)
        self.assertEqual(strobe_count, 0)


if __name__ == '__main__':
    unittest.main()
