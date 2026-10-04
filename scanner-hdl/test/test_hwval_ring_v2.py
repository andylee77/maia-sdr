#
# Fishball hwval - RingWriterV2 tests
#
# The ring is driven by RateGen + WordPattern (ramp64, data = sequence
# number) and writes into AxiWriteSlaveModel, which checks the AXI rules
# (VALID/payload stability, WLAST, 4 KiB) on every edge and records every
# burst.
#
# SPDX-License-Identifier: MIT
#

import random
import unittest

from amaranth import *
from amaranth.sim import Simulator

from hwval_hdl.lat_hist import LatencyTracker, latency_bin
from hwval_hdl.pattern import RateGen, WordPattern, rate_inc
from hwval_hdl.ring_v2 import RingWriterV2, ring_marker, PAD_MAGIC, HDR_MAGIC

from .hwval_axi_wmodel import AxiWriteSlaveModel


BASE = 0x2000_0000
MIB = 1 << 20


class RingV2Harness(Elaboratable):
    """RateGen -> WordPattern -> RingWriterV2, plus a manual strobe."""
    def __init__(self, **kwargs):
        self.rate = RateGen()
        self.pat = WordPattern()
        self.ring = RingWriterV2(**kwargs)
        self.man_strobe = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.rate = self.rate
        m.submodules.pat = self.pat
        m.submodules.ring = self.ring
        m.d.comb += [
            self.pat.strobe.eq(self.rate.strobe | self.man_strobe),
            self.ring.in_data.eq(self.pat.data),
            self.ring.in_valid.eq(self.pat.valid),
        ]
        return m


def classify(words):
    """Split a written word stream into data / header / pad words.

    Ramp data values stay far below 2**48 in these tests, so the magic in
    bits 63:48 is unambiguous.
    """
    out = []
    for w in words:
        magic = w >> 48
        if magic == HDR_MAGIC:
            out.append(('hdr', w))
        elif magic == PAD_MAGIC:
            out.append(('pad', w))
        else:
            out.append(('data', w))
    return out


def data_words(words):
    return [w for kind, w in classify(words) if kind == 'data']


