#
# Fishball hardware validation (hwval) - top level and register map tests
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.sim import Simulator
import amaranth.back.verilog

import json
import os
import re
import unittest

from hwval_hdl.axil_regs import UNMAPPED_READ_VALUE
from hwval_hdl.config import HwvalConfig
from hwval_hdl.dna import DEFAULT_SIM_DNA
from hwval_hdl.regmap import (
    MARKDOWN_PREAMBLE, build_register_table)
from .hwval_axil_bfm import axil_read, axil_write

try:
    from hwval_hdl.hwval_top import HwvalCore
    from maia_hdl.pluto_platform import PlutoPlatform
    _import_error = None
except ImportError as e:  # a gateware submodule is not there yet
    HwvalCore = None
    _import_error = e

requires_core = unittest.skipIf(
    HwvalCore is None, f'hwval submodules missing: {_import_error}')

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))
DESIGN_DOC = os.path.join(REPO, 'doc', 'HW_VALIDATION_SUITE.md')
REGMAP_MD = os.path.join(REPO, 'doc', 'hwval_register_map.md')
REGMAP_JSON = os.path.join(REPO, 'bench', 'share', 'hwval_regs.json')

HWVAL_ID = 0x6877_7631

# Upper-case tokens of section 6.4 that are not register names
NON_REGISTER_TOKENS = {'DNA_PORT'}


def doc_register_names():
    """Register names of doc/HW_VALIDATION_SUITE.md section 6.4, with the
    mt blocks expanded to MT0_/MT1_ and EVT_DATA to EVT_DATA_LO/HI."""
    with open(DESIGN_DOC, encoding='utf-8') as f:
        text = f.read()
    start = text.index('### 6.4')
    end = text.index('## 7.', start)
    section = text[start:end]
    names = set()
    for token in re.findall(r'`([^`]+)`', section):
        m = re.fullmatch(r'([A-Z][A-Z0-9_]*[A-Z0-9])(_LO/HI)?', token)
        if not m:
            continue
        base = m.group(1)
        if base in NON_REGISTER_TOKENS:
            continue
        if m.group(2):
            expanded = [f'{base}_LO', f'{base}_HI']
        else:
            expanded = [base]
        for name in expanded:
            if name.startswith('MT_'):
                names.add('MT0_' + name[3:])
                names.add('MT1_' + name[3:])
            elif name == 'EVT_DATA':
                names.update({'EVT_DATA_LO', 'EVT_DATA_HI'})
            else:
                names.add(name)
    return names


