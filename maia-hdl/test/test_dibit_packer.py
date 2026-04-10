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
        """overflow fires for (at least) one cycle when a new word is
        latched while the previous one is still waiting for stream_ready.

        Note that `overflow` is a one-cycle pulse on the packer side,
        not a latched level -- see the DibitPacker class docstring
        and doc/changes/020_iq_dibit_packer_overflow_pulse.md.
        """
        self.dut = DibitPacker()

        overflow_seen = False

        async def bench(ctx):
            nonlocal overflow_seen
            ctx.set(self.dut.stream_ready, 0)
            # Feed 64 dibits (2 words) without accepting any. Check
            # overflow on every cycle so a one-cycle pulse is not missed.
            for i in range(64):
                ctx.set(self.dut.dibit_in, i % 4)
                ctx.set(self.dut.symbol_strobe, 1)
                await ctx.tick()
                if ctx.get(self.dut.overflow) == 1:
                    overflow_seen = True
            ctx.set(self.dut.symbol_strobe, 0)
            await ctx.tick()
            if ctx.get(self.dut.overflow) == 1:
                overflow_seen = True

        self._simulate(bench)
        self.assertTrue(overflow_seen, "overflow should pulse when word stalls")

    def test_overflow_is_pulse_not_latched(self):
        """overflow must return to 0 within a couple of cycles after the
        trigger event. A latched-level bug would cause the Rsticky
        register wrapper to re-accumulate it on every cycle, breaking
        PS-side clear-on-read. This test guards against reintroducing
        the Phase 6C spurious-overflow bug documented in
        doc/changes/020_iq_dibit_packer_overflow_pulse.md.
        """
        self.dut = DibitPacker()
        high_cycles = 0
        overflow_ever_fired = False

        async def bench(ctx):
            nonlocal high_cycles, overflow_ever_fired
            ctx.set(self.dut.stream_ready, 0)
            # Feed enough dibits to guarantee at least one overflow
            # trigger (2+ full 32-dibit words while stalled).
            for i in range(96):
                ctx.set(self.dut.dibit_in, i % 4)
                ctx.set(self.dut.symbol_strobe, 1)
                await ctx.tick()
                if ctx.get(self.dut.overflow) == 1:
                    overflow_ever_fired = True
                    high_cycles += 1
            ctx.set(self.dut.symbol_strobe, 0)
            # After the last strobe, stream_ready is still 0 but no new
            # symbols are arriving. overflow must fall to 0 within at
            # most one tick because its only driver is the trigger edge.
            await ctx.tick()
            await ctx.tick()
            final_overflow = ctx.get(self.dut.overflow) == 1

            # Now release back-pressure and wait a few cycles. overflow
            # must stay at 0 -- anything else means the packer is
            # latching a level the Rsticky wrapper cannot clear.
            ctx.set(self.dut.stream_ready, 1)
            for _ in range(8):
                await ctx.tick()
                if ctx.get(self.dut.overflow) == 1:
                    high_cycles += 1000

            self.assertFalse(
                final_overflow,
                'overflow must not be held high when no trigger fires')

        self._simulate(bench)
        self.assertTrue(overflow_ever_fired,
                        'expected at least one overflow trigger in this bench')
        # Expected pulse count: ~2-3 cycles across the 96-strobe test
        # (one per word boundary while stalled). Definitely < 10.
        self.assertLess(
            high_cycles, 10,
            f'overflow stayed high for {high_cycles} cycles -- this is the '
            f'Phase 6C latched-level bug (see doc 020); it must pulse for '
            f'only ~1 cycle per trigger')


if __name__ == '__main__':
    unittest.main()
