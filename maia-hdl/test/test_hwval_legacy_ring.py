#
# Fishball hwval - LegacyRing tests (production wideband ring replica)
#
# The unmodified IQPacker + DmaStreamRingWrite are fed with the
# SamplePattern ramp at the production 8 MSPS rate (62.5 MHz clock, one
# sample every 7.8125 cycles, one 64-bit word every 15.625 cycles, one
# 128 B burst every 250 cycles = 4 us) and write into
# AxiWriteSlaveModel. The model runs non-strict here: AXI rule
# violations of the production DMA are recorded, not raised, because
# documenting them is part of the point.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from hwval_hdl.legacy_ring import LegacyRing
from hwval_hdl.pattern import RateGen, SamplePattern, rate_inc

from .hwval_axi_wmodel import AxiWriteSlaveModel


F_CLK = 62.5e6
INC_8MSPS = rate_inc(8e6, F_CLK)
BASE = 0x2200_0000


class LegacyHarness(Elaboratable):
    """RateGen -> SamplePattern (ramp) -> LegacyRing.

    ``gate`` hides samples from the ring while the ramp keeps counting,
    which models a source that stops and restarts later in time.
    """
    def __init__(self, **kwargs):
        self.rate = RateGen()
        self.src = SamplePattern()
        self.ring = LegacyRing(**kwargs)
        self.gate = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.rate = self.rate
        m.submodules.src = self.src
        m.submodules.ring = self.ring
        m.d.comb += [
            self.src.strobe.eq(self.rate.strobe),
            self.ring.re_in.eq(self.src.re),
            self.ring.im_in.eq(self.src.im),
            self.ring.strobe_in.eq(self.src.strobe_out & ~self.gate),
        ]
        return m


COUNTERS = ['packer_ovf', 'packer_ovf_disabled', 'words_in',
            'words_accepted', 'aw_count', 'b_count', 'bresp_err',
            'subbuf_done', 'stall_cycles', 'max_stall', 'max_outstanding',
            'lat_max', 'last_buffer', 'next_address']


def sample_index(word):
    """Ramp sample index ``c`` of a word ``{c+1, c}``, or None if torn."""
    lo = word & 0xFFFF_FFFF
    hi = word >> 32
    if hi != (lo + 1) & 0xFFFF_FFFF:
        return None
    return lo


def jumps(idx):
    """(position, size in samples) of every discontinuity (step != 2)."""
    return [(k, b - a) for k, (a, b) in enumerate(zip(idx, idx[1:]))
            if b != a + 2]


class LegacyTestCase(unittest.TestCase):
    def make(self, *, seed=0, **kwargs):
        ring_kw = {k: kwargs.pop(k) for k in
                   ('base', 'num_buffers_log2', 'buffer_size') if k in kwargs}
        self.h = LegacyHarness(**ring_kw)
        self.ring = self.h.ring
        self.model = AxiWriteSlaveModel(self.ring.axi, seed=seed,
                                        strict=False, **kwargs)
        self.monitors = []

    def simulate(self, bench):
        sim = Simulator(self.h)
        sim.add_clock(16e-9)
        sim.add_testbench(self.model.bench, background=True)
        for mon in self.monitors:
            sim.add_testbench(mon, background=True)
        sim.add_testbench(bench)
        sim.run()

    def counters(self, ctx):
        return {n: ctx.get(getattr(self.ring, n)) for n in COUNTERS}

    async def start(self, ctx, inc=INC_8MSPS):
        ctx.set(self.h.src.mode, SamplePattern.RAMP)
        ctx.set(self.ring.enable, 1)
        ctx.set(self.h.rate.inc, inc)
        ctx.set(self.h.rate.enable, 1)

    def check_contiguous_addresses(self, base, total_bursts):
        for k, b in enumerate(self.model.bursts):
            self.assertEqual(b['addr'], base + (k % total_bursts) * 128)
            self.assertEqual(b['len'], 15)

    def indices(self):
        """Sample indices of all written words (completed bursts)."""
        idx = [sample_index(w) for w in self.model.stream()]
        self.assertNotIn(None, idx, 'torn word (not a {c+1, c} pair)')
        return idx


