#
# Fishball P25 -- PLL/timing "no signal" hold tests
#
# 1. `LsmSignalHold` hysteresis against a Python model: enter after
#    `enter_symbols` consecutive gated symbols, leave after
#    `exit_symbols` consecutive non-gated ones, reset -> held.
# 2. `LsmDemodLoop` integration: the synthetic golden with a carrier
#    offset, a sub-gate noise gap, then the golden again, with no
#    reset. With the hold the PLL keeps its locked value through the
#    gap, the Gardner correction is not applied (sample_point only
#    advances by the nominal samples-per-symbol), and the second
#    segment decodes at once. Without it (`hold_exit_symbols=0`,
#    the pre-2026-09-27 design) the PLL random-walks on the noise.
#
# SPDX-License-Identifier: MIT
#

import math
import random
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_demod_loop import (
    LsmDemodLoop,
    HOLD_ENTER_SYMBOLS_DEFAULT,
    HOLD_EXIT_SYMBOLS_DEFAULT,
)
from p25_hdl.lsm_signal_hold import LsmSignalHold

from .golden_vector_loader import load_demod_stage, to_fixed


CYCLES_BETWEEN_STROBES = 16
ONE_Q12 = 1 << 12


def _signed(value, width):
    return value - (1 << width) if value >= (1 << (width - 1)) else value


def _hold_model(gated_seq, enter, exit_):
    """Python reference: hold flag after each symbol."""
    hold, run, out = True, 0, []
    for g in gated_seq:
        if hold:
            if g:
                run = 0
            elif run >= exit_ - 1:
                hold, run = False, 0
            else:
                run += 1
        else:
            if not g:
                run = 0
            elif run >= enter - 1:
                hold, run = True, 0
            else:
                run += 1
        out.append(hold)
    return out


def _drive_hold(dut, gated_seq, *, reset_at=()):
    """Strobe one symbol every 4 cycles; return hold_out after each
    strobe (and after each reset pulse, tagged None)."""
    out = []

    async def bench(ctx):
        for k, g in enumerate(gated_seq):
            if k in reset_at:
                ctx.set(dut.reset_in, 1)
                await ctx.tick()
                ctx.set(dut.reset_in, 0)
                out.append((None, ctx.get(dut.hold_out)))
            ctx.set(dut.gated_in, int(g))
            ctx.set(dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.strobe_in, 0)
            # gated_in toggling between strobes must be ignored
            ctx.set(dut.gated_in, int(not g))
            await ctx.tick()
            out.append((k, ctx.get(dut.hold_out)))
            await ctx.tick()
            await ctx.tick()

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return out


