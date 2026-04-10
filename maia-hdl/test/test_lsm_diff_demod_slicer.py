#
# Fishball P25 -- LSM differential demod + slicer HDL tests
#
# Phase 6E.5 of the LSM HDL port. Drives `LsmDiffDemodSlicer` with
# synthetic constant-phase-rotation streams and verifies the dibit
# output matches the expected 4-PSK quadrant for each of the four
# possible per-symbol phase deltas. Plus a Python reference test
# on a varying-phase input to catch fixed-point math regressions.
#
# This sub-phase does NOT compare against the Phase 6D
# `demod_loop_synthetic` golden vector -- the full pipeline still
# lacks AGC, PLL update, and Gardner TED, none of which exist
# until 6E.6. Until then, the dibit output of the HDL diverges
# from the Rust reference whenever the PLL drifts away from 0.
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_diff_demod_slicer import LsmDiffDemodSlicer


def _q15(x):
    """Saturate-clip a float in [-1, 1] to Q1.15."""
    q = int(round(x * (1 << 15)))
    return max(-(1 << 15), min((1 << 15) - 1, q))


def _drive_decisions(dut, samples, *, prefix_silence=2, drain_cycles=4):
    """Helper: pulse decision_strobe once per (i_mid, q_mid, i_cur, q_cur)
    tuple in `samples`, collect (dibit, i_sym, q_sym) per symbol_strobe.

    The HDL has a 2-cycle pipeline latency from decision_strobe to
    symbol_strobe, so the helper drains a few extra cycles after the
    last input.
    """
    out = []

    async def bench(ctx):
        # Idle a couple of cycles so the strobes start cleanly.
        for _ in range(prefix_silence):
            ctx.set(dut.decision_strobe, 0)
            await ctx.tick()
        for (im, qm, ic, qc) in samples:
            ctx.set(dut.i_mid_in, im)
            ctx.set(dut.q_mid_in, qm)
            ctx.set(dut.i_cur_in, ic)
            ctx.set(dut.q_cur_in, qc)
            ctx.set(dut.decision_strobe, 1)
            await ctx.tick()
            ctx.set(dut.decision_strobe, 0)
            if ctx.get(dut.symbol_strobe):
                out.append((
                    ctx.get(dut.dibit_out),
                    ctx.get(dut.i_sym_out),
                    ctx.get(dut.q_sym_out),
                ))
            # Wait at least one extra cycle between strobes so the
            # 2-stage pipeline doesn't overlap two decisions.
            await ctx.tick()
            if ctx.get(dut.symbol_strobe):
                out.append((
                    ctx.get(dut.dibit_out),
                    ctx.get(dut.i_sym_out),
                    ctx.get(dut.q_sym_out),
                ))
        # Drain the pipeline.
        for _ in range(drain_cycles):
            await ctx.tick()
            if ctx.get(dut.symbol_strobe):
                out.append((
                    ctx.get(dut.dibit_out),
                    ctx.get(dut.i_sym_out),
                    ctx.get(dut.q_sym_out),
                ))

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return out


