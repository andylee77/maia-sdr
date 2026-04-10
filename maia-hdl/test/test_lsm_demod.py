#
# Fishball P25 -- LSM demod top-level pass-through tests
#
# Phase 6E.8c. Verifies that `LsmDemod` (LsmDemodLoop +
# LsmNidPipeline) wires its sub-modules correctly:
#
#   - the dibit pass-through still produces the same dibits
#     `LsmDemodLoop` does on the synthetic IQ golden, so the
#     existing dibit DMA path keeps working
#   - on a synthetic input that contains no sync pattern, the
#     NID pipeline stays quiescent: bch_busy never asserts,
#     no NID events fire, and the drop counter stays at 0
#   - the in_nid_window debug tap stays low across the run
#
# The "does the sync->BCH chain decode a real NID end-to-end"
# integration test lives in `test_lsm_nid_pipeline.py` because
# it can drive constructed dibits directly without paying for a
# full IQ-to-dibit demod loop on top of the BCH sweep. This
# file is the complementary "does the demod loop wire correctly"
# test.
#
# Sim cost: ~24 seconds (same as test_lsm_demod_loop, since the
# stimulus is the same).
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_demod import LsmDemod

from .golden_vector_loader import load_demod_stage, to_fixed


# Same loop-drain budget as test_lsm_demod_loop.
CYCLES_BETWEEN_STROBES = 16


class TestLsmDemod(unittest.TestCase):

    def test_dibit_passthrough_and_quiescent_nid_pipeline(self):
        """Drive `LsmDemod` with the demod_loop_synthetic golden
        IQ and verify:

          1. dibit_out / symbol_strobe still produce dibits at
             roughly the expected count (the demod loop's job
             is unchanged, just wrapped one level deeper).
          2. The synthetic golden has no sync pattern in it, so
             nid_event_strobe must never fire, bch_busy must
             stay low, in_nid_window must stay low, and
             nid_drop_count must stay at 0.

        This is the LsmDemod-level wiring test. The standalone
        demod loop's per-dibit accuracy vs truth is covered by
        `test_lsm_demod_loop` and is not re-checked here.
        """
        stage = load_demod_stage('demod_loop_synthetic')

        # Same Q1.15 pre-scale as test_lsm_demod_loop, for the
        # same reason: post-RRC magnitude is ~2.4 and there's no
        # AGC yet (deferred to 6E.6.5).
        SCALE = 0.34
        scaled_re = [SCALE * x for x in stage.input_re]
        scaled_im = [SCALE * x for x in stage.input_im]
        in_re_q = to_fixed(scaled_re, frac_bits=15, width=16)
        in_im_q = to_fixed(scaled_im, frac_bits=15, width=16)

        dibits = []
        bch_busy_ever = False
        nid_window_ever = False
        nid_strobes = 0
        final_drop_count = None

        async def bench(ctx):
            nonlocal bch_busy_ever, nid_window_ever, nid_strobes
            nonlocal final_drop_count
            for k in range(stage.n_input):
                ctx.set(dut.re_in, in_re_q[k])
                ctx.set(dut.im_in, in_im_q[k])
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                for _ in range(CYCLES_BETWEEN_STROBES - 1):
                    await ctx.tick()
                    if ctx.get(dut.symbol_strobe):
                        dibits.append(ctx.get(dut.dibit_out))
                    if ctx.get(dut.bch_busy):
                        bch_busy_ever = True
                    if ctx.get(dut.in_nid_window):
                        nid_window_ever = True
                    if ctx.get(dut.nid_event_strobe):
                        nid_strobes += 1
            for _ in range(20):
                await ctx.tick()
                if ctx.get(dut.symbol_strobe):
                    dibits.append(ctx.get(dut.dibit_out))
            final_drop_count = ctx.get(dut.nid_drop_count)

        dut = LsmDemod()
        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        # ── 1. Dibit pass-through ──────────────────────────────
        # Synthetic golden produces ~239 dibits in the demod loop
        # standalone test. Check we're in the same ballpark via
        # the same loose 0.9x..1.1x bound used over there.
        self.assertGreaterEqual(
            len(dibits), int(0.9 * stage.n_symbols),
            f"LsmDemod emitted {len(dibits)} dibits via pass-"
            f"through, expected ~{stage.n_symbols}")
        self.assertLessEqual(
            len(dibits), int(1.1 * stage.n_symbols) + 4,
            f"LsmDemod emitted {len(dibits)} dibits via pass-"
            f"through, expected ~{stage.n_symbols}")

        # ── 2. NID pipeline quiescent on no-sync input ─────────
        self.assertFalse(
            bch_busy_ever,
            "BCH decoder went busy on a no-sync IQ input -- "
            "this means the sync detector spuriously triggered")
        self.assertFalse(
            nid_window_ever,
            "in_nid_window asserted on a no-sync input -- "
            "the sync detector spuriously triggered")
        self.assertEqual(
            nid_strobes, 0,
            f"got {nid_strobes} nid_event_strobes on a no-sync "
            f"IQ input, expected 0")
        self.assertEqual(
            final_drop_count, 0,
            f"nid_drop_count is non-zero on a no-sync input: "
            f"{final_drop_count}")


if __name__ == '__main__':
    unittest.main()