class TestLsmSignalHold(unittest.TestCase):

    def test_defaults(self):
        # The documented choice (lsm_demod_loop.py): enter quickly,
        # leave only on a clear run of signal.
        self.assertEqual(HOLD_ENTER_SYMBOLS_DEFAULT, 4)
        self.assertEqual(HOLD_EXIT_SYMBOLS_DEFAULT, 8)

    def test_hysteresis_matches_model(self):
        rnd = random.Random(7)
        seq = ([False] * 12 + [True] * 3 + [False] * 5 + [True] * 4
               + [False] * 7 + [True] + [False] * 8
               + [rnd.random() < 0.3 for _ in range(200)])
        for enter, exit_ in ((4, 8), (1, 8), (1, 1), (3, 5)):
            dut = LsmSignalHold(enter_symbols=enter, exit_symbols=exit_)
            got = [h for k, h in _drive_hold(dut, seq)]
            self.assertEqual(
                got, [int(h) for h in _hold_model(seq, enter, exit_)],
                f"enter={enter} exit={exit_}")

    def test_starts_held_and_releases_after_exit_run(self):
        dut = LsmSignalHold(enter_symbols=4, exit_symbols=8)
        got = [h for _, h in _drive_hold(dut, [False] * 10)]
        self.assertEqual(got, [1] * 7 + [0] * 3)

    def test_isolated_gated_symbols_do_not_hold(self):
        dut = LsmSignalHold(enter_symbols=4, exit_symbols=8)
        seq = [False] * 8 + ([True] * 3 + [False]) * 20
        got = [h for _, h in _drive_hold(dut, seq)]
        self.assertEqual(got[7:], [0] * (len(seq) - 7))

    def test_reset_enters_hold(self):
        dut = LsmSignalHold(enter_symbols=4, exit_symbols=8)
        seq = [False] * 20
        out = _drive_hold(dut, seq, reset_at=(12,))
        tagged = dict((k, h) for k, h in out if k is not None)
        at_reset = [h for k, h in out if k is None]
        self.assertEqual(tagged[11], 0)
        self.assertEqual(at_reset, [1])
        # 8 more symbols above the gate needed after the reset.
        self.assertEqual([tagged[k] for k in range(12, 20)],
                         [1] * 7 + [0])

    def test_rejects_zero_counts(self):
        with self.assertRaises(ValueError):
            LsmSignalHold(enter_symbols=0, exit_symbols=8)
        with self.assertRaises(ValueError):
            LsmSignalHold(enter_symbols=4, exit_symbols=0)
        with self.assertRaises(ValueError):
            LsmDemodLoop(hold_enter_symbols=0, hold_exit_symbols=8)


# ── LsmDemodLoop integration ────────────────────────────────────────

CFO_HZ = 150.0
# 1498 samples = 230.1 symbols at 31.25 kSPS: the second segment keeps
# the first one's symbol timing, so the test isolates the PLL (timing
# re-acquisition of a new talker is covered by the recording replays
# in tools/p25_lsm_hdl_replay.py).
GAP_SAMPLES = 1498
# Gate raised to 4000 (the golden's |x| is ~26800) with gap noise at
# sigma 1500 per component (median |x| ~1770, ~3 % of symbols above
# the gate): noise large enough for the Gardner TED to produce
# non-zero corrections, so the timing mask is actually exercised.
GATE = 4000
NOISE_SIGMA = 1500


def _scenario():
    """(re, im, truth, seg2_start, gap_range) at the loop input."""
    stage = load_demod_stage('demod_loop_synthetic')
    SCALE = 0.34
    fs = stage.input_rate_hz
    n = stage.n_input
    rnd = random.Random(11)
    re, im = [], []
    k_total = 0

    def add_signal():
        nonlocal k_total
        for x_re, x_im in zip(stage.input_re, stage.input_im):
            ph = 2 * math.pi * CFO_HZ * k_total / fs
            c, s = math.cos(ph), math.sin(ph)
            re.append(SCALE * (x_re * c - x_im * s))
            im.append(SCALE * (x_re * s + x_im * c))
            k_total += 1

    add_signal()
    gap0 = len(re)
    for _ in range(GAP_SAMPLES):
        re.append(rnd.gauss(0.0, NOISE_SIGMA) / 32768.0)
        im.append(rnd.gauss(0.0, NOISE_SIGMA) / 32768.0)
        k_total += 1
    seg2 = len(re)
    add_signal()
    return (to_fixed(re, frac_bits=15, width=16),
            to_fixed(im, frac_bits=15, width=16),
            list(stage.truth_dibit), seg2, (gap0, seg2), n)


def _run(re_q, im_q, **kw):
    """Per symbol: (input index, dibit, pll, sample_point, hold)."""
    dut = LsmDemodLoop(**kw)
    rows = []

    async def bench(ctx):
        ctx.set(dut.agc_mag_update_threshold_in, GATE)
        for k in range(len(re_q)):
            ctx.set(dut.re_in, re_q[k])
            ctx.set(dut.im_in, im_q[k])
            ctx.set(dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.strobe_in, 0)
            for _ in range(CYCLES_BETWEEN_STROBES - 1):
                await ctx.tick()
                if ctx.get(dut.symbol_strobe):
                    rows.append((k, ctx.get(dut.dibit_out),
                                 _signed(ctx.get(dut.pll_dbg), 16),
                                 _signed(ctx.get(dut.sample_point_dbg), 18),
                                 ctx.get(dut.hold_dbg)))

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()
    return rows


