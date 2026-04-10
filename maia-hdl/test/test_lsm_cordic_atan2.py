#
# Fishball P25 -- LsmCordicAtan2 unit tests
#
# Phase 6E.6e. Verifies the new 10-iteration CORDIC vectoring block
# that replaces the small-angle linearisation in LsmPllUpdate.
#
# Tests cover:
#   - Pipeline latency (12 sync cycles from strobe_in to strobe_out)
#   - Cardinal-axis inputs (zero, +x, +y, -x, -y) for quadrant
#     coverage and pre-rotation correctness
#   - All four constellation-corner inputs (+/- pi/4, +/- 3pi/4)
#   - Bit-exact match against the Python `cordic_vectoring_reference`
#   - Float-precision sweep against `math.atan2`, asserting the
#     N=10 worst-case is below 5 mrad
#
# SPDX-License-Identifier: MIT
#

import math
import random
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_cordic_atan2 import (
    LsmCordicAtan2,
    cordic_vectoring_reference,
    ANGLE_FRAC_BITS,
    ANGLE_WIDTH,
    N_ITERS,
)


# Latency from `strobe_in` to `strobe_out`. Match the docstring in
# `lsm_cordic_atan2.py`. If you change N_ITERS, this changes too.
EXPECTED_LATENCY_CYCLES = 2 + N_ITERS  # IDLE->ITER0 + N iters + DONE->latch


def _signed20(value):
    if value >= (1 << (ANGLE_WIDTH - 1)):
        return value - (1 << ANGLE_WIDTH)
    return value


def _drive_one(dut, x, y):
    """Drive a single (x, y) input through the CORDIC and return
    (angle_q416, latency_in_cycles).

    The latency is measured from the cycle that asserts strobe_in to
    the cycle that asserts strobe_out, inclusive of both endpoints.
    """
    out = {'angle': None, 'latency': None}

    async def bench(ctx):
        # Cycle 0: assert strobe_in with the input on the bus.
        ctx.set(dut.x_in, x)
        ctx.set(dut.y_in, y)
        ctx.set(dut.strobe_in, 1)
        await ctx.tick()
        ctx.set(dut.strobe_in, 0)
        # Sweep up to 4x expected latency hunting for strobe_out.
        # Anything beyond that means the FSM hung.
        for cycle in range(1, 4 * EXPECTED_LATENCY_CYCLES + 4):
            if ctx.get(dut.strobe_out):
                out['angle'] = _signed20(ctx.get(dut.angle_out))
                out['latency'] = cycle
                return
            await ctx.tick()
        # One last check after the final tick.
        if ctx.get(dut.strobe_out):
            out['angle'] = _signed20(ctx.get(dut.angle_out))
            out['latency'] = 4 * EXPECTED_LATENCY_CYCLES + 4

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return out['angle'], out['latency']


def _drive_many(dut, samples):
    """Drive a list of (x, y) inputs back-to-back, spacing them with
    enough cycles to drain the pipeline. Returns the list of output
    angles in input order.

    The CORDIC is iterative (one input at a time), so back-to-back
    inputs need at least EXPECTED_LATENCY_CYCLES + 1 cycles between
    strobes. We use 2x to be safe.
    """
    out = []

    async def bench(ctx):
        for (x, y) in samples:
            ctx.set(dut.x_in, x)
            ctx.set(dut.y_in, y)
            ctx.set(dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.strobe_in, 0)
            captured = False
            for _ in range(2 * EXPECTED_LATENCY_CYCLES):
                if ctx.get(dut.strobe_out) and not captured:
                    out.append(_signed20(ctx.get(dut.angle_out)))
                    captured = True
                await ctx.tick()
            if not captured:
                # Final check after the loop.
                if ctx.get(dut.strobe_out):
                    out.append(_signed20(ctx.get(dut.angle_out)))
                    captured = True
            if not captured:
                out.append(None)

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return out


