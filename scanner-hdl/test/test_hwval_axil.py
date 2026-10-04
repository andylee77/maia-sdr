#
# Fishball hardware validation (hwval) - AXI4-Lite register file tests
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.sim import Simulator

import random
import unittest

from hwval_hdl.axil_regs import (
    AxiLiteRegisterFile, Reg, RegBlock, RegField, RegisterTable,
    UNMAPPED_READ_VALUE)
from amaranth_sim import AmaranthSim
from .hwval_axil_bfm import axil_read, axil_write

OKAY = 0


def make_table():
    return RegisterTable(
        'test', 0x4000_0000, 4096, [
            RegBlock('a', 0x000, [
                Reg('ID', 'ro', const=0xCAFE_F00D),
                Reg('RW0', 'rw', reset=0x1234_5678),
                Reg('RW1', 'rw', width=12, reset=0xABC,
                    fields=[RegField('lo', 0, 4), RegField('hi', 4, 8)]),
                Reg('WO', 'wo', width=8),
                Reg('W1C', 'w1c', width=4),
                Reg('RO', 'ro', width=16),
                Reg('LOCKED', 'rw', reset=0x55),
            ]),
            RegBlock('b', 0x100, [
                Reg('RW2', 'rw', offset=0x80),
                Reg('RW3', 'rw', offset=0xFC),
            ]),
            RegBlock('c', 0xF00, [
                Reg('RW4', 'rw', width=1),
            ]),
        ],
        id_reg='ID', id_value=0xCAFE_F00D, version='0.0.1',
        snapshot_domains={'sync': 1})


class TestRegisterTable(unittest.TestCase):
    def test_offsets(self):
        t = make_table()
        self.assertEqual(t.address('ID'), 0x000)
        self.assertEqual(t.address('RW0'), 0x004)
        self.assertEqual(t.address('LOCKED'), 0x018)
        self.assertEqual(t.address('RW2'), 0x180)
        self.assertEqual(t.address('RW3'), 0x1FC)
        self.assertEqual(t.address('RW4'), 0xF00)

    def test_validation(self):
        with self.assertRaises(ValueError):
            Reg('X', 'rw', width=4, reset=0x10)
        with self.assertRaises(ValueError):
            Reg('X', 'rw', width=8, fields=[RegField('a', 0, 4),
                                            RegField('b', 3, 2)])
        with self.assertRaises(ValueError):
            Reg('X', 'bogus')
        with self.assertRaises(ValueError):
            RegisterTable('t', 0, 4096, [
                RegBlock('a', 0, [Reg('ID', 'ro'), Reg('ID', 'rw')])],
                id_reg='ID', id_value=0, version='0',
                snapshot_domains={})
        with self.assertRaises(ValueError):
            RegisterTable('t', 0, 4096, [
                RegBlock('a', 0, [Reg('ID', 'ro'),
                                  Reg('X', 'ro', offset=0x100)])],
                id_reg='ID', id_value=0, version='0',
                snapshot_domains={})

    def test_json_schema(self):
        d = make_table().to_dict()
        self.assertEqual(
            list(d.keys()),
            ['schema', 'core', 'base', 'size', 'id_reg', 'id_value',
             'version', 'snapshot_domains', 'blocks'])
        self.assertEqual(d['schema'], 'fbench.regmap/1')
        self.assertEqual(d['base'], '0x40000000')
        self.assertEqual(d['id_value'], '0xCAFEF00D')
        reg = d['blocks'][1]['regs'][0]
        self.assertEqual(
            list(reg.keys()),
            ['name', 'offset', 'access', 'width', 'reset', 'snapshot',
             'desc', 'fields'])
        self.assertEqual(reg['offset'], '0x180')
        self.assertEqual(list(reg['fields'][0].keys()),
                         ['name', 'lsb', 'width', 'desc'])


class AxilDut(Elaboratable):
    def __init__(self):
        self.table = make_table()
        self.regs = AxiLiteRegisterFile(self.table, name='s_axi_lite')
        self.ro_src = Signal(16)
        self.w1c_set = Signal(4)
        self.pending = Signal(4)
        self.lock = Signal()
        self.clear_locked = Signal()
        self.wo_pulses = Signal(8)
        self.wo_last = Signal(8)

    def elaborate(self, platform):
        m = Module()
        m.submodules.regs = regs = self.regs
        m.d.comb += [
            regs['RO'].eq(self.ro_src),
            regs.rvalue['W1C'].eq(self.pending),
            regs.inhibit['LOCKED'].eq(self.lock),
            regs.clear['LOCKED'].eq(self.clear_locked),
        ]
        m.d.sync += self.pending.eq(
            (self.pending & ~regs['W1C']) | self.w1c_set)
        with m.If(regs.wstb['WO']):
            m.d.sync += [
                self.wo_pulses.eq(self.wo_pulses + 1),
                self.wo_last.eq(regs['WO']),
            ]
        return m


