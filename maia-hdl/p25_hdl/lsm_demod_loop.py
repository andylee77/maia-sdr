#
# Fishball P25 -- LSM demod loop top-level integration
#
# Phase 6E.6d of the LSM HDL port. Wires together the front-end
# blocks built in 6E.4..6E.6c into a closed-loop demod that
# matches the Rust LSM pipeline structure (minus AGC, which is
# deferred -- see the change doc for the rationale and the
# expected accuracy impact).
#
# Pipeline
# --------
#
#   re_in/im_in --> LsmTimingInterp --> diff demod --> 2x rotate --> slice
#       (31.25 kSPS)        |               |              |          |
#                           |               |              |          v
#                           |               |              |       dibit_out
#                           |               |              |
#                           |               +--> Gardner TED --> +-->|
#                           |                                        |
#                           +<-- timing_adj_in <-----------------+----+
#                                                                |
#                                              PLL update <------+
#                                                  ^
#                                                  pll feedback
#                                                       |
#                                              fed back to both rotates
#
# Block dependencies (`m.submodules`):
#
#   timing       : LsmTimingInterp     6E.4 -- 4-way lerp
#   diff_demod   : LsmDiffDemodSlicer  6E.5 -- per-symbol diff demod
#                                              (slicer output is
#                                              ignored; we slice
#                                              the *rotated* values
#                                              inside this module)
#   rotate_mid   : LsmPllRotate        6E.6c -- midpoint rotation
#   rotate_sym   : LsmPllRotate        6E.6c -- current-symbol rotation
#   gardner      : LsmGardnerTed       6E.6a -- timing error
#   pll_update   : LsmPllUpdate        6E.6b -- PLL accumulator
#
# Why two `LsmPllRotate` instances rather than time-multiplexed
# -----------------------------------------------------------
# A single rotate block could service both the midpoint and the
# current-symbol stream sequentially -- the LUT lookup is the
# bottleneck and would only be used twice per symbol. Two
# instances trade ~32 Kbit of duplicated LUT BRAM (one extra
# BRAM18) for a much simpler dataflow with no time-multiplex
# scheduler. Resource budget has plenty of room (~10 BRAM18 spare
# even after 6E.7 BCH FEC), so the simpler form wins.
#
# Phase 10-prep: AGC added
# ------------------------
# An `LsmAgc` submodule was added between LsmTimingInterp and
# LsmDiffDemodSlicer (doc/changes/040_phase10_lsm_agc.md). It is a
# direct fixed-point port of SDRTrunk's per-symbol AGC at
# `P25P1DemodulatorLSM.java:157-172` — L2 magnitude via integer
# sqrt, `required_gain = OBJECTIVE_MAGNITUDE / magnitude`, exact
# 0.05 IIR lerp, asymmetric min clamps, applied to all four
# interpolated samples before the diff demod. See `lsm_agc.py` for
# the full algorithm and Q-format derivation. The original Phase
# 6E.6d "no AGC" note below is obsolete.
#
# What 6E.6d did NOT do (historical — now addressed above)
# --------------------------------------------------------
# - **No `pre_curr` differential demod re-routing.** The Rust loop
#   has a subtle: it computes diff demod against the
#   *unrotated* prev sample, then rotates. The HDL does the same
#   because LsmDiffDemodSlicer's `prev_*` registers are updated
#   with the *unrotated* current sample (matching the Rust). The
#   PLL rotate happens after the diff demod, on the demod
#   output -- so the rotation step is correct.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .lsm_timing_interp import LsmTimingInterp
from .lsm_agc import LsmAgc, MAG_UPDATE_THRESHOLD_DEFAULT
from .lsm_diff_demod_slicer import LsmDiffDemodSlicer
from .lsm_pll_rotate import LsmPllRotate
from .lsm_gardner_ted import LsmGardnerTed
from .lsm_pll_update import LsmPllUpdate, LsmPllUpdateLinearised


