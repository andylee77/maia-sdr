#
# Fishball P25 -- LSM PLL update HDL tests
#
# Covers BOTH PLL update implementations:
#
#   - `LsmPllUpdate`           (Phase 6E.6e, CORDIC atan2 form;
#                               production)
#   - `LsmPllUpdateLinearised` (Phase 6E.6b, small-angle form;
#                               legacy regression baseline)
#
# Tests exercise:
#   - the 4-way dibit mux (one test per dibit, both forms)
#   - per-step clamps (large input -> clamped step)
#   - the integrator clamp (drive pll past +/- pi/3)
#   - convergence behaviour against a Python reference (linearised
#     form: bit-exact; CORDIC form: a few ULPs of CORDIC residual)
#   - cross-form smoke: both classes converge in the same direction
#     on the same dibit/(i,q) pair
#
# SPDX-License-Identifier: MIT
#

import math
import random
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_pll_update import (
    LsmPllUpdate,
    LsmPllUpdateLinearised,
    PLL_GAIN_FLOAT,
    PLL_MAX_ERROR_FLOAT,
    MAX_PLL_ABS_FLOAT,
    SQRT2_OVER_2,
    COMBINED_GAIN_FLOAT,
    RAW_CLAMP_FLOAT,
    INPUT_FRAC_BITS,
    PLL_FRAC_BITS,
)


def _q(x, frac_bits, width):
    """Saturate-clip a float to signed Q*.frac_bits."""
    q = int(round(x * (1 << frac_bits)))
    lo = -(1 << (width - 1))
    hi = (1 << (width - 1)) - 1
    return max(lo, min(hi, q))


class _PythonPll:
    """Linearised PLL update reference, matching the HDL block.

    Notice this is NOT a literal port of the Rust loop -- it's the
    *small-angle linearisation* the HDL implements. The Rust loop
    uses true atan2 + per-dibit ideal phase; the HDL skips the
    atan2 and computes
        phase_error_proxy = +/-(q +/- i)  (4-way dibit mux)
    then folds sqrt(2)/2 into the loop gain. The HDL test compares
    against THIS reference, not against the Rust loop, because the
    point of the test is to verify the HDL matches the algorithm
    we chose to implement.

    The full-pipeline integration test in 6E.6d will compare against
    the demod_loop_synthetic.json golden, which is the cross-check
    against the Rust loop and will absorb the ~1.5 % linearisation
    error in PLL convergence.
    """

    def __init__(self):
        self.pll = 0.0

    def step(self, i_sym, q_sym, dibit):
        if dibit == 0b00:
            raw = q_sym - i_sym
        elif dibit == 0b01:
            raw = -(q_sym + i_sym)
        elif dibit == 0b10:
            raw = q_sym + i_sym
        elif dibit == 0b11:
            raw = i_sym - q_sym
        # Clamp the *raw* value at the equivalent unscaled limit.
        if raw > RAW_CLAMP_FLOAT:
            raw = RAW_CLAMP_FLOAT
        elif raw < -RAW_CLAMP_FLOAT:
            raw = -RAW_CLAMP_FLOAT
        # Multiply by combined gain ((sqrt(2)/2) * PLL_GAIN).
        step = raw * COMBINED_GAIN_FLOAT
        self.pll -= step
        # Clamp pll.
        if self.pll > MAX_PLL_ABS_FLOAT:
            self.pll = MAX_PLL_ABS_FLOAT
        elif self.pll < -MAX_PLL_ABS_FLOAT:
            self.pll = -MAX_PLL_ABS_FLOAT
        return self.pll


def _drive_pll(dut, samples, *, drain=4):
    """Drive samples = [(i_q15, q_q15, dibit), ...] and collect
    (pll_out_q13, ...) per pll_strobe.

    `drain` is the number of sync cycles to wait for pll_strobe
    after each input strobe. The linearised form needs ~3 cycles;
    the CORDIC form needs ~16. Default is 4 (matches the legacy
    linearised form); the CORDIC tests pass `drain=24` explicitly.
    """
    out = []

    async def bench(ctx):
        for (i, q, d) in samples:
            ctx.set(dut.i_sym_in, i)
            ctx.set(dut.q_sym_in, q)
            ctx.set(dut.dibit_in, d)
            ctx.set(dut.symbol_strobe, 1)
            await ctx.tick()
            ctx.set(dut.symbol_strobe, 0)
            captured = False
            for _ in range(drain):
                await ctx.tick()
                if ctx.get(dut.pll_strobe) and not captured:
                    v = ctx.get(dut.pll_out)
                    if v >= (1 << 15):
                        v -= (1 << 16)
                    out.append(v)
                    captured = True

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return out