class TestRegisterMap(unittest.TestCase):
    def setUp(self):
        self.table = build_register_table()

    def test_doc_names_present(self):
        names = doc_register_names()
        self.assertGreater(len(names), 150)
        missing = sorted(n for n in names if n not in self.table)
        self.assertEqual(missing, [])

    def test_block_bases(self):
        bases = {b.name: b.offset for b in self.table.blocks}
        self.assertEqual(bases, {
            'id': 0x000, 'census': 0x100, 'ingest': 0x200,
            'legacy': 0x400, 'ringv2': 0x500, 'mt0': 0x600, 'mt1': 0x700,
            'evt': 0x800})
        for reg in self.table.registers():
            off = self.table.address(reg)
            self.assertEqual(off & ~0xFF, reg.block.offset, reg.name)
            self.assertEqual(off % 4, 0)

    def test_id_block(self):
        t = self.table
        self.assertEqual(t.address('ID'), 0)
        self.assertEqual(t['ID'].const, HWVAL_ID)
        self.assertEqual(t['VERSION'].const, 0x000100)
        self.assertEqual(t['FEATURES'].const, 0x3FF)
        self.assertEqual(t['FEATURES'].field('dna').lsb, 9)
        self.assertEqual(t['CORE_RESET'].reset, 0)
        self.assertEqual(t['IRQ_CLEAR'].access, 'w1c')
        self.assertEqual(t['SNAP_REQ'].access, 'wo')
        for name in ['DNA_LO', 'DNA_HI', 'DNA_STATUS']:
            self.assertEqual(t[name].access, 'ro')
        self.assertEqual(t['DNA_HI'].width, 25)

    def test_snapshot_tags(self):
        t = self.table
        self.assertEqual(t['TS_LO'].snapshot, 'sync')
        self.assertEqual(t['MT1_BYTES_WR_HI'].snapshot, 'mem')
        self.assertEqual(t['PRBS_ERRORS'].snapshot, 'sampling')
        self.assertIsNone(t['RINGV2_COMMITTED_BURSTS'].snapshot)
        self.assertIsNone(t['RINGV2_STATUS'].snapshot)
        self.assertIsNone(t['CENSUS_SYNC'].snapshot)

    def test_json(self):
        d = json.loads(self.table.to_json())
        self.assertEqual(d['schema'], 'fbench.regmap/1')
        self.assertEqual(d['core'], 'hwval')
        self.assertEqual(d['base'], '0x7C460000')
        self.assertEqual(d['size'], 4096)
        self.assertEqual(d['id_reg'], 'ID')
        self.assertEqual(d['id_value'], '0x68777631')
        self.assertEqual(d['version'], '0.1.0')
        self.assertEqual(d['snapshot_domains'],
                         {'sync': 1, 'mem': 2, 'sampling': 4})
        self.assertEqual([b['name'] for b in d['blocks']],
                         ['id', 'census', 'ingest', 'legacy', 'ringv2',
                          'mt0', 'mt1', 'evt'])
        reg = d['blocks'][0]['regs'][0]
        self.assertEqual(reg['name'], 'ID')
        self.assertEqual(reg['offset'], '0x000')
        for block in d['blocks']:
            self.assertRegex(block['offset'], r'^0x[0-9A-F]{3}$')
            for reg in block['regs']:
                self.assertRegex(reg['offset'], r'^0x[0-9A-F]{3}$')
                self.assertIn(reg['access'], ('ro', 'rw', 'wo', 'w1c'))
                self.assertIn(reg['snapshot'],
                              (None, 'sync', 'mem', 'sampling'))
                self.assertRegex(reg['reset'], r'^0x[0-9A-F]+$')
                self.assertTrue(reg['fields'])

    def test_svd(self):
        import xml.etree.ElementTree as ET
        root = ET.fromstring(self.table.to_svd())
        regs = root.findall('./peripherals/peripheral/registers/register')
        self.assertEqual(len(regs), len(list(self.table.registers())))
        names = [r.find('name').text for r in regs]
        self.assertEqual(len(names), len(set(names)))

    def test_generated_files_current(self):
        """doc/hwval_register_map.md and bench/share/hwval_regs.json are
        generated from the table (regenerate with
        ``python -m hwval_hdl.regmap --json ... --md ...``)."""
        if os.path.exists(REGMAP_JSON):
            with open(REGMAP_JSON, encoding='utf-8') as f:
                self.assertEqual(json.load(f), self.table.to_dict())
        if os.path.exists(REGMAP_MD):
            with open(REGMAP_MD, encoding='utf-8') as f:
                self.assertEqual(
                    f.read(), self.table.to_markdown(
                        title='hwval register map',
                        preamble=MARKDOWN_PREAMBLE))


EXPECTED_PORTS = [
    's_axi_lite_awaddr', 's_axi_lite_awprot', 's_axi_lite_awvalid',
    's_axi_lite_awready', 's_axi_lite_wdata', 's_axi_lite_wstrb',
    's_axi_lite_wvalid', 's_axi_lite_wready', 's_axi_lite_bresp',
    's_axi_lite_bvalid', 's_axi_lite_bready', 's_axi_lite_araddr',
    's_axi_lite_arprot', 's_axi_lite_arvalid', 's_axi_lite_arready',
    's_axi_lite_rdata', 's_axi_lite_rresp', 's_axi_lite_rvalid',
    's_axi_lite_rready',
    's_axi_lite_clk', 's_axi_lite_rst', 'clk', 'rst', 'clk2x_clk',
    'clk3x_clk', 'sampling_clk', 'lclk_clk', 'fclk1_clk', 'y1_clk',
    're_in', 'im_in', 'valid_in', 'ctrl_out', 'ad_clkout',
    'interrupt_out',
]
for _m in ['ringv2', 'legacy', 'mt0', 'mt1']:
    for _p in ['awaddr', 'awlen', 'awsize', 'awburst', 'awlock', 'awcache',
               'awprot', 'awvalid', 'awready', 'wdata', 'wstrb', 'wlast',
               'wvalid', 'wready', 'bresp', 'bvalid', 'bready']:
        EXPECTED_PORTS.append(f'm_axi_{_m}_{_p}')
