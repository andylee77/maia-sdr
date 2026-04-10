#
# Fishball P25 -- LSM PLL rotate HDL tests
#
# Phase 6E.6c. Drives `LsmPllRotate` with deterministic
# (i, q, pll) inputs and verifies the rotated output matches a
# Python floating-point reference of the same rotation.
#
# Tolerance: the LUT has 3.9 mrad step (no interpolation), so
# worst-case sin/cos quantisation error is ~0.004. After the
# multiply with a Q1.15 input that's ~131 Q15 ULPs of error per
# component. The test allows 200 Q15 ULPs to give the LUT and the
# rounding margin combined room.
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_pll_rotate import LsmPllRotate


def _q(x, frac_bits, width):
    """Saturate-clip a float to signed Q*.frac_bits."""
    q = int(round(x * (1 << frac_bits)))
    lo = -(1 << (width - 1))
    hi = (1 << (width - 1)) - 1
    return max(lo, min(hi, q))


def _signed(value, width):
    """Sign-extend an unsigned ctx.get() result to a Python int."""
    if value >= (1 << (width - 1)):
        return value - (1 << width)
    return value


class TestLsmPllRotate(unittest.TestCase):

    def _drive(self, dut, samples, drain=8):
        """samples = list of (i_q15, q_q15, pll_q13)
        Returns list of (i_out, q_out) after each strobe_out."""
        out = []

        async def bench(ctx):
            for (i, q, p) in samples:
                ctx.set(dut.i_in, i)
                ctx.set(dut.q_in, q)
                ctx.set(dut.pll_in, p)
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                for _ in range(drain):
                    await ctx.tick()
                    if ctx.get(dut.strobe_out):
                        out.append((
                            _signed(ctx.get(dut.i_out), 18),
                            _signed(ctx.get(dut.q_out), 18),
                        ))

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()
        return out

    def test_rotation_by_zero_is_identity(self):
        """pll == 0 -> cos=1, sin=0 -> output equals input."""
        dut = LsmPllRotate()
        cases = [
            (_q(0.5, 15, 18),  _q(0.5, 15, 18),  0),
            (_q(-0.3, 15, 18), _q(0.7, 15, 18),  0),
            (_q(0.0, 15, 18),  _q(-0.6, 15, 18), 0),
        ]
        out = self._drive(dut, cases)
        self.assertEqual(len(out), len(cases))
        for (got_i, got_q), (exp_i, exp_q, _) in zip(out, cases):
            # cos(0)=32767 ≈ 1.0 (one ULP short of 1), so the
            # rotation by 0 produces input * (32767/32768) ≈ input.
            # Allow 2 ULPs.
            self.assertLessEqual(abs(got_i - exp_i), 2)
            self.assertLessEqual(abs(got_q - exp_q), 2)

    def test_rotation_against_python_reference(self):
        """Drive a sweep of (i, q, pll) values and verify the rotation
        matches `i*cos(pll) - q*sin(pll)` and `i*sin(pll) + q*cos(pll)`
        in Q15 fixed point within 200 ULPs."""
        cases_float = []
        for k in range(40):
            phi_pll = math.pi / 3.0 * (k / 40.0 - 0.5) * 2.0  # span ~+/- pi/3
            phi_iq = 2.0 * math.pi * (k / 40.0)
            i = 0.7 * math.cos(phi_iq)
            q = 0.7 * math.sin(phi_iq)
            cases_float.append((i, q, phi_pll))

        # Compute float reference output.
        ref = []
        for (i, q, p) in cases_float:
            c = math.cos(p)
            s = math.sin(p)
            i_out = i * c - q * s
            q_out = i * s + q * c
            ref.append((i_out, q_out))

        # Convert inputs to fixed-point.
        cases_q = [
            (_q(i, 15, 18), _q(q, 15, 18), _q(p, 13, 16))
            for (i, q, p) in cases_float
        ]

        dut = LsmPllRotate()
        out = self._drive(dut, cases_q)

        self.assertEqual(len(out), len(cases_q))
        TOLERANCE = 200  # Q15 ULPs ~ 6e-3 absolute
        for k, ((got_i, got_q), (ref_i, ref_q)) in enumerate(zip(out, ref)):
            ref_i_q = int(round(ref_i * (1 << 15)))
            ref_q_q = int(round(ref_q * (1 << 15)))
            err_i = abs(got_i - ref_i_q)
            err_q = abs(got_q - ref_q_q)
            self.assertLessEqual(
                err_i, TOLERANCE,
                f"sample {k} i: HDL {got_i} vs ref {ref_i_q}, err {err_i}")
            self.assertLessEqual(
                err_q, TOLERANCE,
                f"sample {k} q: HDL {got_q} vs ref {ref_q_q}, err {err_q}")

    def test_rotation_preserves_magnitude(self):
        """Rotation is unitary -- |output| should equal |input|
        within the LUT precision. Sanity check on the slicer-side
        invariant: a rotation must not change the magnitude of the
        constellation point.
        """
        dut = LsmPllRotate()
        # Use a unit-magnitude input at various pll values.
        i_q = _q(0.7, 15, 18)
        q_q = _q(0.7, 15, 18)
        cases = []
        for k in range(20):
            p = (k - 10) * 200  # Q2.13 step
            cases.append((i_q, q_q, p))
        out = self._drive(dut, cases)

        # |input|^2 ~= 0.7^2 + 0.7^2 = 0.98 -> sqrt ~ 0.99
        # In Q15: ~32440
        in_mag_sq = i_q * i_q + q_q * q_q
        for k, (got_i, got_q) in enumerate(out):
            out_mag_sq = got_i * got_i + got_q * got_q
            ratio = out_mag_sq / in_mag_sq
            self.assertAlmostEqual(
                ratio, 1.0, places=2,
                msg=f"sample {k}: magnitude ratio {ratio:.4f} not ~ 1.0")


if __name__ == '__main__':
    unittest.main()