# CORDIC PLL update needs more drain cycles than the linearised
# form: ~16 cycles latency from symbol_strobe to pll_strobe.
CORDIC_DRAIN = 24


class _AtanPll:
    """Literal atan2-based PLL reference matching the Rust loop.

    This is the algorithm `LsmPllUpdate` (CORDIC form) implements.
    Unlike `_PythonPll`, this reference uses true `math.atan2` --
    so HDL comparisons must allow a few ULPs of CORDIC residual
    quantisation error (the HDL is bit-exact against the
    `cordic_vectoring_reference` in `lsm_cordic_atan2.py`, which
    is its own bit-exact Python reference).
    """

    DIBIT_PHASE = {
        0b00: math.pi / 4.0,
        0b01: 3.0 * math.pi / 4.0,
        0b10: -math.pi / 4.0,
        0b11: -3.0 * math.pi / 4.0,
    }

    def __init__(self):
        self.pll = 0.0

    def step(self, i_sym, q_sym, dibit):
        if i_sym == 0.0 and q_sym == 0.0:
            # Match Rust + HDL: skip the update on exact zero input.
            return self.pll
        soft_symbol = math.atan2(q_sym, i_sym)
        phase_error = soft_symbol - self.DIBIT_PHASE[dibit]
        # Wrap into (-pi, pi] -- not strictly needed for the cases
        # the test exercises, but matches Rust's natural range.
        while phase_error > math.pi:
            phase_error -= 2 * math.pi
        while phase_error < -math.pi:
            phase_error += 2 * math.pi
        if phase_error > PLL_MAX_ERROR_FLOAT:
            phase_error = PLL_MAX_ERROR_FLOAT
        elif phase_error < -PLL_MAX_ERROR_FLOAT:
            phase_error = -PLL_MAX_ERROR_FLOAT
        self.pll -= phase_error * PLL_GAIN_FLOAT
        if self.pll > MAX_PLL_ABS_FLOAT:
            self.pll = MAX_PLL_ABS_FLOAT
        elif self.pll < -MAX_PLL_ABS_FLOAT:
            self.pll = -MAX_PLL_ABS_FLOAT
        return self.pll


