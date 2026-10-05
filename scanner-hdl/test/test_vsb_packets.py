#
# Fishball radio core - the 8-VSB packets (doc/changes/085): codewords in format 2 packets (083's
# layout, pl/packets.rs) against a reference packer, a partial packet closed by a flush, a packet
# the slots could not take and the next one marked lost; snapshots in format 3, two in a row, each
# value at its place.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from radio_core.vsb.packets import CodewordPackets, SnapshotPackets

MASK = (1 << 64) - 1


def fold(w):
    return (w ^ (w >> 32)) & 0xFFFF_FFFF


def info_of(field):
    """A field's header values in the tests."""
    return dict(field=field, index=0x1_0000_0000 + 1000 * field, freq=(-1234 - field) & 0xFFFF_FFFF,
                rate=(56 + field) & 0xFFFF_FFFF, gain=300 + field, sync=-(1000 + field),
                tracking=field % 2, inverted=(field + 1) % 2, found=1, loaded=field // 2,
                energy_field=field, energy=123_456 * (field + 1))


def set_info(ctx, info, values):
    for name, v in values.items():
        ctx.set(getattr(info, name), v)


def codeword_packet(cws, seq, flags):
    """pl/packets.rs `pack` for one packet, the error energy of the field before."""
    field, index = cws[0][0], cws[0][1]
    i = info_of(field)
    before = info_of(field - 1)
    words = [0] * 512
    words[0] = 0x5243 | 2 << 16 | 15 << 20 | flags << 24 | len(cws) << 32 | seq << 48
    words[1] = i['index']
    words[2] = field | index << 32 | i['loaded'] << 48
    words[3] = (before['energy'] + 8) >> 4
    words[4] = i['freq'] | i['rate'] << 32
    words[5] = i['gain'] | (i['sync'] & 0xFFFF_FFFF) << 32
    payload = bytearray(4096 - 64)
    for c, (_, _, data, flag) in enumerate(cws):
        payload[208 * c:208 * c + 207] = bytes(data)
        payload[208 * c + 207] = flag
    for k in range(504):
        words[8 + k] = int.from_bytes(payload[8 * k:8 * k + 8], 'little')
    check = 0
    for w in words:
        check ^= fold(w)
    words[7] = check
    return words


class Reader:
    """The lane ring's read side, a word a cycle."""
    def __init__(self, src):
        self.src = src
        self.packets = []
        self.hold = False

    async def run(self, ctx):
        s = self.src
        while True:
            await ctx.tick()
            if self.hold or not ctx.get(s.ready):
                continue
            words = []
            for a in range(512):
                ctx.set(s.rd_addr, a)
                ctx.set(s.rd_en, 1)
                await ctx.tick()
                ctx.set(s.rd_en, 0)
                words.append(ctx.get(s.rd_data))
            ctx.set(s.release, 1)
            await ctx.tick()
            ctx.set(s.release, 0)
            self.packets.append(words)