class TestLsmDiffDemodSlicer(unittest.TestCase):

    def _rotation_samples(self, dphi, n=20):
        """Build n consecutive samples on the unit circle, advancing
        by `dphi` radians per symbol. Used as both the midpoint and
        the current-symbol input streams (the diff demod doesn't
        care that they're the same -- it computes them independently
        against their own prev state).
        """
        samples = []
        phi = 0.0
        for _ in range(n):
            i = _q15(math.cos(phi))
            q = _q15(math.sin(phi))
            samples.append((i, q, i, q))
            phi += dphi
        return samples

    def test_dibit_for_plus_pi_4(self):
        """+pi/4 per symbol -> dibit 0b00 (Dibit +1)."""
        dut = LsmDiffDemodSlicer()
        samples = self._rotation_samples(math.pi / 4.0)
        out = _drive_decisions(dut, samples)
        # Skip the first 1-2 outputs while prev_* is settling from 0.
        steady = out[2:]
        self.assertGreaterEqual(len(steady), 10)
        for d, ix, qx in steady:
            self.assertEqual(d, 0b00, f"expected 00, got {d:02b} (i={ix} q={qx})")

    def test_dibit_for_plus_3pi_4(self):
        """+3pi/4 per symbol -> dibit 0b01 (Dibit +3)."""
        dut = LsmDiffDemodSlicer()
        samples = self._rotation_samples(3.0 * math.pi / 4.0)
        out = _drive_decisions(dut, samples)
        steady = out[2:]
        self.assertGreaterEqual(len(steady), 10)
        for d, ix, qx in steady:
            self.assertEqual(d, 0b01, f"expected 01, got {d:02b} (i={ix} q={qx})")

    def test_dibit_for_minus_pi_4(self):
        """-pi/4 per symbol -> dibit 0b10 (Dibit -1)."""
        dut = LsmDiffDemodSlicer()
        samples = self._rotation_samples(-math.pi / 4.0)
        out = _drive_decisions(dut, samples)
        steady = out[2:]
        self.assertGreaterEqual(len(steady), 10)
        for d, ix, qx in steady:
            self.assertEqual(d, 0b10, f"expected 10, got {d:02b} (i={ix} q={qx})")

    def test_dibit_for_minus_3pi_4(self):
        """-3pi/4 per symbol -> dibit 0b11 (Dibit -3)."""
        dut = LsmDiffDemodSlicer()
        samples = self._rotation_samples(-3.0 * math.pi / 4.0)
        out = _drive_decisions(dut, samples)
        steady = out[2:]
        self.assertGreaterEqual(len(steady), 10)
        for d, ix, qx in steady:
            self.assertEqual(d, 0b11, f"expected 11, got {d:02b} (i={ix} q={qx})")

    def test_dibit_for_constant_phase(self):
        """0 phase change per symbol -> dibit 0b00 (positive real axis)."""
        dut = LsmDiffDemodSlicer()
        samples = self._rotation_samples(0.0)
        out = _drive_decisions(dut, samples)
        # After even one decision, prev_curr matches curr exactly,
        # so the diff demod produces (|z|^2, 0) -- positive real,
        # zero imag -- which slices to 00.
        steady = out[2:]
        self.assertGreaterEqual(len(steady), 10)
        for d, ix, qx in steady:
            self.assertEqual(d, 0b00, f"expected 00, got {d:02b} (i={ix} q={qx})")

    def test_dibit_sequence_walks_all_quadrants(self):
        """A dibit-driven phase sequence reproduces the source dibits.

        Builds a known-good test pattern by stepping through all four
        possible LSM dibits in a fixed cycle, encoding each as the
        corresponding +/- pi/4 or +/- 3pi/4 phase delta, and feeding
        the resulting sample stream into the slicer. The output
        dibit sequence must match the input dibit sequence (after
        the warmup transient).
        """
        dut = LsmDiffDemodSlicer()

        # Build a test pattern: cycle through dibits 00, 01, 10, 11
        # several times.
        truth = [0b00, 0b01, 0b10, 0b11] * 6
        delta_for_dibit = {
            0b00:  math.pi / 4.0,
            0b01:  3.0 * math.pi / 4.0,
            0b10: -math.pi / 4.0,
            0b11: -3.0 * math.pi / 4.0,
        }
        samples = []
        phi = 0.0
        for d in truth:
            phi += delta_for_dibit[d]
            i = _q15(math.cos(phi))
            q = _q15(math.sin(phi))
            samples.append((i, q, i, q))

        out = _drive_decisions(dut, samples)
        # The very first output uses prev_curr = (0, 0), so
        # i_sym_full = q_sym_full = 0, dibit = 00 regardless of
        # what the input dibit "should" be. Skip it.
        skip = 1
        decoded = [d for (d, _, _) in out[skip:skip + len(truth) - skip]]
        # Compare the same number of samples we have on each side.
        n = min(len(decoded), len(truth) - skip)
        for i in range(n):
            self.assertEqual(
                decoded[i], truth[skip + i],
                f"sample {i + skip}: expected dibit {truth[skip + i]:02b}, "
                f"got {decoded[i]:02b}")

    def test_demod_outputs_have_expected_sign(self):
        """For a +pi/4 rotation stream, the demod outputs i_sym/q_sym
        should track ~(cos(pi/4), sin(pi/4)) -- both positive, roughly
        equal magnitude. Sanity check on the truncated soft outputs
        (not just the slicer).
        """
        dut = LsmDiffDemodSlicer()
        samples = self._rotation_samples(math.pi / 4.0, n=20)
        out = _drive_decisions(dut, samples)
        steady = out[3:]
        for d, ix, qx in steady:
            self.assertGreater(ix, 0, f"i_sym should be positive, got {ix}")
            self.assertGreater(qx, 0, f"q_sym should be positive, got {qx}")
            # Roughly equal -- within a factor of 2 (Q15 quantisation
            # of cos/sin(pi/4) ~ 23170 each, after the >>15 shift the
            # values land around 16384 ish).
            ratio = max(ix, qx) / max(min(ix, qx), 1)
            self.assertLess(ratio, 2.0,
                            f"i_sym/q_sym ratio too large: {ix}/{qx}")


if __name__ == '__main__':
    unittest.main()
