#
# Fishball radio core - the 8-VSB decoder's stages against the model, bit for bit (doc/changes/085):
# the trellis decoders' soft symbols and decisions from the model's equalizer output, and the bytes
# they make placed as the transmitter's byte map (fec::byte_map) places them; the deinterleaver and
# Reed-Solomon's syndromes.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from radio_core.vsb.deinterleaver import Deinterleaver
from radio_core.vsb.tables import (DEINTERLEAVER_DELAY, FIELD, PER_FIELD, SEGMENT, SYNC_SYMBOLS,
                                   byte_of, syndromes)
from radio_core.vsb.trellis import TrellisDecoders

from .vsb_vectors import Vectors


def data_symbols(eq_out, count):
    """The first `count` data symbols from the first field's start: (e, k among the data symbols)."""
    out = []
    m = 0
    while len(out) < count:
        within = m % FIELD
        seg, place = divmod(within, SEGMENT)
        if seg >= 1 and place >= SYNC_SYMBOLS:
            out.append((int(eq_out[m]), place - SYNC_SYMBOLS))
        m += 1
    return out


class TestTrellis(unittest.TestCase):
    def test_decisions_and_bytes(self):
        w = Vectors().window('decoder')
        decisions = w['decisions']
        count = len(decisions)
        symbols = data_symbols(w['eq_out'], count)
        dut = TrellisDecoders()
        got = []
        got_bytes = []

        async def bench(ctx):
            for e, k in symbols:
                ctx.set(dut.in_valid, 1)
                ctx.set(dut.in_e, e)
                ctx.set(dut.in_k, k)
                await ctx.tick()
                ctx.set(dut.in_valid, 0)
                for _ in range(4):
                    await ctx.tick()
            for _ in range(10):
                await ctx.tick()

        async def collect(ctx):
            async for _ in ctx.tick():
                if ctx.get(dut.dec_valid):
                    got.append((ctx.get(dut.dec_enc), ctx.get(dut.dec_soft),
                                ctx.get(dut.dec_pair) if ctx.get(dut.dec_decided) else 255))
                if ctx.get(dut.byte_valid):
                    got_bytes.append((ctx.get(dut.byte_field), ctx.get(dut.byte_index), ctx.get(dut.byte)))

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(collect, background=True)
        sim.add_testbench(bench)
        sim.run()

        self.assertEqual(len(got), count)
        for i, (enc, soft, pair) in enumerate(got):
            want = decisions[i]
            self.assertEqual((enc, soft, pair), (int(want['enc']), int(want['soft']), int(want['pair'])),
                             f'data symbol {i}')
        # The bytes: each decoder's p-th decision is the pair at place p mod 4 of its byte p / 4
        # (within the field), placed by the transmitter's byte map.
        made = [0] * 12
        acc = [0] * 12
        expected = []
        for enc, _, pair in got:
            if pair == 255:
                continue
            p = made[enc]
            made[enc] += 1
            field, q = divmod(p, PER_FIELD)
            acc[enc] = (acc[enc] << 2 | pair) & 0xFF
            if q % 4 == 3:
                expected.append((field % 8, byte_of(enc, q // 4), acc[enc]))
        self.assertEqual(got_bytes, expected)
        self.assertGreater(len(expected), 2000)


class TestDeinterleaver(unittest.TestCase):
    """The deinterleaver's bytes against the index relation of 081's map (byte m out is byte
    m + 208 (m mod 52) in), the syndromes against fec::rs's, the codewords counted."""
    def test_bytes_codewords_and_flags(self):
        import random
        rng = random.Random(85)
        n = DEINTERLEAVER_DELAY + 5 * 207 * 2
        stream = [rng.randrange(256) for _ in range(n)]
        # Codewords 1 and 3 out are all zero (a codeword the code holds): their bytes in.
        for c in (1, 3):
            for i in range(207):
                m = 207 * c + i
                stream[m + 208 * (m % 52)] = 0
        dut = Deinterleaver()
        out = []
        ends = []

        async def bench(ctx):
            for b in stream:
                ctx.set(dut.in_valid, 1)
                ctx.set(dut.in_byte, b)
                await ctx.tick()
                ctx.set(dut.in_valid, 0)
                await ctx.tick()
            for _ in range(8):
                await ctx.tick()

        async def collect(ctx):
            async for _ in ctx.tick():
                if ctx.get(dut.out_valid):
                    out.append((ctx.get(dut.out_byte), ctx.get(dut.out_place), ctx.get(dut.out_codeword)))
                    if ctx.get(dut.out_end):
                        ends.append(ctx.get(dut.out_flag))

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(collect, background=True)
        sim.add_testbench(bench)
        sim.run()
        expect = [stream[m + 208 * (m % 52)] for m in range(n - DEINTERLEAVER_DELAY)]
        self.assertEqual([o[0] for o in out], expect)
        self.assertEqual([o[1] for o in out], [m % 207 for m in range(len(expect))])
        self.assertEqual([o[2] for o in out], [m // 207 for m in range(len(expect))])
        flags = [any(syndromes(expect[207 * c:207 * (c + 1)])) for c in range(len(expect) // 207)]
        self.assertEqual(ends, [int(f) for f in flags])
        self.assertEqual(flags[1], False)
        self.assertEqual(flags[3], False)
        self.assertTrue(flags[0])


if __name__ == '__main__':
    unittest.main()
