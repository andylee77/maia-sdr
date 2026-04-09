#
# Fishball P25 - Dibit Packer tests
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.dibit_packer import DibitPacker


class TestDibitPacker(unittest.TestCase):
    """Test dibit -> 64-bit DMA word packing."""

    def _simulate(self, bench, *, vcd=None):
        sim = Simulator(self.dut)
        sim.add_clock(12e-9)
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    def test_pack_32_dibits(self):
        """32 dibits produce one 64-bit word with correct packing."""
        self.dut = DibitPacker()

        # Known dibit sequence: 0, 1, 2, 3 repeated 8 times = 32 dibits
        dibits = [0, 1, 2, 3] * 8
        output_words = []

        async def bench(ctx):
            ctx.set(self.dut.stream_ready, 1)
            for dibit in dibits:
                ctx.set(self.dut.dibit_in, dibit)
                ctx.set(self.dut.symbol_strobe, 1)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_words.append(ctx.get(self.dut.data_out))
            # Extra cycles to catch pipeline output
            for _ in range(5):
                ctx.set(self.dut.symbol_strobe, 0)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_words.append(ctx.get(self.dut.data_out))

        self._simulate(bench)

        self.assertEqual(len(output_words), 1,
                         f"Expected 1 output word, got {len(output_words)}")

        # Verify each dibit is packed correctly
        word = output_words[0]
        for i, expected_dibit in enumerate(dibits):
            actual = (word >> (i * 2)) & 0x3
            self.assertEqual(actual, expected_dibit,
                             f"Dibit {i}: expected {expected_dibit}, got {actual}")

    def test_multiple_words(self):
        """64 dibits produce exactly 2 output words."""
        self.dut = DibitPacker()

        dibits = list(range(4)) * 16  # 64 dibits
        output_count = 0

        async def bench(ctx):
            nonlocal output_count
            ctx.set(self.dut.stream_ready, 1)
            for dibit in dibits:
                ctx.set(self.dut.dibit_in, dibit)
                ctx.set(self.dut.symbol_strobe, 1)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_count += 1
            for _ in range(5):
                ctx.set(self.dut.symbol_strobe, 0)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_count += 1

        self._simulate(bench)
        self.assertEqual(output_count, 2)

    def test_no_output_without_strobe(self):
        """No output when symbol_strobe is low."""
        self.dut = DibitPacker()

        valid_count = 0

        async def bench(ctx):
            nonlocal valid_count
            ctx.set(self.dut.stream_ready, 1)
            for _ in range(100):
                ctx.set(self.dut.dibit_in, 1)
                ctx.set(self.dut.symbol_strobe, 0)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    valid_count += 1

        self._simulate(bench)
        self.assertEqual(valid_count, 0)

    def test_backpressure(self):
        """data_valid stays high until stream_ready handshake."""
        self.dut = DibitPacker()

        valid_seen = False
        valid_held = False
        valid_cleared = False

        async def bench(ctx):
            nonlocal valid_seen, valid_held, valid_cleared
            # Feed 32 dibits with stream_ready deasserted
            ctx.set(self.dut.stream_ready, 0)
            for i in range(32):
                ctx.set(self.dut.dibit_in, i % 4)
                ctx.set(self.dut.symbol_strobe, 1)
                await ctx.tick()
            ctx.set(self.dut.symbol_strobe, 0)

            # Wait a cycle for output to register
            await ctx.tick()

            # data_valid should be high and held
            valid_seen = ctx.get(self.dut.data_valid) == 1
            await ctx.tick()
            valid_held = ctx.get(self.dut.data_valid) == 1

            # Assert stream_ready -> should clear on next cycle
            ctx.set(self.dut.stream_ready, 1)
            await ctx.tick()
            await ctx.tick()
            valid_cleared = ctx.get(self.dut.data_valid) == 0

        self._simulate(bench)
        self.assertTrue(valid_seen, "data_valid should assert after 32 dibits")
        self.assertTrue(valid_held, "data_valid should hold without stream_ready")
        self.assertTrue(valid_cleared, "data_valid should clear after handshake")

    def test_overflow_flag(self):
        """overflow flag sets if new word ready while previous stalled."""
        self.dut = DibitPacker()

        overflow = False

        async def bench(ctx):
            nonlocal overflow
            # Disable stream_ready -> words will stall
            ctx.set(self.dut.stream_ready, 0)
            # Feed 64 dibits (2 words) without accepting any
            for i in range(64):
                ctx.set(self.dut.dibit_in, i % 4)
                ctx.set(self.dut.symbol_strobe, 1)
                await ctx.tick()
            ctx.set(self.dut.symbol_strobe, 0)
            await ctx.tick()
            overflow = ctx.get(self.dut.overflow) == 1

        self._simulate(bench)
        self.assertTrue(overflow, "overflow should set when word stalls")


if __name__ == '__main__':
    unittest.main()
