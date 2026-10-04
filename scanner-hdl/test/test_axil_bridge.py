#
# Fishball P25 - AnsweringRegisterBridge: every AXI4-Lite access completes
#
# A fake bank behind the bridge claims word addresses 0-7 and answers after a set number of
# cycles (or never). Reads and writes to it, to unclaimed addresses, without byte strobes and to
# a bank that does not answer must all complete; only claimed accesses with strobes reach it.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from radio_core.axil_bridge import AnsweringRegisterBridge

from .hwval_axil_bfm import axil_read, axil_write

TIMEOUT = 40


class FakeBank(Elaboratable):
    """Claims word addresses 0-7. Word 1 is a register; other words read 0xC0DE0000 | address.
    Answers `delay` cycles after an access; never while `mute`."""
    def __init__(self):
        self.bridge = AnsweringRegisterBridge(8, timeout=TIMEOUT, name='s_axi_lite')
        self.delay = Signal(8, init=2)
        self.mute = Signal()
        self.reg = Signal(32)
        self.reads = Signal(16)
        self.writes = Signal(16)

    def elaborate(self, platform):
        m = Module()
        m.submodules.bridge = b = self.bridge
        m.d.comb += b.claimed.eq(b.address < 8)

        countdown = Signal(9)
        is_read = Signal()
        address = Signal(8)
        with m.If(b.ren | b.wstrobe.any()):
            m.d.sync += [countdown.eq(self.delay + 1), is_read.eq(b.ren), address.eq(b.address)]
        with m.If(b.ren):
            m.d.sync += self.reads.eq(self.reads + 1)
        with m.If(b.wstrobe.any()):
            m.d.sync += self.writes.eq(self.writes + 1)
            with m.If(b.address == 1):
                for i in range(4):
                    with m.If(b.wstrobe[i]):
                        m.d.sync += self.reg[8 * i:8 * i + 8].eq(b.wdata[8 * i:8 * i + 8])
        with m.If(countdown != 0):
            m.d.sync += countdown.eq(countdown - 1)
        answer = (countdown == 1) & ~self.mute
        m.d.comb += [
            b.rdone.eq(answer & is_read),
            b.wdone.eq(answer & ~is_read),
            b.rdata.eq(Mux(answer & is_read,
                           Mux(address == 1, self.reg, 0xC0DE_0000 | address), 0)),
        ]
        return m


class AnsweringBridgeTest(unittest.TestCase):
    def run_bench(self, bench):
        self.dut = FakeBank()
        sim = Simulator(self.dut)
        sim.add_clock(10e-9)
        sim.add_testbench(bench)
        sim.run()

    def test_claimed_read_and_write(self):
        async def bench(ctx):
            axi = self.dut.bridge.axi
            self.assertEqual(await axil_read(ctx, axi, 4 * 3, with_resp=True), (0xC0DE_0003, 0))
            self.assertEqual(await axil_write(ctx, axi, 4 * 1, 0x1234_5678), 0)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1), 0x1234_5678)
            # Byte strobes reach the bank.
            self.assertEqual(await axil_write(ctx, axi, 4 * 1, 0xAABB_CCDD, strb=0b0101), 0)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1), 0x12BB_56DD)
            # AW after W, W after AW, a slow BREADY.
            await axil_write(ctx, axi, 4 * 1, 1, aw_delay=3)
            await axil_write(ctx, axi, 4 * 1, 2, w_delay=3, b_delay=4)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1, r_delay=5), 2)
            self.assertEqual((ctx.get(self.dut.reads), ctx.get(self.dut.writes)), (4, 4))
        self.run_bench(bench)

    def test_unclaimed_addresses_answer_zero(self):
        async def bench(ctx):
            axi = self.dut.bridge.axi
            for addr in [4 * 8, 4 * 100, 0x3FC]:
                self.assertEqual(await axil_read(ctx, axi, addr, with_resp=True), (0, 0), addr)
                self.assertEqual(await axil_write(ctx, axi, addr, 0xFFFF_FFFF), 0, addr)
            self.assertEqual((ctx.get(self.dut.reads), ctx.get(self.dut.writes)), (0, 0))
        self.run_bench(bench)

    def test_write_without_strobes_completes(self):
        async def bench(ctx):
            axi = self.dut.bridge.axi
            await axil_write(ctx, axi, 4 * 1, 0x55)
            self.assertEqual(await axil_write(ctx, axi, 4 * 1, 0xFF, strb=0), 0)
            self.assertEqual(ctx.get(self.dut.writes), 1)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1), 0x55)
        self.run_bench(bench)

    def test_silent_bank_times_out(self):
        async def bench(ctx):
            axi = self.dut.bridge.axi
            await axil_write(ctx, axi, 4 * 1, 0x77)
            ctx.set(self.dut.mute, 1)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1, timeout=TIMEOUT + 20), 0)
            self.assertEqual(await axil_write(ctx, axi, 4 * 1, 0x88, timeout=TIMEOUT + 20), 0)
            ctx.set(self.dut.mute, 0)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1), 0x88)
        self.run_bench(bench)

    def test_late_answer_is_ignored(self):
        """A bank that answers after the timeout: the access gets 0, and the late answer does
        not complete anything else."""
        async def bench(ctx):
            axi = self.dut.bridge.axi
            await axil_write(ctx, axi, 4 * 1, 0x99)
            ctx.set(self.dut.delay, TIMEOUT + 10)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1, timeout=TIMEOUT + 20), 0)
            await ctx.tick().repeat(30)
            ctx.set(self.dut.delay, 2)
            self.assertEqual(await axil_read(ctx, axi, 4 * 5), 0xC0DE_0005)
            self.assertEqual(await axil_read(ctx, axi, 4 * 1), 0x99)
        self.run_bench(bench)


if __name__ == '__main__':
    unittest.main()