for _m in ['mt0', 'mt1']:
    for _p in ['araddr', 'arlen', 'arsize', 'arburst', 'arlock', 'arcache',
               'arprot', 'arvalid', 'arready', 'rdata', 'rresp', 'rlast',
               'rvalid', 'rready']:
        EXPECTED_PORTS.append(f'm_axi_{_m}_{_p}')


def verilog_top_ports(verilog):
    i = verilog.index('module top(')
    j = verilog.index(');', i)
    return [p.strip() for p in
            verilog[i + len('module top('):j].replace('\n', '').split(',')]


@requires_core
class TestHwvalElaboration(unittest.TestCase):
    def test_verilog_ports(self):
        top = HwvalCore()
        verilog = amaranth.back.verilog.convert(
            top, platform=PlutoPlatform(), ports=top.ports())
        ports = verilog_top_ports(verilog)
        missing = [p for p in EXPECTED_PORTS if p not in ports]
        self.assertEqual(missing, [])
        # every port() signal is a module port
        names = {s.name for s in top.ports()}
        self.assertTrue(names <= set(ports), names - set(ports))
        # snapshot shadows and CDC hold registers carry the XDC suffixes
        self.assertIn('_snapshadow', verilog)
        self.assertIn('_cdchold', verilog)
        self.assertIn('DNA_PORT', verilog)
        self.assertIn('ASYNC_REG', verilog)


# Simulation clocks (seconds)
CLOCKS = {
    's_axi_lite': 10e-9,
    'sync': 16e-9,
    'clk2x': 8e-9,
    'clk3x': 5.333e-9,
    'sampling': 31.25e-9,
    'lclk': 8.138e-9,
    'fclk1': 5e-9,
    'y1': 20e-9,
}


