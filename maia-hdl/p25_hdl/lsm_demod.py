#
# Fishball P25 -- LSM demod top-level (front end + sync + BCH)
#
# Phase 6E.8 of the LSM HDL port, extended in Phase 6G.1 with a
# pair of front-end DC blockers (one each for I and Q).
#
# The complete LSM demod chain from post-RRC IQ to recovered
# (NAC, DUID) NID events. Wires together three submodules:
#
#     LsmDcBlocker x2       -- per-channel one-pole leaky
#                              integrator DC blockers, runtime
#                              bypassable via dc_block_enable.
#                              See doc/changes/031 + the
#                              lsm_dc_blocker.py docstring for
#                              the motivation (the 2-3 minute
#                              PLL acquisition transient on
#                              cold boot).               [6G.1]
#
#     LsmDemodLoop          -- closed-loop demod (front end +
#                              timing recovery + diff demod +
#                              PLL rotation + slicer)  [6E.6d]
#
#     LsmNidPipeline        -- 48-bit hard sync detector +
#                              status-skip NID extractor +
#                              ML BCH(63,16,11) decoder, with
#                              the start/done handshake and
#                              the NID-drop counter           [6E.8]
#
# Pipeline (one-line)
# -------------------
#
#   IQ -> [LsmDcBlocker x2] -> [LsmDemodLoop] -> dibits -> [LsmNidPipeline]
#                                                                 |
#                                                                 v
#                                                         NID events + dibit
#                                                         pass-through to
#                                                         dibit DMA
#
# This top-level is the HDL counterpart of the Rust
# `LsmPipeline::process_iq()` flow in
# `p25-httpd/src/lsm/demod.rs`. After this sub-phase the only
# HDL work remaining for Phase 6E is wiring `LsmDemod` into
# `p25_top.py` (6E.9) and the Vivado bake (6E.10).
#
# Why a separate `LsmNidPipeline` submodule
# -----------------------------------------
# Splitting the dibit-to-NID half off into its own Elaboratable
# lets us integration-test the sync->BCH wiring (the only
# non-trivial control logic in this top-level) by driving
# constructed dibits directly, **without** dragging the IQ-to-
# dibit demod loop and a synthetic IQ golden into the test
# bench. The demod loop is already exhaustively tested by
# `test_lsm_demod_loop` against the synthetic golden; the
# integration-level question for this sub-phase is "does the
# new sync->BCH chain do the right thing on a hand-built dibit
# stream", and `LsmNidPipeline` makes that question testable.
#
# I/O
# ---
# Inputs (sync domain):
#     re_in, im_in : signed 16   Q1.15 IQ at 31.25 kSPS
#                                (post-decimator, post-LPF,
#                                post-RRC -- same as LsmDemodLoop)
#     strobe_in    : Signal()    one cycle per new IQ sample
#     dc_block_enable : Signal() 1 = run the front-end DC blockers
#                                (default), 0 = bypass them and
#                                feed raw IQ straight to the
#                                demod loop. Lets us A/B the
#                                blocker on hardware. Phase 6G.1.
#
# Outputs (sync domain):
#     dibit_out, symbol_strobe : pass-through from LsmDemodLoop
#                                so the existing dibit DMA can
#                                still consume the LSM dibits
#                                without any plumbing changes
#
#     nid_event_strobe : Signal()      -- one cycle per NID
#     nac_out          : Signal(12)
#     duid_out         : Signal(4)
#     n_errors_out     : Signal(7)     -- 0..63, BCH Hamming dist
#     valid_out        : Signal()      -- 1 if n_errors_out <= 11
#     sync_distance_out: Signal(7)     -- the SYNC hit Hamming dist
#                                         (0..47, separate from
#                                         the BCH n_errors)
#     in_nid_window    : Signal()      -- high while collecting
#                                         the 33-dibit NID payload
#                                         (useful for the
#                                         dashboard's "have lock"
#                                         indicator)
#     bch_busy         : Signal()      -- BCH decoder is sweeping
#     nid_drop_count   : Signal(16)    -- saturating counter of
#                                         NIDs dropped because BCH
#                                         was busy. **Should
#                                         always be 0 in normal
#                                         operation.**
#
#     pll_dbg          : signed 16     -- pass-through from loop
#     sample_point_dbg : signed 18     -- pass-through from loop
#
# Resource estimate (Z7020)
# -------------------------
# Sum of submodule estimates:
#     LsmDcBlocker x2   ~12 LUT total, 0 BRAM, 0 DSP   [6G.1]
#     LsmDemodLoop      ~30 DSP48, 2 BRAM18 (PLL rotate LUT x 2)
#     LsmNidPipeline    ~390 LUT, 0 BRAM, 0 DSP
#                       (sync_nid + bch + drop counter)
#     glue              ~10 LUT
#
#     Total per LSM channel: ~30 DSP48 (14% of Z7020), 2 BRAM18
#     (1.4%), <520 LUT (~1%).
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .lsm_dc_blocker import LsmDcBlocker
from .lsm_demod_loop import LsmDemodLoop
from .lsm_nid_pipeline import LsmNidPipeline


