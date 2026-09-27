#
# Fishball hardware validation (hwval) - CDC helper tests
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.sim import Simulator

import random
import unittest

from hwval_hdl.cdc_util import (
    ConfigSync, DomainCrossing, GrayCounterSync, Snapshot)


def run(dut, benches, clocks):
    sim = Simulator(dut)
    for domain, period in clocks.items():
        sim.add_clock(period, domain=domain)
    for bench in benches:
        sim.add_testbench(bench)
    sim.run()


async def wait_ack(ctx, snap, timeout, domain='req'):
    for n in range(timeout):
        if ctx.get(snap.ack):
            return n
        await ctx.tick(domain)
    return None


class SnapshotDut(Elaboratable):
    def __init__(self, latch_delay=0):
        self.cnt = Signal(32)
        self.frozen = Signal(32)
        self.snap = Snapshot('src', {'cnt': self.cnt, 'frozen': self.frozen},
                             req_domain='req', latch_delay=latch_delay)

    def elaborate(self, platform):
        m = Module()
        m.domains.req = ClockDomain()
        m.domains.src = ClockDomain()
        m.submodules.snap = self.snap
        m.d.src += self.cnt.eq(self.cnt + 1)
        # 'frozen' models a statistics window that updates on trigger
        with m.If(self.snap.trigger):
            m.d.src += self.frozen.eq(self.cnt)
        return m


class TestSnapshot(unittest.TestCase):
    def check_speeds(self, req_period, src_period):
        dut = SnapshotDut()
        results = []

        async def bench(ctx):
            await ctx.tick('req').repeat(5)
            for j in range(6):
                before = ctx.get(dut.cnt)
                ctx.set(dut.snap.req, 1)
                await ctx.tick('req')
                ctx.set(dut.snap.req, 0)
                assert not ctx.get(dut.snap.ack)
                n = await wait_ack(ctx, dut.snap, 500)
                assert n is not None, 'snapshot did not complete'
                after = ctx.get(dut.cnt)
                value = ctx.get(dut.snap.shadow['cnt'])
                assert before <= value <= after, (before, value, after)
                assert ctx.get(dut.snap.seq) == j + 1
                assert not ctx.get(dut.snap.busy)
                # the shadow is quasi-static until the next request
                await ctx.tick('req').repeat(7)
                assert ctx.get(dut.snap.shadow['cnt']) == value
                assert ctx.get(dut.snap.ack)
                results.append(value)
            assert results == sorted(results)

        run(dut, [bench], {'req': req_period, 'src': src_period})

    def test_slower_source(self):
        self.check_speeds(10e-9, 16e-9)

    def test_faster_source(self):
        self.check_speeds(10e-9, 8e-9)

    def test_much_slower_source(self):
        self.check_speeds(10e-9, 97e-9)

    def test_much_faster_source(self):
        self.check_speeds(50e-9, 5.3e-9)

    def test_back_to_back_requests(self):
        # Requests issued faster than the handshake completes are queued;
        # ack only rises for data latched after the last request.
        dut = SnapshotDut()

        async def bench(ctx):
            await ctx.tick('req').repeat(3)
            for _ in range(3):
                ctx.set(dut.snap.req, 1)
                await ctx.tick('req')
                ctx.set(dut.snap.req, 0)
                await ctx.tick('req')
            last_req_cnt = ctx.get(dut.cnt)
            n = await wait_ack(ctx, dut.snap, 2000)
            assert n is not None
            assert ctx.get(dut.snap.shadow['cnt']) >= last_req_cnt - 1
            # one acknowledged snapshot (the queued requests merge)
            assert ctx.get(dut.snap.seq) == 1

        run(dut, [bench], {'req': 10e-9, 'src': 101e-9})

    def test_dead_source_clock(self):
        # No clock on 'src': the request never completes, the requester
        # never blocks and ack stays low.
        dut = SnapshotDut()

        async def bench(ctx):
            await ctx.tick('req').repeat(3)
            ctx.set(dut.snap.req, 1)
            await ctx.tick('req')
            ctx.set(dut.snap.req, 0)
            n = await wait_ack(ctx, dut.snap, 3000)
            assert n is None
            assert ctx.get(dut.snap.busy)
            assert ctx.get(dut.snap.seq) == 0
            # a second request does not wedge anything either
            ctx.set(dut.snap.req, 1)
            await ctx.tick('req')
            ctx.set(dut.snap.req, 0)
            assert await wait_ack(ctx, dut.snap, 1000) is None
            assert ctx.get(dut.snap.busy)

        run(dut, [bench], {'req': 10e-9})

    def test_latch_delay(self):
        # With latch_delay, the source gets a trigger pulse and the shadows
        # are latched afterwards, so they capture the post-trigger value.
        dut = SnapshotDut(latch_delay=3)

        async def bench(ctx):
            await ctx.tick('req').repeat(5)
            for _ in range(3):
                ctx.set(dut.snap.req, 1)
                await ctx.tick('req')
                ctx.set(dut.snap.req, 0)
                assert await wait_ack(ctx, dut.snap, 500) is not None
                frozen = ctx.get(dut.snap.shadow['frozen'])
                cnt = ctx.get(dut.snap.shadow['cnt'])
                # trigger at cycle T updates 'frozen' to cnt(T) at T+1;
                # the latch happens at T+3 when cnt = cnt(T)+3
                assert cnt - frozen == 3, (cnt, frozen)

        async def trigger_width(ctx):
            high = 0
            for _ in range(600):
                await ctx.tick('src')
                high += ctx.get(dut.snap.trigger)
            assert high == 3

        run(dut, [bench, trigger_width], {'req': 10e-9, 'src': 13e-9})