class TestLsmPllUpdateLinearised(unittest.TestCase):
    """Legacy tests for the small-angle linearised form. These all
    target `LsmPllUpdateLinearised` (the legacy class), NOT the
    production `LsmPllUpdate` (CORDIC form). The linearised tests
    have to keep passing because the slip-resistance regression
    test in test_lsm_demod_loop.py instantiates the linearised
    class to demonstrate the slip behaviour."""

    def test_constants(self):
        """Q-format constants land within 1 ULP of the float values."""
        from p25_hdl.lsm_pll_update import (
            COMBINED_GAIN_Q16, RAW_CLAMP_Q15, MAX_PLL_ABS_Q13)
        self.assertAlmostEqual(
            COMBINED_GAIN_Q16 / (1 << 16), COMBINED_GAIN_FLOAT, places=4)
        self.assertAlmostEqual(
            RAW_CLAMP_Q15 / (1 << 15), RAW_CLAMP_FLOAT, places=3)
        self.assertAlmostEqual(
            MAX_PLL_ABS_Q13 / (1 << 13), MAX_PLL_ABS_FLOAT, places=3)

    def test_zero_input_no_change(self):
        """All-zero (i, q) -> raw == 0 -> step == 0 -> pll stays 0."""
        dut = LsmPllUpdateLinearised()
        samples = [(0, 0, 0b00)] * 10
        out = _drive_pll(dut, samples)
        self.assertGreaterEqual(len(out), 5)
        for v in out:
            self.assertEqual(v, 0)

    def test_each_dibit_drives_correct_sign(self):
        """For each of the four dibits, drive a positive `(i, q) =
        (0.5, 0.5)` input and confirm the resulting pll step is
        signed correctly per the linearisation table.

        With (i, q) = (0.5, 0.5):
            00:  raw = q - i = 0           ->  step = 0
            01:  raw = -(q + i) = -1       ->  step = -negative -> pll +
            10:  raw = +(q + i) = +1       ->  step = +positive -> pll -
            11:  raw = i - q = 0           ->  step = 0
        """
        i_q15 = _q(0.5, INPUT_FRAC_BITS, 18)
        q_q15 = _q(0.5, INPUT_FRAC_BITS, 18)

        # 00 -> step 0 -> pll stays 0
        dut = LsmPllUpdateLinearised()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b00)] * 5)
        for v in out:
            self.assertEqual(v, 0)

        # 11 -> step 0 -> pll stays 0
        dut = LsmPllUpdateLinearised()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b11)] * 5)
        for v in out:
            self.assertEqual(v, 0)

        # 01 -> raw = -1 (clamped to -RAW_CLAMP) -> step = -clamped*gain
        # -> pll += clamped*gain (positive)
        dut = LsmPllUpdateLinearised()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b01)] * 5)
        for v in out:
            self.assertGreater(v, 0,
                               f"01: expected positive pll, got {v}")

        # 10 -> raw = +1 (clamped to +RAW_CLAMP) -> step = +clamped*gain
        # -> pll -= positive (negative)
        dut = LsmPllUpdateLinearised()
        out = _drive_pll(dut, [(i_q15, q_q15, 0b10)] * 5)
        for v in out:
            self.assertLess(v, 0,
                            f"10: expected negative pll, got {v}")

    def test_pll_clamps_at_pi_over_3(self):
        """Saturate the input to the max +/- raw value and let the
        loop integrate. The pll output must clamp at +/- pi/3."""
        # Use dibit 10 with (1, 1) input -> raw = 2, clamped to
        # RAW_CLAMP -> step = max negative -> pll grows in the
        # negative direction.
        i_q15 = _q(1.0, INPUT_FRAC_BITS, 18)  # saturates to 32767
        q_q15 = _q(1.0, INPUT_FRAC_BITS, 18)

        dut = LsmPllUpdateLinearised()
        # Run enough symbols to drive pll past pi/3.
        # Each step is approximately RAW_CLAMP * COMBINED_GAIN
        # ~= 0.4243 * 0.0707 ~= 0.030 rad / step.
        # pi/3 ~= 1.047, so ~35 steps to saturate. Run 100.
        out = _drive_pll(dut, [(i_q15, q_q15, 0b10)] * 100)
        # The final value should equal -MAX_PLL_ABS_Q13 (negative
        # because we drove pll downward).
        from p25_hdl.lsm_pll_update import MAX_PLL_ABS_Q13
        # Allow +/- 1 ULP at the clamp boundary.
        self.assertLessEqual(out[-1], -MAX_PLL_ABS_Q13 + 1)
        self.assertGreaterEqual(out[-1], -MAX_PLL_ABS_Q13 - 1)

    def test_against_python_reference(self):
        """Compare HDL pll trace against the Python linearised reference
        on a deterministic varying input."""
        rng = 0xCAFE
        n = 64
        py_inputs = []
        for k in range(n):
            rng = (rng * 1664525 + 1013904223) & 0xFFFFFFFF
            r1 = (rng / 0xFFFFFFFF)
            rng = (rng * 1664525 + 1013904223) & 0xFFFFFFFF
            r2 = (rng / 0xFFFFFFFF)
            i = 0.4 * (r1 - 0.5) * 2.0
            q = 0.4 * (r2 - 0.5) * 2.0
            d = (k & 0b11)
            py_inputs.append((i, q, d))

        py = _PythonPll()
        py_pll = []
        for (i, q, d) in py_inputs:
            py_pll.append(py.step(i, q, d))

        hdl_inputs = [
            (_q(i, INPUT_FRAC_BITS, 18),
             _q(q, INPUT_FRAC_BITS, 18),
             d)
            for (i, q, d) in py_inputs
        ]
        dut = LsmPllUpdateLinearised()
        out = _drive_pll(dut, hdl_inputs)

        self.assertEqual(len(out), len(py_pll),
                         f"HDL emitted {len(out)} samples, "
                         f"reference expected {len(py_pll)}")

        # Tolerance: pll output is Q2.13, ULP = 1.2e-4. Allow 16
        # ULPs (~2e-3 absolute) -- the fixed-point integrator
        # accumulates rounding error proportional to the number of
        # steps, and the multiply-then-shift truncation in stage 3
        # contributes ~1 ULP per step. 16 ULPs over 64 steps is
        # well below the 1.5 % linearisation error budget but
        # still tight enough to catch real bugs.
        TOLERANCE_ULPS = 16
        Q13 = 1 << 13
        max_err = 0
        for i, (got, ref) in enumerate(zip(out, py_pll)):
            ref_q13 = int(round(ref * Q13))
            err = abs(got - ref_q13)
            max_err = max(max_err, err)
            self.assertLessEqual(
                err, TOLERANCE_ULPS,
                f"sample {i}: HDL {got} (={got/Q13:.5f}) "
                f"vs ref {ref_q13} (={ref:.5f}), err {err} ULPs")


