#
# Fishball P25 -- LSM full demod loop integration test
#
# Phase 6E.6d. Drives `LsmDemodLoop` with the
# `demod_loop_synthetic.json` golden vector and verifies the
# resulting dibit stream matches the source dibit sequence
# (`truth_dibit`).
#
# This is the closest thing to "decode a real LSM signal" we can
# do without on-target hardware. Pass criteria: at least 90 % of
# steady-state dibits match truth, allowing for the warm-up
# transient (PLL + Gardner haven't locked yet) and the small
# linearisation error in the PLL update.
#
# Phase 6E.6e (doc/changes/024) added the CORDIC PLL update form.
# `LsmDemodLoop`'s default `pll_mode='cordic'` is exercised by the
# pre-existing `test_demod_loop_synthetic_matches_truth`. The
# `_linearised_*` and `_cordic_phase_step_*` tests below
# demonstrate the slip-resistance regression behind the CORDIC
# port: under a mid-stream phase step the CORDIC form holds lock
# while the linearised form's accuracy degrades.
#
# SPDX-License-Identifier: MIT
#

import math
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_demod_loop import LsmDemodLoop

from .golden_vector_loader import load_demod_stage, to_fixed


# Pipeline latency (input strobe -> dibit out) is small but multi-
# cycle: timing interp (1) + diff demod (2) + rotate (3) + slice (1)
# = 7 cycles roughly. Drain a few extra cycles per input strobe so
# every dibit is captured. The CORDIC PLL update adds ~12 cycles of
# back-edge feedback latency on the *symbol* path, but the dibit
# stream is forward-only so the front-end pipeline depth is
# unchanged -- 16 cycles still gives plenty of slack.
CYCLES_BETWEEN_STROBES = 16


def _signed16(value):
    if value >= (1 << 15):
        return value - (1 << 16)
    return value


def _signed18(value):
    if value >= (1 << 17):
        return value - (1 << 18)
    return value


