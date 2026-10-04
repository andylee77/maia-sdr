#
# Fishball hwval - LatencyTracker tests
#
# SPDX-License-Identifier: MIT
#

import random
import unittest

from amaranth.sim import Simulator

from hwval_hdl.lat_hist import LatencyTracker, latency_bin


class TestLatencyTracker(unittest.TestCase):
    def run_sim(self, dut, bench):
        sim = Simulator(dut)
        sim.add_clock(10e-9)
        sim.add_testbench(bench)
        sim.run()

    async def read_hist(self, ctx, dut):
        vals = []
        for k in range(LatencyTracker.NUM_BINS):
            ctx.set(dut.hist_sel, k)
            await ctx.tick()
            vals.append(ctx.get(dut.hist_val))
        return vals

    def test_latency_bins_reference(self):
        self.assertEqual([latency_bin(x) for x in (0, 1, 2, 3, 4, 7, 8)],
                         [0, 0, 1, 1, 2, 2, 3])
        self.assertEqual(latency_bin(2**15 - 1), 14)
        self.assertEqual(latency_bin(2**15), 15)
        self.assertEqual(latency_bin(2**31), 15)

    def check_random(self, max_outstanding, seed, cycles, long_prob=0.0):
        """Random in-order request/response traffic against a Python model."""
        dut = LatencyTracker(max_outstanding=max_outstanding)
        rng = random.Random(seed)
        # Build the stimulus up front: per cycle (start, done)
        stim = []
        inflight = []  # start cycles
        pending_resp_at = []
        exp_lat = []
        exp_out_max = 0
        for cyc in range(cycles):
            start = (len(inflight) < max_outstanding
                     and rng.random() < 0.3)
            done = False
            if inflight and pending_resp_at[0] <= cyc:
                done = True
            if done:
                exp_lat.append(cyc - inflight.pop(0))
                pending_resp_at.pop(0)
            if start:
                inflight.append(cyc)
                lat = rng.randint(1, 40)
                if rng.random() < long_prob:
                    lat = rng.randint(2**15 - 4, 2**15 + 40)
                # responses in order: never before the previous one
                due = cyc + lat
                if pending_resp_at:
                    due = max(due, pending_resp_at[-1] + 1)
                pending_resp_at.append(due)
            exp_out_max = max(exp_out_max, len(inflight))
            stim.append((int(start), int(done)))
        exp_hist = [0] * LatencyTracker.NUM_BINS
        for lat in exp_lat:
            exp_hist[latency_bin(lat)] += 1
        if long_prob:
            self.assertGreater(exp_hist[15], 0)
            self.assertGreater(exp_hist[14], 0)
        result = {}

        async def bench(ctx):
            for start, done in stim:
                ctx.set(dut.start, start)
                ctx.set(dut.done, done)
                await ctx.tick()
            ctx.set(dut.start, 0)
            ctx.set(dut.done, 0)
            await ctx.tick().repeat(5)
            result['lat_max'] = ctx.get(dut.lat_max)
            result['out_max'] = ctx.get(dut.outstanding_max)
            result['out'] = ctx.get(dut.outstanding)
            result['error'] = ctx.get(dut.error)
            result['hist'] = await self.read_hist(ctx, dut)

        self.run_sim(dut, bench)
        self.assertEqual(result['error'], 0)
        self.assertEqual(result['lat_max'], max(exp_lat))
        self.assertEqual(result['out_max'], exp_out_max)
        self.assertEqual(result['out'], len(inflight))
        self.assertEqual(result['hist'], exp_hist)

    def test_random_16(self):
        self.check_random(16, seed=1, cycles=3000)

    def test_random_small_fifo(self):
        # Non-power-of-2 depth, FIFO frequently full
        self.check_random(3, seed=2, cycles=3000)

    def test_single_outstanding(self):
        self.check_random(1, seed=3, cycles=1500)

    def test_long_latency_bin15(self):
        self.check_random(4, seed=4, cycles=80000, long_prob=0.02)

    def test_back_to_back_and_clear(self):
        """Latency 1, simultaneous start/done, clear, and error flag."""
        dut = LatencyTracker(max_outstanding=2)
        result = {}

        async def bench(ctx):
            # start at cycle 0, done at cycle 1 -> latency 1 (bin 0)
            ctx.set(dut.start, 1)
            await ctx.tick()
            # cycle 1: done for #0 and start for #1 in the same cycle
            ctx.set(dut.done, 1)
            await ctx.tick()
            ctx.set(dut.start, 0)
            # cycle 2: done for #1 -> latency 1
            await ctx.tick()
            ctx.set(dut.done, 0)
            await ctx.tick().repeat(4)
            result['hist_a'] = await self.read_hist(ctx, dut)
            result['max_a'] = ctx.get(dut.lat_max)
            # clear
            ctx.set(dut.clear, 1)
            await ctx.tick()
            ctx.set(dut.clear, 0)
            await ctx.tick()
            result['hist_b'] = await self.read_hist(ctx, dut)
            result['max_b'] = ctx.get(dut.lat_max)
            # response without a request -> error
            ctx.set(dut.done, 1)
            await ctx.tick()
            ctx.set(dut.done, 0)
            await ctx.tick()
            result['err'] = ctx.get(dut.error)
            # three starts with depth 2 -> overflow error
            ctx.set(dut.clear, 1)
            await ctx.tick()
            ctx.set(dut.clear, 0)
            ctx.set(dut.start, 1)
            await ctx.tick().repeat(3)
            ctx.set(dut.start, 0)
            await ctx.tick()
            result['err2'] = ctx.get(dut.error)
            result['out'] = ctx.get(dut.outstanding)

        self.run_sim(dut, bench)
        self.assertEqual(result['hist_a'][0], 2)
        self.assertEqual(sum(result['hist_a']), 2)
        self.assertEqual(result['max_a'], 1)
        self.assertEqual(sum(result['hist_b']), 0)
        self.assertEqual(result['max_b'], 0)
        self.assertEqual(result['err'], 1)
        self.assertEqual(result['err2'], 1)
        self.assertEqual(result['out'], 2)


if __name__ == '__main__':
    unittest.main()