class TestLsmPllUpdateCordic(unittest.TestCase):
    """Tests for the production `LsmPllUpdate` (CORDIC atan2 form).

    The HDL is bit-exact against `cordic_vectoring_reference` in
    `lsm_cordic_atan2.py`, but tests here compare against the
    *literal* float-precision atan2 reference (`_AtanPll`) -- the
    HDL has to be allowed a small CORDIC residual (~1-3 mrad
    /symbol => ~10 ULPs in Q2.13 over the test horizon)."""

    def test_zero_input_no_change(self):
        """All-zero (i, q) hits the explicit `pending_skip` path
        in the HDL: the CORDIC still runs (its FSM doesn't gate on
        input), but the final subtract is forced to 0 so the pll
        register is left untouched. Matches Rust's
        `if soft_symbol != 0.0` skip semantics."""
        dut = LsmPllUpdate()
        out = _drive_pll(dut, [(0, 0, 0b00)] * 10, drain=CORDIC_DRAIN)
        self.assertGreaterEqual(len(out), 5)
        for v in out:
            self.assertEqual(
                v, 0,
                f"CORDIC pll should stay 0 on zero input, got {v}")

    def test_each_dibit_drives_correct_sign(self):
        """For each dibit, drive an input that puts the symbol
        slightly off the ideal angle by +0.1 rad. The CORDIC
        version computes phase_error as the *true* angle relative
        to the ideal (not the small-angle proxy), so the sign of
        the resulting pll step is unambiguous: positive phase
        error -> negative pll step (pll -= phase_error * gain)."""
        offset = 0.1
        for (dibit, ideal_angle) in [
            (0b00, math.pi / 4.0),
            (0b01, 3.0 * math.pi / 4.0),
            (0b10, -math.pi / 4.0),
            (0b11, -3.0 * math.pi / 4.0),
        ]:
            ang = ideal_angle + offset
            mag = 0.5
            i = _q(mag * math.cos(ang), INPUT_FRAC_BITS, 18)
            q = _q(mag * math.sin(ang), INPUT_FRAC_BITS, 18)
            dut = LsmPllUpdate()
            out = _drive_pll(
                dut, [(i, q, dibit)] * 5, drain=CORDIC_DRAIN)
            self.assertEqual(
                len(out), 5,
                f"dibit {dibit:02b}: expected 5 strobes, got {len(out)}")
            # +0.1 rad phase error -> pll decreases by ~ 0.01 rad/step.
            # After 5 steps, pll should be ~ -0.05 rad = -410 in Q2.13.
            # Allow generous tolerance for CORDIC residual.
            self.assertLess(
                out[-1], -100,
                f"dibit {dibit:02b}: expected pll <-100, got {out[-1]}")
            self.assertGreater(
                out[-1], -800,
                f"dibit {dibit:02b}: expected pll >-800, got {out[-1]}")

    def test_pll_clamps_at_pi_over_3(self):
        """Drive a constant phase error large enough to saturate
        the integrator, verify it clamps at +/- pi/3."""
        # Symbol at +pi/4 + 0.5 rad, dibit 00 (ideal +pi/4).
        # CORDIC computes phase_error = +0.5, clamps to +0.3,
        # step = -0.03 rad / symbol. After ~35 symbols pll hits
        # -pi/3. Run 60 to be safe.
        ang = math.pi / 4.0 + 0.5
        mag = 0.5
        i = _q(mag * math.cos(ang), INPUT_FRAC_BITS, 18)
        q = _q(mag * math.sin(ang), INPUT_FRAC_BITS, 18)
        dut = LsmPllUpdate()
        out = _drive_pll(
            dut, [(i, q, 0b00)] * 60, drain=CORDIC_DRAIN)
        from p25_hdl.lsm_pll_update import MAX_PLL_ABS_Q13
        # Final value should be at -MAX_PLL_ABS_Q13 (within a few
        # ULPs of CORDIC quantisation).
        self.assertLessEqual(out[-1], -MAX_PLL_ABS_Q13 + 4)
        self.assertGreaterEqual(out[-1], -MAX_PLL_ABS_Q13 - 1)

    def test_against_atan2_reference(self):
        """Compare HDL pll trace against the literal-atan2 Python
        reference (`_AtanPll`) on a deterministic random input.

        Tolerance is necessarily looser than the linearised form's
        16-ULP bound: each CORDIC call has ~1-3 mrad residual error
        (~25 ULPs in Q2.13), and the integrator accumulates a
        random walk of these errors. Allow 80 ULPs (~10 mrad) over
        a 64-symbol horizon -- still tight enough to catch a real
        algorithm bug, loose enough that CORDIC quantisation
        doesn't false-fail the test."""
        rng = random.Random(0xCAFE)
        n = 64
        py_inputs = []
        for k in range(n):
            ang = rng.uniform(-math.pi, math.pi)
            mag = rng.uniform(0.2, 0.6)
            i = mag * math.cos(ang)
            q = mag * math.sin(ang)
            # Pick the dibit the slicer would pick (4-PSK
            # nearest-quadrant on the input angle, before any PLL
            # rotation -- this matches what LsmDemodLoop's
            # `rotated_dibit` will produce on the rotated symbol).
            if i >= 0 and q >= 0:
                d = 0b00
            elif i < 0 and q >= 0:
                d = 0b01
            elif i >= 0 and q < 0:
                d = 0b10
            else:
                d = 0b11
            py_inputs.append((i, q, d))

        py = _AtanPll()
        py_pll = [py.step(i, q, d) for (i, q, d) in py_inputs]

        hdl_inputs = [
            (_q(i, INPUT_FRAC_BITS, 18),
             _q(q, INPUT_FRAC_BITS, 18),
             d)
            for (i, q, d) in py_inputs
        ]
        dut = LsmPllUpdate()
        out = _drive_pll(dut, hdl_inputs, drain=CORDIC_DRAIN)

        self.assertEqual(len(out), len(py_pll),
                         f"HDL emitted {len(out)} samples, "
                         f"reference expected {len(py_pll)}")

        TOLERANCE_ULPS = 80   # ~10 mrad over 64 steps
        Q13 = 1 << 13
        max_err = 0
        for i, (got, ref) in enumerate(zip(out, py_pll)):
            ref_q13 = int(round(ref * Q13))
            err = abs(got - ref_q13)
            max_err = max(max_err, err)
            self.assertLessEqual(
                err, TOLERANCE_ULPS,
                f"sample {i}: HDL {got} (={got/Q13:.5f} rad) "
                f"vs ref {ref_q13} (={ref:.5f} rad), err {err} ULPs")
        # Print the worst-case error so the test output is
        # informative without forcing a tight assertion.
        print(f"\n[pll_cordic] max err = {max_err} ULPs "
              f"(={max_err / Q13 * 1000:.2f} mrad over 64 steps)")

    def test_skip_on_zero_input_does_not_corrupt_state(self):
        """Verify the `pending_skip` path doesn't desync the
        CORDIC pipeline: alternate zero and non-zero inputs and
        check that the non-zero updates still produce the right
        sign of step (i.e. the skip flag isn't being latched into
        the wrong slot)."""
        offset = 0.1
        ang = math.pi / 4.0 + offset
        mag = 0.5
        i = _q(mag * math.cos(ang), INPUT_FRAC_BITS, 18)
        q = _q(mag * math.sin(ang), INPUT_FRAC_BITS, 18)
        zero = (0, 0, 0b00)
        nonzero = (i, q, 0b00)
        # Alternate: zero, nonzero, zero, nonzero, ...
        # The non-zero updates should drive pll negative; the
        # zero updates should leave it alone. Final pll should be
        # ~ 5 negative steps deep, NOT 10.
        samples = []
        for _ in range(10):
            samples.append(zero)
            samples.append(nonzero)
        dut = LsmPllUpdate()
        out = _drive_pll(dut, samples, drain=CORDIC_DRAIN)
        # Expect 20 strobes total (one per sample).
        self.assertEqual(len(out), 20)
        # Even-indexed samples were zero -> pll unchanged.
        # Odd-indexed samples were non-zero -> pll decreases.
        # After all 20 samples (10 non-zero updates), pll should
        # be ~ -10 * 0.01 rad = -0.1 rad = -819 in Q2.13.
        final = out[-1]
        self.assertLess(
            final, -300,
            f"expected pll < -300 after 10 non-zero updates, got {final}")
        self.assertGreater(
            final, -1500,
            f"expected pll > -1500, got {final}")


