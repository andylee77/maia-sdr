#
# Fishball radio core - the 8-VSB front end's stages against the model, bit for bit
# (doc/changes/085): the matched filter's evaluations, the datapath with its loops, and the whole
# front end with its scheduler, from the reset.
#
# The model's vectors (test/vsb_vectors.py) hold every symbol's instant and stage outputs on a
# capture from unit A, and each segment sync symbol's early and late evaluations.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from radio_core.vsb.datapath import Datapath, KIND_EARLY, KIND_LATE, KIND_MAIN
from radio_core.vsb.front import Front
from radio_core.vsb.matched_filter import CYCLES, MatchedFilter, table_words

from .vsb_vectors import Vectors


class FilterHarness(Elaboratable):
    """The matched filter, optionally feeding the datapath."""
    def __init__(self, with_datapath):
        self.mf = MatchedFilter(tag_width=24)
        self.dp = Datapath(tag_width=24) if with_datapath else None

    def elaborate(self, platform):
        m = Module()
        m.submodules.mf = self.mf
        if self.dp is not None:
            m.submodules.dp = dp = self.dp
            m.d.comb += [
                dp.in_valid.eq(self.mf.out_valid),
                dp.in_re.eq(self.mf.out_re),
                dp.in_im.eq(self.mf.out_im),
                dp.in_kind.eq(self.mf.out_tag[22:24]),
                dp.in_tag.eq(self.mf.out_tag),
            ]
        return m


def run_evaluations(harness, x, x_at, evaluations, table, collect):
    """Feeds the samples `x` (the first is sample `x_at`, a multiple of 64) as the evaluations
    need them and issues the evaluations (instant, kind, tag), at most one every `CYCLES` cycles;
    `collect(ctx)` runs in the background."""
    mf = harness.mf

    async def bench(ctx):
        for addr, word in enumerate(table_words(table)):
            ctx.set(mf.tw_en, 1)
            ctx.set(mf.tw_addr, addr)
            ctx.set(mf.tw_data, word)
            await ctx.tick()
        ctx.set(mf.tw_en, 0)
        written = 0
        for pos, kind, tag in evaluations:
            need = ((int(pos) + (1 << 25)) >> 32) + 21 - x_at
            while written < need:
                ctx.set(mf.wr_en, 1)
                ctx.set(mf.wr_re, int(x[written, 0]))
                ctx.set(mf.wr_im, int(x[written, 1]))
                await ctx.tick()
                written += 1
            ctx.set(mf.wr_en, 0)
            while not ctx.get(mf.ready):
                await ctx.tick()
            ctx.set(mf.issue, 1)
            ctx.set(mf.issue_pos, int(pos) - (x_at << 32))
            ctx.set(mf.issue_tag, tag | (kind << 22))
            await ctx.tick()
            ctx.set(mf.issue, 0)
            for _ in range(CYCLES - 1):
                await ctx.tick()
        for _ in range(64):
            await ctx.tick()

    sim = Simulator(harness)
    sim.add_clock(4e-9)
    sim.add_testbench(collect, background=True)
    sim.add_testbench(bench)
    sim.run()


def filtered_of(rec):
    return tuple(int(f) for f in rec['filtered'])


class TestMatchedFilter(unittest.TestCase):
    def check(self, window, count):
        w = Vectors().window(window)
        trace, trace_at = w['trace'], int(w['trace_at'])
        sides = w.get('sides', [])
        by_symbol = {}
        for s in sides:
            by_symbol.setdefault(int(s['n']), []).append(s)
        evaluations = []
        expected = []
        for i in range(count):
            n = trace_at + i
            for s in by_symbol.get(n, []):
                evaluations.append((int(s['pos']), KIND_LATE if s['late'] else KIND_EARLY, len(expected)))
                expected.append(filtered_of(s))
            evaluations.append((int(trace[i]['pos']), KIND_MAIN, len(expected)))
            expected.append(filtered_of(trace[i]))
        got = {}
        h = FilterHarness(False)

        async def collect(ctx):
            async for _ in ctx.tick():
                if ctx.get(h.mf.out_valid):
                    tag = ctx.get(h.mf.out_tag) & ((1 << 22) - 1)
                    got[tag] = (ctx.get(h.mf.out_re), ctx.get(h.mf.out_im))

        run_evaluations(h, w['x'], int(w['x_at']), evaluations, Vectors().mf_table(), collect)
        self.assertEqual(len(got), len(expected))
        bad = [(i, got[i], e) for i, e in enumerate(expected) if got[i] != e]
        self.assertEqual(bad[:5], [], f'{len(bad)} of {len(expected)} differ')
        return len(expected)

    def test_from_the_reset(self):
        self.check('reset', 300)

    def test_with_the_early_and_late_evaluations(self):
        self.assertGreater(self.check('count', 200), 200)


