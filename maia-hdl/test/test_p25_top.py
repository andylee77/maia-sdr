#
# Fishball P25 -- P25Core top-level tests (core 0.3.0, doc/changes/064)
#
# Covers the second traffic decode chain (`traffic2_*`) and the rule that
# the register map only grows: every register of the 0.2.0 map keeps its
# name, offset, access and field layout, so a p25-httpd built against the
# 0.2.0 PAC keeps working on a 0.3.0 bitstream.
#
# The 0.2.0 baseline is frozen in golden_vectors/p25_core_0.2.0.svd (the
# p25-httpd/p25-pac/p25.svd of commit 86eef5d).
#
# Simulation notes. The full core runs in pysim at a few hundred `sync`
# cycles per second, so the simulation tests stay short:
#
# - Register accesses go through the real AXI4-Lite bridge, bank decoder
#   and RegisterCDCs.
# - `rxiq_cdc` wraps a FIFO18E1 instance that pysim cannot simulate. Its
#   EMPTY output reads 0, so every DDC sees a (zero) input sample on every
#   `sync` cycle. That is enough to push strobes down chain 2 to the
#   symbol clock.
# - A full dibit DMA burst needs 512 symbols (~6 min of simulation), so
#   the DMA / interrupt wiring is exercised from the AXI side instead: the
#   test answers AW handshakes and injects B responses on the chain 2
#   master port, which is what advances `last_buffer` and fires the
#   sub-buffer interrupt.
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
from p25_hdl.p25_top import P25Core

from .hwval_axil_bfm import axil_read, axil_write

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))
BASELINE_SVD = os.path.join(HERE, 'golden_vectors', 'p25_core_0.2.0.svd')
PAC_SVD = os.path.join(REPO, 'p25-httpd', 'p25-pac', 'p25.svd')
BUILD_FPGA_BAT = os.path.join(REPO, 'build_fpga.bat')
PACKAGE_IP_TCL = os.path.join(REPO, 'maia-hdl', 'ip', 'p25-core',
                              'package_ip.tcl')
SYSTEM_BD_TCL = os.path.join(REPO, 'maia-hdl', 'projects',
                             'fishball7020_p25', 'system_bd.tcl')

# Byte offsets of the registers added in 0.3.0 (doc/changes/064).
TRAFFIC2_REGISTERS = {
    'traffic2_ddc_coeff_addr': 0x120,
    'traffic2_ddc_coeff': 0x128,
    'traffic2_ddc_decimation': 0x12C,
    'traffic2_ddc_frequency': 0x130,
    'traffic2_ddc_control': 0x134,
    'traffic2_lsm_control': 0x140,
    'traffic2_lsm_status': 0x144,
    'traffic2_lsm_nid': 0x148,
    'traffic2_lsm_drop_count': 0x14C,
    'traffic2_lsm_dibit_next': 0x150,
    'traffic2_lsm_debug': 0x154,
    'traffic2_lsm_agc_debug': 0x158,
    'traffic2_lsm_agc_config': 0x15C,
    'traffic2_lsm_agc_seed': 0x160,
    'traffic2_lsm_pll_seed': 0x164,
    'traffic2_lsm_timing_seed': 0x168,
}

# Pre-existing register byte offsets used by the simulation tests.
CONTROL = 0x08
INTERRUPTS = 0x0C
TRAFFIC_DDC_FREQUENCY = 0x50
TRAFFIC_LSM_CONTROL = 0xC0
TRAFFIC_LSM_DROP_COUNT = 0xCC
TRAFFIC_LSM_DIBIT_NEXT = 0xD0
TRAFFIC_LSM_AGC_CONFIG = 0xDC
LSM_SEED_BANK = 0x100  # 6 registers, 0x100-0x114
TRAFFIC_LSM_PLL_SEED = 0x110

INTERRUPT_BIT_TRAFFIC2 = 8

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
            int(reg.findtext('addressOffset'), 16),
            reg.findtext('access'), fields)
    return regs


def bit_range(text):
    msb, lsb = (int(x) for x in re.fullmatch(r'\[(\d+):(\d+)\]', text)
                .groups())
    return set(range(lsb, msb + 1))


