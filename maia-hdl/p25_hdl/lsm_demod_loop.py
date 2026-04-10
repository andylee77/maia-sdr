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
# What 6E.6d does NOT do
# ----------------------
# - **No AGC.** The Rust loop's AGC scales the lerped IQ by 1/|z|
#   before the diff demod. Skipping AGC means the diff demod
#   output magnitude tracks the input magnitude (rather than
#   normalising to ~1), and the PLL update's small-angle
#   linearisation has a slightly variable effective loop gain.
#   For the synthetic test fixture (|z| = 1) this is irrelevant;
#   for real-world signals it's a robustness gap that can be
#   closed in a follow-up sub-phase by adding a magnitude
#   normaliser between LsmTimingInterp and LsmDiffDemodSlicer.
#
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
from .lsm_diff_demod_slicer import LsmDiffDemodSlicer
from .lsm_pll_rotate import LsmPllRotate
from .lsm_gardner_ted import LsmGardnerTed
from .lsm_pll_update import LsmPllUpdate


class LsmDemodLoop(Elaboratable):
    """Closed-loop LSM demod from post-RRC IQ to dibits.

    Inputs (sync domain):
        re_in, im_in : signed 16  Q1.15 IQ at 31.25 kSPS (post-decimator,
            post-LPF, post-RRC -- see Phase 6E.1/6E.2/6E.3)
        strobe_in    : Signal()  one cycle per new IQ sample

    Outputs (sync domain):
        dibit_out     : Signal(2)
        symbol_strobe : Signal()

    Debug taps (sync domain):
        pll_dbg          : signed 16  Q2.13 (current PLL value)
        sample_point_dbg : signed 16  Q4.12 (current sample_point)
        i_sym_rot_dbg, q_sym_rot_dbg : signed 18  Q3.15 rotated current
            symbol diff demod -- the value the slicer + PLL update
            actually see.
    """

    def __init__(self):
        # ── Inputs ──────────────────────────────────────────────
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.dibit_out = Signal(2, reset_less=True)
        self.symbol_strobe = Signal()

        # ── Debug taps ──────────────────────────────────────────
        self.pll_dbg = Signal(signed(16), reset_less=True)
        self.sample_point_dbg = Signal(signed(18), reset_less=True)
        self.i_sym_rot_dbg = Signal(signed(18), reset_less=True)
        self.q_sym_rot_dbg = Signal(signed(18), reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # ── Submodules ──────────────────────────────────────────
        m.submodules.timing = timing = LsmTimingInterp()
        m.submodules.diff_demod = diff_demod = LsmDiffDemodSlicer()
        m.submodules.rotate_mid = rotate_mid = LsmPllRotate()
        m.submodules.rotate_sym = rotate_sym = LsmPllRotate()
        m.submodules.gardner = gardner = LsmGardnerTed()
        m.submodules.pll_update = pll_update = LsmPllUpdate()

        # ── Stage 1: timing recovery + lerp ─────────────────────
        m.d.comb += [
            timing.re_in.eq(self.re_in),
            timing.im_in.eq(self.im_in),
            timing.strobe_in.eq(self.strobe_in),
        ]

        # ── Stage 2: differential demod (per-symbol) ────────────
        m.d.comb += [
            diff_demod.i_mid_in.eq(timing.i_mid_out),
            diff_demod.q_mid_in.eq(timing.q_mid_out),
            diff_demod.i_cur_in.eq(timing.i_cur_out),
            diff_demod.q_cur_in.eq(timing.q_cur_out),
            diff_demod.decision_strobe.eq(timing.decision_strobe),
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

        return m