class TestDatapath(unittest.TestCase):
    """The matched filter and the datapath from the reset: every stage of every symbol."""
    def test_from_the_reset(self):
        w = Vectors().window('reset')
        trace = w['trace']
        count = 1500
        evaluations = [(int(trace[n]['pos']), KIND_MAIN, n) for n in range(count)]
        got = {}
        h = FilterHarness(True)

        async def collect(ctx):
            async for _ in ctx.tick():
                dp = h.dp
                if ctx.get(dp.out_valid):
                    n = ctx.get(dp.out_tag) & ((1 << 22) - 1)
                    got[n] = dict(
                        filtered=(ctx.get(dp.out_re_in), ctx.get(dp.out_im_in)),
                        gain=ctx.get(dp.out_gain),
                        gained=(ctx.get(dp.out_gained_re), ctx.get(dp.out_gained_im)),
                        theta=ctx.get(dp.out_theta),
                        turned=(ctx.get(dp.out_turned_re), ctx.get(dp.out_turned_im)),
                        dc=ctx.get(dp.out_dc),
                        y=ctx.get(dp.out_y))

        run_evaluations(h, w['x'], 0, evaluations, Vectors().mf_table(), collect)
        self.assertEqual(len(got), count)
        for n in range(count):
            self.assertEqual(got[n], stages(trace[n]), f'symbol {n}')


def stages(r):
    return dict(filtered=filtered_of(r), gain=int(r['gain']),
                gained=tuple(int(f) for f in r['gained']), theta=int(r['theta']),
                turned=tuple(int(f) for f in r['turned']), dc=int(r['dc']), y=int(r['y']))


def run_front(front, x, symbols, collect, step0, cycles_per_sample=18.75):
    """Runs the front end on `x` at the AD9361's pace (10 MSPS: 18.75 cycles of clk3x a sample)
    until `symbols` main symbols have come out; `collect(ctx)` runs in the background."""
    count = [0]
    table = Vectors().mf_table()

    async def bench(ctx):
        ctx.set(front.step0, step0)
        ctx.set(front.carrier0, 0)
        for addr, word in enumerate(table_words(table)):
            ctx.set(front.tw_en, 1)
            ctx.set(front.tw_addr, addr)
            ctx.set(front.tw_data, word)
            await ctx.tick()
        ctx.set(front.tw_en, 0)
        k = 0
        due = 0.0
        cycle = 0
        while count[0] < symbols and k < len(x):
            if cycle >= due:
                ctx.set(front.wr_en, 1)
                ctx.set(front.wr_re, int(x[k, 0]))
                ctx.set(front.wr_im, int(x[k, 1]))
                k += 1
                due += cycles_per_sample
            else:
                ctx.set(front.wr_en, 0)
            await ctx.tick()
            cycle += 1
        ctx.set(front.wr_en, 0)

    async def counter(ctx):
        async for _ in ctx.tick():
            if ctx.get(front.y_valid):
                count[0] += 1

    sim = Simulator(front)
    sim.add_clock(4e-9)
    sim.add_testbench(collect, background=True)
    sim.add_testbench(counter, background=True)
    sim.add_testbench(bench)
    sim.run()


class TestFront(unittest.TestCase):
    """The front end from the reset, its scheduler setting the instants: every symbol's stages."""
    def test_from_the_reset(self):
        v = Vectors()
        w = v.window('reset')
        trace = w['trace']
        count = 1200
        front = Front()
        got = []

        async def collect(ctx):
            async for _ in ctx.tick():
                if ctx.get(front.y_valid):
                    got.append(dict(
                        n=ctx.get(front.y_n), pos=ctx.get(front.y_pos) & ((1 << 64) - 1),
                        filtered=(ctx.get(front.t_filtered[0]), ctx.get(front.t_filtered[1])),
                        gain=ctx.get(front.t_gain),
                        gained=(ctx.get(front.t_gained[0]), ctx.get(front.t_gained[1])),
                        theta=ctx.get(front.t_theta),
                        turned=(ctx.get(front.t_turned[0]), ctx.get(front.t_turned[1])),
                        dc=ctx.get(front.t_dc), y=ctx.get(front.y)))

        run_front(front, w['x'], count, collect, int(v.meta['step0']))
        self.assertGreaterEqual(len(got), count)
        for n in range(count):
            self.assertEqual(got[n], dict(n=n, pos=int(trace[n]['pos']), **stages(trace[n])),
                             f'symbol {n}')


if __name__ == '__main__':
    unittest.main()
