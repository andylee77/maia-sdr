#
# Fishball hwval -- ClockCensus tests
#
# Covers:
#
#   1.  Clocks of different periods (ref 10 ns, sync 16 ns, clk3x
#       5.333 ns, sampling 125 ns, a 1 us domain that needs the extended
#       settle wait) and a sampled 25 ns square wave: counts match
#       gate * T_ref / T within 2 counts; gate_actual == gate.
#   2.  A domain with no clock reports 0 and the census still completes.
#   3.  Repeated gates of different lengths; a clock stopped between
#       gates reports 0 (not its stale count) and counts again after it
#       restarts; start while busy is ignored.
#   4.  gate = 0 completes with all counts 0.
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from hwval_hdl.clock_census import ClockCensus


REF = 10e-9
PERIODS = {
    'sync': 16e-9,
    'clk3x': 16e-9 / 3,
    'sampling': 125e-9,
    'slow': 1e-6,
}
CLKOUT = 25e-9


class CensusTb(Elaboratable):
    """Census plus a software-driven `stoppable` domain."""
    def __init__(self, census):
        self.census = census
        self.stoppable = ClockDomain('stoppable')

    def elaborate(self, platform):
        m = Module()
        m.domains += self.stoppable
        m.submodules.census = self.census
        return m


class TestClockCensus(unittest.TestCase):
    def build(self):
        counted = [(name, name) for name in PERIODS]
        counted += [('dead', 'dead'), ('stoppable', 'stoppable')]
        census = ClockCensus(counted, ['clkout'],
                             ref_domain='s_axi_lite',
                             sampler_domain='clk3x')
        return census, CensusTb(census)

    def simulate(self, tb, bench, stop_flag=None):
        sim = Simulator(tb)
        sim.add_clock(REF, domain='s_axi_lite')
        for name, period in PERIODS.items():
            sim.add_clock(period, domain=name)
        census = tb.census

        async def clkout(ctx):
            while True:
                ctx.set(census.inputs['clkout'], 1)
                await ctx.delay(CLKOUT / 2)
                ctx.set(census.inputs['clkout'], 0)
                await ctx.delay(CLKOUT / 2)

        async def stoppable(ctx):
            # 20 ns clock while stop_flag[0] is False
            clk = tb.stoppable.clk
            while True:
                if stop_flag is not None and stop_flag[0]:
                    await ctx.delay(20e-9)
                    continue
                ctx.set(clk, 1)
                await ctx.delay(10e-9)
                ctx.set(clk, 0)
                await ctx.delay(10e-9)

        sim.add_process(clkout)
        sim.add_process(stoppable)
        sim.add_testbench(bench)
        sim.run()

    async def measure(self, ctx, census, gate, timeout=200000):
        ctx.set(census.gate, gate)
        ctx.set(census.start, 1)
        await ctx.tick('s_axi_lite')
        ctx.set(census.start, 0)
        self.assertEqual(ctx.get(census.busy), 1)
        self.assertEqual(ctx.get(census.done), 0)
        for _ in range(timeout):
            await ctx.tick('s_axi_lite')
            if ctx.get(census.done):
                break
        else:
            self.fail('census did not complete')
        self.assertEqual(ctx.get(census.busy), 0)
        return ({name: ctx.get(sig) for name, sig in census.counts.items()},
                ctx.get(census.gate_actual))

    def expected(self, gate):
        exp = {name: gate * REF / period for name, period in PERIODS.items()}
        exp['clkout'] = gate * REF / CLKOUT
        exp['dead'] = 0
        exp['stoppable'] = gate * REF / 20e-9
        return exp

    def check(self, counts, gate, overrides={}):
        exp = self.expected(gate)
        exp.update(overrides)
        for name, value in exp.items():
            self.assertLessEqual(
                abs(counts[name] - value), 2,
                f'{name}: got {counts[name]}, expected {value}')

    def test_frequencies(self):
        census, tb = self.build()

        async def bench(ctx):
            for gate in (2000, 3001):
                counts, actual = await self.measure(ctx, census, gate)
                self.assertEqual(actual, gate)
                self.check(counts, gate)
                self.assertEqual(counts['dead'], 0)

        self.simulate(tb, bench)

    def test_repeat_and_stopped_clock(self):
        census, tb = self.build()
        stop = [False]

        async def bench(ctx):
            counts, _ = await self.measure(ctx, census, 1500)
            self.check(counts, 1500)
            stop[0] = True
            for _ in range(10):
                await ctx.tick('s_axi_lite')
            counts, _ = await self.measure(ctx, census, 800)
            self.check(counts, 800, {'stoppable': 0})
            self.assertEqual(counts['stoppable'], 0)
            stop[0] = False
            counts, actual = await self.measure(ctx, census, 1200)
            self.assertEqual(actual, 1200)
            self.check(counts, 1200)
            # start while busy is ignored
            ctx.set(census.gate, 500)
            ctx.set(census.start, 1)
            await ctx.tick('s_axi_lite')
            await ctx.tick('s_axi_lite')
            ctx.set(census.gate, 100)
            await ctx.tick('s_axi_lite')
            ctx.set(census.start, 0)
            for _ in range(100000):
                await ctx.tick('s_axi_lite')
                if ctx.get(census.done):
                    break
            self.assertEqual(ctx.get(census.gate_actual), 500)
            self.check({n: ctx.get(s) for n, s in census.counts.items()},
                       500)

        self.simulate(tb, bench, stop)

    def test_slow_domain_settle_and_midgate_stop(self):
        # A 5 us domain: its synchronized gate falls 1-2 periods (500+
        # ref cycles) after the ref gate, far beyond settle_cycles, so
        # completion must wait for its echo. A clock that stops while
        # its gate is high keeps its echo high: completion then waits
        # for settle_timeout and the partial count is reported.
        census = ClockCensus([('vslow', 'vslow'), ('stoppable', 'stoppable')],
                             settle_cycles=64, settle_timeout=4000)
        tb = CensusTb(census)
        stop = [False]
        sim = Simulator(tb)
        sim.add_clock(REF, domain='s_axi_lite')
        sim.add_clock(5e-6, domain='vslow')

        async def stoppable(ctx):
            clk = tb.stoppable.clk
            while True:
                if stop[0]:
                    await ctx.delay(20e-9)
                    continue
                ctx.set(clk, 1)
                await ctx.delay(10e-9)
                ctx.set(clk, 0)
                await ctx.delay(10e-9)

        async def bench(ctx):
            gate = 10000
            ctx.set(census.gate, gate)
            ctx.set(census.start, 1)
            await ctx.tick('s_axi_lite')
            ctx.set(census.start, 0)
            cycles = 0
            while not ctx.get(census.done):
                await ctx.tick('s_axi_lite')
                cycles += 1
            # at least one vslow period (>> settle_cycles = 64)
            self.assertGreater(cycles, gate + 500)
            self.assertLess(cycles, gate + 4000)
            self.assertLessEqual(abs(ctx.get(census.counts['vslow']) - 20), 1)
            self.assertLessEqual(
                abs(ctx.get(census.counts['stoppable']) - gate / 2), 2)
            # stop the clock in the middle of the next gate
            ctx.set(census.gate, 2000)
            ctx.set(census.start, 1)
            await ctx.tick('s_axi_lite')
            ctx.set(census.start, 0)
            for _ in range(1000):
                await ctx.tick('s_axi_lite')
            stop[0] = True
            cycles = 1000
            while not ctx.get(census.done):
                await ctx.tick('s_axi_lite')
                cycles += 1
            self.assertGreaterEqual(cycles, 2000 + 4000)
            partial = ctx.get(census.counts['stoppable'])
            self.assertLessEqual(abs(partial - 500), 3)
            self.assertLessEqual(abs(ctx.get(census.counts['vslow']) - 4), 1)

        sim.add_process(stoppable)
        sim.add_testbench(bench)
        sim.run()

    def test_zero_gate(self):
        census, tb = self.build()

        async def bench(ctx):
            counts, _ = await self.measure(ctx, census, 1000)
            self.check(counts, 1000)
            counts, actual = await self.measure(ctx, census, 0)
            self.assertEqual(actual, 0)
            self.assertTrue(all(v == 0 for v in counts.values()))

        self.simulate(tb, bench)


if __name__ == '__main__':
    unittest.main()