class LsmDemod(Elaboratable):
    """LSM demod top-level: post-RRC IQ -> dibits + NID events.

    See module-level docstring for the architectural overview,
    handshake conventions, and resource estimate.
    """

    def __init__(self):
        # ── Inputs ──────────────────────────────────────────────
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()
        # 6G.1: front-end DC blocker enable. Defaults high so a
        # bitstream that doesn't drive this input still gets the
        # blocker (the right behaviour for fishball7020_p25).
        self.dc_block_enable = Signal(init=1)
        # Phase 8A: runtime reset. A 1-cycle pulse propagates into
        # the closed-loop demod and the NID pipeline, clearing the
        # PLL accumulator, timing state, diff-slicer prev history,
        # sync register, BCH sweep state, and the event latches.
        # DC blocker state is deliberately NOT touched -- it holds
        # a slow-varying ADC offset that does not change between
        # retunes and re-converges on its own in ~20 ms.
        self.reset_in = Signal()
        # Phase 10-prep: per-symbol AGC enable. Passed through to
        # the LsmDemodLoop submodule. Default high so a bitstream
        # that doesn't drive this input still runs the AGC (the
        # right behaviour for fishball7020_p25).
        self.agc_enable = Signal(init=1)

        # ── Pass-through dibit stream ───────────────────────────
        self.dibit_out = Signal(2)
        self.symbol_strobe = Signal()

        # ── NID event outputs ───────────────────────────────────
        self.nid_event_strobe = Signal()
        self.nac_out = Signal(12)
        self.duid_out = Signal(4)
        self.n_errors_out = Signal(7)
        self.valid_out = Signal()
        self.sync_distance_out = Signal(7)
        self.in_nid_window = Signal()
        self.bch_busy = Signal()
        self.nid_drop_count = Signal(16)

        # ── Debug taps from LsmDemodLoop ────────────────────────
        self.pll_dbg = Signal(signed(16))
        self.sample_point_dbg = Signal(signed(18))
        # Phase 10-prep: AGC debug taps from LsmDemodLoop.agc.
        # gain_dbg is the Q9.7 truncation of the Q9.11 gain
        # register (range 0..500); mag_dbg is the most-recent
        # L2 magnitude in Q1.15 (truncated to 16 bits).
        self.agc_gain_dbg = Signal(16)
        self.agc_mag_dbg = Signal(16)

    def elaborate(self, platform):
        m = Module()

        # 6G.1: front-end DC blockers, one per channel. Each one
        # registers its data + strobe in lockstep, so the demod
        # loop sees IQ that's delayed by exactly one cycle vs the
        # raw input -- invisible to LsmTimingInterp because it
        # samples on its own input strobe.
        m.submodules.dc_block_re = dc_block_re = LsmDcBlocker()
        m.submodules.dc_block_im = dc_block_im = LsmDcBlocker()
        m.submodules.demod_loop = demod_loop = LsmDemodLoop()
        m.submodules.nid_pipeline = nid_pipeline = LsmNidPipeline()

        # Phase 8A runtime reset fan-out: into the closed-loop
        # demod and the NID pipeline. DC blockers are intentionally
        # excluded (see __init__ docstring).
        m.d.comb += [
            demod_loop.reset_in.eq(self.reset_in),
            nid_pipeline.reset_in.eq(self.reset_in),
        ]

        # Phase 10-prep: AGC enable + debug tap pass-through.
        m.d.comb += [
            demod_loop.agc_enable.eq(self.agc_enable),
            self.agc_gain_dbg.eq(demod_loop.agc_gain_dbg),
            self.agc_mag_dbg.eq(demod_loop.agc_mag_dbg),
        ]

        # ── Stage 0: per-channel DC blocking ───────────────────
        # Both blockers run off the same enable + the same strobe
        # so I/Q stay phase-aligned. Bypass via dc_block_enable=0.
        m.d.comb += [
            dc_block_re.x_in.eq(self.re_in),
            dc_block_re.strobe_in.eq(self.strobe_in),
            dc_block_re.enable_in.eq(self.dc_block_enable),

            dc_block_im.x_in.eq(self.im_in),
            dc_block_im.strobe_in.eq(self.strobe_in),
            dc_block_im.enable_in.eq(self.dc_block_enable),
        ]

        # ── Stage 1: IQ -> dibits ──────────────────────────────
        m.d.comb += [
            demod_loop.re_in.eq(dc_block_re.y_out),
            demod_loop.im_in.eq(dc_block_im.y_out),
            # Both blockers strobe together; pick either one.
            demod_loop.strobe_in.eq(dc_block_re.strobe_out),
        ]
        # Pass dibit stream straight through to the existing
        # dibit DMA path.
        m.d.comb += [
            self.dibit_out.eq(demod_loop.dibit_out),
            self.symbol_strobe.eq(demod_loop.symbol_strobe),
            self.pll_dbg.eq(demod_loop.pll_dbg),
            self.sample_point_dbg.eq(demod_loop.sample_point_dbg),
        ]

        # ── Stage 2: dibits -> sync + NID + BCH ────────────────
        m.d.comb += [
            nid_pipeline.dibit_in.eq(demod_loop.dibit_out),
            nid_pipeline.dibit_strobe.eq(demod_loop.symbol_strobe),
            self.nid_event_strobe.eq(nid_pipeline.nid_event_strobe),
            self.nac_out.eq(nid_pipeline.nac_out),
            self.duid_out.eq(nid_pipeline.duid_out),
            self.n_errors_out.eq(nid_pipeline.n_errors_out),
            self.valid_out.eq(nid_pipeline.valid_out),
            self.sync_distance_out.eq(nid_pipeline.sync_distance_out),
            self.in_nid_window.eq(nid_pipeline.in_nid_window),
            self.bch_busy.eq(nid_pipeline.bch_busy),
            self.nid_drop_count.eq(nid_pipeline.nid_drop_count),
        ]

        return m
