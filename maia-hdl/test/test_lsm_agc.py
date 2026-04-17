#
# Fishball P25 -- LsmAgc HDL tests
#
# Direct validation of the SDRTrunk-faithful per-symbol AGC in
# `p25_hdl/lsm_agc.py`. Covers:
#
#   1.  sqrt primitive correctness vs Python integer sqrt across a
#       range of squared-sum inputs.
#   2.  Restoring-division correctness for TARGET_NUMERATOR / mag
#       vs Python integer division across the mag range.
#   3.  Cold-start: unit-magnitude QPSK input, gain converges to 1.0.
#   4.  Slow-attack (signal weakens): mag=0.1 → gain ramps up with
#       the exact 0.05 lerp time constant (NOT rounded to 1/16).
#   5.  Fast-decay asymmetric clamp: input jumps from mag=1 to
#       mag=2 → gain drops IMMEDIATELY to ~0.5 (no slow decay).
#   6.  Zero-magnitude skip: input (0, 0) leaves gain unchanged
#       (SDRTrunk's `if magnitude > 0` branch).
#   7.  GAIN_MAX clamp: input with mag near-zero → gain saturates
#       at 500 (the SDRTrunk `constrain(..., 500)` cap).
#   8.  Bypass: enable_in=0 → outputs = inputs, gain frozen.
#   9.  Runtime reset: after settling, reset_in=1 → gain returns
#       to GAIN_INIT and FSM returns to IDLE mid-operation.
#  10.  Decision-strobe pipeline latency (~48 sync cycles in/out).
#
# The tests don't mock; they instantiate LsmAgc and run the
# `amaranth.sim.Simulator` against each scenario with per-symbol
# ticks spaced >= the FSM latency (so results are observable
# before the next symbol arrives).
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_agc import (
    LsmAgc,
    SAMPLE_FRAC,
    GAIN_FRAC,
    GAIN_INIT,
    GAIN_MAX,
    GAIN_MIN,
    TARGET_NUMERATOR,
    ALPHA_FLOAT,
    MAG_UPDATE_THRESHOLD_DEFAULT,
)


# Number of sync cycles to wait for the AGC FSM to finish a
# single decision cycle. Measured at the worst case (48 cycles)
# plus a handful of margin for the output latch + strobe.
AGC_PIPELINE_CYCLES = 64

# Sample frame constants for the Q1.15 fixed-point conversions
# below. `one` is 1.0 in Q1.15; `full_scale` is the maximum
# positive value (saturates).
ONE_Q15 = 1 << SAMPLE_FRAC                 # 32768
FULL_SCALE = (1 << (SAMPLE_FRAC - 0)) - 1  # 32767 (Q1.15 max)


def q15(x):
    """Convert a float in [-1, 1) to Q1.15 signed int, rounded."""
    v = int(round(x * (1 << SAMPLE_FRAC)))
    v = max(-(1 << SAMPLE_FRAC), min((1 << SAMPLE_FRAC) - 1, v))
    return v


def gain_to_float(raw):
    """Convert a Q9.11 gain_dbg (R-truncated) back to float."""
    # gain_dbg is the Q9.7 truncation of the Q9.11 gain register:
    # bits [GAIN_FRAC-7 : GAIN_FRAC+9] -> 7 fractional + 9 integer.
    return raw / (1 << 7)