class LsmDemodLoop(Elaboratable):
    """Closed-loop LSM demod from post-RRC IQ to dibits.

    Parameters
    ----------
    pll_mode : str
        Which PLL update implementation to instantiate. Default
        ``'cordic'`` selects the production
        :class:`LsmPllUpdate` (10-iteration CORDIC vectoring +
        true atan2 phase-error computation, Phase 6E.6e). The
        legacy small-angle linearised form (Phase 6E.6b) can be
        selected with ``'linearised'`` for the slip-resistance
        regression test in ``test_lsm_demod_loop.py``.

    Inputs (sync domain):
        re_in, im_in : signed 16  Q1.15 IQ at 31.25 kSPS (post-decimator,
            post-LPF, post-RRC -- see Phase 6E.1/6E.2/6E.3)
        strobe_in    : Signal()  one cycle per new IQ sample

    Outputs (sync domain):
        dibit_out     : Signal(2)
        symbol_strobe : Signal()

    Post-PLL IQ tap (sync domain) — Phase 10.7, feeds the dashboard
    Plots tab (eye + constellation + deviation) via `post_pll_iq_dma`.
    Two samples per symbol at 9.6 kSPS (mid + sym emitted on
    adjacent sync cycles). Truncated from the underlying 18-bit
    Q3.15 `LsmPllRotate` outputs to signed 16 Q1.13 (arithmetic
    `>> 2`) so the packer can reuse the existing 16-bit `iq_dma`
    packing format. Q1.13 fits the nominal ±1.4 post-AGC
    constellation comfortably.
        i_rot_out, q_rot_out : signed 16
        rot_strobe_out       : Signal() -- one cycle per rotated
            sample; fires twice per symbol (first mid, then sym).

    Debug taps (sync domain):
        pll_dbg          : signed 16  Q2.13 (current PLL value)
        sample_point_dbg : signed 16  Q4.12 (current sample_point)
        i_sym_rot_dbg, q_sym_rot_dbg : signed 18  Q3.15 rotated current
            symbol diff demod -- the value the slicer + PLL update
            actually see.
    """

    def __init__(self, *, pll_mode='cordic',
                 agc_mag_update_threshold=MAG_UPDATE_THRESHOLD_DEFAULT):
        if pll_mode not in ('cordic', 'linearised'):
            raise ValueError(
                f"pll_mode must be 'cordic' or 'linearised', "
                f"got {pll_mode!r}")
        self.pll_mode = pll_mode
        # Forwarded to LsmAgc to set the idle-noise gate threshold.
        # See lsm_agc.MAG_UPDATE_THRESHOLD_DEFAULT docstring.
        self.agc_mag_update_threshold = agc_mag_update_threshold

        # ── Inputs ──────────────────────────────────────────────
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()
        # Phase 8A: runtime reset. A 1-cycle pulse is propagated
        # to every stateful submodule in the closed-loop demod
        # chain (timing, agc, diff_demod, pll_update).
        self.reset_in = Signal()
        # Phase 10-prep: per-symbol AGC enable. High (default) runs
        # the SDRTrunk-faithful amplitude normaliser on the four
        # timing-interpolated samples before the diff demod. Low
        # bypasses the AGC (pass-through; gain register holds).
        self.agc_enable = Signal(init=1)

        # ── Outputs ─────────────────────────────────────────────
        self.dibit_out = Signal(2, reset_less=True)
        self.symbol_strobe = Signal()

        # ── Post-PLL IQ tap (Phase 10.7) ────────────────────────
        # See class docstring; feeds post_pll_iq_dma.
        self.i_rot_out = Signal(signed(16), reset_less=True)
        self.q_rot_out = Signal(signed(16), reset_less=True)
        self.rot_strobe_out = Signal()

        # ── Debug taps ──────────────────────────────────────────
        self.pll_dbg = Signal(signed(16), reset_less=True)
        self.sample_point_dbg = Signal(signed(18), reset_less=True)
        self.i_sym_rot_dbg = Signal(signed(18), reset_less=True)
        self.q_sym_rot_dbg = Signal(signed(18), reset_less=True)
        # Phase 10-prep: AGC debug taps exposed upstream for the
        # `lsm_agc_debug` register. See lsm_agc.py.
        self.agc_gain_dbg = Signal(16, reset_less=True)
        self.agc_mag_dbg = Signal(16, reset_less=True)
        # Phase 10-prep: count of symbols the AGC gated because
        # `mag < mag_update_threshold` (noise-floor squelch). Exposed
        # so the PS can confirm the idle-gate is firing and tune
        # the threshold if needed.
        self.agc_gate_dbg = Signal(16, reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # ── Submodules ──────────────────────────────────────────
        m.submodules.timing = timing = LsmTimingInterp()
        m.submodules.agc = agc = LsmAgc(
            mag_update_threshold=self.agc_mag_update_threshold)
        m.submodules.diff_demod = diff_demod = LsmDiffDemodSlicer()
        m.submodules.rotate_mid = rotate_mid = LsmPllRotate()
        m.submodules.rotate_sym = rotate_sym = LsmPllRotate()
        m.submodules.gardner = gardner = LsmGardnerTed()
        if self.pll_mode == 'cordic':
            pll_update_cls = LsmPllUpdate
        else:
            pll_update_cls = LsmPllUpdateLinearised
        m.submodules.pll_update = pll_update = pll_update_cls()

        # ── Phase 8A runtime reset fan-out ──────────────────────
        m.d.comb += [
            timing.reset_in.eq(self.reset_in),
            agc.reset_in.eq(self.reset_in),
            diff_demod.reset_in.eq(self.reset_in),
            pll_update.reset_in.eq(self.reset_in),
        ]

        # ── Stage 1: timing recovery + lerp ─────────────────────
        m.d.comb += [
            timing.re_in.eq(self.re_in),
            timing.im_in.eq(self.im_in),
            timing.strobe_in.eq(self.strobe_in),
        ]

        # ── Stage 1b: per-symbol AGC on the four lerped samples ─
        # Direct port of SDRTrunk's P25P1DemodulatorLSM.java lines
        # 157-172. Adds ~48 sync cycles of latency inside the
        # feedback loop, which is negligible vs the ~13000-cycle
        # symbol period; loop stability is unaffected.
        m.d.comb += [
            agc.i_mid_in.eq(timing.i_mid_out),
            agc.q_mid_in.eq(timing.q_mid_out),
            agc.i_cur_in.eq(timing.i_cur_out),
            agc.q_cur_in.eq(timing.q_cur_out),
            agc.decision_strobe_in.eq(timing.decision_strobe),
            agc.enable_in.eq(self.agc_enable),
            self.agc_gain_dbg.eq(agc.gain_dbg),
            self.agc_mag_dbg.eq(agc.mag_dbg),
            self.agc_gate_dbg.eq(agc.gate_dbg),
        ]

        # ── Stage 2: differential demod (per-symbol) ────────────
        m.d.comb += [
            diff_demod.i_mid_in.eq(agc.i_mid_out),
            diff_demod.q_mid_in.eq(agc.q_mid_out),
            diff_demod.i_cur_in.eq(agc.i_cur_out),
            diff_demod.q_cur_in.eq(agc.q_cur_out),
            diff_demod.decision_strobe.eq(agc.decision_strobe_out),
        ]
        # NOTE: diff_demod's own dibit_out is *not* used downstream.
        # We slice the rotated symbol value below.

        # ── Stage 3: PLL rotation of both demod outputs ─────────
        # Drive both rotates from the same pll value (held in
        # pll_update.pll_out) and from diff_demod's symbol_strobe.
        m.d.comb += [
            rotate_mid.i_in.eq(diff_demod.i_mid_demod_out),
            rotate_mid.q_in.eq(diff_demod.q_mid_demod_out),
            rotate_mid.pll_in.eq(pll_update.pll_out),
            rotate_mid.strobe_in.eq(diff_demod.symbol_strobe),

            rotate_sym.i_in.eq(diff_demod.i_sym_out),
            rotate_sym.q_in.eq(diff_demod.q_sym_out),
            rotate_sym.pll_in.eq(pll_update.pll_out),
            rotate_sym.strobe_in.eq(diff_demod.symbol_strobe),
        ]

        # ── Stage 4: slice rotated current-symbol value ─────────
        # Same dibit mapping as LsmDiffDemodSlicer:
        # dibit = Cat(i_sign, q_sign).
        rotated_dibit = Signal(2)
        m.d.comb += rotated_dibit.eq(
            Cat(rotate_sym.i_out[-1], rotate_sym.q_out[-1])
        )

        m.d.sync += self.symbol_strobe.eq(0)
        with m.If(rotate_sym.strobe_out):
            m.d.sync += [
                self.dibit_out.eq(rotated_dibit),
                self.symbol_strobe.eq(1),
            ]

        # ── Stage 5a: Gardner TED on rotated values ─────────────
        m.d.comb += [
            gardner.i_sym_in.eq(rotate_sym.i_out),
            gardner.q_sym_in.eq(rotate_sym.q_out),
            gardner.i_mid_demod_in.eq(rotate_mid.i_out),
            gardner.q_mid_demod_in.eq(rotate_mid.q_out),
            gardner.symbol_strobe.eq(rotate_sym.strobe_out),
        ]

        # Sample-point feedback to LsmTimingInterp.
        m.d.comb += [
            timing.timing_adj_in.eq(gardner.timing_adj_out),
            timing.timing_adj_strobe_in.eq(gardner.timing_adj_strobe),
        ]

        # ── Stage 5b: PLL update from rotated symbol + dibit ────
        # The PLL update reads the *current* dibit (combinational
        # from the rotated outputs) and the current rotated values.
        m.d.comb += [
            pll_update.i_sym_in.eq(rotate_sym.i_out),
            pll_update.q_sym_in.eq(rotate_sym.q_out),
            pll_update.dibit_in.eq(rotated_dibit),
            pll_update.symbol_strobe.eq(rotate_sym.strobe_out),
        ]

        # ── Debug taps ──────────────────────────────────────────
        m.d.comb += [
            self.pll_dbg.eq(pll_update.pll_out),
            self.sample_point_dbg.eq(timing.sample_point_dbg),
        ]
        with m.If(rotate_sym.strobe_out):
            m.d.sync += [
                self.i_sym_rot_dbg.eq(rotate_sym.i_out),
                self.q_sym_rot_dbg.eq(rotate_sym.q_out),
            ]

        # ── Phase 10.7: post-PLL IQ tap interleave ──────────────
        # `rotate_mid.strobe_out` and `rotate_sym.strobe_out` fire
        # on the same sync cycle (both triggered by
        # `diff_demod.symbol_strobe`, same 3-cycle LUT pipeline).
        # To expose both as a single IQ stream, emit `rotate_mid`
        # first and hold `rotate_sym` for the next cycle. The
        # downstream IQPacker sees 2 strobes/symbol → 1 packed
        # 64-bit DMA word/symbol at 4800 Hz = 38 KB/s.
        pending_sym_i = Signal(signed(16))
        pending_sym_q = Signal(signed(16))
        pending_sym_valid = Signal()

        m.d.sync += [
            self.rot_strobe_out.eq(0),
            pending_sym_valid.eq(0),
        ]
        with m.If(rotate_mid.strobe_out):
            # Arithmetic right-shift 18-bit Q3.15 → 16-bit Q1.13.
            m.d.sync += [
                self.i_rot_out.eq(rotate_mid.i_out >> 2),
                self.q_rot_out.eq(rotate_mid.q_out >> 2),
                self.rot_strobe_out.eq(1),
                pending_sym_i.eq(rotate_sym.i_out >> 2),
                pending_sym_q.eq(rotate_sym.q_out >> 2),
                pending_sym_valid.eq(1),
            ]
        with m.Elif(pending_sym_valid):
            m.d.sync += [
                self.i_rot_out.eq(pending_sym_i),
                self.q_rot_out.eq(pending_sym_q),
                self.rot_strobe_out.eq(1),
            ]

        # ── Phase 8A runtime reset override on local outputs ───
        # Clear this module's own registered outputs (the slicer
        # latch + debug taps) so the PS sees a clean view during
        # the first ~microsecond after reset.
        with m.If(self.reset_in):
            m.d.sync += [
                self.dibit_out.eq(0),
                self.symbol_strobe.eq(0),
                self.i_sym_rot_dbg.eq(0),
                self.q_sym_rot_dbg.eq(0),
                self.i_rot_out.eq(0),
                self.q_rot_out.eq(0),
                self.rot_strobe_out.eq(0),
                pending_sym_valid.eq(0),
            ]

        return m