class GrayDut(Elaboratable):
    def __init__(self):
        self.enable = Signal()
        self.count = Signal(32, init=0xFFFF_FF00)
        self.gray = GrayCounterSync('src', 'dst', 32)

    def elaborate(self, platform):
        m = Module()
        m.domains.src = ClockDomain()
        m.domains.dst = ClockDomain()
        m.submodules.gray = self.gray
        with m.If(self.enable):
            m.d.src += self.count.eq(self.count + 1)
        m.d.comb += self.gray.i.eq(self.count)
        return m


class TestGrayCounterSync(unittest.TestCase):
    def check(self, src_period, dst_period):
        dut = GrayDut()
        rng = random.Random(7)
        state = {'done': False}

        async def producer(ctx):
            for _ in range(600):
                ctx.set(dut.enable, rng.random() < 0.7)
                await ctx.tick('src')
            ctx.set(dut.enable, 0)
            await ctx.tick('src').repeat(10)
            state['done'] = True

        async def consumer(ctx):
            init = 0xFFFF_FF00
            last = None
            # let the pipeline fill (output starts at 0)
            await ctx.tick('dst').repeat(8)
            while not state['done']:
                await ctx.tick('dst')
                value = ctx.get(dut.gray.o)
                truth = ctx.get(dut.count)
                if last is None:
                    last = value
                    continue
                # monotonic modulo 2^32 (the counter wraps past 0)
                delta = (value - last) & 0xFFFF_FFFF
                assert delta < 1 << 16, (hex(last), hex(value))
                # never ahead of the true count
                lag = (truth - value) & 0xFFFF_FFFF
                assert lag < 1 << 16, (hex(truth), hex(value))
                last = value
            await ctx.tick('dst').repeat(6)
            assert ctx.get(dut.gray.o) == ctx.get(dut.count)
            # it wrapped through zero during the test
            assert ctx.get(dut.count) < init

        run(dut, [producer, consumer], {'src': src_period,
                                        'dst': dst_period})

    def test_fast_to_slow(self):
        self.check(8e-9, 10e-9)

    def test_slow_to_fast(self):
        self.check(16e-9, 10e-9)


class ConfigDut(Elaboratable):
    def __init__(self):
        self.cs = ConfigSync('src', 'dst', 32, init=0x1234, name='t')

    def elaborate(self, platform):
        m = Module()
        m.domains.src = ClockDomain()
        m.domains.dst = ClockDomain()
        m.submodules.cs = self.cs
        return m


class TestConfigSync(unittest.TestCase):
    def check(self, src_period, dst_period):
        dut = ConfigDut()
        rng = random.Random(3)
        written = {0x1234}
        state = {'done': False}

        async def producer(ctx):
            for _ in range(300):
                value = rng.getrandbits(32)
                written.add(value)
                ctx.set(dut.cs.i, value)
                await ctx.tick('src').repeat(rng.randrange(1, 6))
            state['final'] = value
            await ctx.tick('src').repeat(40)
            state['done'] = True

        async def consumer(ctx):
            assert ctx.get(dut.cs.o) == 0x1234
            changes = 0
            last = 0x1234
            while not state['done']:
                await ctx.tick('dst')
                value = ctx.get(dut.cs.o)
                # never a torn mix of two values
                assert value in written, hex(value)
                changes += value != last
                last = value
            assert ctx.get(dut.cs.o) == state['final']
            assert changes > 10

        run(dut, [producer, consumer], {'src': src_period,
                                        'dst': dst_period})

    def test_fast_to_slow(self):
        self.check(10e-9, 16e-9)

    def test_slow_to_fast(self):
        self.check(16e-9, 8e-9)

    def test_dead_destination(self):
        dut = ConfigDut()

        async def bench(ctx):
            ctx.set(dut.cs.i, 0x55)
            await ctx.tick('src').repeat(50)
            assert ctx.get(dut.cs.busy)
            assert ctx.get(dut.cs.o) == 0x1234

        run(dut, [bench], {'src': 10e-9})