class RingV2TestCase(unittest.TestCase):
    def make(self, *, fifo_depth=2048, max_outstanding=8, seed=0, **model_kw):
        self.h = RingV2Harness(fifo_depth=fifo_depth,
                               max_outstanding=max_outstanding)
        self.ring = self.h.ring
        self.model = AxiWriteSlaveModel(self.ring.axi, seed=seed, **model_kw)
        self.monitors = []

    def simulate(self, bench):
        sim = Simulator(self.h)
        sim.add_clock(16e-9)
        sim.add_testbench(self.model.bench, background=True)
        for mon in self.monitors:
            sim.add_testbench(mon, background=True)
        sim.add_testbench(bench)
        sim.run()

    def configure(self, ctx, *, base=BASE, size_bursts=64, subbuf_bursts=0,
                  header=0, protect=0, irq_every=0, irq_timeout=0,
                  flush_timeout=0, max_out=0, guard_lo=BASE,
                  guard_hi=BASE + 16 * MIB, consumer=0):
        r = self.ring
        ctx.set(r.base, base)
        ctx.set(r.size_bursts, size_bursts)
        ctx.set(r.subbuf_bursts, subbuf_bursts)
        ctx.set(r.header_enable, header)
        ctx.set(r.protect, protect)
        ctx.set(r.irq_every, irq_every)
        ctx.set(r.irq_timeout, irq_timeout)
        ctx.set(r.flush_timeout, flush_timeout)
        ctx.set(r.max_outstanding_cfg, max_out)
        ctx.set(r.guard_lo, guard_lo)
        ctx.set(r.guard_hi, guard_hi)
        ctx.set(r.consumer_bursts, consumer)

    async def pulse(self, ctx, sig):
        ctx.set(sig, 1)
        await ctx.tick()
        ctx.set(sig, 0)

    async def start(self, ctx, *, inc=None, mode=WordPattern.RAMP64):
        """Enable the ring, wait until it runs, then start the source."""
        ctx.set(self.ring.enable, 1)
        await self.wait_for(ctx, self.ring.enabled, 20)
        ctx.set(self.h.pat.mode, mode)
        if inc is not None:
            ctx.set(self.h.rate.inc, inc)
            ctx.set(self.h.rate.enable, 1)

    async def stop(self, ctx, timeout=20000):
        """Stop the source, disable the ring and wait for IDLE."""
        ctx.set(self.h.rate.enable, 0)
        await ctx.tick().repeat(3)
        ctx.set(self.ring.enable, 0)
        await ctx.tick()
        await self.wait_for(ctx, self.ring.idle, timeout)

    async def wait_for(self, ctx, sig, timeout):
        for _ in range(timeout):
            if ctx.get(sig):
                return
            await ctx.tick()
        self.fail(f'timeout waiting for {sig.name}')

    async def manual_words(self, ctx, n, gap=0):
        for _ in range(n):
            ctx.set(self.h.man_strobe, 1)
            await ctx.tick()
            ctx.set(self.h.man_strobe, 0)
            for _ in range(gap):
                await ctx.tick()

    def read_counters(self, ctx):
        r = self.ring
        names = ['committed_bursts', 'issued_bursts', 'words_in',
                 'drop_full', 'drop_protect', 'pad_words', 'flushes',
                 'bresp_err', 'fifo_hwm', 'max_outstanding_seen', 'lat_max',
                 'epoch', 'headers', 'guard_blocked', 'idle', 'enabled',
                 'fifo_empty']
        return {n: ctx.get(getattr(r, n)) for n in names}

    async def read_hist(self, ctx):
        vals = []
        for k in range(LatencyTracker.NUM_BINS):
            ctx.set(self.ring.hist_sel, k)
            await ctx.tick().repeat(2)
            vals.append(ctx.get(self.ring.hist_val))
        return vals

    def check_addresses(self, base, size_bursts, first_index=0):
        for k, b in enumerate(self.model.bursts):
            kk = k + first_index
            self.assertEqual(b['addr'], base + (kk % size_bursts) * 128,
                             f'burst {kk}')
            self.assertEqual(b['len'], 15)
            self.assertEqual(b['cache'], 0b0011)
            self.assertEqual(b['strb'], [0xFF] * 16)

    def check_accounting(self, c):
        """Idle writer: every input word was written or counted as dropped."""
        words = self.model.stream()
        kinds = classify(words)
        n_data = sum(1 for k, _ in kinds if k == 'data')
        n_pad = sum(1 for k, _ in kinds if k == 'pad')
        n_hdr = sum(1 for k, _ in kinds if k == 'hdr')
        self.assertEqual(c['words_in'],
                         n_data + c['drop_full'] + c['drop_protect'])
        self.assertEqual(c['pad_words'], n_pad)
        self.assertEqual(c['committed_bursts'], len(self.model.bursts))
        self.assertEqual(c['issued_bursts'], len(self.model.bursts))
        self.assertEqual(self.model.b_count, len(self.model.bursts))
        self.assertTrue(c['idle'])
        self.assertTrue(c['fifo_empty'])
        return n_data, n_pad, n_hdr