class TestAxiLiteRegisterFile(AmaranthSim):
    def setUp(self):
        self.dut = AxilDut()
        self.axi = self.dut.regs.axi
        self.t = self.dut.table

    def a(self, name):
        return self.t.address(name)

    def test_reset_values(self):
        async def bench(ctx):
            ctx.set(self.dut.ro_src, 0xBEEF)
            await ctx.tick()
            expected = {
                'ID': 0xCAFE_F00D, 'RW0': 0x1234_5678, 'RW1': 0xABC,
                'WO': 0, 'W1C': 0, 'RO': 0xBEEF, 'LOCKED': 0x55,
                'RW2': 0, 'RW3': 0, 'RW4': 0,
            }
            for name, value in expected.items():
                rdata, rresp = await axil_read(
                    ctx, self.axi, self.a(name), with_resp=True)
                assert rresp == OKAY
                assert rdata == value, f'{name}: {rdata:#x}'

        self.simulate(bench)

    def test_rw_and_strobes(self):
        async def bench(ctx):
            assert await axil_write(ctx, self.axi, self.a('RW0'),
                                    0xDEAD_BEEF) == OKAY
            assert await axil_read(ctx, self.axi, self.a('RW0')) == \
                0xDEAD_BEEF
            assert ctx.get(self.dut.regs['RW0']) == 0xDEAD_BEEF
            # byte strobes 0b0101: only bytes 0 and 2 change
            await axil_write(ctx, self.axi, self.a('RW0'), 0x1122_3344,
                             strb=0b0101)
            assert await axil_read(ctx, self.axi, self.a('RW0')) == \
                0xDE22_BE44
            # narrow register: upper bits are not implemented
            await axil_write(ctx, self.axi, self.a('RW1'), 0xFFFF_F123)
            assert await axil_read(ctx, self.axi, self.a('RW1')) == 0x123
            assert ctx.get(self.dut.regs.field('RW1', 'hi')) == 0x12
            # ro and const registers ignore writes
            await axil_write(ctx, self.axi, self.a('ID'), 0)
            assert await axil_read(ctx, self.axi, self.a('ID')) == \
                0xCAFE_F00D
            # sub-word address bits are ignored
            await axil_write(ctx, self.axi, self.a('RW2') + 3, 0x77)
            assert await axil_read(ctx, self.axi, self.a('RW2') + 1) == 0x77
            # last word of the window
            await axil_write(ctx, self.axi, self.a('RW4'), 0xFFFF_FFFF)
            assert await axil_read(ctx, self.axi, self.a('RW4')) == 1

        self.simulate(bench)

    def test_unmapped(self):
        async def bench(ctx):
            unmapped = [0x01C, 0x0FC, 0x100, 0x17C, 0x200, 0x7F0, 0xEFC,
                        0xF04, 0xFFC]
            for addr in unmapped:
                rdata, rresp = await axil_read(ctx, self.axi, addr,
                                               with_resp=True)
                assert rresp == OKAY
                assert rdata == UNMAPPED_READ_VALUE, f'{addr:#x}'
                assert await axil_write(ctx, self.axi, addr,
                                        0xFFFF_FFFF) == OKAY
            # nothing was modified by the unmapped writes
            assert await axil_read(ctx, self.axi, self.a('RW0')) == \
                0x1234_5678
            assert await axil_read(ctx, self.axi, self.a('RW2')) == 0
            assert await axil_read(ctx, self.axi, self.a('RW3')) == 0
            assert ctx.get(self.dut.wo_pulses) == 0
            # the bus still works
            await axil_write(ctx, self.axi, self.a('RW3'), 42)
            assert await axil_read(ctx, self.axi, self.a('RW3')) == 42

        self.simulate(bench)

    def test_wo_pulse(self):
        async def bench(ctx):
            pulses = []

            async def watch():
                for _ in range(20):
                    pulses.append((ctx.get(self.dut.regs.wstb['WO']),
                                   ctx.get(self.dut.regs['WO'])))
                    await ctx.tick()

            await axil_write(ctx, self.axi, self.a('WO'), 0x5A)
            await watch()
            assert ctx.get(self.dut.wo_pulses) == 1
            assert ctx.get(self.dut.wo_last) == 0x5A
            assert all(p == (0, 0) for p in pulses)
            # reads of a wo register return 0 and do not pulse
            assert await axil_read(ctx, self.axi, self.a('WO')) == 0
            assert ctx.get(self.dut.wo_pulses) == 1
            # value strobe: writing 0 still produces a strobe
            await axil_write(ctx, self.axi, self.a('WO'), 0)
            await ctx.tick()
            assert ctx.get(self.dut.wo_pulses) == 2

        async def pulse_width(ctx):
            # count cycles in which the WO pulse is high
            high = 0
            for _ in range(300):
                await ctx.tick()
                if ctx.get(self.dut.regs.wstb['WO']):
                    high += 1
                    assert ctx.get(self.dut.regs['WO']) in (0x5A, 0)
            assert high == 2

        self.simulate([bench, pulse_width])

    def test_w1c(self):
        async def bench(ctx):
            ctx.set(self.dut.w1c_set, 0b1111)
            await ctx.tick()
            ctx.set(self.dut.w1c_set, 0)
            await ctx.tick()
            assert await axil_read(ctx, self.axi, self.a('W1C')) == 0b1111
            await axil_write(ctx, self.axi, self.a('W1C'), 0b0101)
            await ctx.tick()
            assert await axil_read(ctx, self.axi, self.a('W1C')) == 0b1010
            # byte strobe 0 masks the clear
            await axil_write(ctx, self.axi, self.a('W1C'), 0b1111, strb=0)
            await ctx.tick()
            assert await axil_read(ctx, self.axi, self.a('W1C')) == 0b1010
            await axil_write(ctx, self.axi, self.a('W1C'), 0b1111)
            await ctx.tick()
            assert await axil_read(ctx, self.axi, self.a('W1C')) == 0

        self.simulate(bench)

    def test_inhibit_and_clear(self):
        async def bench(ctx):
            await axil_write(ctx, self.axi, self.a('LOCKED'), 0x1)
            assert await axil_read(ctx, self.axi, self.a('LOCKED')) == 0x1
            ctx.set(self.dut.lock, 1)
            assert await axil_write(ctx, self.axi, self.a('LOCKED'),
                                    0x2) == OKAY
            assert await axil_read(ctx, self.axi, self.a('LOCKED')) == 0x1
            ctx.set(self.dut.clear_locked, 1)
            await ctx.tick()
            ctx.set(self.dut.clear_locked, 0)
            assert await axil_read(ctx, self.axi, self.a('LOCKED')) == 0x55
            ctx.set(self.dut.lock, 0)
            await axil_write(ctx, self.axi, self.a('LOCKED'), 0x3)
            assert await axil_read(ctx, self.axi, self.a('LOCKED')) == 0x3

        self.simulate(bench)

    def test_aw_w_ordering(self):
        async def bench(ctx):
            cases = [(0, 0), (0, 5), (5, 0), (3, 3), (0, 1), (1, 0)]
            for j, (aw_delay, w_delay) in enumerate(cases):
                value = 0x1000 + j
                resp = await axil_write(
                    ctx, self.axi, self.a('RW2'), value,
                    aw_delay=aw_delay, w_delay=w_delay, b_delay=j)
                assert resp == OKAY
                assert await axil_read(ctx, self.axi, self.a('RW2'),
                                       ar_delay=j, r_delay=j) == value

        self.simulate(bench)

    def test_back_to_back(self):
        async def bench(ctx):
            regs = ['RW0', 'RW2', 'RW3', 'LOCKED']
            values = {}
            for j in range(40):
                name = regs[j % len(regs)]
                values[name] = (0x9E37_79B9 * (j + 1)) & 0xFFFF_FFFF
                await axil_write(ctx, self.axi, self.a(name), values[name])
            for j in range(40):
                name = regs[j % len(regs)]
                assert await axil_read(ctx, self.axi, self.a(name)) == \
                    values[name]
                assert await axil_read(ctx, self.axi, 0x0F0) == \
                    UNMAPPED_READ_VALUE

        self.simulate(bench)

    def test_random_interleaving(self):
        rng = random.Random(1234)
        writable = ['RW0', 'RW2', 'RW3', 'LOCKED']
        readonly = {'ID': 0xCAFE_F00D, 'RO': 0x4321}
        # addresses owned by the writer; the reader only checks
        # registers whose value cannot change under it
        write_plan = []
        for _ in range(60):
            name = rng.choice(writable + ['UNMAPPED'])
            write_plan.append((name, rng.getrandbits(32),
                               rng.randrange(4), rng.randrange(4),
                               rng.randrange(4)))
        read_plan = []
        for _ in range(80):
            name = rng.choice(list(readonly) + ['UNMAPPED'])
            read_plan.append((name, rng.randrange(4), rng.randrange(4)))
        done = {'w': False, 'r': False}
        model = {name: None for name in writable}

        async def writer(ctx):
            ctx.set(self.dut.ro_src, 0x4321)
            for name, value, d0, d1, d2 in write_plan:
                addr = 0x0F4 if name == 'UNMAPPED' else self.a(name)
                assert await axil_write(ctx, self.axi, addr, value,
                                        aw_delay=d0, w_delay=d1,
                                        b_delay=d2) == OKAY
                if name != 'UNMAPPED':
                    model[name] = value
            done['w'] = True

        async def reader(ctx):
            await ctx.tick()
            for name, d0, d1 in read_plan:
                addr = 0x3F0 if name == 'UNMAPPED' else self.a(name)
                rdata = await axil_read(ctx, self.axi, addr, ar_delay=d0,
                                        r_delay=d1)
                expected = (UNMAPPED_READ_VALUE if name == 'UNMAPPED'
                            else readonly[name])
                assert rdata == expected, f'{name}: {rdata:#x}'
            done['r'] = True

        async def final_check(ctx):
            while not (done['w'] and done['r']):
                await ctx.tick()
            # after both finish, the register values match the model
            for name, value in model.items():
                if value is not None:
                    assert await axil_read(ctx, self.axi,
                                           self.a(name)) == value

        # the reader and writer share the AXI port (separate channels)
        self.simulate([writer, reader, final_check])


if __name__ == '__main__':
    unittest.main()