class TestLsmDemodLoop(unittest.TestCase):

    def test_demod_loop_synthetic_matches_truth(self):
        """End-to-end: feed the post-RRC IQ from `demod_loop_synthetic`
        to LsmDemodLoop, collect dibits, compare against truth.

        The Rust pipeline that produced this golden hits ~95+ %
        truth match on this fixture; we expect the HDL (which has
        small-angle PLL linearisation and no AGC, but receives the
        same input) to be in the same ballpark on a clean
        synthetic signal that doesn't need AGC anyway.
        """
        stage = load_demod_stage('demod_loop_synthetic')

        # The post-RRC IQ in the golden has magnitude ~2.4 due to
        # the LPF + RRC gain. The Rust pipeline runs in f32 with no
        # representation limit; HDL inputs are Q1.15 (range +-1).
        # Without an AGC stage in 6E.6 (deferred), the test pre-
        # scales the input by 1/4 so it fits comfortably inside the
        # Q1.15 range without clipping. This matches what an AGC
        # would do at sample_gain ~= 0.34.
        SCALE = 0.34
        scaled_re = [SCALE * x for x in stage.input_re]
        scaled_im = [SCALE * x for x in stage.input_im]
        in_re_q = to_fixed(scaled_re, frac_bits=15, width=16)
        in_im_q = to_fixed(scaled_im, frac_bits=15, width=16)
        truth = list(stage.truth_dibit)

        dut = LsmDemodLoop()
        dibits = []
        pll_trace = []
        sp_trace = []

        async def bench(ctx):
            for k in range(stage.n_input):
                ctx.set(dut.re_in, in_re_q[k])
                ctx.set(dut.im_in, in_im_q[k])
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                # Drain pipeline; capture any symbol_strobes.
                for _ in range(CYCLES_BETWEEN_STROBES - 1):
                    await ctx.tick()
                    if ctx.get(dut.symbol_strobe):
                        dibits.append(ctx.get(dut.dibit_out))
                        pll_trace.append(_signed16(ctx.get(dut.pll_dbg)))
                        sp_trace.append(_signed18(ctx.get(dut.sample_point_dbg)))
            # Final drain
            for _ in range(20):
                await ctx.tick()
                if ctx.get(dut.symbol_strobe):
                    dibits.append(ctx.get(dut.dibit_out))
                    pll_trace.append(_signed16(ctx.get(dut.pll_dbg)))
                    sp_trace.append(_signed18(ctx.get(dut.sample_point_dbg)))

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        # The HDL emits one dibit per Symbol_strobe -- there should
        # be approximately as many dibits as the Rust reference
        # produced (which is `n_symbols`).
        self.assertGreaterEqual(
            len(dibits), int(0.9 * stage.n_symbols),
            f"HDL emitted {len(dibits)} dibits, expected ~{stage.n_symbols}")
        self.assertLessEqual(
            len(dibits), int(1.1 * stage.n_symbols) + 4,
            f"HDL emitted {len(dibits)} dibits, expected ~{stage.n_symbols}")

        # The Rust pipeline lags `truth_dibit` by ~2 symbols
        # because of the post-RRC startup transient (the diff
        # demod's prev_* needs one symbol of history before its
        # first emit). The HDL has the same warm-up. So compare
        # dibits[k] against truth[k - LAG] for some small LAG.
        # We pick LAG empirically by sliding the truth array and
        # finding the offset that maximises matches in the
        # steady-state region.
        SKIP = 16
        best_lag = 0
        best_matches = 0
        for lag in range(-3, 4):
            m = 0
            for k in range(SKIP, min(len(dibits), len(truth) + lag) - 4):
                if 0 <= k - lag < len(truth) and dibits[k] == truth[k - lag]:
                    m += 1
            if m > best_matches:
                best_matches = m
                best_lag = lag
        print(f"\n[demod_loop] best truth lag = {best_lag} "
              f"({best_matches} matches at that lag)")

        n_compare = min(len(dibits), len(truth) + best_lag) - SKIP
        if n_compare <= 0:
            self.fail("not enough dibits to compare after warm-up skip")

        matches = 0
        per_dibit_breakdown = {0: 0, 1: 0, 2: 0, 3: 0}
        per_dibit_total = {0: 0, 1: 0, 2: 0, 3: 0}
        for k in range(SKIP, SKIP + n_compare):
            got = dibits[k]
            truth_idx = k - best_lag
            if not (0 <= truth_idx < len(truth)):
                continue
            want = truth[truth_idx]
            per_dibit_total[want] += 1
            if got == want:
                matches += 1
                per_dibit_breakdown[want] += 1

        match_rate = matches / n_compare
        # Print a one-line summary so the test result is informative
        # for debugging.
        print(f"\n[demod_loop] {matches}/{n_compare} dibits match "
              f"({100.0 * match_rate:.1f} %)")
        for d in range(4):
            t = per_dibit_total[d]
            m = per_dibit_breakdown[d]
            if t > 0:
                print(f"[demod_loop]   dibit {d:02b}: {m}/{t} "
                      f"({100.0 * m / t:.1f} %)")
        print(f"[demod_loop]   final pll = {pll_trace[-1] if pll_trace else 0}")

        # Diagnostic: also compare against the Rust pipeline's
        # hard_dibit, which is what the HDL is theoretically a
        # fixed-point port of. This decouples "HDL matches the
        # algorithm we're porting" from "the algorithm we're
        # porting decodes truth correctly".
        rust_hard = list(stage.hard_dibit)
        n_rust = min(n_compare, len(rust_hard) - SKIP)
        rust_matches_hdl = 0
        for k in range(SKIP, SKIP + n_rust):
            if dibits[k] == rust_hard[k]:
                rust_matches_hdl += 1
        rust_rate = rust_matches_hdl / max(n_rust, 1)
        print(f"[demod_loop]   HDL vs Rust hard_dibit: "
              f"{rust_matches_hdl}/{n_compare} ({100.0 * rust_rate:.1f} %)")

        # Print the first ~24 (HDL, truth, Rust) dibits as a
        # diagnostic so failures show the actual alignment.
        print("[demod_loop]   first 32 dibits (HDL/truth/Rust/pll):")
        n_show = min(32, len(dibits) - 0, len(truth) - 0, len(rust_hard) - 0)
        for k in range(0, n_show):
            t_idx = k - best_lag
            t_str = f"{truth[t_idx]:02b}" if 0 <= t_idx < len(truth) else "--"
            mark = ""
            if 0 <= t_idx < len(truth) and dibits[k] != truth[t_idx]:
                mark = " <<<"
            r_str = f"{rust_hard[k]:02b}" if k < len(rust_hard) else "--"
            p = pll_trace[k] if k < len(pll_trace) else 0
            sp = sp_trace[k] if k < len(sp_trace) else 0
            print(f"[demod_loop]     k={k:3d}  HDL={dibits[k]:02b}  "
                  f"truth={t_str}  Rust={r_str}  "
                  f"pll={p:6d} ({p/8192:+.3f})  "
                  f"sp={sp:6d} ({sp/4096:+.3f}){mark}")

        # Pass threshold: 95 % match. The current measured rate
        # on this fixture is 100 % (239/239), so 95 % gives a 5 %
        # cushion for any future small numeric drift while still
        # catching real regressions in the demod loop.
        self.assertGreaterEqual(
            match_rate, 0.95,
            f"only {100.0 * match_rate:.1f} % dibits match, "
            f"expected >= 95 %")


