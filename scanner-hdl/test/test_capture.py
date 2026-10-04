#
# Fishball radio core - the capture ring's timing and trigger (radio_core/capture.py,
# doc/changes/082 item 7).
#
# A ramp source (each sample's I and Q are its index's low and high halves) feeds the capture,
# which writes into the AXI write model (strict: AXI rules raise). A small ring (4 sub-buffers of
# 1 KB, 256 samples each) keeps the simulations short. Checked: the start index and the samples
# of every sub-buffer since an enable; a re-enable starting the ring afresh; a trigger stopping
# at a sub-buffer's end with no address past it; AWVALID held through a disable; the overflow
# counting the words lost.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from radio_core.capture import Capture

from .hwval_axi_wmodel import AxiWriteSlaveModel

BASE = 0x2200_0000
BUFFERS_LOG2 = 2
BUFFER_SIZE = 1024
SAMPLES_PER_BUFFER = BUFFER_SIZE // 4


class Harness(Elaboratable):
    """A ramp, one sample every `period` cycles, into the capture."""
    def __init__(self, period):
        self.period = period
        self.capture = Capture(BASE, BUFFERS_LOG2, BUFFER_SIZE, name='m_axi_capture')
        self.index = Signal(64)
        self.run = Signal(init=1)

    def elaborate(self, platform):
        m = Module()
        m.submodules.capture = cap = self.capture
        phase = Signal(range(self.period))
        strobe = Signal()
        m.d.comb += strobe.eq(self.run & (phase == 0))
        m.d.sync += phase.eq(Mux(phase == self.period - 1, 0, phase + 1))
        with m.If(strobe):
            m.d.sync += self.index.eq(self.index + 1)
        m.d.comb += [
            cap.strobe_in.eq(strobe),
            cap.re_in.eq(self.index[:16]),
            cap.im_in.eq(self.index[16:32]),
            cap.sample_index.eq(self.index),
        ]
        return m


def samples_of(word):
    """The two sample indices a ramp word holds (low 32 bits of each)."""
    return [word & 0xFFFF_FFFF, (word >> 32) & 0xFFFF_FFFF]


