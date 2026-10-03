#
# Fishball P25 - the lane ring core's top (doc/changes/079, step 3a)
#
# The register map against the design's table, the build files that name the core's version and
# DMA masters, and the core in simulation: identity registers, a bus that answers every access
# (in reset, at vacant addresses, without byte strobes), register readback without aliasing, the
# sample and clip counters, and one lane's packet through the DDC, lane ring and DMA.
#
# The simulation uses P25Core(sim=True): samples go in on `sim_re`/`sim_im`/`sim_strobe` in
# `sync`, in place of the FIFO-based input crossing. The full core runs at a few hundred `sync`
# cycles per second, so the DDC is set up for a fast output (stage 1 at /4, stages 2 and 3
# bypassed, a sample every `sync` cycle).
#
# SPDX-License-Identifier: MIT
#

import os
import re
import unittest
import xml.etree.ElementTree as ET

import amaranth.back.verilog
from amaranth.sim import Simulator

from maia_hdl.pluto_platform import PlutoPlatform
from p25_hdl import p25_top
from p25_hdl.config import P25Config
from p25_hdl.lane_packetizer import (
    FLAG_RETUNED, HEADER_WORDS, MAGIC, MAX_SAMPLES, PACKET_WORDS, fold)
from p25_hdl.p25_top import P25Core

from .hwval_axi_wmodel import AxiWriteSlaveModel
from .hwval_axil_bfm import axil_read, axil_write

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))
CORE_PAC_SVD = os.path.join(REPO, 'scanner', 'core-pac', 'core.svd')
BUILD_FPGA_BAT = os.path.join(REPO, 'build_fpga.bat')
PACKAGE_IP_TCL = os.path.join(REPO, 'maia-hdl', 'ip', 'p25-core', 'package_ip.tcl')
SYSTEM_BD_TCL = os.path.join(REPO, 'maia-hdl', 'projects', 'fishball7020_p25', 'system_bd.tcl')

MASTERS = ['lanes', 'wideband_spec', 'wideband_iq']


def lane_base(i):
    return 0x20 * (1 + i)


# Byte offsets (doc/changes/079, "Registers").
REGISTERS = {
    'product_id': 0x00, 'version': 0x04, 'control': 0x08, 'interrupts': 0x0C,
    'capabilities': 0x10,
    **{name: lane_base(i) + off for i in range(3) for name, off in [
        (f'lane{i}_ddc_coeff_addr', 0x00), (f'lane{i}_ddc_coeff', 0x08),
        (f'lane{i}_ddc_decimation', 0x0C), (f'lane{i}_ddc_frequency', 0x10),
        (f'lane{i}_ddc_control', 0x14), (f'lane{i}_control', 0x18), (f'lane{i}_status', 0x1C)]},
    'lanes_ring_control': 0x80, 'lanes_ring_status': 0x84, 'lanes_ring_next_address': 0x88,
    'sample_count_lo': 0x8C, 'sample_count_hi': 0x90, 'adc_clips': 0x94,
    'spec_control': 0xA0, 'spec_status': 0xA4, 'spec_next_address': 0xA8,
    'wideband_iq_dma_status': 0xC0, 'wideband_iq_dma_control': 0xC4,
    'wideband_iq_next_address': 0xC8,
}
R = REGISTERS

CONTROL = R['control']
INTERRUPTS = R['interrupts']

# Every clock domain of the core (seconds).
CLOCKS = {
    's_axi_lite': 10e-9,
    'sync': 16e-9,
    'clk2x': 8e-9,
    'clk3x': 16e-9 / 3,
    'sampling': 31.25e-9,
}


def svd_registers(svd_bytes):
    """{name: (offset, access, {field: (bitRange, access)})}"""
    root = ET.fromstring(svd_bytes)
    regs = {}
    for reg in root.iter('register'):
        fields = {
            f.findtext('name'): (f.findtext('bitRange'), f.findtext('access'))
            for f in reg.iter('field')}
        regs[reg.findtext('name')] = (
            int(reg.findtext('addressOffset'), 16), reg.findtext('access'), fields)
    return regs