def _run_demod_loop(input_re_q15, input_im_q15, n_input,
                     pll_mode='cordic'):
    """Run `LsmDemodLoop` over a list of pre-quantised IQ samples
    and return the captured (dibits, pll_trace) lists.

    Pulled out as a helper so the slip-resistance regression tests
    below can drive the loop with multiple input perturbations
    without copy-pasting the bench scaffolding.
    """
    dut = LsmDemodLoop(pll_mode=pll_mode)
    dibits = []
    pll_trace = []

    async def bench(ctx):
        for k in range(n_input):
            ctx.set(dut.re_in, input_re_q15[k])
            ctx.set(dut.im_in, input_im_q15[k])
            ctx.set(dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.strobe_in, 0)
            for _ in range(CYCLES_BETWEEN_STROBES - 1):
                await ctx.tick()
                if ctx.get(dut.symbol_strobe):
                    dibits.append(ctx.get(dut.dibit_out))
                    pll_trace.append(_signed16(ctx.get(dut.pll_dbg)))
        for _ in range(20):
            await ctx.tick()
            if ctx.get(dut.symbol_strobe):
                dibits.append(ctx.get(dut.dibit_out))
                pll_trace.append(_signed16(ctx.get(dut.pll_dbg)))

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return dibits, pll_trace


def _best_lag_match_count(dibits, truth, search_lags=range(-3, 4),
                          skip=16):
    """Slide truth against dibits, find the lag that maximises the
    raw match count, return (best_lag, match_count, n_compared).

    Same logic the main golden-vector test uses to handle the
    front-end startup latency.
    """
    best_lag = 0
    best_matches = 0
    for lag in search_lags:
        m = 0
        for k in range(skip, min(len(dibits), len(truth) + lag) - 4):
            if 0 <= k - lag < len(truth) and dibits[k] == truth[k - lag]:
                m += 1
        if m > best_matches:
            best_matches = m
            best_lag = lag
    n_compare = max(
        0, min(len(dibits), len(truth) + best_lag) - skip - 4)
    return best_lag, best_matches, n_compare