class TestCapture(unittest.TestCase):
    def make(self, period=4, strict=True, **model):
        self.h = Harness(period)
        self.cap = self.h.capture
        self.model = AxiWriteSlaveModel(self.cap.dma.axi, strict=strict, **model)

    def simulate(self, bench):
        sim = Simulator(self.h)
        sim.add_clock(16e-9)
        sim.add_testbench(self.model.bench, background=True)
        sim.add_testbench(bench)
        sim.run()

    def sub_buffer(self, slot):
        """The sample indices a sub-buffer of the ring holds, in order."""
        out = []
        for w in range(BUFFER_SIZE // 8):
            out += samples_of(self.model.mem.get(BASE + slot * BUFFER_SIZE + 8 * w, 0))
        return out

    async def wait_buffers(self, ctx, n, limit=200_000):
        for _ in range(limit):
            if ctx.get(self.cap.buffers) >= n:
                return
            await ctx.tick()
        self.fail(f'{n} sub-buffers not completed')

    def test_sub_buffers_follow_the_start_index(self):
        """Six sub-buffers since an enable (the ring wraps once): the k-th holds the samples from
        start + 256 k, slot k mod 4; last_buffer and the count follow. (Slot 2 is left out: the
        seventh may have begun there.)"""
        self.make(awready_prob=0.7, wready_prob=0.8, b_latency=(1, 12))
        res = {}

        async def bench(ctx):
            await ctx.tick().repeat(37)
            ctx.set(self.cap.enable, 1)
            await self.wait_buffers(ctx, 6)
            res['start'] = ctx.get(self.cap.start_index)
            res['last'] = ctx.get(self.cap.last_buffer)
            res['slots'] = {k: self.sub_buffer(k % 4) for k in [3, 4, 5]}

        self.simulate(bench)
        start = res['start']
        self.assertGreater(start, 0)
        for k, got in res['slots'].items():
            self.assertEqual(got, list(range(start + k * SAMPLES_PER_BUFFER,
                                             start + (k + 1) * SAMPLES_PER_BUFFER)), k)
        self.assertEqual(res['last'], 5 % 4)

    def test_an_enable_starts_the_ring_afresh(self):
        """Disabled mid sub-buffer and enabled again while its last bursts drain: the count
        reads 0 from the enable; the ring restarts at its first sub-buffer, last_buffer -1 until
        one completes, with a new start index."""
        self.make(awready_prob=0.5, wready_prob=0.9, b_latency=(1, 8))
        res = {}

        async def bench(ctx):
            ctx.set(self.cap.enable, 1)
            await self.wait_buffers(ctx, 1)
            await ctx.tick().repeat(300)               # into sub-buffer 1
            ctx.set(self.cap.enable, 0)
            await ctx.tick().repeat(20)
            res['draining'] = not ctx.get(self.cap.dma.idle)
            ctx.set(self.cap.enable, 1)
            await ctx.tick().repeat(2)
            res['count_at_enable'] = ctx.get(self.cap.buffers)
            await ctx.tick().repeat(300)
            res['last_at_restart'] = ctx.get(self.cap.last_buffer)
            res['count_at_restart'] = ctx.get(self.cap.buffers)
            await self.wait_buffers(ctx, 2)
            res['start'] = ctx.get(self.cap.start_index)
            res['slot0'] = self.sub_buffer(0)
            res['slot1'] = self.sub_buffer(1)
            res['last'] = ctx.get(self.cap.last_buffer)

        self.simulate(bench)
        self.assertTrue(res['draining'])
        self.assertEqual(res['count_at_enable'], 0)
        self.assertEqual(res['last_at_restart'], 3)    # -1 in two bits
        self.assertEqual(res['count_at_restart'], 0)
        s = res['start']
        self.assertGreater(s, SAMPLES_PER_BUFFER + 70)
        self.assertEqual(res['slot0'], list(range(s, s + SAMPLES_PER_BUFFER)))
        self.assertEqual(res['slot1'], list(range(s + SAMPLES_PER_BUFFER,
                                                  s + 2 * SAMPLES_PER_BUFFER)))
        self.assertEqual(res['last'], 1)

    def test_a_trigger_stops_at_a_sub_buffer_s_end(self):
        """A trigger in sub-buffer 4 (slot 0) with stop_after 1: sub-buffers 4 and 5 complete,
        then nothing more is addressed; slots 2 and 3 keep sub-buffers 2 and 3."""
        for stop_after in [0, 1, 2]:
            with self.subTest(stop_after=stop_after):
                self.make(awready_prob=0.6, wready_prob=0.8, b_latency=(1, 10))
                res = {}

                async def bench(ctx):
                    ctx.set(self.cap.enable, 1)
                    ctx.set(self.cap.stop_after, stop_after)
                    await self.wait_buffers(ctx, 4)
                    await ctx.tick().repeat(200)
                    res['trigger_index'] = ctx.get(self.h.index)
                    ctx.set(self.cap.trigger, 1)
                    await ctx.tick()
                    ctx.set(self.cap.trigger, 0)
                    for _ in range(100_000):
                        if ctx.get(self.cap.stopped):
                            break
                        await ctx.tick()
                    res['stopped'] = ctx.get(self.cap.stopped)
                    res['buffers'] = ctx.get(self.cap.buffers)
                    aws = self.model.aw_count
                    await ctx.tick().repeat(3000)
                    res['aws_after'] = self.model.aw_count - aws
                    res['start'] = ctx.get(self.cap.start_index)
                    res['overflow'] = ctx.get(self.cap.overflow)
                    res['slots'] = [self.sub_buffer(k) for k in range(4)]

                self.simulate(bench)
                self.assertTrue(res['stopped'])
                self.assertEqual(res['aws_after'], 0)
                last = res['buffers'] - 1
                s = res['start']
                # The trigger fell in the last sub-buffer written whole minus stop_after (or the
                # one before, when its address had already gone out).
                trigger_buffer = (res['trigger_index'] - s) // SAMPLES_PER_BUFFER
                self.assertIn(last - stop_after - trigger_buffer, [0, 1])
                for k in range(last - 3, last + 1):
                    self.assertEqual(res['slots'][k % 4],
                                     list(range(s + k * SAMPLES_PER_BUFFER,
                                                s + (k + 1) * SAMPLES_PER_BUFFER)), k)
                self.assertEqual(self.model.aw_count, (last + 1) * BUFFER_SIZE // 128)

    def test_awvalid_held_through_a_disable(self):
        """With AWREADY rare, disables land while an AW waits: AWVALID stays up until it is taken
        (the strict model raises otherwise), and every burst gets its data."""
        self.make(awready_prob=0.05, wready_prob=1.0, b_latency=(1, 3))

        async def bench(ctx):
            for _ in range(12):
                ctx.set(self.cap.enable, 1)
                await ctx.tick().repeat(97)
                ctx.set(self.cap.enable, 0)
                await ctx.tick().repeat(53)
            await ctx.tick().repeat(2000)
            self.assertEqual(ctx.get(self.cap.dma.axi.awvalid), 0)

        self.simulate(bench)
        self.assertGreater(self.model.aw_count, 3)
        self.assertEqual(len(self.model.bursts), self.model.aw_count)

    def test_overflow_counts_the_words_lost(self):
        """A slow write channel: overflow pulses once for each word not written, and the words
        written are the samples in order with whole-word gaps. The source stops at the end, so
        the last word formed is written (into the burst left open). The packer has one holding
        register, so a word overwritten while it waits for WREADY changes WDATA under WVALID: the
        model records that instead of raising (as the hwval replica's tests do)."""
        self.make(period=2, strict=False, awready_prob=1.0, wready_prob=0.25, b_latency=(1, 2))
        res = {'pulses': 0}

        async def bench(ctx):
            ctx.set(self.cap.enable, 1)
            for _ in range(4000):
                await ctx.tick()
                res['pulses'] += ctx.get(self.cap.overflow)
            ctx.set(self.h.run, 0)
            for _ in range(200):
                await ctx.tick()
                res['pulses'] += ctx.get(self.cap.overflow)
            res['start'] = ctx.get(self.cap.start_index)

        self.simulate(bench)
        words = [w for b in self.model.bursts for w in b['data']]
        words += [beat[0] for beat in self.model._cur_w]
        written = [s for w in words for s in samples_of(w)]
        # Words lost before the first one written count as well.
        pairs = [res['start'] - 2] + written[::2]
        self.assertEqual(written[1::2], [p + 1 for p in pairs[1:]])
        self.assertTrue(all((b - a) % 2 == 0 and b > a for a, b in zip(pairs, pairs[1:])))
        lost = sum((b - a) // 2 - 1 for a, b in zip(pairs, pairs[1:]))
        self.assertGreater(lost, 0)
        self.assertEqual(res['pulses'], lost)


if __name__ == '__main__':
    unittest.main()
