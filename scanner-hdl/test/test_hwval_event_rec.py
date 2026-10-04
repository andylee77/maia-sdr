#
# Fishball hwval -- EventRecorder tests
#
# Covers:
#
#   1.  Transitions produce records with the new value and timestamps
#       whose differences equal the sync-cycle spacing of the changes;
#       the first record is the enable heartbeat with the initial value.
#   2.  mask: changes on unmasked bits produce no record, but the value
#       field always carries the full CTRL_OUT value.
#   3.  Heartbeats appear exactly every 2**heartbeat_log2 cycles when
#       idle; `level` counts pending records; `current` follows ctrl_in.
#   4.  Overflow: without pops the FIFO fills, lost records are counted
#       exactly (records popped + overflows == records generated) and
#       recording resumes after draining; `clear` clears overflows.
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from hwval_hdl.event_rec import EventRecorder


SYNC = 16e-9
AXI = 10e-9


def decode(rec):
    return dict(hb=(rec >> 35) & 1, value=(rec >> 27) & 0xff,
                ts=rec & (2**27 - 1))


class TestEventRecorder(unittest.TestCase):
    def simulate(self, dut, benches):
        sim = Simulator(dut)
        sim.add_clock(SYNC, domain='sync')
        sim.add_clock(AXI, domain='s_axi_lite')
        for bench in benches:
            sim.add_testbench(bench)
        sim.run()

    async def drain(self, ctx, dut, records, idle_limit=50):
        idle = 0
        while idle < idle_limit:
            if ctx.get(dut.level) != 0:
                records.append(ctx.get(dut.data))
                ctx.set(dut.pop, 1)
                await ctx.tick('s_axi_lite')
                ctx.set(dut.pop, 0)
                idle = 0
            else:
                await ctx.tick('s_axi_lite')
                idle += 1

    def test_transitions_and_mask(self):
        dut = EventRecorder(heartbeat_log2=12, sim=True)
        # (sync cycles to wait before the change, new ctrl value)
        changes = [(40, 0x01), (7, 0x03), (1, 0x02), (100, 0x82),
                   (30, 0x80), (5, 0x00), (60, 0x10)]
        mask = 0x7f   # bit 7 changes alone are not recorded
        records = []
        done = [False]

        async def writer(ctx):
            ctx.set(dut.ctrl_in, 0x00)
            ctx.set(dut.mask, mask)
            for _ in range(10):
                await ctx.tick('sync')
            ctx.set(dut.enable, 1)
            for wait, value in changes:
                for _ in range(wait):
                    await ctx.tick('sync')
                ctx.set(dut.ctrl_in, value)
            for _ in range(20):
                await ctx.tick('sync')
            self.assertEqual(ctx.get(dut.current), changes[-1][1])
            ctx.set(dut.enable, 0)
            done[0] = True

        async def reader(ctx):
            while not done[0]:
                await ctx.tick('s_axi_lite')
            await self.drain(ctx, dut, records)

        self.simulate(dut, [writer, reader])
        recs = [decode(r) for r in records]
        # enable heartbeat with the initial value
        self.assertEqual(recs[0]['hb'], 1)
        self.assertEqual(recs[0]['value'], 0x00)
        # expected transitions: those that change a masked bit
        prev = 0x00
        t = 0
        expected = []
        for wait, value in changes:
            t += wait
            if (value ^ prev) & mask:
                expected.append((t, value))
            prev = value
        events = recs[1:]
        self.assertTrue(all(r['hb'] == 0 for r in events))
        self.assertEqual([r['value'] for r in events],
                         [v for _, v in expected])
        # timestamp spacing matches the stimulus exactly
        ts = [r['ts'] for r in events]
        self.assertEqual([b - a for a, b in zip(ts, ts[1:])],
                         [b - a for (a, _), (b, _) in
                          zip(expected, expected[1:])])
        # the 0x82 change (bit 7 only, masked out) carried bit 7 into the
        # later 0x80 record value
        self.assertIn(0x80, [r['value'] for r in events])

    def test_heartbeat_and_level(self):
        log2 = 6
        dut = EventRecorder(heartbeat_log2=log2, sim=True)
        records = []
        done = [False]

        async def writer(ctx):
            ctx.set(dut.ctrl_in, 0x5a)
            ctx.set(dut.mask, 0xff)
            for _ in range(5):
                await ctx.tick('sync')
            ctx.set(dut.enable, 1)
            for _ in range(10 * 2**log2 + 10):
                await ctx.tick('sync')
            ctx.set(dut.enable, 0)
            for _ in range(10):
                await ctx.tick('sync')
            done[0] = True

        async def reader(ctx):
            while not done[0]:
                await ctx.tick('s_axi_lite')
            for _ in range(10):
                await ctx.tick('s_axi_lite')
            # 1 enable heartbeat + 10 periodic heartbeats pending
            self.assertEqual(ctx.get(dut.level), 11)
            await self.drain(ctx, dut, records)
            self.assertEqual(ctx.get(dut.level), 0)

        self.simulate(dut, [writer, reader])
        recs = [decode(r) for r in records]
        self.assertEqual(len(recs), 11)
        self.assertTrue(all(r['hb'] == 1 and r['value'] == 0x5a
                            for r in recs))
        ts = [r['ts'] for r in recs]
        self.assertEqual([b - a for a, b in zip(ts, ts[1:])],
                         [2**log2] * 10)

    def test_overflow(self):
        log2 = 3
        dut = EventRecorder(heartbeat_log2=log2, sim=True)
        records = []
        state = {}

        async def writer(ctx):
            ctx.set(dut.mask, 0xff)
            await ctx.tick('sync')
            ctx.set(dut.enable, 1)
            cycles = 700 * 2**log2
            for _ in range(cycles):
                await ctx.tick('sync')
            ctx.set(dut.enable, 0)
            for _ in range(4):
                await ctx.tick('sync')
            state['overflows'] = ctx.get(dut.overflows)
            # records generated: enable heartbeat on the first enabled
            # cycle, then one every 2**log2 enabled cycles
            state['generated'] = 1 + (cycles - 1) // 2**log2
            state['written'] = True
            while not state.get('drained'):
                await ctx.tick('sync')
            # recording resumes after draining
            ctx.set(dut.enable, 1)
            for _ in range(3 * 2**log2):
                await ctx.tick('sync')
            ctx.set(dut.enable, 0)
            for _ in range(4):
                await ctx.tick('sync')
            self.assertEqual(ctx.get(dut.overflows), state['overflows'])
            ctx.set(dut.clear, 1)
            await ctx.tick('sync')
            ctx.set(dut.clear, 0)
            await ctx.tick('sync')
            self.assertEqual(ctx.get(dut.overflows), 0)
            state['resumed'] = True

        async def reader(ctx):
            while not state.get('written'):
                await ctx.tick('s_axi_lite')
            await self.drain(ctx, dut, records)
            state['drained'] = True
            while not state.get('resumed'):
                await ctx.tick('s_axi_lite')
            after = []
            await self.drain(ctx, dut, after)
            state['after'] = after

        self.simulate(dut, [writer, reader])
        self.assertGreater(state['overflows'], 0)
        # FIFO (512) plus the prefetched head record
        self.assertEqual(len(records), 513)
        self.assertEqual(len(records) + state['overflows'],
                         state['generated'])
        ts = [decode(r)['ts'] for r in records]
        self.assertEqual([b - a for a, b in zip(ts, ts[1:])],
                         [2**log2] * (len(ts) - 1))
        after = [decode(r) for r in state['after']]
        self.assertGreaterEqual(len(after), 3)
        self.assertEqual(after[0]['hb'], 1)


if __name__ == '__main__':
    unittest.main()