class TestLsmCordicAtan2(unittest.TestCase):

    def test_constants_are_sane(self):
        """The CORDIC angle constants and pi/2 round to the same
        Q4.16 integers we expect at module load."""
        from p25_hdl.lsm_cordic_atan2 import (
            _cordic_angle_q,
            PI_OVER_2_Q,
            _EXPECTED_CORDIC_ANGLES,
        )
        self.assertEqual(PI_OVER_2_Q, 102944)
        for i, expected in enumerate(_EXPECTED_CORDIC_ANGLES):
            self.assertEqual(_cordic_angle_q(i), expected,
                             f"CORDIC angle iter {i}")

    def test_pipeline_latency(self):
        """strobe_out fires exactly EXPECTED_LATENCY_CYCLES cycles
        after strobe_in. Locks down the FSM depth so accidental
        state-machine edits show up as a test failure."""
        dut = LsmCordicAtan2(input_width=20)
        # Use a non-degenerate input.
        _, latency = _drive_one(dut, 32768, 16384)
        self.assertEqual(
            latency, EXPECTED_LATENCY_CYCLES,
            f"expected latency {EXPECTED_LATENCY_CYCLES} cycles, "
            f"got {latency}")

    def test_zero_input_matches_reference(self):
        """`atan2(0, 0)` is mathematically undefined; the CORDIC
        FSM treats `y == 0` as `y >= 0` and accumulates clockwise
        rotations every iteration, so the output is the sum of
        every CORDIC angle constant -- garbage as far as atan2 is
        concerned, but a stable, predictable garbage value that
        matches the Python reference. We assert the HDL matches the
        reference (not zero) so degenerate inputs don't desync the
        bit-exact comparison test, and so callers know not to feed
        (0, 0) to the block in real use."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, 0, 0)
        expected = cordic_vectoring_reference(0, 0)
        self.assertEqual(
            angle, expected,
            f"HDL {angle} vs Python reference {expected} on (0,0)")
        # Sanity: this stable garbage value is the algebraic sum of
        # all CORDIC angle constants (the algorithm rotates by every
        # angle in the same direction when y is stuck at zero).
        from p25_hdl.lsm_cordic_atan2 import _EXPECTED_CORDIC_ANGLES
        self.assertEqual(expected, sum(_EXPECTED_CORDIC_ANGLES))

    def test_positive_x_axis(self):
        """(+1, 0) -- atan2 = 0. With a CORDIC iteration the residual
        oscillates around 0, but the magnitude error must be below
        the worst-case empirical bound (~3 mrad at N=10)."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, 32768, 0)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - 0.0), 5e-3,
                        f"expected ~0 rad, got {rad}")

    def test_positive_y_axis(self):
        """(0, +1) -- atan2 = +pi/2."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, 0, 32768)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - math.pi / 2), 5e-3)

    def test_negative_x_axis(self):
        """(-1, 0) -- atan2 = +pi (the "second quadrant" branch
        seeds z = +pi/2, then CORDIC converges +pi/2 more)."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, -32768, 0)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - math.pi), 5e-3,
                        f"expected ~pi, got {rad}")

    def test_negative_y_axis(self):
        """(0, -1) -- atan2 = -pi/2."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, 0, -32768)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - (-math.pi / 2)), 5e-3)

    def test_first_quadrant(self):
        """(1, 1) -- atan2 = +pi/4."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, 32768, 32768)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - math.pi / 4), 5e-3)

    def test_second_quadrant(self):
        """(-1, 1) -- atan2 = +3pi/4."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, -32768, 32768)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - 3 * math.pi / 4), 5e-3)

    def test_third_quadrant(self):
        """(-1, -1) -- atan2 = -3pi/4."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, -32768, -32768)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - (-3 * math.pi / 4)), 5e-3)

    def test_fourth_quadrant(self):
        """(1, -1) -- atan2 = -pi/4."""
        dut = LsmCordicAtan2(input_width=20)
        angle, _ = _drive_one(dut, 32768, -32768)
        rad = angle / (1 << ANGLE_FRAC_BITS)
        self.assertLess(abs(rad - (-math.pi / 4)), 5e-3)

    def test_bit_exact_against_python_reference(self):
        """Sweep 64 deterministic random inputs and compare HDL
        output ULP-by-ULP against `cordic_vectoring_reference`. Any
        mismatch indicates the HDL drifted from the algorithm we're
        documenting."""
        rng = random.Random(0xCAFE)
        samples = []
        py_results = []
        for _ in range(64):
            ang = rng.uniform(-math.pi, math.pi)
            mag = rng.uniform(0.1, 0.99) * 32768
            x = int(round(mag * math.cos(ang)))
            y = int(round(mag * math.sin(ang)))
            samples.append((x, y))
            py_results.append(cordic_vectoring_reference(x, y))

        dut = LsmCordicAtan2(input_width=20)
        hdl_results = _drive_many(dut, samples)

        self.assertEqual(len(hdl_results), len(py_results),
                         "HDL emitted wrong number of strobes")
        for k, (got, want) in enumerate(zip(hdl_results, py_results)):
            self.assertIsNotNone(
                got, f"sample {k}: no strobe_out captured")
            self.assertEqual(
                got, want,
                f"sample {k}: HDL Q4.16 = {got} ({got/65536:+.6f}) "
                f"vs Python ref = {want} ({want/65536:+.6f}) "
                f"for (x,y) = {samples[k]}")

    def test_float_accuracy_sweep(self):
        """Sweep 64 random inputs against true `math.atan2`. Assert
        N=10 worst-case error stays under 5 mrad and rms under
        2 mrad. These bounds match the empirical sweep that
        justified the N=10 choice -- if the HDL ever degrades to
        the linearisation's ~45 mrad error budget this catches it."""
        rng = random.Random(0xBEE0)
        samples = []
        truth = []
        for _ in range(64):
            ang = rng.uniform(-math.pi, math.pi)
            mag = rng.uniform(0.1, 0.99) * 32768
            x = int(round(mag * math.cos(ang)))
            y = int(round(mag * math.sin(ang)))
            samples.append((x, y))
            truth.append(math.atan2(y, x))

        dut = LsmCordicAtan2(input_width=20)
        hdl_results = _drive_many(dut, samples)

        max_err = 0.0
        sq_sum = 0.0
        for k, (got_q, want) in enumerate(zip(hdl_results, truth)):
            got = got_q / (1 << ANGLE_FRAC_BITS)
            err = got - want
            # `atan2` outputs lie in (-pi, pi]; the CORDIC accumulator
            # never wraps so we don't need a modulo here. Sanity:
            self.assertLess(abs(err), 0.05,
                            f"sample {k}: x,y={samples[k]} got {got}"
                            f" want {want} err {err}")
            max_err = max(max_err, abs(err))
            sq_sum += err * err
        rms = math.sqrt(sq_sum / len(samples))
        print(f"\n[cordic] N={N_ITERS} sweep: max_err = "
              f"{max_err * 1000:.3f} mrad, rms = {rms * 1000:.3f} mrad")
        self.assertLess(max_err, 5e-3,
                        f"max err {max_err} exceeds 5 mrad budget")
        self.assertLess(rms, 2e-3,
                        f"rms {rms} exceeds 2 mrad budget")


if __name__ == '__main__':
    unittest.main()