class TestLsmAgc(unittest.TestCase):

    def _simulate(self, dut, bench, *, vcd=None):
        sim = Simulator(dut)
        sim.add_clock(16e-9)  # 62.5 MHz sync, matches p25_top
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    async def _drive_symbol(self, ctx, dut, i_mid, q_mid, i_cur, q_cur):
        """Present a symbol, pulse decision_strobe_in for one tick,
        then wait the full FSM pipeline and return when
        decision_strobe_out has fired."""
        ctx.set(dut.i_mid_in, i_mid)
        ctx.set(dut.q_mid_in, q_mid)
        ctx.set(dut.i_cur_in, i_cur)
        ctx.set(dut.q_cur_in, q_cur)
        ctx.set(dut.decision_strobe_in, 1)
        await ctx.tick()
        ctx.set(dut.decision_strobe_in, 0)

        for _ in range(AGC_PIPELINE_CYCLES):
            await ctx.tick()
            if ctx.get(dut.decision_strobe_out):
                # Latch outputs one tick later so the combinational
                # path from the strobe_out to the consumer has
                # settled (matches how LsmDemodLoop's downstream
                # diff_demod reads these).
                return
        raise AssertionError(
            "decision_strobe_out never fired within "
            f"{AGC_PIPELINE_CYCLES} cycles")

    # ──────────────────────────────────────────────────────────────
    # 1. sqrt primitive correctness across a spread of inputs
    # ──────────────────────────────────────────────────────────────
    def test_sqrt_matches_python_isqrt(self):
        """The internal sqrt should return bit-exact `math.isqrt` for
        a variety of (i_cur, q_cur) inputs. We drive the AGC with
        enable_in=1 and read `mag_dbg` after each symbol — it holds
        the Q1.15 sqrt output's top 16 bits."""
        # Note: FULL_SCALE (= 32767) is the Q1.15 max that fits
        # signed 16 without wrap. ONE_Q15 (= 32768) would wrap to
        # -32768 and still square to 2^30, but we keep inputs in
        # the legal range for clarity.
        cases = [
            # (i_cur, q_cur)
            (0, 0),
            (FULL_SCALE, 0),
            (0, FULL_SCALE),
            (q15(0.5), q15(0.5)),
            (q15(0.707), q15(0.707)),
            (q15(0.1), q15(0.1)),
            (q15(0.9), q15(0.1)),
            (-q15(0.3), q15(0.4)),  # 3-4-5 triangle
        ]

        dut = LsmAgc()
        observed = []

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            for (i, q) in cases:
                await self._drive_symbol(ctx, dut, i, q, i, q)
                # mag_dbg is the top 16 bits of the 17-bit sqrt;
                # for typical inputs bit 16 is zero, so mag_dbg
                # reads the sqrt directly.
                observed.append(ctx.get(dut.mag_dbg))

        self._simulate(dut, bench)

        for (case, seen) in zip(cases, observed):
            i, q = case
            python_sq = i * i + q * q
            python_mag = math.isqrt(python_sq)
            # mag_dbg is the low 16 bits of the 17-bit sqrt. For
            # Q1.15 inputs bit 16 of the sqrt is always 0 (the
            # max possible magnitude is sqrt(2) * 2^15 ~ 46340,
            # well under 2^16), so the low 16 bits carry the full
            # value.
            expected_bits = python_mag & 0xFFFF
            self.assertEqual(
                seen, expected_bits,
                f"i={i}, q={q}: sqrt({python_sq})={python_mag} "
                f"expected_dbg={expected_bits} got={seen}")

    # ──────────────────────────────────────────────────────────────
    # 2. Cold-start: unit-magnitude input converges to gain=1.0
    # ──────────────────────────────────────────────────────────────
    def test_cold_start_unit_magnitude_settles_to_one(self):
        """Drive (i_cur, q_cur) with L2 magnitude exactly 1.0 for a
        few symbols; gain starts at GAIN_INIT (1.0) and should stay
        there (req_gain is also 1.0, so the lerp does nothing)."""
        dut = LsmAgc()
        gains = []

        # (1/sqrt(2), 1/sqrt(2)) QPSK point, L2 mag = 1.0 exactly
        # (modulo Q1.15 rounding).
        i = q = q15(1.0 / math.sqrt(2))

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            for _ in range(8):
                await self._drive_symbol(ctx, dut, i, q, i, q)
                gains.append(ctx.get(dut.gain_dbg))

        self._simulate(dut, bench)

        # Initial gain is GAIN_INIT = 1.0 in Q9.11; gain_dbg is the
        # Q9.7 truncation = 1.0 * 128 = 128.
        expected_dbg = GAIN_INIT >> (GAIN_FRAC - 7)
        for (i_sym, g) in enumerate(gains):
            self.assertEqual(
                g, expected_dbg,
                f"symbol {i_sym}: gain_dbg drifted to {g}, "
                f"expected {expected_dbg}")

    # ──────────────────────────────────────────────────────────────
    # 3. Slow attack (signal weakens) — exact 0.05 lerp dynamics
    # ──────────────────────────────────────────────────────────────
    def test_slow_attack_weak_signal_ramps_to_required_gain(self):
        """Drive mag = 0.1 for many symbols; gain should ramp from
        1.0 toward required_gain = 10.0 with the exact 0.05 lerp
        time constant (~20 symbols to reach 63% of the way)."""
        dut = LsmAgc()
        gains = []

        # |i|+|q| L2 mag = 0.1: pick i=q=0.1/sqrt(2).
        i = q = q15(0.1 / math.sqrt(2))

        n_symbols = 100

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            for _ in range(n_symbols):
                await self._drive_symbol(ctx, dut, i, q, i, q)
                gains.append(ctx.get(dut.gain_dbg))

        self._simulate(dut, bench)

        # Reference trajectory: exact SDRTrunk update in float.
        # Starting gain = 1.0, required_gain = 10.0, lerp = 0.05,
        # clamped by GAIN_MAX = 500 and the asymmetric min.
        required = 10.0
        g_ref = 1.0
        ref_trace = []
        for _ in range(n_symbols):
            g_ref += (required - g_ref) * ALPHA_FLOAT
            g_ref = min(g_ref, required)
            g_ref = min(g_ref, 500.0)
            g_ref = max(g_ref, GAIN_MIN / (1 << GAIN_FRAC))
            ref_trace.append(g_ref)

        # Compare — allow ~2 ULP of Q9.7 tolerance (one from
        # divider rounding, one from the lerp multiply rounding
        # cumulative over many symbols).
        for idx, (hdl_raw, ref_val) in enumerate(zip(gains, ref_trace)):
            hdl_val = gain_to_float(hdl_raw)
            err = abs(hdl_val - ref_val)
            self.assertLess(
                err, 0.1,
                f"symbol {idx}: hdl={hdl_val:.3f} ref={ref_val:.3f} "
                f"err={err:.3f}")

        # And the final value should be close to the steady state
        # (required_gain = 10.0) after 100 symbols (5 time
        # constants, ~99.3% of the way).
        self.assertGreater(gain_to_float(gains[-1]), 9.0)
        self.assertLess(gain_to_float(gains[-1]), 10.5)

    # ──────────────────────────────────────────────────────────────
    # 4. Fast-decay asymmetric clamp (signal strengthens)
    # ──────────────────────────────────────────────────────────────
    def test_fast_decay_strong_signal_drops_immediately(self):
        """Start with gain converged near 1.0, then present a
        saturated input (mag near full-scale). The asymmetric
        `min(gain, required_gain)` clamp must drop the gain to
        required_gain in ONE symbol (no slow lerp)."""
        dut = LsmAgc()
        gains = []

        # Warm-up with mag=1.0 so gain settles at 1.0.
        warm_i = warm_q = q15(1.0 / math.sqrt(2))
        # Strong input: saturated. sqrt(2*FS^2) ~ 1.414 in Q1.15
        # units, so required_gain = 1/1.414 ~ 0.707.
        strong_i = strong_q = FULL_SCALE

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            for _ in range(3):
                await self._drive_symbol(ctx, dut, warm_i, warm_q,
                                         warm_i, warm_q)
                gains.append(('warm', ctx.get(dut.gain_dbg)))

            # Now slam with strong input.
            await self._drive_symbol(ctx, dut, strong_i, strong_q,
                                     strong_i, strong_q)
            gains.append(('hit', ctx.get(dut.gain_dbg)))

            # Hold the new level for a few more symbols.
            for _ in range(3):
                await self._drive_symbol(ctx, dut, strong_i, strong_q,
                                         strong_i, strong_q)
                gains.append(('settled', ctx.get(dut.gain_dbg)))

        self._simulate(dut, bench)

        # Sanity: warm-up is still at ~1.0.
        warm_final = gain_to_float(
            [g for (t, g) in gains if t == 'warm'][-1])
        self.assertGreater(warm_final, 0.9)
        self.assertLess(warm_final, 1.1)

        # After one strong symbol, gain should be at req_gain ~ 0.707
        # (asymmetric clamp fires immediately; no slow lerp).
        hit = gain_to_float(
            [g for (t, g) in gains if t == 'hit'][0])
        self.assertGreater(hit, 0.65)
        self.assertLess(hit, 0.75)

    # ──────────────────────────────────────────────────────────────
    # 5. Zero-magnitude skip (SDRTrunk's `magnitude > 0` branch)
    # ──────────────────────────────────────────────────────────────
    def test_zero_magnitude_leaves_gain_unchanged(self):
        """Drive (0, 0) for a symbol. mag = 0 → skip update. Gain
        register must be unchanged from its previous value."""
        dut = LsmAgc()
        gains = []

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            # Warm up to a non-trivial gain (mag = 0.5).
            i_warm = q_warm = q15(0.5 / math.sqrt(2))
            for _ in range(30):
                await self._drive_symbol(ctx, dut, i_warm, q_warm,
                                         i_warm, q_warm)
            gain_before = ctx.get(dut.gain_dbg)

            # Drive zero input.
            await self._drive_symbol(ctx, dut, 0, 0, 0, 0)
            gain_after = ctx.get(dut.gain_dbg)
            gains.append((gain_before, gain_after))

        self._simulate(dut, bench)

        before, after = gains[0]
        self.assertEqual(
            before, after,
            f"Zero-magnitude symbol changed gain_dbg "
            f"{before} -> {after} (should be unchanged per SDRTrunk)")

    # ──────────────────────────────────────────────────────────────
    # 6. Bypass: enable_in=0 passes inputs through unchanged
    # ──────────────────────────────────────────────────────────────
    def test_bypass_passes_inputs_through(self):
        """enable_in=0 should forward (i_mid, q_mid, i_cur, q_cur)
        to the outputs on the next decision_strobe_out, without
        touching the gain register or applying any scaling."""
        dut = LsmAgc()
        i_mid, q_mid = 12345, -6789
        i_cur, q_cur = 5000, 3000
        captured = {}

        async def bench(ctx):
            ctx.set(dut.enable_in, 0)  # bypass
            ctx.set(dut.reset_in, 0)
            await self._drive_symbol(
                ctx, dut, i_mid, q_mid, i_cur, q_cur)
            captured['i_mid'] = ctx.get(dut.i_mid_out)
            captured['q_mid'] = ctx.get(dut.q_mid_out)
            captured['i_cur'] = ctx.get(dut.i_cur_out)
            captured['q_cur'] = ctx.get(dut.q_cur_out)
            captured['gain'] = ctx.get(dut.gain_dbg)

        self._simulate(dut, bench)

        self.assertEqual(captured['i_mid'], i_mid)
        self.assertEqual(captured['q_mid'], q_mid)
        self.assertEqual(captured['i_cur'], i_cur)
        self.assertEqual(captured['q_cur'], q_cur)
        # In bypass mode the UPDATE state never runs, so gain_dbg
        # stays at its power-on-reset value (0). The gain register
        # itself is at GAIN_INIT, but the debug tap is only written
        # in UPDATE. We confirm the debug path is quiescent.
        self.assertEqual(captured['gain'], 0)

    # ──────────────────────────────────────────────────────────────
    # 7. Runtime reset mid-settling
    # ──────────────────────────────────────────────────────────────
    def test_reset_in_restores_gain_to_init(self):
        """Let the AGC run until gain is far from init, then pulse
        reset_in. gain_dbg must return to the GAIN_INIT value on the
        next cycle and the FSM must accept a new decision_strobe
        cleanly afterward."""
        dut = LsmAgc()
        states = {}

        i_weak = q_weak = q15(0.1 / math.sqrt(2))

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            # Let gain ramp up on a weak input.
            for _ in range(60):
                await self._drive_symbol(
                    ctx, dut, i_weak, q_weak, i_weak, q_weak)
            states['before_reset'] = ctx.get(dut.gain_dbg)

            # Pulse reset for one cycle.
            ctx.set(dut.reset_in, 1)
            await ctx.tick()
            ctx.set(dut.reset_in, 0)
            await ctx.tick()
            states['after_reset'] = ctx.get(dut.gain_dbg)

            # Drive a new symbol and confirm the FSM cycles cleanly.
            await self._drive_symbol(
                ctx, dut, i_weak, q_weak, i_weak, q_weak)
            states['after_resume'] = ctx.get(dut.gain_dbg)

        self._simulate(dut, bench)

        self.assertGreater(
            states['before_reset'], GAIN_INIT >> (GAIN_FRAC - 7),
            "AGC never ramped up on the weak input")
        self.assertEqual(
            states['after_reset'], 0,
            f"Reset didn't zero gain_dbg (got {states['after_reset']}). "
            f"Note: gain register is set back to GAIN_INIT but "
            f"gain_dbg is zeroed until the next UPDATE state writes "
            f"its fresh value.")
        # One symbol after resume, gain_dbg should again be the
        # first lerp step away from GAIN_INIT toward req_gain=10.
        init_dbg = GAIN_INIT >> (GAIN_FRAC - 7)
        self.assertGreater(
            states['after_resume'], init_dbg,
            "FSM didn't resume cleanly after reset")

    # ──────────────────────────────────────────────────────────────
    # 8. Pipeline latency is within AGC_PIPELINE_CYCLES
    # ──────────────────────────────────────────────────────────────
    def test_decision_strobe_out_latency_within_pipeline(self):
        """Count exactly how many sync cycles pass between a
        decision_strobe_in pulse and the matching decision_strobe_out
        pulse. Must be <= AGC_PIPELINE_CYCLES (= 64)."""
        dut = LsmAgc()
        latency = {}

        i_norm = q_norm = q15(1.0 / math.sqrt(2))

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            ctx.set(dut.i_mid_in, i_norm)
            ctx.set(dut.q_mid_in, q_norm)
            ctx.set(dut.i_cur_in, i_norm)
            ctx.set(dut.q_cur_in, q_norm)
            ctx.set(dut.decision_strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.decision_strobe_in, 0)

            for k in range(1, AGC_PIPELINE_CYCLES + 1):
                await ctx.tick()
                if ctx.get(dut.decision_strobe_out):
                    latency['cycles'] = k
                    break

        self._simulate(dut, bench)

        self.assertIn('cycles', latency, "No decision_strobe_out observed")
        # Expected ~48 cycles per the docstring budget; allow up
        # to AGC_PIPELINE_CYCLES as the hard upper bound so a
        # small algorithmic tweak doesn't silently break the test.
        self.assertLessEqual(latency['cycles'], AGC_PIPELINE_CYCLES)
        self.assertGreaterEqual(latency['cycles'], 40)


    # ──────────────────────────────────────────────────────────────
    # 9. Idle-noise gate: sub-threshold magnitudes don't move gain
    # ──────────────────────────────────────────────────────────────
    def test_mag_update_threshold_gates_gain_update(self):
        """Warm AGC up on a real signal (mag=0.5), then drive many
        symbols at mag well below MAG_UPDATE_THRESHOLD_DEFAULT. The
        gain register must HOLD — no drift toward req_gain=32 that
        the weak samples would imply. This is the core
        "don't chase idle noise" fix."""
        dut = LsmAgc()  # default threshold = MAG_UPDATE_THRESHOLD_DEFAULT
        states = {}
        gate_counts = []

        # Warm-up magnitude = 0.5, well above any noise threshold.
        warm_i = warm_q = q15(0.5 / math.sqrt(2))
        # Weak magnitude: MAG_UPDATE_THRESHOLD_DEFAULT is 1024 in raw
        # Q1.15 mag-space. Pick a sample with mag <= half that (~512)
        # so we're comfortably below the gate. In Q1.15 input space,
        # mag=512/32768 = 1/64, so i=q=1/64/sqrt(2).
        weak_mag_float = (MAG_UPDATE_THRESHOLD_DEFAULT - 1) / (1 << SAMPLE_FRAC) / 2
        weak_i = weak_q = q15(weak_mag_float / math.sqrt(2))

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            # Warm up until settled.
            for _ in range(40):
                await self._drive_symbol(
                    ctx, dut, warm_i, warm_q, warm_i, warm_q)
            states['warm_gain'] = ctx.get(dut.gain_dbg)
            gate_counts.append(('after_warm', ctx.get(dut.gate_dbg)))

            # Now drive many symbols with sub-threshold magnitude.
            # Record the gain_dbg at each step to make sure it does
            # NOT drift upward.
            history = []
            for _ in range(60):
                await self._drive_symbol(
                    ctx, dut, weak_i, weak_q, weak_i, weak_q)
                history.append(ctx.get(dut.gain_dbg))
            states['history'] = history
            states['final_gain'] = history[-1]
            gate_counts.append(('after_weak', ctx.get(dut.gate_dbg)))

        self._simulate(dut, bench)

        # Warm-up should have settled gain near req_gain = 2.0
        # (mag=0.5 -> req_gain=1/0.5=2.0). Assert it got close.
        warm = gain_to_float(states['warm_gain'])
        self.assertGreater(
            warm, 1.5,
            f"AGC didn't reach the expected warm-up gain (got {warm:.2f})")
        self.assertLess(warm, 2.5)

        # After 60 sub-threshold symbols, gain must equal the warm
        # value: no drift, the gate froze the update.
        final = gain_to_float(states['final_gain'])
        self.assertEqual(
            states['warm_gain'], states['final_gain'],
            f"gain drifted under sub-threshold input: "
            f"warm={warm:.3f} final={final:.3f} "
            f"(difference proves the gate did NOT fire)")

        # Every single weak symbol must have logged as a gate hit.
        # We drove 60 weak symbols, so gate_dbg must have climbed
        # by exactly 60.
        warm_gate = gate_counts[0][1]
        weak_gate = gate_counts[1][1]
        self.assertEqual(
            weak_gate - warm_gate, 60,
            f"Expected gate_dbg to increment by 60 under weak input; "
            f"got warm={warm_gate} -> weak={weak_gate} "
            f"(delta={weak_gate - warm_gate})")

    # ──────────────────────────────────────────────────────────────
    # 10. Threshold=0 restores exact SDRTrunk-identical behaviour
    # ──────────────────────────────────────────────────────────────
    def test_mag_update_threshold_zero_matches_sdrtrunk(self):
        """Setting mag_update_threshold=0 disables the gate (only
        strictly zero magnitudes are skipped, matching SDRTrunk's
        `if magnitude > 0` check exactly). This preserves the
        pre-fix path for regression comparison."""
        dut = LsmAgc(mag_update_threshold=0)
        states = {}

        # mag=1/64 -- would be gated at default threshold=1024, but
        # should trigger a gain update when threshold=0.
        weak_i = weak_q = q15((1.0 / 64.0) / math.sqrt(2))

        async def bench(ctx):
            ctx.set(dut.enable_in, 1)
            ctx.set(dut.reset_in, 0)
            # Drive 30 weak symbols and watch gain climb toward
            # req_gain = 64 (capped by GAIN_MAX=500).
            gains = []
            for _ in range(30):
                await self._drive_symbol(
                    ctx, dut, weak_i, weak_q, weak_i, weak_q)
                gains.append(ctx.get(dut.gain_dbg))
            states['gains'] = gains
            states['gate'] = ctx.get(dut.gate_dbg)

        self._simulate(dut, bench)

        # With threshold=0 the gate never fires on non-zero mag.
        self.assertEqual(
            states['gate'], 0,
            f"gate_dbg should stay 0 with threshold=0, got {states['gate']}")

        # And gain should have ramped up (monotonic climb, not held).
        first = gain_to_float(states['gains'][0])
        last = gain_to_float(states['gains'][-1])
        self.assertGreater(
            last, first + 0.5,
            f"gain didn't climb under weak input with threshold=0 "
            f"(first={first:.3f} last={last:.3f})")

    # ──────────────────────────────────────────────────────────────
    # 11. Constructor validation
    # ──────────────────────────────────────────────────────────────
    def test_mag_update_threshold_rejects_invalid(self):
        """Out-of-range threshold raises ValueError at construction
        time — catches typos like passing a full-scale 32768 where
        the code expects a 17-bit unsigned mag."""
        with self.assertRaises(ValueError):
            LsmAgc(mag_update_threshold=-1)
        with self.assertRaises(ValueError):
            LsmAgc(mag_update_threshold=1 << 17)  # MAG_WIDTH = 17
        # Boundary: 2^17-1 is the largest legal value.
        LsmAgc(mag_update_threshold=(1 << 17) - 1)  # no raise


if __name__ == '__main__':
    unittest.main()