class TestLegacyRing(LegacyTestCase):
    def test_constants(self):
        r = LegacyRing()
        Fragment.get(r, None)  # elaborates (production DMA parameters)
        self.assertEqual((r.base, r.size, r.num_buffers),
                         (0x2200_0000, 16 << 20, 16))

    def test_fast_responses_contiguous(self):
        """(a) Fast subordinate: contiguous ramp, no packer overflow."""
        self.make(seed=1, awready_prob=0.7, wready_prob=0.7,
                  b_latency=(2, 60))
        res = {}

        async def bench(ctx):
            await self.start(ctx)
            await ctx.tick().repeat(40000)
            res['c'] = self.counters(ctx)

        self.simulate(bench)
        c = res['c']
        idx = self.indices()
        self.assertGreater(len(idx), 16 * 150)
        self.assertEqual(idx, list(range(0, 2 * len(idx), 2)))
        self.check_contiguous_addresses(BASE, 16 * 8192)
        self.assertEqual(c['packer_ovf'], 0)
        self.assertEqual(c['packer_ovf_disabled'], 0)
        # random WREADY causes short stalls, well inside one word period
        self.assertLess(c['max_stall'], 15)
        self.assertIn(c['words_in'] - c['words_accepted'], (0, 1))
        self.assertEqual(c['words_accepted'], self.model.w_beats)
        self.assertEqual(c['aw_count'], self.model.aw_count)
        self.assertEqual(c['b_count'], self.model.b_count)
        self.assertEqual(self.model.violations, [])
        # WLAST -> B statistics match the model's response latencies
        lats = [b['b_cycle'] - b['wlast_cycle']
                for b in self.model.bursts if b['b_cycle'] is not None]
        self.assertEqual(c['lat_max'], max(lats))
        self.assertLessEqual(c['max_outstanding'], 2)

    def run_b_latency(self, lat, cycles=14000):
        self.make(b_latency=(lat, lat))
        res = {}

        async def bench(ctx):
            await self.start(ctx)
            await ctx.tick().repeat(cycles)
            res['c'] = self.counters(ctx)

        self.simulate(bench)
        return res['c']

    def test_b_latency_cliff(self):
        """(b) F5: the latency cliff of the production ring.

        The DMA stops accepting stream words once 5 bursts wait for their
        write response, and the packer holds a single word. With one burst
        every 4 us that gives ~16 us of WLAST -> B tolerance. Observed in
        simulation (constant B latency after WLAST):

        - 1020 cycles (16.3 us): stalls up to 5 cycles, no loss.
        - 1040 cycles (16.6 us): words lost.
        - 1200 cycles (19.2 us): ~11 % of the words lost.

        Losses are whole 64-bit words (sample pairs), and the addresses
        stay contiguous, so a reader sees a seamless ring with silent
        time gaps.
        """
        c_ok = self.run_b_latency(1020)
        self.assertEqual(c_ok['packer_ovf'], 0)
        self.assertEqual(c_ok['words_in'] - c_ok['words_accepted'], 0)
        # the cap was reached (short stalls) but the packer held
        self.assertEqual(c_ok['max_outstanding'], 5)
        self.assertEqual(c_ok['lat_max'], 1020)
        self.assertEqual(self.model.outstanding_max, 7)  # AW -> B: 2 + 5
        self.assertEqual(jumps(self.indices()), [])

        c = self.run_b_latency(1200)
        idx = self.indices()  # raises on a torn (non-pair) word
        self.check_contiguous_addresses(BASE, 16 * 8192)
        lost = c['words_in'] - c['words_accepted']
        self.assertGreater(lost, 50)
        # memory: increasing, whole-word (2-sample) gaps only
        self.assertTrue(all(i % 2 == 0 for i in idx))
        gaps = [size for _, size in jumps(idx)]
        self.assertGreater(len(gaps), 0)
        self.assertTrue(all(g > 2 for g in gaps))
        self.assertLessEqual(sum(g // 2 - 1 for g in gaps), lost)
        # every formed word is either accepted or overwritten
        self.assertEqual(c['words_accepted'], self.model.w_beats)
        self.assertGreaterEqual(c['packer_ovf'], lost)
        self.assertGreater(c['max_stall'], 100)
        self.assertEqual(c['max_outstanding'], 5)
        self.assertEqual(c['lat_max'], 1200)
        self.assertEqual(self.model.violations, [])

    def test_packer_ovf_overcounts_near_cliff(self):
        """IQPacker's overflow pulse over-reports the loss.

        The packer flags an overflow whenever the second sample of a pair
        arrives while its holding register is valid, even if the DMA
        accepts the old word in that same cycle (no data lost). With a
        constant 1060-cycle B latency the stall release lines up with the
        pair strobe, and the pulse count is more than twice the number of
        words actually lost. The exact loss is words_in - words_accepted.
        """
        c = self.run_b_latency(1060)
        lost = c['words_in'] - c['words_accepted']
        self.assertGreater(lost, 0)
        self.assertGreater(c['packer_ovf'], 2 * lost)

    def test_packer_ovf_false_positive_directed(self):
        """Directed: overflow pulse with no data lost.

        WREADY is held low while word 0 waits, then raised exactly in the
        cycle of the strobe that completes word 1: the W handshake takes
        word 0 at the same edge at which word 1 is latched. Memory holds
        both words, yet packer_ovf = 1.
        """
        self.make(awready_prob=1.0, b_latency=(2, 2))
        release = {'cycle': None}
        self.model.wready_fn = lambda m: (release['cycle'] is not None
                                          and m.cycle >= release['cycle'])
        ring = self.ring
        res = {}

        src = self.h.src

        async def bench(ctx):
            ctx.set(ring.enable, 1)
            ctx.set(src.mode, SamplePattern.LIVE)
            cycle = 0  # index of the next edge, as AxiWriteSlaveModel.cycle

            async def tick():
                nonlocal cycle
                await ctx.tick()
                cycle += 1

            async def sample(v, release_now=False):
                # SamplePattern registers the sample: the packer sees the
                # strobe one cycle later, at edge `cycle + 1`
                ctx.set(src.live_re, v & 0xFFFF)
                ctx.set(src.live_im, v >> 16)
                ctx.set(src.live_strobe, 1)
                if release_now:
                    # WREADY sampled at the same edge as the packer strobe
                    release['cycle'] = cycle + 1
                await tick()
                ctx.set(src.live_strobe, 0)
                for _ in range(6):
                    await tick()

            for _ in range(10):  # let the DMA pre-issue its AWs
                await tick()
            await sample(0)
            await sample(1)   # word 0 formed, waits (WREADY low)
            await sample(2)
            await sample(3, release_now=True)
            for k in range(4, 32):
                await sample(k)
            for _ in range(50):
                await tick()
            res['c'] = self.counters(ctx)

        self.simulate(bench)
        c = res['c']
        beats = [sample_index(w) for w in self.model.bursts[0]['data']]
        self.assertEqual(beats, list(range(0, 32, 2)))  # nothing lost
        self.assertEqual(c['words_in'], c['words_accepted'])
        self.assertEqual(c['packer_ovf'], 1)      # ... but flagged
        self.assertEqual(self.model.violations, [])

    def test_enable_toggle_splice(self):
        """(c) F6: toggling the DMA enable.

        Observed behaviour of the production DMA:

        1. AWVALID is ``enable & ~two_outstanding``, so dropping enable
           while an AW waits for AWREADY withdraws AWVALID without a
           handshake (AXI violation, recorded by the model).
        2. Disabling does not stop the writes: the AW already accepted
           keeps taking stream words until its 16 beats are done, then the
           stream stalls and the packer overwrites its holding word on
           every pair (``packer_ovf_disabled`` counts disabled time, not
           loss).
        3. Re-enabling continues at the next ring address. The first word
           written is the packer's latest word, so memory shows the two
           sessions spliced at a burst boundary with contiguous addresses
           and a silent time gap; nothing in the data marks the splice.
        """
        self.make(awready_prob=1.0, wready_prob=1.0, b_latency=(5, 30))
        ring = self.ring
        res = {}
        off_cycles = 3000

        async def bench(ctx):
            await self.start(ctx)
            await ctx.tick().repeat(5000)
            # hold AWREADY low until an AW is waiting, then disable
            self.model.awready_prob = 0.0
            for _ in range(1000):
                await ctx.tick()
                if ctx.get(ring.axi.awvalid):
                    break
            await ctx.tick().repeat(2)
            res['aw_before'] = ctx.get(ring.aw_count)
            ctx.set(ring.enable, 0)
            await ctx.tick().repeat(2)
            self.model.awready_prob = 1.0
            await ctx.tick().repeat(off_cycles)
            res['c_off'] = self.counters(ctx)
            res['bursts_off'] = len(self.model.bursts)
            ctx.set(ring.enable, 1)
            await ctx.tick().repeat(3000)
            res['c'] = self.counters(ctx)

        self.simulate(bench)
        c, c_off = res['c'], res['c_off']
        # 1. AWVALID withdrawn without handshake
        self.assertTrue(any('AWVALID dropped' in v
                            for v in self.model.violations),
                        self.model.violations)
        # 2. the in-flight burst completed after the disable
        self.assertEqual(c_off['aw_count'], res['aw_before'])
        self.assertEqual(res['bursts_off'], c_off['aw_count'])
        self.assertEqual(c_off['packer_ovf'], 0)
        per_word = 15.625
        self.assertAlmostEqual(c_off['packer_ovf_disabled'],
                               off_cycles / per_word, delta=20)
        # 3. splice: contiguous addresses, one jump, at a burst boundary
        self.check_contiguous_addresses(BASE, 16 * 8192)
        j = jumps(self.indices())
        self.assertEqual(len(j), 1, j)
        pos, size = j[0]
        self.assertEqual((pos + 1) % 16, 0)  # first word of a burst
        self.assertEqual(pos + 1, 16 * res['bursts_off'])
        self.assertAlmostEqual(size, off_cycles / (per_word / 2), delta=80)
        self.assertEqual(c['packer_ovf'], 0)

    def test_source_stop_holds_burst_open(self):
        """(d) A stopped source leaves a pre-addressed burst open.

        The DMA issues AW before it has data. If the sample source stops
        (hwval legacy src = off) with the DMA enabled, the accepted burst
        waits mid-way for W data indefinitely: on the real HP1 port this
        holds the interconnect's write data path for that master, and
        disabling the DMA does not close it. When the source restarts the
        burst is completed with new data, so the time gap lands inside a
        burst.
        """
        self.make(awready_prob=1.0, wready_prob=1.0, b_latency=(5, 30))
        ring = self.ring
        res = {}

        async def bench(ctx):
            await self.start(ctx)
            await ctx.tick().repeat(1000 + 125)  # stop in mid-burst
            # gate the samples at a pair boundary (just after a word)
            n = ctx.get(ring.words_in)
            while ctx.get(ring.words_in) == n:
                await ctx.tick()
            ctx.set(self.h.gate, 1)
            await ctx.tick().repeat(20)
            ctx.set(ring.enable, 0)
            await ctx.tick().repeat(2000)
            res['open_beats'] = self.model.open_w_beats()
            res['c_stop'] = self.counters(ctx)
            ctx.set(ring.enable, 1)
            ctx.set(self.h.gate, 0)
            await ctx.tick().repeat(2000)
            res['c'] = self.counters(ctx)

        self.simulate(bench)
        c_stop = res['c_stop']
        self.assertGreater(res['open_beats'], 0)
        self.assertLess(res['open_beats'], 16)
        self.assertGreater(c_stop['aw_count'], c_stop['b_count'])
        j = jumps(self.indices())
        self.assertEqual(len(j), 1, j)
        self.assertNotEqual((j[0][0] + 1) % 16, 0)  # inside a burst
        self.assertGreater(j[0][1], 2000 / 7.8125 - 10)
        self.check_contiguous_addresses(BASE, 16 * 8192)

    def test_small_ring_wrap_and_irq(self):
        """Sub-buffer interrupt, last_buffer and wrap on a small ring."""
        # Always-ready subordinate: at 31.25 MSPS the 1-word packer only
        # tolerates ~4 cycles of W stall.
        self.make(base=0x2200_0000, num_buffers_log2=2, buffer_size=2048,
                  awready_prob=1.0, wready_prob=1.0, b_latency=(2, 40),
                  bresp={70})
        ring = self.ring
        irqs = []

        async def monitor(ctx):
            async for _, _, irq in ctx.tick().sample(ring.irq):
                irqs.append(irq)

        self.monitors.append(monitor)
        res = {}

        async def bench(ctx):
            # 31.25 MSPS: a word every 4 cycles, one lap every ~4100 cycles
            await self.start(ctx, inc=rate_inc(0.5, 1.0))
            await ctx.tick().repeat(10000)
            res['c'] = self.counters(ctx)
            # the source keeps running: read right after the clear edge
            ctx.set(ring.clear, 1)
            await ctx.tick()
            res['cleared'] = self.counters(ctx)
            ctx.set(ring.clear, 0)

        self.simulate(bench)
        c = res['c']
        total_bursts = 4 * 16
        self.assertGreater(c['b_count'], 2 * total_bursts)
        self.check_contiguous_addresses(0x2200_0000, total_bursts)
        self.assertEqual(c['subbuf_done'], c['b_count'] // 16)
        self.assertEqual(sum(irqs[:-2]), c['subbuf_done'])
        self.assertEqual(c['last_buffer'], (c['subbuf_done'] - 1) % 4)
        self.assertEqual(c['bresp_err'], 1)
        self.assertEqual(c['packer_ovf'], 0)
        for n in ('words_in', 'words_accepted', 'aw_count', 'b_count',
                  'bresp_err', 'subbuf_done', 'lat_max', 'max_stall'):
            self.assertEqual(res['cleared'][n], 0, n)


if __name__ == '__main__':
    unittest.main()
