#
# Fishball hwval - pattern source tests (RateGen, WordPattern,
# SamplePattern, PRBS31 reference)
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from hwval_hdl.pattern import (
    RateGen, WordPattern, SamplePattern, prbs31_words, prbs31_step32,
    PRBS31_SEED, rate_inc)


def run_sim(dut, bench):
    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()


class TestPrbs31Reference(unittest.TestCase):
    def test_bit_recurrence(self):
        """Words match the bit recurrence a[n] = a[n-31] ^ a[n-28]."""
        seed = 0x1234_5678
        # History bits: s[i] is the bit generated i steps before the first
        # output, so a[-1-i] = s[i].
        bits = [(seed >> (30 - k)) & 1 for k in range(31)]  # a[-31] .. a[-1]
        n_words = 8
        for _ in range(32 * n_words):
            bits.append(bits[-31] ^ bits[-28])
        out = bits[31:]
        words = []
        for w in range(n_words):
            v = 0
            for b in out[32 * w:32 * w + 32]:
                v = (v << 1) | b
            words.append(v)
        self.assertEqual(prbs31_words(n_words, seed), words)

    def test_self_sync(self):
        words = prbs31_words(50)
        for a, b in zip(words, words[1:]):
            self.assertEqual(prbs31_step32(a & 0x7FFF_FFFF)[0], b)

    def test_zero_seed_rejected(self):
        with self.assertRaises(ValueError):
            prbs31_words(1, 0)


class TestRateGen(unittest.TestCase):
    def count_strobes(self, inc, cycles):
        dut = RateGen()
        result = {}

        async def bench(ctx):
            ctx.set(dut.inc, inc)
            ctx.set(dut.enable, 1)
            n = 0
            for _ in range(cycles + 1):
                await ctx.tick()
                n += ctx.get(dut.strobe)
            result['n'] = n
            # disable: no more strobes
            ctx.set(dut.enable, 0)
            await ctx.tick()
            quiet = 0
            for _ in range(20):
                await ctx.tick()
                quiet += ctx.get(dut.strobe)
            result['quiet'] = quiet

        run_sim(dut, bench)
        return result

    def test_rates(self):
        cycles = 2000
        for inc in [0, 1 << 28, rate_inc(8e6, 62.5e6), 0x8000_0000,
                    0xC000_0000]:
            with self.subTest(inc=hex(inc)):
                r = self.count_strobes(inc, cycles)
                self.assertEqual(r['n'], ((cycles + 1) * inc) >> 32)
                self.assertEqual(r['quiet'], 0)

    def test_max_rate(self):
        r = self.count_strobes(0xFFFF_FFFF, 500)
        # 501 cycles observed: a strobe on every cycle except the first
        self.assertEqual(r['n'], 500)

    def test_rate_inc_helper(self):
        self.assertEqual(rate_inc(62.5e6, 62.5e6), 0xFFFF_FFFF)
        self.assertEqual(rate_inc(8e6, 62.5e6), 549755814)