class TestP25Config(unittest.TestCase):
    def test_default_validates(self):
        P25Config().validate()

    def test_traffic2_ring(self):
        c = P25Config()
        self.assertEqual(c.traffic2_lsm_dibit_dma_address, 0x1D00_0000)
        self.assertEqual(c.traffic2_lsm_dibit_dma_num_buffers, 8)
        self.assertEqual(c.traffic2_lsm_dibit_dma_buffer_size, 0x1000)
        self.assertEqual(c.traffic2_lsm_dibit_dma_total_size, 0x8000)
        # Same geometry as chain 1, so the PS ring reader is shared.
        self.assertEqual(c.traffic2_lsm_dibit_dma_total_size,
                         c.traffic_lsm_dibit_dma_total_size)

    def test_rings_disjoint(self):
        rings = sorted(P25Config().rings(), key=lambda r: r[1])
        self.assertIn('traffic2_lsm_dibit_dma', [r[0] for r in rings])
        for (_, base_a, size_a), (_, base_b, _) in zip(rings, rings[1:]):
            self.assertLessEqual(base_a + size_a, base_b)

    def test_overlap_rejected(self):
        c = P25Config()
        c.traffic2_lsm_dibit_dma_address = c.traffic_lsm_dibit_dma_address
        with self.assertRaises(AssertionError):
            c.validate()


