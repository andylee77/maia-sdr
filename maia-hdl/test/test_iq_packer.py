#
# Fishball P25 - IQPacker tests (Phase 6C)
#
# Pure-Python pysim test for the post-DDC IQ packer. Validates the
# 64-bit packed word layout and the sticky overflow behaviour.
# Mirrors the structure of test_dibit_packer.py.
#
# See p25_hdl/iq_packer.py for the bit layout and
# doc/P25_ADDRESS_MAP.md for context.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.iq_packer import IQPacker


def s16(x):
    """Two's complement masking to 16 bits."""
    return x & 0xFFFF


def expected_word(re0, im0, re1, im1):
    """{im1, re1, im0, re0}, sample 0 in low half. See iq_packer.py."""
    return ((s16(im1) << 48)
            | (s16(re1) << 32)
            | (s16(im0) << 16)
            | s16(re0))


class TestIQPacker(unittest.TestCase):
    """Test (re, im) -> 64-bit DMA word packing."""

    def _simulate(self, bench, *, vcd=None):
        sim = Simulator(self.dut)
        sim.add_clock(12e-9)
        sim.add_testbench(bench)
        if vcd is None:
            sim.run()
        else:
            with sim.write_vcd(vcd):
                sim.run()

    def test_pack_one_pair(self):
        """Two strobed (re, im) pairs produce one packed 64-bit word.

        Note: with stream_ready=1, data_valid is high for exactly one
        cycle (then the handshake clears it), so we must poll after
        every tick rather than in a separate post-loop drain.
        """
        self.dut = IQPacker()
        output_words = []

        async def bench(ctx):
            ctx.set(self.dut.stream_ready, 1)
            # Sample 0
            ctx.set(self.dut.re_in, s16(+1))
            ctx.set(self.dut.im_in, s16(-1))
            ctx.set(self.dut.strobe_in, 1)
            await ctx.tick()
            if ctx.get(self.dut.data_valid):
                output_words.append(ctx.get(self.dut.data_out))
            # Sample 1
            ctx.set(self.dut.re_in, s16(+2))
            ctx.set(self.dut.im_in, s16(-2))
            await ctx.tick()
            if ctx.get(self.dut.data_valid):
                output_words.append(ctx.get(self.dut.data_out))
            ctx.set(self.dut.strobe_in, 0)
            for _ in range(4):
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_words.append(ctx.get(self.dut.data_out))

        self._simulate(bench)
        self.assertEqual(len(output_words), 1,
                         f'expected 1 word, got {len(output_words)}')
        self.assertEqual(output_words[0],
                         expected_word(+1, -1, +2, -2))

    def test_pack_multiple_pairs(self):
        """Eight pairs produce four packed words with the right contents."""
        self.dut = IQPacker()
        output_words = []
        n_pairs = 8

        async def bench(ctx):
            ctx.set(self.dut.stream_ready, 1)
            for k in range(n_pairs):
                re, im = (k + 1), -(k + 1)
                ctx.set(self.dut.re_in, s16(re))
                ctx.set(self.dut.im_in, s16(im))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_words.append(ctx.get(self.dut.data_out))
            ctx.set(self.dut.strobe_in, 0)
            for _ in range(4):
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_words.append(ctx.get(self.dut.data_out))

        self._simulate(bench)

        # 8 pairs / 2 = 4 packed words
        self.assertEqual(len(output_words), n_pairs // 2,
                         f'expected {n_pairs // 2} words, got {len(output_words)}')

        # Verify each word matches the expected pair-of-pairs.
        # The packer fires data_valid one cycle after the second strobe,
        # so the word for samples (2k, 2k+1) appears at iteration 2k+1
        # of the loop above. We collected them in-order, so:
        for w in range(n_pairs // 2):
            re0, im0 = (2 * w + 1), -(2 * w + 1)
            re1, im1 = (2 * w + 2), -(2 * w + 2)
            self.assertEqual(
                output_words[w], expected_word(re0, im0, re1, im1),
                f'word {w}: got {output_words[w]:#x}, '
                f'expected {expected_word(re0, im0, re1, im1):#x}')

    def test_negative_values(self):
        """Negative IQ values pack correctly as two's complement."""
        self.dut = IQPacker()
        output_words = []

        async def bench(ctx):
            ctx.set(self.dut.stream_ready, 1)
            # Sample 0: extreme negatives
            ctx.set(self.dut.re_in, s16(-1))
            ctx.set(self.dut.im_in, s16(-32768))
            ctx.set(self.dut.strobe_in, 1)
            await ctx.tick()
            if ctx.get(self.dut.data_valid):
                output_words.append(ctx.get(self.dut.data_out))
            # Sample 1: extreme positives
            ctx.set(self.dut.re_in, s16(32767))
            ctx.set(self.dut.im_in, s16(1))
            await ctx.tick()
            if ctx.get(self.dut.data_valid):
                output_words.append(ctx.get(self.dut.data_out))
            ctx.set(self.dut.strobe_in, 0)
            for _ in range(4):
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    output_words.append(ctx.get(self.dut.data_out))

        self._simulate(bench)
        self.assertEqual(len(output_words), 1)
        self.assertEqual(
            output_words[0],
            expected_word(-1, -32768, 32767, 1))

    def test_no_output_without_strobe(self):
        """No output when strobe_in stays low."""
        self.dut = IQPacker()
        valid_count = 0

        async def bench(ctx):
            nonlocal valid_count
            ctx.set(self.dut.stream_ready, 1)
            ctx.set(self.dut.re_in, s16(+5))
            ctx.set(self.dut.im_in, s16(-5))
            for _ in range(100):
                ctx.set(self.dut.strobe_in, 0)
                await ctx.tick()
                if ctx.get(self.dut.data_valid):
                    valid_count += 1

        self._simulate(bench)
        self.assertEqual(valid_count, 0)

    def test_backpressure(self):
        """data_valid stays high until stream_ready handshakes."""
        self.dut = IQPacker()

        valid_seen = False
        valid_held = False
        valid_cleared = False

        async def bench(ctx):
            nonlocal valid_seen, valid_held, valid_cleared
            ctx.set(self.dut.stream_ready, 0)
            # Feed one complete pair
            ctx.set(self.dut.re_in, s16(+1))
            ctx.set(self.dut.im_in, s16(-1))
            ctx.set(self.dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(self.dut.re_in, s16(+2))
            ctx.set(self.dut.im_in, s16(-2))
            await ctx.tick()
            ctx.set(self.dut.strobe_in, 0)

            await ctx.tick()
            valid_seen = ctx.get(self.dut.data_valid) == 1
            await ctx.tick()
            valid_held = ctx.get(self.dut.data_valid) == 1

            ctx.set(self.dut.stream_ready, 1)
            await ctx.tick()
            await ctx.tick()
            valid_cleared = ctx.get(self.dut.data_valid) == 0

        self._simulate(bench)
        self.assertTrue(valid_seen, 'data_valid should assert after one pair')
        self.assertTrue(valid_held, 'data_valid should hold without stream_ready')
        self.assertTrue(valid_cleared, 'data_valid should clear after handshake')

    def test_overflow_flag(self):
        """overflow fires for (at least) one cycle when a word is latched
        while the previous word is still waiting for stream_ready.

        Note that `overflow` is now a one-cycle pulse on the packer
        side, not a latched level -- see the IQPacker class docstring
        and doc/changes/020_iq_dibit_packer_overflow_pulse.md. The
        accumulation across multiple triggers is handled by the
        ``Rsticky`` wrapper in maia_hdl.register.Registers.
        """
        self.dut = IQPacker()
        overflow_seen = False

        async def bench(ctx):
            nonlocal overflow_seen
            ctx.set(self.dut.stream_ready, 0)
            # Feed 4 pairs (8 strobes = 2 words) without accepting any.
            # Check overflow on every cycle so a one-cycle pulse is not
            # missed.
            for k in range(8):
                ctx.set(self.dut.re_in, s16(k + 1))
                ctx.set(self.dut.im_in, s16(-(k + 1)))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.overflow) == 1:
                    overflow_seen = True
            ctx.set(self.dut.strobe_in, 0)
            await ctx.tick()
            if ctx.get(self.dut.overflow) == 1:
                overflow_seen = True

        self._simulate(bench)
        self.assertTrue(overflow_seen, 'overflow should pulse when a word stalls')

    def test_overflow_is_pulse_not_latched(self):
        """overflow must return to 0 within a couple of cycles after the
        trigger event. A latched-level bug would cause the Rsticky
        register wrapper to re-accumulate it on every cycle, breaking
        PS-side clear-on-read. This test guards against reintroducing
        the Phase 6C spurious-overflow bug documented in
        doc/changes/020_iq_dibit_packer_overflow_pulse.md.
        """
        self.dut = IQPacker()
        high_cycles = 0
        overflow_ever_fired = False

        async def bench(ctx):
            nonlocal high_cycles, overflow_ever_fired
            ctx.set(self.dut.stream_ready, 0)
            # Feed enough strobes to guarantee at least one overflow
            # trigger.
            for k in range(8):
                ctx.set(self.dut.re_in, s16(k))
                ctx.set(self.dut.im_in, s16(-k))
                ctx.set(self.dut.strobe_in, 1)
                await ctx.tick()
                if ctx.get(self.dut.overflow) == 1:
                    overflow_ever_fired = True
                    high_cycles += 1
            ctx.set(self.dut.strobe_in, 0)
            # After the last strobe, stream_ready is still 0 but no new
            # strobes are arriving. overflow must fall to 0 within at
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
                    # latched level -- this is the bug we're guarding
                    # against
                    high_cycles += 1000

            self.assertFalse(
                final_overflow,
                'overflow must not be held high when no trigger fires')

        self._simulate(bench)
        self.assertTrue(overflow_ever_fired,
                        'expected at least one overflow trigger in this bench')
        self.assertLess(
            high_cycles, 4,
            f'overflow stayed high for {high_cycles} cycles -- this is the '
            f'Phase 6C latched-level bug (see doc 020); it must pulse for '
            f'only ~1 cycle per trigger')


if __name__ == '__main__':
    unittest.main()
