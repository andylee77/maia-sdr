#
# Fishball radio core - the 8-VSB equalizer against the model, bit for bit (doc/changes/085): the
# model's equalizer input across the field where its first tap set loads, the set armed as the
# model applies it; every output and the swap's field.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from radio_core.vsb.equalizer import Equalizer
from radio_core.vsb.tables import FIELD, SEGMENT

from .vsb_vectors import Vectors


def place_tag(n, field_starts):
    """A symbol's place as the front end tags it: fields counted, segment, place in segment."""
    for start in field_starts:
        if start <= n < start + FIELD:
            seg, place = divmod(n - start, SEGMENT)
            return 1 | (seg << 1) | (place << 10)
    return 0


class TestEqualizer(unittest.TestCase):
    def test_across_the_first_load(self):
        v = Vectors()
        w = v.window('equalizer')
        y, y_at = w['y'], int(w['y_at'])
        eq_out, eq_at = w['eq_out'], int(w['eq_out_at'])
        load = w['load'][0]
        starts = [int(f['start']) for f in w['fields']]
        first = int(v.meta['first_field'])
        at = int(load['start'])
        lo, hi = y_at, y_at + len(y)
        dut = Equalizer(tag_width=20)
        got = {}

        async def bench(ctx):
            for i, t in enumerate(load['taps']):
                ctx.set(dut.tap_wen, 1)
                ctx.set(dut.tap_addr, i)
                ctx.set(dut.tap_data, int(t))
                await ctx.tick()
            ctx.set(dut.tap_wen, 0)
            ctx.set(dut.arm, 1)
            await ctx.tick()
            ctx.set(dut.arm, 0)
            for n in range(lo, hi):
                ctx.set(dut.in_valid, 1)
                ctx.set(dut.in_y, int(y[n - y_at]))
                ctx.set(dut.in_tag, place_tag(n, starts))
                await ctx.tick()
                ctx.set(dut.in_valid, 0)
                for _ in range(16):
                    await ctx.tick()
            for _ in range(40):
                await ctx.tick()

        async def collect(ctx):
            k = 0
            async for _ in ctx.tick():
                if ctx.get(dut.out_valid):
                    # The first output is for the first symbol fed, once 32 more are in.
                    got[lo + k] = (ctx.get(dut.out_e), ctx.get(dut.out_loaded))
                    k += 1

        sim = Simulator(dut)
        sim.add_clock(4e-9)
        sim.add_testbench(collect, background=True)
        sim.add_testbench(bench)
        sim.run()
        # Outputs whose whole window was fed, from the first field on.
        checked = 0
        for m in range(lo + 128, hi - 32):
            if m < first:
                continue
            e, loaded = got[m]
            self.assertEqual(e, int(eq_out[m - eq_at]), f'symbol {m} ({m - at} from the load)')
            self.assertEqual(loaded, 1 if m >= at else 0, f'symbol {m}')
            checked += 1
        self.assertGreater(checked, 3000)


if __name__ == '__main__':
    unittest.main()