class TestP25Config(unittest.TestCase):
    def test_default_validates(self):
        P25Config().validate()

    def test_rings(self):
        self.assertEqual(P25Config().rings(), [
            ('lanes_dma', 0x1900_0000, 0x20_0000),
            ('wideband_spec_dma', 0x2100_0000, 0x1_0000),
            ('wideband_iq_dma', 0x2200_0000, 0x100_0000),
        ])

    def test_packets_fill_sub_buffers(self):
        c = P25Config()
        self.assertEqual(c.lanes_dma_buffer_size % (PACKET_WORDS * 8), 0)

    def test_overlap_rejected(self):
        c = P25Config()
        c.wideband_spec_dma_address = 0x1910_0000
        with self.assertRaises(AssertionError):
            c.validate()

    def test_misaligned_rejected(self):
        c = P25Config()
        c.lanes_dma_address = 0x1910_0000
        with self.assertRaises(AssertionError):
            c.validate()

    def test_lanes_bounds(self):
        for lanes in [0, 16]:
            c = P25Config()
            c.lanes = lanes
            with self.assertRaises(AssertionError):
                c.validate()


class TestP25RegisterMap(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.svd = P25Core().svd()
        cls.regs = svd_registers(cls.svd)

    def test_version_1_0_0(self):
        self.assertEqual(p25_top._version, '1.0.0')
        self.assertEqual(ET.fromstring(self.svd).findtext('version'), '1.0.0')

    def test_build_script_version_matches(self):
        # build_fpga.bat packages the IP with its own IP_CORE_VERSION for --p25; the block
        # design instantiates that VLNV.
        with open(BUILD_FPGA_BAT, encoding='utf-8') as f:
            text = f.read()
        m = re.search(r'set "FPGA_PROJECT=fishball7020_p25".*?'
                      r'set "IP_CORE_VERSION=([0-9.]+)"', text, re.S)
        self.assertIsNotNone(m)
        self.assertEqual(m.group(1), p25_top._version)

    def test_offsets(self):
        self.assertEqual({name: v[0] for name, v in self.regs.items()}, REGISTERS)

    def test_lane_banks_alike(self):
        def layout(i):
            p = f'lane{i}_'
            return {name[len(p):]: (access, fields) for name, (_, access, fields)
                    in self.regs.items() if name.startswith(p)}
        self.assertEqual(layout(1), layout(0))
        self.assertEqual(layout(2), layout(0))

    def test_lane_control_and_status(self):
        self.assertEqual(self.regs['lane0_control'][2]['enable'], ('[0:0]', 'read-write'))
        self.assertEqual(self.regs['lane0_control'][2]['tag'], ('[31:16]', 'read-write'))
        # `lost` is read-to-clear, so it is alone in its word.
        self.assertEqual(self.regs['lane0_status'][2], {'lost': ('[0:0]', 'read-only')})

    def test_core_pac_svd_matches_hdl(self):
        """scanner/core-pac/core.svd is the SVD of this gateware."""
        with open(CORE_PAC_SVD, 'rb') as f:
            pac = f.read().replace(b'\r\n', b'\n')
        self.assertEqual(pac, self.svd.replace(b'\r\n', b'\n'))


def verilog_top_ports(verilog):
    i = verilog.index('module top(')
    j = verilog.index(');', i)
    return [p.strip() for p in verilog[i + len('module top('):j].replace('\n', '').split(',')]


class TestP25Elaboration(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.top = P25Core()
        cls.verilog = amaranth.back.verilog.convert(
            cls.top, platform=PlutoPlatform(), ports=cls.top.ports())
        cls.ports = verilog_top_ports(cls.verilog)

    def test_dma_masters(self):
        masters = sorted({m.group(1) for p in self.ports
                          if (m := re.fullmatch(r'm_axi_(\w+)_awaddr', p))})
        self.assertEqual(masters, sorted(MASTERS))

    def test_masters_packaged_and_wired(self):
        with open(PACKAGE_IP_TCL, encoding='utf-8') as f:
            package = f.read()
        with open(SYSTEM_BD_TCL, encoding='utf-8') as f:
            bd = f.read()
        self.assertEqual(sorted(re.findall(r'-busif m_axi_(\w+) -clock clk', package)),
                         sorted(MASTERS))
        self.assertEqual(
            sorted(re.findall(r'(?m)^ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 '
                              r'p25_core/m_axi_(\w+)\s*$', bd)),
            sorted(MASTERS))


class P25CoreSim:
    """The core in pysim, with AXI4-Lite register access."""

    def setUp(self):
        self.top = P25Core(sim=True)
        self.axi = self.top.axi4lite.axi

    def run_sim(self, bench, background=()):
        sim = Simulator(self.top)
        for domain, period in CLOCKS.items():
            sim.add_clock(period, domain=domain)
        for bg in background:
            sim.add_testbench(bg, background=True)
        sim.add_testbench(bench)
        sim.run()

    async def wr(self, ctx, addr, value, **kw):
        return await axil_write(ctx, self.axi, addr, value, domain='s_axi_lite', **kw)

    async def rd(self, ctx, addr, **kw):
        return await axil_read(ctx, self.axi, addr, domain='s_axi_lite', **kw)

    async def release_reset(self, ctx):
        # `sdr_reset` resets to 1; the sync-domain banks are claimed a little after it clears.
        await self.wr(ctx, CONTROL, 0)
        await ctx.tick('s_axi_lite').repeat(30)


class TestP25CoreRegisters(P25CoreSim, unittest.TestCase):
    def test_identity(self):
        async def bench(ctx):
            self.assertEqual(await self.rd(ctx, R['product_id']), 0x7261_6431)   # "rad1"
            self.assertEqual(await self.rd(ctx, R['version']), 0x0001_0000)
            # 3 lanes, 2^9 packet words, 8 header words, spectrum, capture.
            self.assertEqual(await self.rd(ctx, R['capabilities']),
                             3 | 9 << 4 | 8 << 8 | 1 << 12 | 1 << 13)
        self.run_sim(bench)

    def test_every_access_is_answered(self):
        """In reset, at vacant addresses and without byte strobes the bus answers at once
        (well within the bridge's timeout)."""
        quick = dict(timeout=40)

        async def bench(ctx):
            self.assertEqual(await self.rd(ctx, CONTROL), 1)
            # Sync-domain banks while `sdr_reset` holds their domain in reset.
            for addr in [R['lane0_ddc_frequency'], R['lanes_ring_status'], R['spec_status']]:
                self.assertEqual(await self.rd(ctx, addr, with_resp=True, **quick), (0, 0))
                self.assertEqual(await self.wr(ctx, addr, 0x1234, **quick), 0)
            # A write without strobes changes nothing.
            self.assertEqual(await self.wr(ctx, CONTROL, 0, strb=0, **quick), 0)
            self.assertEqual(await self.rd(ctx, CONTROL), 1)
            await self.release_reset(ctx)
            for addr in [0x0E0, 0x100, 0x200, 0x3FC, 0x014, 0x01C]:
                self.assertEqual(await self.rd(ctx, addr, with_resp=True, **quick), (0, 0),
                                 hex(addr))
                self.assertEqual(await self.wr(ctx, addr, 0xFFFF_FFFF, **quick), 0, hex(addr))
            await self.wr(ctx, R['lane1_ddc_frequency'], 0x123)
            self.assertEqual(await self.wr(ctx, R['lane1_ddc_frequency'], 0x456, strb=0), 0)
            self.assertEqual(await self.rd(ctx, R['lane1_ddc_frequency']), 0x123)
            # Unused words of a bank answer 0.
            self.assertEqual(await self.rd(ctx, lane_base(0) + 0x04), 0)
            self.assertEqual(await self.rd(ctx, 0x98), 0)
        self.run_sim(bench)

    def test_reset_values(self):
        async def bench(ctx):
            await self.release_reset(ctx)
            expect = {
                'interrupts': 0,
                'lanes_ring_control': 0,
                'lanes_ring_status': 0x7F,              # last_buffer = -1 (7 bits)
                'lanes_ring_next_address': 0x1900_0000,
                'spec_next_address': 0x2100_0000,
                'wideband_iq_dma_status': 0xF << 1,     # last_buffer = -1 (4 bits) in [4:1]
                'wideband_iq_next_address': 0x2200_0000,
                **{f'lane{i}_{n}': 0 for i in range(3)
                   for n in ['control', 'status', 'ddc_frequency', 'ddc_control']},
            }
            for name, value in expect.items():
                self.assertEqual(await self.rd(ctx, R[name]), value, name)
        self.run_sim(bench)

    def test_readback_without_aliasing(self):
        """Distinct values in every writable register, then all read back."""
        writes = {}
        for i in range(3):
            writes[R[f'lane{i}_ddc_coeff_addr']] = (0x3FF, 0x100 + i)
            writes[R[f'lane{i}_ddc_decimation']] = (0xF_FFFF, 0x5_4321 + i)
            writes[R[f'lane{i}_ddc_frequency']] = (0xFFF_FFFF, 0x123_4567 * (i + 1))
            writes[R[f'lane{i}_ddc_control']] = (0x1FF_FFFF, 0x0AB_CDEF - i)
            writes[R[f'lane{i}_control']] = (0xFFFF_0001, (0xBEE0 + i) << 16 | 0xFFFE | i & 1)
        writes[R['lanes_ring_control']] = (0x1, 0x1)
        # spec_abort (bit 2) is a pulse and reads 0.
        writes[R['spec_control']] = (0x1FFB, 0x1A6B)
        writes[R['wideband_iq_dma_control']] = (0x1, 0x1)

        async def bench(ctx):
            await self.release_reset(ctx)
            for addr, (_, value) in writes.items():
                await self.wr(ctx, addr, value)
            for addr, (mask, value) in writes.items():
                self.assertEqual(await self.rd(ctx, addr), value & mask, hex(addr))
            top = self.top
            await ctx.tick('sync').repeat(4)
            self.assertEqual(ctx.get(top.ddcs[2].frequency), 0x123_4567 * 3)
            self.assertEqual(ctx.get(top.packetizers[1].tag), 0xBEE1)
            self.assertEqual(ctx.get(top.packetizers[1].enable), 1)
            self.assertEqual(ctx.get(top.packetizers[0].enable), 0)
            self.assertEqual(ctx.get(top.lane_ring.enable), 1)
        self.run_sim(bench)


class TestP25CoreSamples(P25CoreSim, unittest.TestCase):
    def test_sample_and_clip_counters(self):
        n = 200
        clipped = {7, 8, 50, 51, 52, 199}

        async def bench(ctx):
            top = self.top
            await self.release_reset(ctx)
            for k in range(n):
                ctx.set(top.sim_strobe, 1)
                ctx.set(top.sim_re, 2047 if k in {7, 50, 199} else 100)
                ctx.set(top.sim_im, -2048 if k in {8, 50, 51, 52} else -100)
                await ctx.tick('sync')
            ctx.set(top.sim_strobe, 0)
            await ctx.tick('sync').repeat(4)
            self.assertEqual(await self.rd(ctx, R['sample_count_lo']), n)
            self.assertEqual(await self.rd(ctx, R['sample_count_hi']), 0)
            self.assertEqual(await self.rd(ctx, R['adc_clips']), len(clipped))
        self.run_sim(bench)

    def test_lane_packet_through_the_core(self):
        """Lane 1 tuned, tagged and enabled: its first packet reaches the ring with the lane's
        tag, NCO word and a sample index from the shared count, and the ring's next address
        moves on by one packet. Lanes 0 and 2 stay off."""
        lane = 1
        base = lane_base(lane)
        model = AxiWriteSlaveModel(self.top.lanes_dma.axi)
        nco = 0x0ABC_DEF0 & 0xFFF_FFFF
        tag = 0x5A5A

        async def bench(ctx):
            top = self.top
            await self.release_reset(ctx)
            await self.wr(ctx, base + 0x0C, 4)                                  # stage 1 /4
            await self.wr(ctx, base + 0x10, nco)
            await self.wr(ctx, base + 0x14, (1 << 22) | (1 << 23) | (1 << 24))  # bypass 2, 3; on
            await self.wr(ctx, base + 0x18, tag << 16 | 1)
            await self.wr(ctx, R['lanes_ring_control'], 1)
            start = None
            for k in range(MAX_SAMPLES * 4 + 2000):
                ctx.set(top.sim_strobe, 1)
                ctx.set(top.sim_re, (k * 37) % 2000 - 1000)
                ctx.set(top.sim_im, (k * 91) % 1800 - 900)
                if start is None and ctx.get(top.ddcs[lane].strobe_out):
                    start = k
                await ctx.tick('sync')
                if len(model.stream()) >= PACKET_WORDS:
                    break
            words = model.stream()
            self.assertEqual(len(words), PACKET_WORDS)
            h = words[:HEADER_WORDS]
            self.assertEqual(h[0] & 0xFFFF, MAGIC)
            self.assertEqual((h[0] >> 20) & 0xF, lane)
            self.assertEqual((h[0] >> 24) & 0xFF, FLAG_RETUNED)
            self.assertEqual((h[0] >> 32) & 0xFFFF, MAX_SAMPLES)
            self.assertEqual(h[0] >> 48, tag)
            self.assertEqual(h[3] & 0xFFF_FFFF, nco)
            self.assertEqual(h[3] >> 32, 0)                     # sequence 0
            # The sample index is the count of input samples registered when the DDC made its
            # first output: the input register puts the count one sample behind the bench's.
            self.assertEqual(h[1], start - 1)
            check = 0
            for w in words:
                check ^= fold(w)
            self.assertEqual(check, 0)                          # w7 holds the XOR of the rest
            self.assertEqual(ctx.get(top.lanes_dma.axi.awvalid), 0)
            await ctx.tick('s_axi_lite').repeat(10)
            self.assertEqual(await self.rd(ctx, R['lanes_ring_next_address']), 0x1900_1000)
            self.assertEqual(await self.rd(ctx, R['lane1_status']), 0)
            for other in [0, 2]:
                self.assertEqual(ctx.get(top.packetizers[other].ready), 0)
        self.run_sim(bench, background=[model.bench])


if __name__ == '__main__':
    unittest.main()