class TestWordPattern(unittest.TestCase):
    def capture(self, mode, n_strobes, *, tag=0, live=None, clear_at=None):
        dut = WordPattern()
        words = []
        result = {}

        async def bench(ctx):
            ctx.set(dut.mode, mode)
            ctx.set(dut.tag, tag)
            for k in range(n_strobes):
                if live is None:
                    ctx.set(dut.strobe, 1)
                else:
                    ctx.set(dut.live_data, live[k])
                    ctx.set(dut.live_valid, 1)
                    # strobes must be ignored in live mode
                    ctx.set(dut.strobe, k % 2)
                ctx.set(dut.clear, int(clear_at == k))
                await ctx.tick()
                if ctx.get(dut.valid):
                    words.append(ctx.get(dut.data))
                # idle cycle between words
                ctx.set(dut.strobe, 0)
                ctx.set(dut.live_valid, 0)
                ctx.set(dut.clear, 0)
                await ctx.tick()
                if ctx.get(dut.valid):
                    words.append(ctx.get(dut.data))
            await ctx.tick()
            if ctx.get(dut.valid):
                words.append(ctx.get(dut.data))
            result['count'] = ctx.get(dut.count)

        run_sim(dut, bench)
        return words, result['count']

    def test_off(self):
        words, count = self.capture(WordPattern.OFF, 10)
        self.assertEqual(words, [])
        self.assertEqual(count, 0)

    def test_ramp64(self):
        words, count = self.capture(WordPattern.RAMP64, 40)
        self.assertEqual(words, list(range(40)))
        self.assertEqual(count, 40)

    def test_tagged(self):
        words, count = self.capture(WordPattern.TAGGED, 20, tag=0xA)
        self.assertEqual(words, [(0xA << 60) | k for k in range(20)])

    def test_prbs31(self):
        words, count = self.capture(WordPattern.PRBS31, 64)
        ref = prbs31_words(64)
        self.assertEqual(words, [(k << 32) | ref[k] for k in range(64)])
        self.assertEqual(count, 64)

    def test_live(self):
        live = [(0x1111_2222_3333_4444 * (k + 1)) & (2**64 - 1)
                for k in range(15)]
        words, count = self.capture(WordPattern.LIVE, 15, live=live)
        self.assertEqual(words, live)
        self.assertEqual(count, 15)

    def test_clear_restarts_sequence(self):
        # clear in the cycle of strobe #10 suppresses that word and restarts
        words, count = self.capture(WordPattern.PRBS31, 30, clear_at=10)
        ref = prbs31_words(30)
        self.assertEqual(words[:10], [(k << 32) | ref[k] for k in range(10)])
        self.assertEqual(words[10:], [(k << 32) | ref[k] for k in range(19)])
        self.assertEqual(count, 19)

    def test_back_to_back(self):
        """A strobe every cycle produces a word every cycle."""
        dut = WordPattern()
        words = []

        async def bench(ctx):
            ctx.set(dut.mode, WordPattern.RAMP64)
            ctx.set(dut.strobe, 1)
            for _ in range(20):
                await ctx.tick()
                if ctx.get(dut.valid):
                    words.append(ctx.get(dut.data))

        run_sim(dut, bench)
        self.assertEqual(words, list(range(20)))


class TestSamplePattern(unittest.TestCase):
    def test_ramp_and_live(self):
        dut = SamplePattern()
        samples = []
        result = {}

        async def collect(ctx):
            await ctx.tick()
            if ctx.get(dut.strobe_out):
                samples.append((ctx.get(dut.re), ctx.get(dut.im)))

        async def bench(ctx):
            ctx.set(dut.mode, SamplePattern.RAMP)
            for k in range(10):
                ctx.set(dut.strobe, 1)
                await collect(ctx)
                ctx.set(dut.strobe, 0)
                for _ in range(k % 3):
                    await collect(ctx)
            result['count_ramp'] = ctx.get(dut.count)
            # live mode ignores strobe
            ctx.set(dut.mode, SamplePattern.LIVE)
            ctx.set(dut.strobe, 1)
            for k in range(5):
                ctx.set(dut.live_re, 0x100 + k)
                ctx.set(dut.live_im, 0xFF00 - k)
                ctx.set(dut.live_strobe, 1)
                await collect(ctx)
                ctx.set(dut.live_strobe, 0)
                await collect(ctx)
            ctx.set(dut.strobe, 0)
            await collect(ctx)
            result['count'] = ctx.get(dut.count)
            # off
            ctx.set(dut.mode, SamplePattern.OFF)
            ctx.set(dut.strobe, 1)
            for _ in range(4):
                await collect(ctx)
            ctx.set(dut.strobe, 0)
            # clear then ramp restarts at 0
            ctx.set(dut.clear, 1)
            await collect(ctx)
            ctx.set(dut.clear, 0)
            ctx.set(dut.mode, SamplePattern.RAMP)
            ctx.set(dut.strobe, 1)
            await collect(ctx)
            ctx.set(dut.strobe, 0)
            await collect(ctx)

        run_sim(dut, bench)
        self.assertEqual(samples[:10], [(k, 0) for k in range(10)])
        self.assertEqual(result['count_ramp'], 10)
        self.assertEqual(samples[10:15],
                         [(0x100 + k, 0xFF00 - k) for k in range(5)])
        self.assertEqual(result['count'], 15)
        self.assertEqual(samples[15:], [(0, 0)])

    def test_ramp_high_half(self):
        """im carries c[31:16]: check across the 16-bit boundary."""
        dut = SamplePattern()
        samples = []

        async def bench(ctx):
            ctx.set(dut.mode, SamplePattern.RAMP)
            ctx.set(dut.strobe, 1)
            for _ in range(0x10003):
                await ctx.tick()
                if ctx.get(dut.strobe_out):
                    samples.append((ctx.get(dut.re), ctx.get(dut.im)))

        run_sim(dut, bench)
        self.assertEqual(samples[0xFFFF], (0xFFFF, 0))
        self.assertEqual(samples[0x10000], (0, 1))
        self.assertEqual(samples[0x10001], (1, 1))


if __name__ == '__main__':
    unittest.main()