class TestCodewordPackets(unittest.TestCase):
    def test_packets(self):
        dut = CodewordPackets()
        reader = Reader(dut)
        # 40 codewords from field 5, place 300 (field 6 from the 13th), then a flush.
        cws = []
        for c in range(40):
            field, index = (5, 300 + c) if c < 12 else (6, c - 12)
            data = [(c * 31 + k * 7) & 0xFF for k in range(207)]
            cws.append((field, index, data, c % 3 == 0))
        lost = []

        async def feed(ctx, run):
            for field, index, data, flag in run:
                set_info(ctx, dut.info, info_of(field))
                set_info(ctx, dut.info_before, info_of(field - 1))
                for k in range(207):
                    ctx.set(dut.in_valid, 1)
                    ctx.set(dut.in_byte, data[k])
                    ctx.set(dut.in_place, k)
                    ctx.set(dut.in_codeword, index)
                    ctx.set(dut.in_field, field)
                    ctx.set(dut.in_end, k == 206)
                    ctx.set(dut.in_flag, flag)
                    await ctx.tick()
                    if ctx.get(dut.lost):
                        lost.append(index)
                    ctx.set(dut.in_valid, 0)
                    await ctx.tick()

        async def bench(ctx):
            await feed(ctx, cws)
            ctx.set(dut.flush, 1)
            await ctx.tick()
            ctx.set(dut.flush, 0)
            await ctx.tick().repeat(1200)
            # The ring stalled: two packets fill the slots, the third is dropped, the fourth
            # says so.
            reader.hold = True
            more = [(7, k, [k] * 207, 0) for k in range(4 * 19)]
            await feed(ctx, more[:3 * 19])
            reader.hold = False
            await ctx.tick().repeat(2200)
            await feed(ctx, more[3 * 19:])
            await ctx.tick().repeat(1200)

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(reader.run, background=True)
        sim.add_testbench(bench)
        sim.run()

        flags = 0b0110
        want = [codeword_packet(cws[0:19], 0, flags | 1 << 3),     # field 5: tracking (odd)
                codeword_packet(cws[19:38], 1, flags),            # field 6
                codeword_packet(cws[38:40], 2, flags)]            # flushed: two codewords
        self.assertEqual(len(reader.packets), 6)
        for k in range(3):
            got = reader.packets[k]
            self.assertEqual([hex(w) for w in got[:8]], [hex(w) for w in want[k][:8]], f'packet {k}')
            self.assertEqual(got, want[k], f'packet {k}')
        self.assertEqual(lost, list(range(38, 57)))
        self.assertEqual([(p[0] >> 24) & 1 for p in reader.packets[3:]], [0, 0, 1])
        self.assertEqual([(p[0] >> 48) & 0xFFFF for p in reader.packets[3:]], [3, 4, 5])


class TestSnapshotPackets(unittest.TestCase):
    def test_two_snapshots(self):
        dut = SnapshotPackets()
        reader = Reader(dut)

        def y_of(i):
            return (i * 37) % 4095 - 2047

        async def feed(ctx, field, segments):
            i0 = field * 1_000_000
            for seg in segments:
                f = field + (seg >= 313)
                set_info(ctx, dut.info, info_of(f if seg >= 313 else field + 1))
                for place in range(832):
                    ctx.set(dut.in_valid, 1)
                    ctx.set(dut.in_y, y_of(i0 + seg * 832 + place))
                    ctx.set(dut.in_fields, 1)
                    ctx.set(dut.in_segment, seg % 313)
                    ctx.set(dut.in_place, place)
                    ctx.set(dut.in_field, f)
                    await ctx.tick()
                    ctx.set(dut.in_valid, 0)

        async def bench(ctx):
            ctx.set(dut.request, 1)
            await ctx.tick()
            ctx.set(dut.request, 0)
            await feed(ctx, 4, [311, 312] + list(range(313, 313 + 12)))
            ctx.set(dut.request, 1)
            await ctx.tick()
            ctx.set(dut.request, 0)
            await feed(ctx, 9, [311, 312] + list(range(313, 313 + 12)))
            await ctx.tick().repeat(1200)

        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(reader.run, background=True)
        sim.add_testbench(bench)
        sim.run()

        self.assertEqual(len(reader.packets), 10)
        for s, field in enumerate((4, 9)):
            values = []
            for k in range(5):
                p = reader.packets[5 * s + k]
                count = (p[0] >> 32) & 0xFFFF
                self.assertEqual(p[0] & 0xFFFFFF, 0x5243 | 3 << 16 | 15 << 20)
                self.assertEqual(p[2] & 0xFFFF_FFFF, field + 1)
                self.assertEqual((p[2] >> 32) & 0xFFFF, 2016 * k)
                self.assertEqual(p[2] >> 48, info_of(field + 1)['loaded'])
                self.assertEqual(p[1], info_of(field + 1)['index'])
                check = 0
                for w in p:
                    check ^= fold(w)
                self.assertEqual(check, 0)
                for v in range(count):
                    x = (p[8 + v // 4] >> (16 * (v % 4))) & 0xFFFF
                    values.append(x - 0x10000 if x & 0x8000 else x)
            i0 = field * 1_000_000 + 312 * 832
            self.assertEqual(values, [y_of(i0 + v) for v in range(9984)], f'snapshot {s}')


if __name__ == '__main__':
    unittest.main()