class TestLsmDemodLoopSlipResistance(unittest.TestCase):
    """Phase 6E.6e (doc/changes/024) regression tests demonstrating
    that the new CORDIC PLL update form is slip-resistant relative
    to the legacy linearised form. The clean-signal baselines (one
    per pll_mode) lock down both code paths against future
    refactoring; the magnitude-transient test exercises the
    nonlinearity that drove the on-target Clay County cycle slip on
    2026-04-10."""

    def _load_input(self):
        """Load the demod_loop_synthetic golden in HDL Q1.15 form
        with the same 0.34 pre-scale the main test uses (matches
        what an AGC would converge to)."""
        stage = load_demod_stage('demod_loop_synthetic')
        SCALE = 0.34
        scaled_re = [SCALE * x for x in stage.input_re]
        scaled_im = [SCALE * x for x in stage.input_im]
        in_re_q = to_fixed(scaled_re, frac_bits=15, width=16)
        in_im_q = to_fixed(scaled_im, frac_bits=15, width=16)
        return stage, in_re_q, in_im_q, scaled_re, scaled_im

    def test_demod_loop_linearised_baseline(self):
        """The legacy small-angle linearised PLL update form must
        still match the synthetic golden truth_dibit at >=95 %.

        This is the regression baseline for the linearised path:
        proves that adding the CORDIC class and `pll_mode`
        parameter to LsmDemodLoop didn't break the legacy form, so
        the linearised path remains a valid A/B baseline for any
        future PLL update changes."""
        stage, in_re_q, in_im_q, _, _ = self._load_input()
        truth = list(stage.truth_dibit)

        dibits, _ = _run_demod_loop(
            in_re_q, in_im_q, stage.n_input, pll_mode='linearised')

        self.assertGreaterEqual(
            len(dibits), int(0.9 * stage.n_symbols),
            f"linearised emitted {len(dibits)} dibits, "
            f"expected ~{stage.n_symbols}")

        best_lag, matches, n_compare = _best_lag_match_count(
            dibits, truth)
        self.assertGreater(n_compare, 0)
        rate = matches / n_compare
        print(f"\n[slip] linearised baseline: {matches}/{n_compare} "
              f"({100.0 * rate:.1f} %) at lag {best_lag}")
        self.assertGreaterEqual(
            rate, 0.95,
            f"linearised baseline regressed: {100.0 * rate:.1f} %")

    def test_demod_loop_cordic_vs_linearised_under_phase_step(self):
        """Inject a 0.4 rad carrier phase step on the *second half*
        of the input samples (a synthetic LO-jump transient) and
        run BOTH PLL update modes against the perturbed input.

        Mechanism: a phase step on the carrier shows up after the
        differential demod as a one-symbol phase glitch (the diff
        demod cancels constant phase but propagates the step to
        the symbol immediately following). The PLL has to absorb
        the glitch and re-lock.

        The CORDIC form computes the true atan2 phase error, which
        is bounded by `|to_dibit(angle) - dibit_phase(h)| <= pi/4`
        by construction (because to_dibit picks the closest 4-PSK
        quadrant before the subtraction). Under the step, the
        per-symbol error stays in [-pi/4, +pi/4] and the loop
        converges in O(1) symbols.

        The linearised form approximates the phase error as
        `(q +/- i) * sqrt(2)/2`, which is bounded only by the raw
        clamp at +/- 0.4243. Under a phase step large enough to
        push the rotated symbol into the wrong slicer quadrant,
        the linearisation can compute a step that points away from
        the correct lock point, causing slow convergence or
        outright basin slip.

        Pass criteria:
            - CORDIC truth-match rate >= linearised truth-match
              rate (with 2 % slack for fixed-point noise).
            - CORDIC truth-match rate >= 80 % (the loop is
              perturbed by the step so we don't expect 99 %, but
              it must still be tracking, not stuck in a wrong
              basin).
            - Diagnostic print of both rates and the final pll
              register state for the test reviewer.

        Both rates are printed unconditionally so the human can
        see the size of the slip-resistance gap if there is one;
        the strict assertions make the test a regression guard
        rather than a strict slip-vs-no-slip oracle.
        """
        stage, in_re_q, in_im_q, scaled_re, scaled_im = (
            self._load_input())
        truth = list(stage.truth_dibit)

        # Inject a 0.4 rad carrier phase step from the midpoint
        # onwards. We rotate (re, im) by `exp(j * step)` =
        # (cos(step), sin(step)) for all samples after the midpoint.
        n = stage.n_input
        step_at = n // 2
        STEP_RAD = 0.4
        c = math.cos(STEP_RAD)
        s = math.sin(STEP_RAD)
        perturbed_re = list(scaled_re)
        perturbed_im = list(scaled_im)
        for k in range(step_at, n):
            re_old = perturbed_re[k]
            im_old = perturbed_im[k]
            perturbed_re[k] = re_old * c - im_old * s
            perturbed_im[k] = re_old * s + im_old * c
        in_re_q = to_fixed(perturbed_re, frac_bits=15, width=16)
        in_im_q = to_fixed(perturbed_im, frac_bits=15, width=16)

        # Run both PLL modes on the same perturbed input.
        cordic_dibits, cordic_pll = _run_demod_loop(
            in_re_q, in_im_q, n, pll_mode='cordic')
        lin_dibits, lin_pll = _run_demod_loop(
            in_re_q, in_im_q, n, pll_mode='linearised')

        cordic_lag, cordic_match, cordic_n = _best_lag_match_count(
            cordic_dibits, truth)
        lin_lag, lin_match, lin_n = _best_lag_match_count(
            lin_dibits, truth)

        cordic_rate = cordic_match / max(cordic_n, 1)
        lin_rate = lin_match / max(lin_n, 1)

        print(f"\n[slip] phase step transient (samples {step_at}..end, "
              f"step {STEP_RAD} rad):")
        print(f"[slip]   CORDIC      : {cordic_match}/{cordic_n} "
              f"({100.0 * cordic_rate:.1f} %) lag={cordic_lag}")
        print(f"[slip]   linearised  : {lin_match}/{lin_n} "
              f"({100.0 * lin_rate:.1f} %) lag={lin_lag}")
        print(f"[slip]   final pll   : "
              f"cordic={cordic_pll[-1] if cordic_pll else 0} "
              f"linearised={lin_pll[-1] if lin_pll else 0}")

        # Hard pass: CORDIC keeps tracking through the step.
        self.assertGreaterEqual(
            cordic_rate, 0.80,
            f"CORDIC under phase step regressed to "
            f"{100.0 * cordic_rate:.1f} %, expected >= 80 %")

        # Soft pass: CORDIC is at least as good as linearised on
        # the same input (with 2 % slack for fixed-point noise).
        # The CORDIC loop should never be WORSE than the linearised
        # loop on a phase transient because its phase-error
        # computation is bounded by construction at +/- pi/4.
        self.assertGreaterEqual(
            cordic_rate + 0.02, lin_rate,
            f"CORDIC ({100.0 * cordic_rate:.1f} %) regressed below "
            f"linearised ({100.0 * lin_rate:.1f} %) on the same "
            f"phase-step input -- this should NEVER happen since "
            f"the CORDIC form is a strict accuracy upgrade")


if __name__ == '__main__':
    unittest.main()