class TestRingV2Data(RingV2TestCase):
    def run_ramp(self, rate, seed, *, cycles=3000, size_bursts=37,
                 awready_prob=0.6, wready_prob=0.8, b_latency=(2, 80)):
        self.make(seed=seed, awready_prob=awready_prob,
                  wready_prob=wready_prob, b_latency=b_latency)
        res = {}

        async def bench(ctx):
            self.configure(ctx, size_bursts=size_bursts)
            await self.start(ctx, inc=rate_inc(rate, 1.0))
            await ctx.tick().repeat(cycles)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)
            res['gen'] = ctx.get(self.h.pat.count)
            res['hist'] = await self.read_hist(ctx)

        self.simulate(bench)
        c = res['c']
        n_data, n_pad, _ = self.check_accounting(c)
        self.check_addresses(BASE, size_bursts)
        self.assertEqual(c['drop_full'], 0)
        self.assertEqual(c['words_in'], res['gen'])
        self.assertEqual(data_words(self.model.stream()),
                         list(range(res['gen'])))
        # final flush pads the partial burst
        self.assertEqual((n_data + n_pad) % 16, 0)
        self.assertEqual(c['flushes'], 1 if n_pad else 0)
        self.assertEqual(c['epoch'], 1)
        self.assertLessEqual(self.model.outstanding_max, 8)
        self.assertEqual(sum(res['hist']), c['committed_bursts'])
        return c

    def test_ramp_low_rate(self):
        c = self.run_ramp(0.05, seed=1)
        self.assertGreater(c['committed_bursts'], 5)

    def test_ramp_mid_rate(self):
        self.run_ramp(0.3, seed=2)

    def test_ramp_high_rate(self):
        self.run_ramp(0.6, seed=3)

    def test_ramp_near_full_rate_ideal_axi(self):
        """0.95 words/cycle with an always-ready subordinate: no loss."""
        c = self.run_ramp(0.95, seed=4, awready_prob=1.0, wready_prob=1.0,
                          b_latency=(1, 6), cycles=2500)
        self.assertLess(c['fifo_hwm'], 64)

    def test_wrap_non_power_of_two(self):
        base = BASE + 0x3000
        size = 5
        self.make(seed=5, awready_prob=0.7, wready_prob=0.7,
                  b_latency=(2, 30))
        res = {}

        async def bench(ctx):
            self.configure(ctx, base=base, size_bursts=size)
            await self.start(ctx, inc=rate_inc(0.4, 1.0))
            await ctx.tick().repeat(1200)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        self.check_accounting(c)
        self.check_addresses(base, size)
        self.assertGreater(len(self.model.bursts), 4 * size)
        # nothing outside the ring, and memory holds the last lap
        lo, hi = base, base + size * 128
        self.assertTrue(all(lo <= addr < hi for addr in self.model.mem))
        for b in self.model.bursts[-size:]:
            for i, w in enumerate(b['data']):
                self.assertEqual(self.model.mem[b['addr'] + 8 * i], w)
        self.assertEqual(data_words(self.model.stream()),
                         list(range(c['words_in'])))

    def test_committed_only_after_b(self):
        """committed_bursts == number of B handshakes, never ahead of them."""
        self.make(seed=6, awready_prob=0.8, wready_prob=0.9,
                  b_latency=(150, 400))
        ring = self.ring
        stats = {'max_lag': 0, 'checks': 0}

        async def monitor(ctx):
            nb = 0
            async for _, _, bv, br in ctx.tick().sample(ring.axi.bvalid,
                                                        ring.axi.bready):
                nb += bv & br
                committed = ctx.get(ring.committed_bursts)
                issued = ctx.get(ring.issued_bursts)
                assert committed == nb, (committed, nb)
                stats['max_lag'] = max(stats['max_lag'], issued - committed)
                stats['checks'] += 1

        self.monitors.append(monitor)
        res = {}

        async def bench(ctx):
            self.configure(ctx)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(2000)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        self.check_accounting(res['c'])
        self.assertGreaterEqual(stats['max_lag'], 4)
        self.assertGreater(stats['checks'], 2000)
        self.assertGreaterEqual(res['c']['lat_max'], 150)

    def test_long_b_latency_drops(self):
        """Small FIFO + long B latency: drop_full counts every lost word."""
        self.make(fifo_depth=64, seed=7, awready_prob=0.9, wready_prob=0.9,
                  b_latency=(600, 900))
        res = {}

        async def bench(ctx):
            self.configure(ctx)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(6000)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)
            res['gen'] = ctx.get(self.h.pat.count)
            res['hist'] = await self.read_hist(ctx)

        self.simulate(bench)
        c = res['c']
        n_data, _, _ = self.check_accounting(c)
        self.assertGreater(c['drop_full'], 100)
        self.assertEqual(c['drop_protect'], 0)
        self.assertEqual(c['words_in'], res['gen'])
        data = data_words(self.model.stream())
        self.assertEqual(data, sorted(set(data)))  # strictly increasing
        missing = set(range(res['gen'])) - set(data)
        self.assertEqual(len(missing), c['drop_full'])
        self.assertEqual(c['max_outstanding_seen'], 8)
        self.assertEqual(self.model.outstanding_max, 8)
        self.assertGreaterEqual(c['lat_max'], 600)
        self.assertEqual(c['fifo_hwm'], 64)
        # the histogram agrees with the model's AW -> B latencies
        exp = [0] * 16
        for b in self.model.bursts:
            exp[latency_bin(b['b_cycle'] - b['aw_cycle'])] += 1
        self.assertEqual(res['hist'], exp)
        self.assertEqual(max(b['b_cycle'] - b['aw_cycle']
                             for b in self.model.bursts), c['lat_max'])

    def test_max_outstanding_cfg(self):
        self.make(seed=8, b_latency=(200, 200))
        res = {}

        async def bench(ctx):
            self.configure(ctx, max_out=2)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(1500)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        self.check_accounting(res['c'])
        self.assertEqual(self.model.outstanding_max, 2)
        self.assertEqual(res['c']['max_outstanding_seen'], 2)

    def test_bresp_error_counted(self):
        self.make(seed=9, awready_prob=0.8, b_latency=(2, 20),
                  bresp={2, 5})
        res = {}

        async def bench(ctx):
            self.configure(ctx)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(600)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        self.check_accounting(c)
        self.assertGreater(c['committed_bursts'], 6)
        self.assertEqual(c['bresp_err'], 2)