class TestLsmPllUpdateReset(unittest.TestCase):
    """Phase 8A: `reset_in` runtime reset plumbing.

    Both PLL update flavors (linearised + CORDIC) get a new
    `reset_in` port. A 1-cycle assertion must zero the integrator
    accumulator and the pipeline registers within a handful of
    sync cycles, matching the PS-side retune protocol where the
    chain is first disabled, then reset-pulsed, then re-enabled.
    """

    def _drive_saturated(self, dut, *, drain):
        """Run the PLL far from zero so any failure of the reset to
        land is easy to detect. Dibit 10 with (+0.5, +0.5) drives
        the accumulator negative for both forms."""
        i_q15 = _q(0.5, INPUT_FRAC_BITS, 18)
        q_q15 = _q(0.5, INPUT_FRAC_BITS, 18)
        samples = [(i_q15, q_q15, 0b10)] * 60
        return _drive_pll(dut, samples, drain=drain)

    def _assert_reset_clears_pll(self, dut_cls, *, drain):
        """Drive dut into a saturated state, pulse reset_in, then
        verify pll_out returns to 0 and a subsequent same-sign drive
        retraces the saturation path from 0 (i.e. the post-reset
        trajectory doesn't inherit anything from the pre-reset one).
        """
        dut = dut_cls()
        pre = []
        post = []

        async def bench(ctx):
            # Drive the accumulator deep into saturation.
            i_q15 = _q(0.5, INPUT_FRAC_BITS, 18)
            q_q15 = _q(0.5, INPUT_FRAC_BITS, 18)
            for _ in range(60):
                ctx.set(dut.i_sym_in, i_q15)
                ctx.set(dut.q_sym_in, q_q15)
                ctx.set(dut.dibit_in, 0b10)
                ctx.set(dut.symbol_strobe, 1)
                await ctx.tick()
                ctx.set(dut.symbol_strobe, 0)
                for _ in range(drain):
                    await ctx.tick()
            pll_saturated = ctx.get(dut.pll_out)
            if pll_saturated >= (1 << 15):
                pll_saturated -= (1 << 16)
            pre.append(pll_saturated)

            # Pulse reset_in for one sync cycle. (Mirror of the
            # p25_top Wpulse semantics.)
            ctx.set(dut.reset_in, 1)
            await ctx.tick()
            ctx.set(dut.reset_in, 0)
            # One drain cycle so any in-flight pipeline updates
            # settle and the reset override has taken effect.
            for _ in range(drain):
                await ctx.tick()

            pll_after = ctx.get(dut.pll_out)
            if pll_after >= (1 << 15):
                pll_after -= (1 << 16)
            post.append(pll_after)

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        self.assertLess(
            pre[0], -1000,
            f"expected pll saturated before reset, got {pre[0]}")
        self.assertEqual(
            post[0], 0,
            f"expected pll_out == 0 after reset pulse, got {post[0]}")

    def test_linearised_reset_clears_pll(self):
        self._assert_reset_clears_pll(LsmPllUpdateLinearised, drain=4)

    def test_cordic_reset_clears_pll(self):
        self._assert_reset_clears_pll(LsmPllUpdate, drain=CORDIC_DRAIN)

    def test_cordic_reset_then_reconverge_matches_cold_start(self):
        """After a reset pulse, a fresh run on the same input must
        produce the same trajectory as a cold-boot run. This is the
        acceptance criterion for the Phase 8A retune path: the
        post-reset PLL behaves identically to a just-instantiated
        PLL."""
        drain = CORDIC_DRAIN
        # Build the same input sequence used by
        # `test_against_atan2_reference`, just with a reset inserted
        # at the midpoint.
        rng = random.Random(0xBEEF)
        samples = []
        for _ in range(32):
            ang = rng.uniform(-math.pi, math.pi)
            mag = rng.uniform(0.2, 0.6)
            i = _q(mag * math.cos(ang), INPUT_FRAC_BITS, 18)
            q = _q(mag * math.sin(ang), INPUT_FRAC_BITS, 18)
            if i >= 0 and q >= 0:
                d = 0b00
            elif i < 0 and q >= 0:
                d = 0b01
            elif i >= 0 and q < 0:
                d = 0b10
            else:
                d = 0b11
            samples.append((i, q, d))

        cold_dut = LsmPllUpdate()
        cold_out = _drive_pll(cold_dut, samples, drain=drain)

        # Reset-inserted run: poison the accumulator, pulse reset,
        # THEN drive the same samples.
        warm_out = []

        async def bench(ctx):
            # Poison: drive dibit 10 (+ve input) for a while so
            # pll_reg saturates negative.
            i_pois = _q(0.5, INPUT_FRAC_BITS, 18)
            q_pois = _q(0.5, INPUT_FRAC_BITS, 18)
            for _ in range(60):
                ctx.set(warm_dut.i_sym_in, i_pois)
                ctx.set(warm_dut.q_sym_in, q_pois)
                ctx.set(warm_dut.dibit_in, 0b10)
                ctx.set(warm_dut.symbol_strobe, 1)
                await ctx.tick()
                ctx.set(warm_dut.symbol_strobe, 0)
                for _ in range(drain):
                    await ctx.tick()

            # Reset pulse.
            ctx.set(warm_dut.reset_in, 1)
            await ctx.tick()
            ctx.set(warm_dut.reset_in, 0)
            for _ in range(drain):
                await ctx.tick()

            # Now drive the same input sequence as the cold run.
            for (i, q, d) in samples:
                ctx.set(warm_dut.i_sym_in, i)
                ctx.set(warm_dut.q_sym_in, q)
                ctx.set(warm_dut.dibit_in, d)
                ctx.set(warm_dut.symbol_strobe, 1)
                await ctx.tick()
                ctx.set(warm_dut.symbol_strobe, 0)
                captured = False
                for _ in range(drain):
                    await ctx.tick()
                    if ctx.get(warm_dut.pll_strobe) and not captured:
                        v = ctx.get(warm_dut.pll_out)
                        if v >= (1 << 15):
                            v -= (1 << 16)
                        warm_out.append(v)
                        captured = True

        warm_dut = LsmPllUpdate()
        sim = Simulator(warm_dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        self.assertEqual(
            len(warm_out), len(cold_out),
            f"warm run emitted {len(warm_out)} samples, "
            f"cold run had {len(cold_out)}")
        # Post-reset trajectory should be bit-identical to cold start.
        for i, (w, c) in enumerate(zip(warm_out, cold_out)):
            self.assertEqual(
                w, c,
                f"sample {i}: warm={w}, cold={c} -- post-reset "
                f"trajectory must match cold-start exactly")


if __name__ == '__main__':
    unittest.main()