class TestP25RegisterMap(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.svd = P25Core().svd()
        cls.regs = svd_registers(cls.svd)
        with open(BASELINE_SVD, 'rb') as f:
            cls.baseline = svd_registers(f.read())

    def test_version_0_3_0(self):
        self.assertEqual(p25_top._version, '0.3.0')
        root = ET.fromstring(self.svd)
        self.assertEqual(root.findtext('version'), '0.3.0')

    def test_build_script_version_matches(self):
        # build_fpga.bat packages the IP with its own IP_CORE_VERSION
        # for --p25; the block design instantiates that VLNV.
        with open(BUILD_FPGA_BAT, encoding='utf-8') as f:
            text = f.read()
        m = re.search(r'set "FPGA_PROJECT=fishball7020_p25".*?'
                      r'set "IP_CORE_VERSION=([0-9.]+)"', text, re.S)
        self.assertIsNotNone(m)
        self.assertEqual(m.group(1), p25_top._version)

    def test_superset_of_0_2_0(self):
        """Every 0.2.0 register keeps name, offset, access and fields."""
        for name, (offset, access, fields) in self.baseline.items():
            with self.subTest(register=name):
                self.assertIn(name, self.regs)
                new_offset, new_access, new_fields = self.regs[name]
                self.assertEqual(new_offset, offset)
                self.assertEqual(new_access, access)
                for fname, layout in fields.items():
                    self.assertEqual(new_fields.get(fname), layout,
                                     f'{name}.{fname}')
                # Fields added to an existing register may only use bits
                # that were unused in 0.2.0.
                old_bits = set().union(
                    *(bit_range(r) for r, _ in fields.values()))
                for fname, (rng, _) in new_fields.items():
                    if fname not in fields:
                        self.assertFalse(bit_range(rng) & old_bits,
                                         f'{name}.{fname}')

    def test_only_additions(self):
        added = set(self.regs) - set(self.baseline)
        self.assertEqual(added, set(TRAFFIC2_REGISTERS))
        old_offsets = {v[0] for v in self.baseline.values()}
        for name in added:
            self.assertNotIn(self.regs[name][0], old_offsets, name)

    def test_traffic2_offsets(self):
        for name, offset in TRAFFIC2_REGISTERS.items():
            with self.subTest(register=name):
                self.assertEqual(self.regs[name][0], offset)

    def test_traffic2_mirrors_traffic(self):
        """Chain 2 registers have chain 1's field layout (prefix swap)."""
        pairs = {n: n.replace('traffic2_', 'traffic_')
                 for n in TRAFFIC2_REGISTERS}
        for new, old in pairs.items():
            with self.subTest(register=new):
                self.assertIn(old, self.regs)
                new_fields = {f.replace('traffic2_', 'traffic_'): v
                              for f, v in self.regs[new][2].items()}
                self.assertEqual(new_fields, self.regs[old][2])
                self.assertEqual(self.regs[new][1], self.regs[old][1])

    def test_interrupt_bit(self):
        fields = self.regs['interrupts'][2]
        self.assertEqual(fields['traffic2_lsm_dibit_dma'],
                         ('[8:8]', 'read-only'))

    def test_offsets_unique_and_decodable(self):
        offsets = [v[0] for v in self.regs.values()]
        self.assertEqual(len(offsets), len(set(offsets)))
        for name, (offset, _, _) in self.regs.items():
            # 16 banks x 32 B: the decoder uses word-address bits [6:3].
            self.assertLess(offset, 0x200, name)
            self.assertEqual(offset % 4, 0, name)

    def test_pac_svd_matches_hdl(self):
        """p25-httpd/p25-pac/p25.svd is the SVD of this gateware."""
        if not os.path.exists(PAC_SVD):
            self.skipTest('p25-httpd not present')
        with open(PAC_SVD, 'rb') as f:
            pac = f.read().replace(b'\r\n', b'\n')
        self.assertEqual(pac, self.svd.replace(b'\r\n', b'\n'))


def verilog_top_ports(verilog):
    i = verilog.index('module top(')
    j = verilog.index(');', i)
    return [p.strip() for p in
            verilog[i + len('module top('):j].replace('\n', '').split(',')]


class TestP25Elaboration(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.top = P25Core()
        cls.verilog = amaranth.back.verilog.convert(
            cls.top, platform=PlutoPlatform(), ports=cls.top.ports())
        cls.ports = verilog_top_ports(cls.verilog)

    def test_traffic2_axi_master_port(self):
        for p in ['awaddr', 'awlen', 'awsize', 'awburst', 'awcache',
                  'awprot', 'awvalid', 'awready', 'wdata', 'wstrb', 'wlast',
                  'wvalid', 'wready', 'bresp', 'bvalid', 'bready']:
            self.assertIn(f'm_axi_traffic2_lsm_dibit_{p}', self.ports)

    def test_existing_masters_kept(self):
        for name in ['iq', 'lsm_dibit', 'pre_diff_iq', 'wideband_spec',
                     'traffic_lsm_dibit', 'traffic_iq', 'traffic_pre_diff_iq',
                     'wideband_iq']:
            self.assertIn(f'm_axi_{name}_awaddr', self.ports)

    def test_traffic2_chain_present(self):
        for module in [
                'top.traffic2_ddc.mixer',
                'top.traffic2_lsm_decimator',
                'top.traffic2_lsm_lpf',
                'top.traffic2_lsm_rrc',
                # 059 PLL/timing no-signal hold on chain 2 too.
                'top.traffic2_lsm_demod.demod_loop.signal_hold',
                'top.traffic2_lsm_dibit_packer',
                'top.traffic2_lsm_dibit_dma',
                'top.traffic2_lsm_dibit_dma_irq_sync',
                'top.traffic2_lsm_registers_cdc',
                'top.traffic2_sdr_registers_cdc']:
            self.assertIn(f'module \\{module} ', self.verilog, module)

    def test_every_master_is_packaged_and_wired(self):
        """New DMA master: bus/clock association + an HP1 slave port."""
        with open(PACKAGE_IP_TCL, encoding='utf-8') as f:
            package = f.read()
        with open(SYSTEM_BD_TCL, encoding='utf-8') as f:
            bd = f.read()
        self.assertIn('-busif m_axi_traffic2_lsm_dibit -clock clk', package)
        self.assertRegex(
            bd, r'(?m)^ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 '
                r'p25_core/m_axi_traffic2_lsm_dibit\s*$')
        # Masters wired on HP1 in 0.2.0 must stay wired.
        for name in ['iq', 'lsm_dibit', 'pre_diff_iq', 'wideband_spec',
                     'traffic_lsm_dibit', 'wideband_iq']:
            self.assertRegex(
                bd, rf'(?m)^ad_mem_hp1_interconnect maia_sdr_clk/clk_out1 '
                    rf'p25_core/m_axi_{name}\s*$')


class P25CoreSim:
    """Full P25Core in pysim with AXI4-Lite register access."""

    def run_sim(self, bench, background=()):
        sim = Simulator(self.top)
        for domain, period in CLOCKS.items():
            sim.add_clock(period, domain=domain)
        for bg in background:
            sim.add_testbench(bg, background=True)
        sim.add_testbench(bench)
        sim.run()

    def setUp(self):
        self.top = P25Core()
        self.axi = self.top.axi4lite.axi

    async def wr(self, ctx, addr, value):
        await axil_write(ctx, self.axi, addr, value, domain='s_axi_lite')

    async def rd(self, ctx, addr):
        return await axil_read(ctx, self.axi, addr, domain='s_axi_lite')

    async def release_reset(self, ctx):
        # sdr_reset resets 1 and holds the sync-domain banks in reset
        # (the PS clears it first thing in IpCore::take).
        await self.wr(ctx, CONTROL, 0)
        await ctx.tick('s_axi_lite').repeat(20)


class TestP25CoreRegisters(P25CoreSim, unittest.TestCase):
    def test_version_register(self):
        async def bench(ctx):
            self.assertEqual(await self.rd(ctx, 0x00), 0x70323566)
            self.assertEqual(await self.rd(ctx, 0x04), 0x00_00_03_00)
        self.run_sim(bench)

    def test_reset_values(self):
        async def bench(ctx):
            await self.release_reset(ctx)
            regs = TRAFFIC2_REGISTERS
            self.assertEqual(await self.rd(ctx, regs['traffic2_lsm_control']),
                             0)
            self.assertEqual(
                await self.rd(ctx, regs['traffic2_lsm_agc_config']), 256)
            # last_buffer resets to -1 (3 bits) in [18:16].
            self.assertEqual(
                await self.rd(ctx, regs['traffic2_lsm_drop_count']),
                0x7 << 16)
            # Next burst address: the ring base.
            self.assertEqual(
                await self.rd(ctx, regs['traffic2_lsm_dibit_next']),
                0x1D00_0000)
            self.assertEqual(
                await self.rd(ctx, TRAFFIC_LSM_DIBIT_NEXT), 0x1B00_0000)
            for name in ['traffic2_lsm_agc_seed', 'traffic2_lsm_pll_seed',
                         'traffic2_lsm_timing_seed', 'traffic2_ddc_frequency',
                         'traffic2_ddc_control', 'traffic2_ddc_decimation']:
                self.assertEqual(await self.rd(ctx, regs[name]), 0, name)
            self.assertEqual(await self.rd(ctx, INTERRUPTS), 0)
        self.run_sim(bench)

    def test_rw_readback_and_no_aliasing(self):
        """Write distinct values to chain 1, chain 2 and seed registers,
        then read all back: no bank or word aliases another."""
        writes = {
            # chain 2 (new)
            0x120: (0x3FF, 0x2A5),
            0x12C: (0xFFFFF, 0x5_4321),
            0x130: (0xFFF_FFFF, 0x123_4567),
            0x134: (0x1FF_FFFF, 0x0AB_CDEF),
            0x15C: (0xFFFF, 0x1357),
            0x160: (0xF_FFFF, 0xA_BCDE),
            0x164: (0xFFFF, 0x8765),
            0x168: (0x3_FFFF, 0x2_468A),
            # chain 1 and the 0.2.0 seed bank (must be unaffected)
            TRAFFIC_DDC_FREQUENCY: (0xFFF_FFFF, 0x0FE_DCBA),
            TRAFFIC_LSM_AGC_CONFIG: (0xFFFF, 0x0246),
        }
        seed_bank = {LSM_SEED_BANK + 4 * i: 0x100 + i for i in range(6)}

        async def bench(ctx):
            await self.release_reset(ctx)
            for addr, (mask, value) in writes.items():
                await self.wr(ctx, addr, value)
            for addr, value in seed_bank.items():
                await self.wr(ctx, addr, value)
            for addr, (mask, value) in writes.items():
                self.assertEqual(await self.rd(ctx, addr), value & mask,
                                 hex(addr))
            for addr, value in seed_bank.items():
                self.assertEqual(await self.rd(ctx, addr), value, hex(addr))
            # Unused words of the 16-word bank answer with 0. (A read of
            # vacant bank 15 gets no answer at all, as in 0.2.0.)
            for addr in [0x16C, 0x170, 0x17C]:
                self.assertEqual(await self.rd(ctx, addr), 0, hex(addr))
        self.run_sim(bench)

    def test_controls_reach_chain2_only(self):
        top = self.top
        t2 = top.traffic2_lsm_demod
        t1 = top.traffic_lsm_demod
        pulses = {'t1': 0, 't2': 0}

        async def count_resets(ctx):
            # The Wpulse fires in `sync` before the AXI write response
            # comes back, so it is counted concurrently.
            while True:
                await ctx.tick('sync')
                pulses['t1'] += ctx.get(t1.reset_in)
                pulses['t2'] += ctx.get(t2.reset_in)

        async def bench(ctx):
            await self.release_reset(ctx)
            await self.wr(ctx, 0x130, 0x123_4567)          # NCO
            await self.wr(ctx, 0x12C, 3 | (4 << 7) | (5 << 13))
            await self.wr(ctx, 0x134, (1 << 22) | (1 << 24))
            await self.wr(ctx, 0x160, 0xA_BCDE)            # agc seed
            await self.wr(ctx, 0x164, 0x8765)              # pll seed
            await self.wr(ctx, 0x168, 0x2_468A)            # timing seed
            await self.wr(ctx, 0x15C, 0x1357)              # AGC gate
            # enable | dma_enable | dc_block | agc  (no reset)
            await self.wr(ctx, 0x140, 0b11011)
            await ctx.tick('sync').repeat(4)

            ddc2 = top.traffic2_ddc
            self.assertEqual(ctx.get(ddc2.frequency), 0x123_4567)
            self.assertEqual(ctx.get(ddc2.decimation1), 3)
            self.assertEqual(ctx.get(ddc2.decimation2), 4)
            self.assertEqual(ctx.get(ddc2.decimation3), 5)
            self.assertEqual(ctx.get(ddc2.bypass2), 1)
            self.assertEqual(ctx.get(ddc2.enable_input), 1)
            self.assertEqual(ctx.get(top.traffic_ddc.frequency), 0)
            self.assertEqual(ctx.get(top.traffic_ddc.bypass2), 0)

            self.assertEqual(ctx.get(t2.agc_seed_in), 0xA_BCDE)
            self.assertEqual(ctx.get(t2.pll_seed_in), 0x8765 - 0x10000)
            self.assertEqual(ctx.get(t2.timing_seed_in), 0x2_468A - 0x4_0000)
            self.assertEqual(ctx.get(t2.agc_mag_update_threshold_in), 0x1357)
            self.assertEqual(ctx.get(t2.dc_block_enable), 1)
            self.assertEqual(ctx.get(t2.agc_enable), 1)
            self.assertEqual(ctx.get(top.traffic2_lsm_dibit_dma.enable), 1)
            self.assertEqual(ctx.get(t1.agc_seed_in), 0)
            self.assertEqual(ctx.get(t1.pll_seed_in), 0)
            self.assertEqual(ctx.get(t1.agc_mag_update_threshold_in), 256)
            self.assertEqual(ctx.get(top.traffic_lsm_dibit_dma.enable), 0)

            # traffic2_lsm_reset (Wpulse, bit 2) -> chain 2 reset_in only.
            self.assertEqual(pulses, {'t1': 0, 't2': 0})
            await self.wr(ctx, 0x140, 0b11111)
            await ctx.tick('sync').repeat(10)
            self.assertEqual(pulses, {'t1': 0, 't2': 1})
            # Wpulse bit reads back 0; the level bits stay set.
            self.assertEqual(await self.rd(ctx, 0x140), 0b11011)
            # Chain 1's reset does not touch chain 2.
            await self.wr(ctx, TRAFFIC_LSM_CONTROL, 0b100)
            await ctx.tick('sync').repeat(10)
            self.assertEqual(pulses, {'t1': 1, 't2': 1})
        self.run_sim(bench, background=[count_resets])


class TestP25CoreTraffic2Dma(P25CoreSim, unittest.TestCase):
    def test_dma_and_interrupt(self):
        """Chain 2 ring: gated by its enable, bursts at 0x1D00_0000,
        sub-buffer completion -> last_buffer + interrupt bit 8."""
        async def bench(ctx):
            top = self.top
            dma2 = top.traffic2_lsm_dibit_dma.axi
            dma1 = top.traffic_lsm_dibit_dma.axi
            await self.release_reset(ctx)
            await ctx.tick('sync').repeat(4)
            self.assertEqual(ctx.get(dma2.awvalid), 0)

            await self.wr(ctx, 0x140, 0b10)   # dibit DMA enable only
            await ctx.tick('sync').repeat(4)
            self.assertEqual(ctx.get(dma2.awvalid), 1)
            self.assertEqual(ctx.get(dma2.awaddr), 0x1D00_0000)
            self.assertEqual(ctx.get(dma1.awvalid), 0)

            # Accept one AW: the next burst address moves on by 128 B.
            ctx.set(dma2.awready, 1)
            await ctx.tick('sync')
            ctx.set(dma2.awready, 0)
            await ctx.tick('sync').repeat(4)
            self.assertEqual(await self.rd(ctx, 0x150), 0x1D00_0080)

            # One 4 KB sub-buffer = 32 bursts = 32 B responses.
            bursts = (P25Config().traffic2_lsm_dibit_dma_buffer_size
                      // (16 * 8))
            self.assertEqual(bursts, 32)
            for i in range(bursts):
                if i == bursts - 1:
                    self.assertEqual(await self.rd(ctx, INTERRUPTS), 0)
                ctx.set(dma2.bvalid, 1)
                await ctx.tick('sync')
                ctx.set(dma2.bvalid, 0)
                await ctx.tick('sync')
            await ctx.tick('s_axi_lite').repeat(10)

            self.assertEqual(ctx.get(top.interrupt_out), 1)
            # last_buffer 7 -> 0 in traffic2_lsm_drop_count[18:16].
            self.assertEqual(await self.rd(ctx, 0x14C) >> 16, 0)
            self.assertEqual(await self.rd(ctx, TRAFFIC_LSM_DROP_COUNT) >> 16,
                             7)
            irq = await self.rd(ctx, INTERRUPTS)
            self.assertEqual(irq, 1 << INTERRUPT_BIT_TRAFFIC2)
            # Read-to-clear.
            await ctx.tick('s_axi_lite').repeat(4)
            self.assertEqual(await self.rd(ctx, INTERRUPTS), 0)
            await ctx.tick('s_axi_lite').repeat(4)
            self.assertEqual(ctx.get(top.interrupt_out), 0)
        self.run_sim(bench)


class TestP25CoreTraffic2Datapath(P25CoreSim, unittest.TestCase):
    def test_chain2_runs_on_its_own_enable(self):
        """traffic2_lsm_enable wakes chain 2 down to the symbol clock;
        chain 1 (traffic_lsm_enable = 0) stays idle."""
        async def bench(ctx):
            top = self.top
            await self.release_reset(ctx)
            # Chain 2 DDC: /64 stage 1, stages 2 and 3 bypassed. With the
            # simulated input strobe on every sync cycle this puts one
            # LSM sample per ~128 cycles into the LPF (needs >= 123).
            await self.wr(ctx, 0x12C, 64)
            await self.wr(ctx, 0x134, (1 << 22) | (1 << 23) | (1 << 24))
            await ctx.tick('sync').repeat(300)
            # Not enabled yet: the decimator input is gated.
            self.assertEqual(ctx.get(top.traffic2_lsm_lpf.strobe_in), 0)

            await self.wr(ctx, 0x140, 0b11001)  # enable | dc | agc
            seen = dict.fromkeys(['dec', 'lpf', 'rrc', 'sym'], 0)
            t1_dec = 0
            for _ in range(2500):
                await ctx.tick('sync')
                seen['dec'] += ctx.get(top.traffic2_lsm_decimator.strobe_out)
                seen['lpf'] += ctx.get(top.traffic2_lsm_lpf.strobe_out)
                seen['rrc'] += ctx.get(top.traffic2_lsm_rrc.strobe_out)
                seen['sym'] += ctx.get(top.traffic2_lsm_demod.symbol_strobe)
                t1_dec += ctx.get(top.traffic_lsm_decimator.strobe_out)
                if seen['sym'] >= 2:
                    break
            self.assertGreater(seen['dec'], 0, seen)
            self.assertGreater(seen['lpf'], 0, seen)
            self.assertGreater(seen['rrc'], 0, seen)
            self.assertGreaterEqual(seen['sym'], 2, seen)
            self.assertEqual(t1_dec, 0)
        self.run_sim(bench)


if __name__ == '__main__':
    unittest.main()
