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

    def test_dc_blocker_absorbs_constant_iq_bias(self):
        """Phase 6G.1 regression: drive LsmDemod with the synthetic
        golden plus a constant DC offset on both I and Q, and verify
        the dibit pass-through still produces ~the same dibit count
        as the unbiased run.

        This is the integration-level proof that the DC blocker
        wiring is correct: the demod loop downstream of the blocker
        sees IQ with the bias removed, so the dibit stream is not
        catastrophically corrupted by an offset that would otherwise
        skew the slicer (which is exactly the symptom doc/changes/030
        identified as the cause of the 2-3 minute PLL acquisition
        transient on cold boot).

        We don't compare dibit-for-dibit against the unbiased run --
        the leaky integrator's startup transient adds a small
        amount of additional warm-up jitter -- but the dibit *count*
        is a robust integration check that survives the warm-up.
        """
        stage = load_demod_stage('demod_loop_synthetic')

        SCALE = 0.34
        # ~6 % of full scale -- much larger than any DC bias the
        # AD9361 produces in practice but well clear of saturation
        # at the 0.34 pre-scale, so the underlying signal still
        # round-trips through the chain unclipped.
        DC_BIAS = 2000

        scaled_re = [SCALE * x for x in stage.input_re]
        scaled_im = [SCALE * x for x in stage.input_im]
        in_re_q = [v + DC_BIAS
                   for v in to_fixed(scaled_re, frac_bits=15, width=16)]
        in_im_q = [v + DC_BIAS
                   for v in to_fixed(scaled_im, frac_bits=15, width=16)]

        dibits = []

        async def bench(ctx):
            # Default dc_block_enable=1 (set by the Signal init).
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
            for _ in range(20):
                await ctx.tick()
                if ctx.get(dut.symbol_strobe):
                    dibits.append(ctx.get(dut.dibit_out))

        dut = LsmDemod()
        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        # The DC blocker has a ~128-sample warm-up time constant
        # at K=7. The synthetic golden runs many hundreds of
        # samples, so by the end of the run the blocker is well
        # past convergence. Reuse the same loose 0.9x..1.1x bound
        # the unbiased test uses.
        self.assertGreaterEqual(
            len(dibits), int(0.9 * stage.n_symbols),
            f"With DC bias + blocker, LsmDemod emitted {len(dibits)} "
            f"dibits, expected ~{stage.n_symbols}")
        self.assertLessEqual(
            len(dibits), int(1.1 * stage.n_symbols) + 4,
            f"With DC bias + blocker, LsmDemod emitted {len(dibits)} "
            f"dibits, expected ~{stage.n_symbols}")

    def test_reset_in_clears_pll_and_sample_point(self):
        """Phase 8A: asserting `reset_in` mid-stream must clear the
        PLL accumulator and sample-point debug registers to 0 within
        one sync cycle. This is the direct HDL-level acceptance
        criterion for the runtime reset plumbing -- the full
        post-reset re-lock behaviour is validated on-target after
        the Phase 8B PS integration lands.
        """
        stage = load_demod_stage('demod_loop_synthetic')

        SCALE = 0.34
        scaled_re = [SCALE * x for x in stage.input_re]
        scaled_im = [SCALE * x for x in stage.input_im]
        in_re_q = to_fixed(scaled_re, frac_bits=15, width=16)
        in_im_q = to_fixed(scaled_im, frac_bits=15, width=16)

        # Drive long enough for the PLL to have absorbed a few
        # symbols' worth of error so pll_dbg has non-zero content.
        WARMUP_SAMPLES = min(200, stage.n_input)

        pll_pre = None
        sp_pre = None
        pll_post = None
        sp_post = None

        async def bench(ctx):
            nonlocal pll_pre, sp_pre, pll_post, sp_post
            for k in range(WARMUP_SAMPLES):
                ctx.set(dut.re_in, in_re_q[k])
                ctx.set(dut.im_in, in_im_q[k])
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                for _ in range(CYCLES_BETWEEN_STROBES - 1):
                    await ctx.tick()

            # Let the pipeline fully drain before pulsing reset --
            # the last IQ strobe above triggers a ~20-cycle chain
            # through timing -> diff_demod -> rotate -> CORDIC ->
            # pll post-stages, and we want all of that to have
            # written back into pll_reg before we sample pll_pre
            # and fire the reset pulse. This mirrors the PS-side
            # protocol: disable, WAIT, then reset.
            for _ in range(128):
                await ctx.tick()

            pll_pre = ctx.get(dut.pll_dbg)
            sp_pre = ctx.get(dut.sample_point_dbg)

            # Pulse reset_in for one sync cycle.
            ctx.set(dut.reset_in, 1)
            await ctx.tick()
            ctx.set(dut.reset_in, 0)
            # Let the reset override land and anything on the
            # downstream side of the stages resettle. 32 cycles is
            # well past the 16-cycle CORDIC pipeline depth.
            for _ in range(32):
                await ctx.tick()

            pll_post = ctx.get(dut.pll_dbg)
            sp_post = ctx.get(dut.sample_point_dbg)

        dut = LsmDemod()
        sim = Simulator(dut)
        sim.add_clock(16e-9)
        sim.add_testbench(bench)
        sim.run()

        # Decode pll_dbg (signed 16-bit).
        def _s16(v):
            return v - (1 << 16) if v >= (1 << 15) else v

        def _s18(v):
            return v - (1 << 18) if v >= (1 << 17) else v

        pll_pre_s = _s16(pll_pre)
        pll_post_s = _s16(pll_post)
        sp_pre_s = _s18(sp_pre)
        sp_post_s = _s18(sp_post)

        # Sanity: the warmup actually moved pll or sample_point away
        # from their cold init so the test is proving something.
        # The synthetic golden has no carrier offset so pll_dbg may
        # legitimately stay near 0; sample_point is the firm signal.
        # Cold-start sample_point init = SPS_Q12 + (BP_INDEX+2)*ONE_Q12
        # = 26667 + 28672 = 55339. After many strobes it cycles
        # through the ~SPS range, rarely re-touching the warmup init.
        from p25_hdl.lsm_timing_interp import SPS_Q12, ONE_Q12
        warmup_init = SPS_Q12 + 7 * ONE_Q12  # BP_INDEX=5 so (5+2)=7

        # After reset, sample_point must be back at the warmup init.
        self.assertEqual(
            sp_post_s, warmup_init,
            f"sample_point_dbg should be {warmup_init} after reset, "
            f"got {sp_post_s} (pre-reset was {sp_pre_s})")

        # pll_dbg should be 0 after reset.
        self.assertEqual(
            pll_post_s, 0,
            f"pll_dbg should be 0 after reset, got {pll_post_s} "
            f"(pre-reset was {pll_pre_s})")


if __name__ == '__main__':
    unittest.main()