class TestRingV2Protocol(RingV2TestCase):
    def test_flush_timeout_and_pulse(self):
        self.make(seed=10, b_latency=(1, 4))
        ring = self.ring
        res = {}

        async def bench(ctx):
            self.configure(ctx, flush_timeout=100)
            await self.start(ctx)
            # (a) 5 words, then idle: padded after the timeout
            await self.manual_words(ctx, 5, gap=2)
            t0 = 0
            while self.model.b_count == 0:
                await ctx.tick()
                t0 += 1
                self.assertLess(t0, 1000)
            res['flush_latency'] = t0
            res['c1'] = self.read_counters(ctx)
            # (b) timeout disabled: 3 words sit in the FIFO until a pulse
            ctx.set(ring.flush_timeout, 0)
            await self.manual_words(ctx, 3, gap=1)
            await ctx.tick().repeat(400)
            res['bursts_before_pulse'] = len(self.model.bursts)
            await self.pulse(ctx, ring.flush)
            await ctx.tick().repeat(60)
            res['c2'] = self.read_counters(ctx)
            # (c) a flush pulse followed by a back-to-back run of words:
            # the words wait in the elastic FIFO during the padding
            await self.manual_words(ctx, 2)
            await ctx.tick().repeat(3)
            await self.pulse(ctx, ring.flush)
            ctx.set(self.h.man_strobe, 1)
            await ctx.tick().repeat(12)
            ctx.set(self.h.man_strobe, 0)
            await self.stop(ctx)
            res['c3'] = self.read_counters(ctx)

        self.simulate(bench)
        b0 = self.model.bursts[0]['data']
        self.assertEqual(b0[:5], [0, 1, 2, 3, 4])
        self.assertEqual(b0[5:], [ring_marker(PAD_MAGIC, 0, 5)] * 11)
        # 100 idle cycles + pipeline + B latency
        self.assertLess(res['flush_latency'], 100 + 40)
        self.assertEqual(res['c1']['flushes'], 1)
        self.assertEqual(res['c1']['pad_words'], 11)
        self.assertEqual(res['bursts_before_pulse'], 1)
        b1 = self.model.bursts[1]['data']
        self.assertEqual(b1[:3], [5, 6, 7])
        self.assertEqual(b1[3:], [ring_marker(PAD_MAGIC, 0, 8)] * 13)
        self.assertEqual(res['c2']['flushes'], 2)
        self.assertEqual(res['c2']['pad_words'], 24)
        c3 = res['c3']
        self.check_accounting(c3)
        self.assertEqual(c3['drop_full'], 0)
        self.assertEqual(data_words(self.model.stream()),
                         list(range(c3['words_in'])))
        b2 = self.model.bursts[2]['data']
        # 2 words + 14 pads (flush), then the 12 words that waited
        self.assertEqual(b2[:2], [8, 9])
        self.assertEqual(b2[2:], [ring_marker(PAD_MAGIC, 0, 10)] * 14)
        self.assertEqual(self.model.bursts[3]['data'][:12],
                         list(range(10, 22)))

    def test_header_insertion(self):
        """Header on every 3rd burst; with drops, fields stay consistent."""
        self.make(fifo_depth=64, seed=11, awready_prob=0.8, wready_prob=0.8,
                  b_latency=(20, 700))
        res = {}

        async def bench(ctx):
            self.configure(ctx, header=1, subbuf_bursts=3, size_bursts=50)
            await self.start(ctx, inc=rate_inc(0.4, 1.0))
            await ctx.tick().repeat(5000)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        n_data, n_pad, n_hdr = self.check_accounting(c)
        self.check_addresses(BASE, 50)
        self.assertGreater(c['drop_full'], 0)
        self.assertEqual(c['headers'], n_hdr)
        total_drops = c['drop_full']
        written_before = 0
        for k, b in enumerate(self.model.bursts):
            kinds = classify(b['data'])
            if k % 3 == 0:
                self.assertEqual(kinds[0][0], 'hdr', f'burst {k}')
                hdr = kinds[0][1]
                word_count = hdr & 0xFFFF_FFFF
                drop_count = (hdr >> 32) & 0xFFFF
                nxt = kinds[1][1]
                self.assertEqual(kinds[1][0], 'data')
                # word_count is the index of the data word that follows
                self.assertEqual(word_count, nxt)
                # drops so far: at least those before that word
                self.assertGreaterEqual(drop_count, nxt - written_before)
                self.assertLessEqual(drop_count, total_drops)
                self.assertEqual([kd for kd, _ in kinds[1:]].count('hdr'), 0)
            else:
                self.assertNotIn('hdr', [kd for kd, _ in kinds])
            written_before += sum(1 for kd, _ in kinds if kd == 'data')
        self.assertEqual(n_hdr, (len(self.model.bursts) + 2) // 3)
        data = data_words(self.model.stream())
        self.assertEqual(data, sorted(set(data)))

    def test_header_no_drops_contiguous(self):
        self.make(seed=12, awready_prob=0.8, wready_prob=0.9,
                  b_latency=(2, 40))
        res = {}

        async def bench(ctx):
            self.configure(ctx, header=1, subbuf_bursts=1, size_bursts=9)
            await self.start(ctx, inc=rate_inc(0.3, 1.0))
            await ctx.tick().repeat(1500)
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        _, _, n_hdr = self.check_accounting(c)
        # every burst starts with a header whose count is the next word
        for b in self.model.bursts:
            hdr, nxt = b['data'][0], b['data'][1]
            if nxt >> 48 == PAD_MAGIC:
                continue
            self.assertEqual(hdr, ring_marker(HDR_MAGIC, 0, nxt))
        self.assertEqual(n_hdr, len(self.model.bursts))
        self.assertEqual(data_words(self.model.stream()),
                         list(range(c['words_in'])))

    def test_protect_mode(self):
        size = 4
        self.make(fifo_depth=64, seed=13, awready_prob=0.8, wready_prob=0.8,
                  b_latency=(5, 30))
        ring = self.ring
        consumer = {'v': 0}
        violations = []

        async def monitor(ctx):
            k = 0  # index of the next AW
            async for _, _, av, ar in ctx.tick().sample(ring.axi.awvalid,
                                                        ring.axi.awready):
                if av and ar:
                    # never more than size-1 bursts past the consumer
                    if k - consumer['v'] > size - 2:
                        violations.append((k, consumer['v']))
                    k += 1

        self.monitors.append(monitor)
        res = {}

        async def set_consumer(ctx, v):
            consumer['v'] = v
            ctx.set(ring.consumer_bursts, v)

        async def bench(ctx):
            self.configure(ctx, size_bursts=size, protect=1)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(1500)
            res['c1'] = self.read_counters(ctx)
            await set_consumer(ctx, 3)
            await ctx.tick().repeat(1500)
            res['c2'] = self.read_counters(ctx)
            await set_consumer(ctx, 5)
            await ctx.tick().repeat(1500)
            res['c3'] = self.read_counters(ctx)
            # disable while blocked: leftovers are discarded, writer idles
            await self.stop(ctx)
            res['c4'] = self.read_counters(ctx)

        self.simulate(bench)
        self.assertEqual(violations, [])
        c1, c2, c3, c4 = res['c1'], res['c2'], res['c3'], res['c4']
        self.assertEqual(c1['issued_bursts'], size - 1)
        self.assertGreater(c1['drop_protect'], 0)
        self.assertEqual(c1['drop_full'], 0)
        self.assertEqual(c2['issued_bursts'], 3 + size - 1)
        self.assertEqual(c3['issued_bursts'], 5 + size - 1)
        self.assertGreater(c3['drop_protect'], c2['drop_protect'])
        self.check_accounting(c4)
        self.assertEqual(c4['drop_full'], 0)
        self.check_addresses(BASE, size)
        data = data_words(self.model.stream())
        self.assertEqual(data, sorted(set(data)))
        # the first 3 bursts are the very first 48 words (nothing lost
        # before the ring filled)
        self.assertEqual(data[:48], list(range(48)))

    def test_soft_reset_only_when_idle(self):
        self.make(seed=14, awready_prob=0.8, b_latency=(2, 50))
        ring = self.ring
        res = {}

        async def bench(ctx):
            self.configure(ctx, size_bursts=10)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(500)
            before = ctx.get(ring.issued_bursts)
            await self.pulse(ctx, ring.soft_reset)
            await ctx.tick().repeat(300)
            res['during_run'] = (before, ctx.get(ring.issued_bursts))
            # soft_reset during the drain is ignored as well
            ctx.set(self.h.rate.enable, 0)
            ctx.set(ring.enable, 0)
            await ctx.tick()
            self.assertFalse(ctx.get(ring.idle))
            await self.pulse(ctx, ring.soft_reset)
            await self.wait_for(ctx, ring.idle, 5000)
            res['c1'] = self.read_counters(ctx)
            await self.pulse(ctx, ring.soft_reset)
            await ctx.tick()
            res['after_reset'] = (ctx.get(ring.issued_bursts),
                                  ctx.get(ring.committed_bursts))
            res['n_first'] = len(self.model.bursts)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(200)
            await self.stop(ctx)
            res['c2'] = self.read_counters(ctx)

        self.simulate(bench)
        before, after = res['during_run']
        self.assertGreater(after, before)
        self.assertGreater(res['c1']['issued_bursts'], after)
        self.assertEqual(res['c1']['issued_bursts'], res['n_first'])
        self.assertEqual(res['after_reset'], (0, 0))
        second = self.model.bursts[res['n_first']:]
        self.assertGreater(len(second), 2)
        for k, b in enumerate(second):
            self.assertEqual(b['addr'], BASE + (k % 10) * 128)
        self.assertEqual(res['c2']['committed_bursts'], len(second))
        self.assertEqual(res['c2']['epoch'], 2)

    def test_enable_toggling_axi_hygiene(self):
        """Random enable toggles under random AXI stalls.

        The model raises on any VALID drop or payload change without a
        handshake and on any bad WLAST. Every word presented while the
        ring was enabled must be in memory, in order, and nothing else.
        """
        self.make(seed=15, awready_prob=0.3, wready_prob=0.5,
                  b_latency=(1, 150))
        ring = self.ring
        accepted = []

        async def monitor(ctx):
            async for _, _, en, v, d in ctx.tick().sample(
                    ring.enabled, ring.in_valid, ring.in_data):
                if en and v:
                    accepted.append(d)

        self.monitors.append(monitor)
        res = {'enables': 0}
        rng = random.Random(99)

        async def bench(ctx):
            self.configure(ctx, size_bursts=23, flush_timeout=300)
            ctx.set(self.h.pat.mode, WordPattern.RAMP64)
            ctx.set(self.h.rate.inc, rate_inc(0.35, 1.0))
            ctx.set(self.h.rate.enable, 1)
            for _ in range(24):
                ctx.set(ring.enable, 1)
                await ctx.tick().repeat(rng.randint(5, 400))
                ctx.set(ring.enable, 0)
                await ctx.tick().repeat(rng.randint(1, 200))
            await self.stop(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        self.check_accounting(c)
        self.check_addresses(BASE, 23)
        self.assertEqual(c['drop_full'], 0)
        self.assertEqual(data_words(self.model.stream()), accepted)
        self.assertEqual(c['words_in'], len(accepted))
        self.assertGreater(c['epoch'], 5)
        self.assertEqual(self.model.violations, [])

    def test_guard_refusal(self):
        self.make(seed=16)
        ring = self.ring
        res = {}

        async def try_enable(ctx, n=30):
            ctx.set(ring.enable, 1)
            await ctx.tick().repeat(n)
            r = (ctx.get(ring.enabled), ctx.get(ring.idle),
                 ctx.get(ring.guard_blocked))
            ctx.set(ring.enable, 0)
            await ctx.tick().repeat(3)
            return r

        async def bench(ctx):
            ctx.set(self.h.pat.mode, WordPattern.RAMP64)
            ctx.set(self.h.rate.inc, rate_inc(0.5, 1.0))
            ctx.set(self.h.rate.enable, 1)
            # base below guard_lo
            self.configure(ctx, guard_lo=BASE + 0x1000)
            res['lo'] = await try_enable(ctx)
            # ring end above guard_hi
            self.configure(ctx, size_bursts=64, guard_hi=BASE + 64 * 128 - 1)
            res['hi'] = await try_enable(ctx)
            # exactly fits: allowed (guard_hi is exclusive)
            self.configure(ctx, size_bursts=64, guard_hi=BASE + 64 * 128)
            res['fit'] = await try_enable(ctx, 200)
            await self.wait_for(ctx, ring.idle, 3000)
            # size_bursts < 2 and unaligned base are refused too
            self.configure(ctx, size_bursts=1)
            res['size1'] = await try_enable(ctx)
            self.configure(ctx, base=BASE + 0x80)
            res['unaligned'] = await try_enable(ctx)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        self.assertEqual(res['lo'], (0, 1, 1))
        self.assertEqual(res['hi'], (0, 1, 2))
        self.assertEqual(res['fit'][:2], (1, 0))
        self.assertEqual(res['size1'], (0, 1, 3))
        self.assertEqual(res['unaligned'], (0, 1, 4))
        self.assertEqual(res['c']['epoch'], 1)
        # only the allowed session wrote, and only inside its window
        self.assertGreater(len(self.model.bursts), 0)
        for b in self.model.bursts:
            self.assertTrue(BASE <= b['addr'] < BASE + 64 * 128)

    def test_irq_every(self):
        self.make(seed=17, awready_prob=0.8, b_latency=(2, 60))
        ring = self.ring
        irqs = []

        async def monitor(ctx):
            # edge index, counted like AxiWriteSlaveModel.cycle
            cycle = 0
            async for _, _, irq in ctx.tick().sample(ring.irq):
                if irq:
                    irqs.append(cycle)
                cycle += 1

        self.monitors.append(monitor)
        res = {}

        async def bench(ctx):
            self.configure(ctx, irq_every=4)
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(2500)
            await self.stop(ctx)
            await ctx.tick().repeat(10)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        self.assertGreater(c['committed_bursts'], 20)
        self.assertEqual(len(irqs), c['committed_bursts'] // 4)
        # each IRQ follows the 4k-th B response by 2 cycles
        b_cycles = [b['b_cycle'] for b in self.model.bursts]
        for k, t in enumerate(irqs):
            self.assertEqual(t, b_cycles[4 * k + 3] + 2)

    def test_irq_timeout(self):
        timeout = 300
        self.make(seed=18, b_latency=(2, 10))
        ring = self.ring
        irqs = []

        async def monitor(ctx):
            # edge index, counted like AxiWriteSlaveModel.cycle
            cycle = 0
            async for _, _, irq in ctx.tick().sample(ring.irq):
                if irq:
                    irqs.append(cycle)
                cycle += 1

        self.monitors.append(monitor)

        async def bench(ctx):
            self.configure(ctx, irq_timeout=timeout)
            await self.start(ctx, inc=rate_inc(0.25, 1.0))
            await ctx.tick().repeat(3000)
            await self.stop(ctx)
            await ctx.tick().repeat(2 * timeout)

        self.simulate(bench)
        b_cycles = [b['b_cycle'] for b in self.model.bursts]
        self.assertGreater(len(b_cycles), 30)
        self.assertGreater(len(irqs), 5)
        # IRQs are at least `timeout` apart
        for t0, t1 in zip(irqs, irqs[1:]):
            self.assertGreaterEqual(t1 - t0, timeout)
        # every IRQ has a commit since the previous IRQ
        prev = -1
        for t in irqs:
            self.assertTrue(any(prev < b < t for b in b_cycles), t)
            prev = t
        # every commit is followed by an IRQ within timeout + 4 cycles
        for b in b_cycles:
            self.assertTrue(any(b < t <= b + timeout + 4 for t in irqs), b)
        # once the last commit has been signalled the IRQs stop
        last = max(b_cycles)
        self.assertTrue(all(t <= last + timeout + 4 for t in irqs))


class TestRingV2Random(RingV2TestCase):
    """Randomized runs mixing every feature, checked against invariants.

    Each seed picks a FIFO depth, ring size, sub-buffer size, header and
    flush settings, AXI stall/latency profile and a random schedule of
    rate changes, enable toggles, flush pulses and (ignored) soft resets.
    Checked: AXI rules (model), burst addresses, exact word accounting,
    data order, header placement, pad tails and header/pad fields.
    """
    def run_random(self, seed, protect):
        rng = random.Random(seed)
        fifo_depth = rng.choice([32, 48, 64, 256])
        size = rng.choice([2, 3, 5, 7, 16, 37])
        subbuf = rng.choice([0, 1, 2, 3, 5])
        header = rng.random() < 0.6
        flush_to = rng.choice([0, 20, 100, 400])
        max_out = rng.choice([0, 1, 2, 5, 8])
        self.make(fifo_depth=fifo_depth, seed=seed,
                  awready_prob=rng.uniform(0.1, 1.0),
                  wready_prob=rng.uniform(0.2, 1.0),
                  b_latency=(1, rng.choice([5, 50, 400, 1200])),
                  bresp=lambda i: 2 if rng.random() < 0.02 else 0)
        ring = self.ring
        accepted = []
        consumer = {'v': 0}
        protect_bad = []

        async def monitor(ctx):
            k = 0
            async for _, _, en, v, d, av, ar in ctx.tick().sample(
                    ring.enabled, ring.in_valid, ring.in_data,
                    ring.axi.awvalid, ring.axi.awready):
                if en and v:
                    accepted.append(d)
                if av and ar:
                    if protect and k - consumer['v'] > size - 2:
                        protect_bad.append((k, consumer['v']))
                    k += 1

        self.monitors.append(monitor)
        res = {}

        async def bench(ctx):
            self.configure(ctx, size_bursts=size, subbuf_bursts=subbuf,
                           header=int(header), flush_timeout=flush_to,
                           max_out=max_out, protect=int(protect))
            ctx.set(self.h.pat.mode, WordPattern.RAMP64)
            ctx.set(self.h.rate.enable, 1)
            for _ in range(rng.randint(3, 8)):
                ctx.set(ring.enable, 1)
                for _ in range(rng.randint(1, 6)):
                    ctx.set(self.h.rate.inc, rate_inc(
                        rng.choice([0.0, 0.002, 0.05, 0.3, 0.9, 1.0]), 1.0))
                    await ctx.tick().repeat(rng.randint(5, 500))
                    if rng.random() < 0.3:
                        await self.pulse(ctx, ring.flush)
                    if rng.random() < 0.2:
                        await self.pulse(ctx, ring.soft_reset)  # ignored
                    if protect and rng.random() < 0.6:
                        consumer['v'] = rng.randint(
                            consumer['v'], ctx.get(ring.committed_bursts))
                        ctx.set(ring.consumer_bursts, consumer['v'])
                ctx.set(ring.enable, 0)
                await ctx.tick().repeat(rng.randint(1, 300))
            await self.stop(ctx, timeout=100000)
            res['c'] = self.read_counters(ctx)

        self.simulate(bench)
        c = res['c']
        self.assertEqual(protect_bad, [])
        self.check_accounting(c)
        self.check_addresses(BASE, size)
        kinds = classify(self.model.stream())
        data = [w for k, w in kinds if k == 'data']
        idx_of = {v: i for i, v in enumerate(accepted)}
        self.assertEqual(len(accepted), c['words_in'])
        pos = [idx_of[v] for v in data]
        self.assertEqual(pos, sorted(set(pos)))
        if not protect:
            self.assertEqual(c['drop_protect'], 0)
        total_drops = c['drop_full'] + c['drop_protect']
        for k, b in enumerate(self.model.bursts):
            names = [x for x, _ in classify(b['data'])]
            if header and subbuf and k % subbuf == 0:
                self.assertEqual(names[0], 'hdr', k)
            else:
                self.assertNotEqual(names[0], 'hdr', k)
            self.assertNotIn('hdr', names[1:])
            if 'pad' in names:  # pads form the tail of a burst
                first = names.index('pad')
                self.assertEqual(set(names[first:]), {'pad'}, k)
                pads = set(b['data'][first:])
                self.assertEqual(len(pads), 1)
        for i, (kind, w) in enumerate(kinds):
            if kind == 'data':
                continue
            self.assertLessEqual((w >> 32) & 0xFFFF, total_drops)
            nxt = next((v for kk, v in kinds[i + 1:] if kk == 'data'), None)
            if kind == 'hdr':
                self.assertEqual(w & 0xFFFF_FFFF, idx_of[nxt])
            elif nxt is not None:
                self.assertLessEqual(w & 0xFFFF_FFFF, idx_of[nxt])

    def test_random_overwrite(self):
        for seed in (0, 2, 8, 9):
            with self.subTest(seed=seed):
                self.run_random(seed, protect=False)

    def test_random_protect(self):
        for seed in (1, 3, 5):
            with self.subTest(seed=seed):
                self.run_random(seed, protect=True)


if __name__ == '__main__':
    unittest.main()