class CrossingDut(Elaboratable):
    def __init__(self):
        self.xing = DomainCrossing('src', 'dst', name='t')
        self.cfg_a = Signal(16)
        self.cfg_b = Signal(8, init=0x5A)
        self.cmd = Signal(2)
        self.cmd_stb = Signal()
        self.a_o = self.xing.config(self.cfg_a, name='a')
        self.b_o = self.xing.config(self.cfg_b, init=0x5A, name='b')
        self.cmd_o = self.xing.command(self.cmd, self.cmd_stb, name='c')

    def elaborate(self, platform):
        m = Module()
        m.domains.src = ClockDomain()
        m.domains.dst = ClockDomain()
        m.submodules.xing = self.xing
        return m


async def write_cmd(ctx, dut, value):
    ctx.set(dut.cmd, value)
    ctx.set(dut.cmd_stb, 1)
    await ctx.tick('src')
    ctx.set(dut.cmd_stb, 0)
    ctx.set(dut.cmd, 0)


class TestDomainCrossing(unittest.TestCase):
    def check_order(self, src_period, dst_period, seed):
        # Each command is written right after a config write with a new
        # increasing id. Pulse k must see a config id >= the id written
        # before command k (configuration never arrives after a command
        # written after it), and the last pulse sees the last id.
        dut = CrossingDut()
        rng = random.Random(seed)
        ids = []
        pulses = []
        state = {'done': False}

        async def producer(ctx):
            await ctx.tick('src').repeat(3)
            for n in range(1, 40):
                ctx.set(dut.cfg_a, n)
                await ctx.tick('src').repeat(rng.choice([1, 1, 2, 5]))
                ids.append(n)
                await write_cmd(ctx, dut, 0b01)
                gap = rng.choice([0, 0, 1, 3, 20])
                if gap:
                    await ctx.tick('src').repeat(gap)
            await ctx.tick('src').repeat(100)
            state['done'] = True

        async def consumer(ctx):
            while not state['done']:
                await ctx.tick('dst')
                if ctx.get(dut.cmd_o) & 1:
                    pulses.append(ctx.get(dut.a_o))

        run(dut, [producer, consumer], {'src': src_period,
                                        'dst': dst_period})
        self.assertGreater(len(pulses), 5)
        self.assertLessEqual(len(pulses), len(ids))
        for k, a in enumerate(pulses):
            self.assertGreaterEqual(a, ids[k], (k, pulses, ids))
        self.assertEqual(pulses[-1], ids[-1])
        self.assertEqual(pulses, sorted(pulses))

    def test_order_fast_to_slow(self):
        self.check_order(10e-9, 16e-9, 1)

    def test_order_slow_to_fast(self):
        self.check_order(16e-9, 7e-9, 2)

    def test_order_much_slower_destination(self):
        self.check_order(10e-9, 90e-9, 3)

    def test_commands_not_lost(self):
        dut = CrossingDut()
        count = {'c0': 0, 'c1': 0, 'both': 0}

        async def producer(ctx):
            await ctx.tick('src').repeat(3)
            # well separated commands: one pulse each
            for _ in range(3):
                await write_cmd(ctx, dut, 0b01)
                await ctx.tick('src').repeat(40)
            # an even number of quick writes must not cancel out
            await write_cmd(ctx, dut, 0b10)
            await write_cmd(ctx, dut, 0b10)
            await ctx.tick('src').repeat(40)
            # both bits in one write arrive in the same cycle
            await write_cmd(ctx, dut, 0b11)
            await ctx.tick('src').repeat(40)
            assert not ctx.get(dut.xing.busy)

        async def consumer(ctx):
            for _ in range(250):
                await ctx.tick('dst')
                v = ctx.get(dut.cmd_o)
                count['c0'] += v & 1
                count['c1'] += v >> 1
                count['both'] += v == 0b11

        run(dut, [producer, consumer], {'src': 10e-9, 'dst': 17e-9})
        self.assertEqual(count['c0'], 4)
        self.assertIn(count['c1'], (2, 3))
        self.assertEqual(count['both'], 1)

    def test_initial_values(self):
        dut = CrossingDut()

        async def bench(ctx):
            assert ctx.get(dut.b_o) == 0x5A
            await ctx.tick('dst').repeat(20)
            assert ctx.get(dut.b_o) == 0x5A
            assert ctx.get(dut.cmd_o) == 0
            assert not ctx.get(dut.xing.busy)

        run(dut, [bench], {'src': 10e-9, 'dst': 17e-9})

    def test_dead_destination(self):
        dut = CrossingDut()

        async def producer(ctx):
            ctx.set(dut.cfg_a, 7)
            await write_cmd(ctx, dut, 0b01)
            await ctx.tick('src').repeat(50)
            assert ctx.get(dut.xing.busy)
            assert ctx.get(dut.a_o) == 0

        run(dut, [producer], {'src': 10e-9})


if __name__ == '__main__':
    unittest.main()