class SimpleAxiSubordinate:
    """Zero-latency AXI3 subordinate (always ready, in-order B and R,
    dict-backed 64-bit memory) for the integration tests. Checks that
    bursts are INCR 64-bit, 8-byte aligned and inside ``window``."""
    def __init__(self, axi_if, domain, *, reads=False, window=None):
        self.a = axi_if
        self.domain = domain
        self.reads = reads
        self.window = window
        self.mem = {}
        self.aw_log = []
        self.errors = []

    def _check(self, kind, addr, length, size, burst):
        if size != 3 or burst != 1 or addr % 8:
            self.errors.append((kind, hex(addr), length, size, burst))
        if self.window is not None:
            end = addr + 8 * (length + 1)
            if not (self.window[0] <= addr and end <= self.window[1]):
                self.errors.append((kind, 'outside window', hex(addr)))

    async def process(self, ctx):
        a = self.a
        ctx.set(a.awready, 1)
        ctx.set(a.wready, 1)
        sampled = [a.awvalid, a.awaddr, a.awlen, a.awsize, a.awburst,
                   a.wvalid, a.wdata, a.wstrb, a.wlast, a.bready]
        if self.reads:
            ctx.set(a.arready, 1)
            sampled += [a.arvalid, a.araddr, a.arlen, a.arsize, a.arburst,
                        a.rready]
        aw_q = []
        w_q = []
        b_q = []
        r_q = []
        b_cur = r_cur = None
        async for v in ctx.tick(self.domain).sample(*sampled):
            v = v[2:]
            awvalid, awaddr, awlen, awsize, awburst = v[0:5]
            wvalid, wdata, wstrb, wlast, bready = v[5:10]
            if awvalid:
                self._check('AW', awaddr, awlen, awsize, awburst)
                aw_q.append([awaddr, awlen, 0])
                self.aw_log.append((awaddr, awlen))
            if wvalid:
                w_q.append((wdata, wstrb, wlast))
            while aw_q and w_q:
                burst = aw_q[0]
                wdata_, wstrb_, wlast_ = w_q.pop(0)
                addr = burst[0] + 8 * burst[2]
                old = self.mem.get(addr, 0)
                mask = sum(0xFF << (8 * j) for j in range(8)
                           if (wstrb_ >> j) & 1)
                self.mem[addr] = (old & ~mask) | (wdata_ & mask)
                if wlast_ != (burst[2] == burst[1]):
                    self.errors.append(('WLAST', hex(burst[0]), burst[2]))
                burst[2] += 1
                if burst[2] > burst[1]:
                    aw_q.pop(0)
                    b_q.append(0)
            if b_cur is not None and bready:
                b_cur = None
            if b_cur is None and b_q:
                b_cur = b_q.pop(0)
            ctx.set(a.bvalid, int(b_cur is not None))
            ctx.set(a.bresp, 0)
            if self.reads:
                arvalid, araddr, arlen, arsize, arburst, rready = v[10:16]
                if arvalid:
                    self._check('AR', araddr, arlen, arsize, arburst)
                    r_q.extend((araddr + 8 * j, j == arlen)
                               for j in range(arlen + 1))
                if r_cur is not None and rready:
                    r_cur = None
                if r_cur is None and r_q:
                    r_cur = r_q.pop(0)
                if r_cur is not None:
                    ctx.set(a.rvalid, 1)
                    ctx.set(a.rdata, self.mem.get(r_cur[0], 0))
                    ctx.set(a.rlast, int(r_cur[1]))
                    ctx.set(a.rresp, 0)
                else:
                    ctx.set(a.rvalid, 0)
                    ctx.set(a.rlast, 0)


