#
# Fishball hardware validation (hwval) - device DNA reader tests
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
import amaranth.back.verilog

import unittest

from hwval_hdl.dna import DnaReader, DNA_BITS
from .amaranth_sim import AmaranthSim


class TestDnaReader(AmaranthSim):
    def check(self, value):
        self.dut = DnaReader(sim=True, sim_value=value)

        async def bench(ctx):
            cycles = 0
            while not ctx.get(self.dut.valid):
                await ctx.tick()
                cycles += 1
                assert cycles < 1000, 'DNA read did not finish'
            assert ctx.get(self.dut.dna) == value, \
                f'{ctx.get(self.dut.dna):#x} != {value:#x}'
            # stays valid and stable
            await ctx.tick().repeat(100)
            assert ctx.get(self.dut.valid)
            assert ctx.get(self.dut.dna) == value

        async def interface_timing(ctx):
            # READ/SHIFT only change while the DNA clock is low and at
            # least 3 cycles away from its rising edges.
            hist = []
            for _ in range(600):
                hist.append((ctx.get(self.dut.dna_clk),
                             ctx.get(self.dut.dna_read),
                             ctx.get(self.dut.dna_shift)))
                await ctx.tick()
            rises = [j for j in range(1, len(hist))
                     if hist[j][0] and not hist[j - 1][0]]
            assert len(rises) > 60
            changes = [j for j in range(1, len(hist))
                       if hist[j][1:] != hist[j - 1][1:]]
            assert changes
            for j in changes:
                assert not hist[j][0], f'control changed with clk high @{j}'
                assert min(abs(j - r) for r in rises) >= 3, j

        self.simulate([bench, interface_timing])

    def test_default_value(self):
        self.check(0x0_1234_5678_9ABC_DE)

    def test_msb_lsb(self):
        self.check((1 << (DNA_BITS - 1)) | 1)

    def test_all_ones(self):
        self.check(2**DNA_BITS - 1)

    def test_instance_in_verilog(self):
        dna = DnaReader()
        v = amaranth.back.verilog.convert(dna, ports=[dna.dna, dna.valid])
        assert 'DNA_PORT' in v
        assert 'SIM_DNA_VALUE' in v


if __name__ == '__main__':
    unittest.main()