def _match_rate(dibits, truth, skip):
    best = 0.0
    for lag in range(-3, 6):
        m = n = 0
        for k in range(skip, len(dibits)):
            if 0 <= k - lag < len(truth):
                n += 1
                m += dibits[k] == truth[k - lag]
        if n:
            best = max(best, m / n)
    return best


class TestLsmDemodLoopSignalHold(unittest.TestCase):

    @classmethod
    def setUpClass(cls):
        re_q, im_q, cls.truth, cls.seg2, cls.gap, cls.n_seg = _scenario()
        cls.new = _run(re_q, im_q)
        cls.legacy = _run(re_q, im_q, hold_exit_symbols=0)

    def _split(self, rows):
        g0, g1 = self.gap
        # A symbol is attributed to the input sample at which it
        # left the loop. Skip the first 12 symbols (~7 samples each)
        # of the gap: loop latency plus the 4-symbol hold entry.
        seg1 = [r for r in rows if r[0] < g0]
        gap = [r for r in rows if g0 + 12 * 7 <= r[0] < g1]
        seg2 = [r for r in rows if r[0] >= g1]
        return seg1, gap, seg2

    def test_signal_releases_hold(self):
        seg1, _, _ = self._split(self.new)
        self.assertGreater(len(seg1), 200)
        self.assertEqual([r[4] for r in seg1[40:]], [0] * (len(seg1) - 40))

    def test_pll_frozen_through_gap(self):
        seg1, gap, _ = self._split(self.new)
        self.assertGreater(len(gap), 150)
        self.assertTrue(all(r[4] == 1 for r in gap))
        plls = {r[2] for r in gap}
        self.assertEqual(len(plls), 1, f"pll moved in the gap: {sorted(plls)}")
        # ... at (close to) the value locked on the signal: at most
        # the 3 noise symbols before the hold engages moved it.
        locked = seg1[-1][2]
        self.assertLess(abs(gap[0][2] - locked), 3 * 246 + 1)
        # 150 Hz at 4800 sym/s = 0.196 rad/symbol = 1608 in Q2.13.
        self.assertGreater(abs(locked), 1000)

    def test_timing_not_adjusted_in_gap(self):
        _, gap, _ = self._split(self.new)
        sps_frac = round(31250 / 4800 * ONE_Q12) % ONE_Q12
        steps = {(b[3] - a[3]) % ONE_Q12 for a, b in zip(gap, gap[1:])}
        self.assertEqual(steps, {sps_frac})

    def test_legacy_walks_in_gap(self):
        _, gap, _ = self._split(self.legacy)
        plls = [r[2] for r in gap]
        self.assertGreater(max(plls) - min(plls), 1000)
        steps = {(b[3] - a[3]) % ONE_Q12 for a, b in zip(gap, gap[1:])}
        self.assertGreater(len(steps), 1)

    def test_second_segment_decodes_without_reset(self):
        _, _, seg2 = self._split(self.new)
        dibits = [r[1] for r in seg2]
        rate = _match_rate(dibits, self.truth, skip=30)
        print(f"\n[hold] second segment after the gap: {100 * rate:.1f} % "
              f"(first pll {seg2[0][2]}, last {seg2[-1][2]})")
        self.assertGreaterEqual(rate, 0.95)

    def test_legacy_mode_is_the_old_loop(self):
        # hold_exit_symbols=0 removes the hold: never asserted.
        self.assertEqual({r[4] for r in self.legacy}, {0})


if __name__ == '__main__':
    unittest.main()