@requires_core
class TestHwvalSim(unittest.TestCase):
    def run_sim(self, benches, *, dead=(), memories=False):
        self.top = HwvalCore(sim=True)
        self.t = self.top.table
        self.axi = self.top.regs.axi
        sim = Simulator(self.top)
        for domain, period in CLOCKS.items():
            if domain not in dead:
                sim.add_clock(period, domain=domain)
        if memories:
            c = self.top.config
            self.mem_ringv2 = SimpleAxiSubordinate(
                self.top.ringv2.axi, 'sync',
                window=(c.ringv2_window_base,
                        c.ringv2_window_base + c.ringv2_window_size))
            self.mem_legacy = SimpleAxiSubordinate(
                self.top.legacy.axi, 'sync',
                window=(c.legacy_base, c.legacy_base + c.legacy_size))
            self.mem_mt0 = SimpleAxiSubordinate(
                self.top.mt0.axi, 'clk2x', reads=True,
                window=(c.memtest_window_base,
                        c.memtest_window_base + c.memtest_window_size))
            for mem in [self.mem_ringv2, self.mem_legacy, self.mem_mt0]:
                sim.add_testbench(mem.process, background=True)
        for bench in benches:
            sim.add_testbench(bench)
        sim.run()

    async def rd(self, ctx, name):
        return await axil_read(ctx, self.axi, self.t.address(name),
                               domain='s_axi_lite')

    async def wr(self, ctx, name, value):
        return await axil_write(ctx, self.axi, self.t.address(name), value,
                                domain='s_axi_lite')

    async def snapshot(self, ctx, mask, timeout=400):
        await self.wr(ctx, 'SNAP_REQ', mask)
        for _ in range(timeout):
            ack = await self.rd(ctx, 'SNAP_ACK')
            if ack == mask:
                return True
        return False

    def test_smoke(self):
        async def bench(ctx):
            ctx.set(self.top.re_in, 0x123)
            ctx.set(self.top.im_in, 0xF00)
            ctx.set(self.top.valid_in, 1)
            assert await self.rd(ctx, 'ID') == HWVAL_ID
            assert await self.rd(ctx, 'VERSION') == 0x000100
            assert await self.rd(ctx, 'FEATURES') == 0x3FF
            # SCRATCH
            assert await self.rd(ctx, 'SCRATCH') == 0
            assert await self.wr(ctx, 'SCRATCH', 0xA5A5_5A5A) == 0
            assert await self.rd(ctx, 'SCRATCH') == 0xA5A5_5A5A
            # unmapped addresses: inside a block, the vacant 0x300 block
            # and the end of the window
            for addr in [0x0FC, 0x300, 0x3FC, 0x9F0, 0xFFC]:
                assert await axil_read(ctx, self.axi, addr,
                                       domain='s_axi_lite') == \
                    UNMAPPED_READ_VALUE, hex(addr)
                assert await axil_write(ctx, self.axi, addr, 0,
                                        domain='s_axi_lite') == 0
            assert await self.rd(ctx, 'SCRATCH') == 0xA5A5_5A5A
            # sync snapshot: timestamp advances between snapshots
            assert await self.snapshot(ctx, 0x1)
            ts0 = (await self.rd(ctx, 'TS_HI') << 32) | \
                await self.rd(ctx, 'TS_LO')
            assert ts0 > 0
            assert await self.rd(ctx, 'SNAP_SEQ') == 1
            assert await self.snapshot(ctx, 0x1)
            ts1 = (await self.rd(ctx, 'TS_HI') << 32) | \
                await self.rd(ctx, 'TS_LO')
            assert ts1 > ts0
            # all three domains
            assert await self.snapshot(ctx, 0x7)
            assert await self.rd(ctx, 'SNAP_SEQ') == 3
            # the ingest monitor saw samples (stats_enable defaults to 1)
            samples = await self.rd(ctx, 'SAMPLES_LO')
            assert samples > 0
            assert await self.rd(ctx, 'I_MIN') == 0x123
            assert await self.rd(ctx, 'Q_MAX') == 0xFFFF_FF00  # -256
            # device DNA (sim model, ~470 cycles after reset)
            for _ in range(200):
                if await self.rd(ctx, 'DNA_STATUS') == 1:
                    break
            assert await self.rd(ctx, 'DNA_STATUS') == 1
            dna = (await self.rd(ctx, 'DNA_HI') << 32) | \
                await self.rd(ctx, 'DNA_LO')
            assert dna == DEFAULT_SIM_DNA, hex(dna)
            # guard defaults and lock
            assert await self.rd(ctx, 'GUARD_LO') == 0x2000_0000
            assert await self.rd(ctx, 'GUARD_HI') == 0x2800_0000
            await self.wr(ctx, 'GUARD_LO', 0x2100_0000)
            await self.wr(ctx, 'GUARD_LOCK', 1)
            await self.wr(ctx, 'GUARD_LO', 0x3000_0000)
            await self.wr(ctx, 'GUARD_LOCK', 0)
            assert await self.rd(ctx, 'GUARD_LO') == 0x2100_0000
            assert await self.rd(ctx, 'GUARD_LOCK') == 1
            # CORE_RESET clears the lock and resets the sync domain; a
            # snapshot still completes while the domain is in reset
            await self.wr(ctx, 'CORE_RESET', 1)
            assert await self.rd(ctx, 'GUARD_LOCK') == 0
            await ctx.tick('s_axi_lite').repeat(20)
            assert await self.snapshot(ctx, 0x1)
            assert await self.rd(ctx, 'TS_LO') == 0
            await self.wr(ctx, 'CORE_RESET', 0)
            await ctx.tick('s_axi_lite').repeat(20)
            assert await self.snapshot(ctx, 0x1)
            assert 0 < await self.rd(ctx, 'TS_LO') < ts1

        self.run_sim([bench])

    def test_census_irq(self):
        async def bench(ctx):
            await self.wr(ctx, 'IRQ_ENABLE', 1 << 4)
            await self.wr(ctx, 'CENSUS_GATE', 1000)
            await self.wr(ctx, 'CENSUS_CTRL', 1)
            # gate + settle (>= 256 cycles + echo quiet time)
            for _ in range(2000):
                if await self.rd(ctx, 'CENSUS_STATUS') == 0b10:
                    break
            else:
                assert False, 'census did not finish'
            gate = await self.rd(ctx, 'CENSUS_GATE_ACTUAL')
            assert gate == 1000
            for suffix, domain in [('SYNC', 'sync'), ('MEM', 'clk2x'),
                                   ('CLK3X', 'clk3x'),
                                   ('SAMPLING', 'sampling'),
                                   ('LCLK', 'lclk'), ('FCLK1', 'fclk1'),
                                   ('Y1', 'y1')]:
                count = await self.rd(ctx, f'CENSUS_{suffix}')
                expected = gate * CLOCKS['s_axi_lite'] / CLOCKS[domain]
                assert abs(count - expected) <= 3, (suffix, count, expected)
            assert await self.rd(ctx, 'CENSUS_CLKOUT') == 0
            assert await self.rd(ctx, 'IRQ_PENDING') == 1 << 4
            assert ctx.get(self.top.interrupt_out)
            assert await self.rd(ctx, 'IRQ_COUNT') == 1
            await self.wr(ctx, 'IRQ_CLEAR', 1 << 4)
            assert await self.rd(ctx, 'IRQ_PENDING') == 0
            await ctx.tick('s_axi_lite')
            assert not ctx.get(self.top.interrupt_out)

        self.run_sim([bench])

    def test_ringv2_ramp(self):
        # ring v2 end to end: config/command crossings, ramp64 source,
        # AXI writes, gray-synchronized committed pointer, IRQ, snapshot
        base = 0x2000_0000
        size_bursts = 6

        async def bench(ctx):
            await self.wr(ctx, 'IRQ_ENABLE', 1 << 0)
            await self.wr(ctx, 'RINGV2_BASE', base)
            await self.wr(ctx, 'RINGV2_SIZE_BURSTS', size_bursts)
            await self.wr(ctx, 'RINGV2_IRQ_EVERY', 4)
            await self.wr(ctx, 'RINGV2_RATE_INC', 0x8000_0000)
            await self.wr(ctx, 'RINGV2_CMD', 0b100)   # clear
            # enable, src = ramp64, irq_threshold
            await self.wr(ctx, 'RINGV2_CTRL', 1 | (1 << 3) | (1 << 8))
            committed = 0
            for _ in range(1500):
                committed = await self.rd(ctx, 'RINGV2_COMMITTED_BURSTS')
                if committed >= 9:
                    break
            assert committed >= 9, committed
            status = await self.rd(ctx, 'RINGV2_STATUS')
            assert status & 0b010, status          # enabled
            assert (await self.rd(ctx, 'IRQ_PENDING')) & 1
            assert ctx.get(self.top.interrupt_out)
            # disable and drain
            await self.wr(ctx, 'RINGV2_CTRL', 1 << 3)
            for _ in range(500):
                if await self.rd(ctx, 'RINGV2_STATUS') & 0b001:
                    break
            assert await self.rd(ctx, 'RINGV2_STATUS') & 0b001   # idle
            committed = await self.rd(ctx, 'RINGV2_COMMITTED_BURSTS')
            assert await self.snapshot(ctx, 0x1)
            assert await self.rd(ctx, 'RINGV2_ISSUED_BURSTS') == committed
            assert await self.rd(ctx, 'RINGV2_EPOCH') == 1
            assert await self.rd(ctx, 'RINGV2_GUARD_BLOCKED') == 0
            assert await self.rd(ctx, 'RINGV2_BRESP_ERR') == 0
            words_in = await self.rd(ctx, 'RINGV2_WORDS_IN_LO')
            gen = await self.rd(ctx, 'RINGV2_GEN_LO')
            assert 0 < words_in <= gen
            mem = self.mem_ringv2
            assert mem.errors == []
            assert len(mem.aw_log) == committed
            for k, (addr, length) in enumerate(mem.aw_log):
                assert addr == base + (k % size_bursts) * 128
                assert length == 15
            # the last lap, oldest burst first, holds consecutive ramp
            # values apart from the pad words of the final flush
            laps = min(committed, size_bursts)
            first_slot = committed % size_bursts if committed > size_bursts \
                else 0
            ordered = []
            for n in range(laps):
                slot = (first_slot + n) % size_bursts
                ordered += [mem.mem[base + 128 * slot + 8 * j]
                            for j in range(16)]
            ramp = [w for w in ordered if w >> 48 != 0xF1B1]
            assert len(ramp) > 16
            assert all(b == a + 1 for a, b in zip(ramp, ramp[1:])), ramp

        self.run_sim([bench], memories=True)

    def test_legacy_and_memtest(self):
        async def bench(ctx):
            # legacy ring: sample ramp at 1/2 rate into the production DMA
            await self.wr(ctx, 'LEGACY_RATE_INC', 0x8000_0000)
            await self.wr(ctx, 'LEGACY_CTRL', 1 | (1 << 1))
            # mt0: write-then-verify 4 KiB, 16-beat bursts
            await self.wr(ctx, 'IRQ_ENABLE', 1 << 2)
            await self.wr(ctx, 'MT0_BASE', 0x2400_0000)
            await self.wr(ctx, 'MT0_SIZE', 4096)
            await self.wr(ctx, 'MT0_PASSES', 1)
            await self.wr(ctx, 'MT0_CTRL', 2 | (0 << 3) | (16 << 7)
                          | (4 << 12))
            await self.wr(ctx, 'MT0_CMD', 0b101)   # start + clear
            for _ in range(1000):
                if await self.rd(ctx, 'MT0_STATUS') & 0b010:
                    break
            assert await self.rd(ctx, 'MT0_STATUS') == 0b010   # done
            assert await self.rd(ctx, 'IRQ_PENDING') == 1 << 2
            assert await self.snapshot(ctx, 0x3)
            assert await self.rd(ctx, 'MT0_PASS_COUNT') == 1
            assert await self.rd(ctx, 'MT0_BYTES_WR_LO') == 4096
            assert await self.rd(ctx, 'MT0_BYTES_RD_LO') == 4096
            assert await self.rd(ctx, 'MT0_ERR_COUNT') == 0
            assert await self.rd(ctx, 'MT0_GUARD_BLOCKED') == 0
            assert self.mem_mt0.errors == []
            assert len(self.mem_mt0.mem) == 4096 // 8
            # legacy counters moved; the DMA wrote into its window
            assert await self.rd(ctx, 'LEGACY_WORDS_IN') > 0
            assert await self.rd(ctx, 'LEGACY_WORDS_ACCEPTED') > 0
            assert await self.rd(ctx, 'LEGACY_AW') > 0
            assert await self.rd(ctx, 'LEGACY_GEN_LO') > 0
            assert await self.rd(ctx, 'LEGACY_BASE') == 0x2200_0000
            assert self.mem_legacy.errors == []
            first = self.mem_legacy.mem.get(0x2200_0000)
            assert first is not None
            # ramp through IQPacker: word = (c + 1) << 32 | c
            assert first >> 32 == (first & 0xFFFF_FFFF) + 1, hex(first)

        self.run_sim([bench], memories=True)

    def test_dead_sampling_clock(self):
        # No sampling clock: the sampling snapshot never completes, but the
        # bus keeps answering and the other domains still snapshot.
        async def bench(ctx):
            assert await self.rd(ctx, 'ID') == HWVAL_ID
            assert not await self.snapshot(ctx, 0x4, timeout=100)
            assert await self.rd(ctx, 'SNAP_ACK') == 0
            assert await self.snapshot(ctx, 0x3)
            await self.wr(ctx, 'SNAP_REQ', 0x7)
            for _ in range(100):
                ack = await self.rd(ctx, 'SNAP_ACK')
            assert ack == 0x3
            assert await self.rd(ctx, 'SCRATCH') == 0

        self.run_sim([bench], dead=('sampling',))


if __name__ == '__main__':
    unittest.main()
