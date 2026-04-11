#
# Fishball P25 -- LSM IQ DC blocker (one-pole leaky integrator form)
#
# Phase 6G.1 of the LSM HDL port. A pair of these (one for I, one
# for Q) is instantiated at the very front of `LsmDemod`, between
# the post-RRC IQ samples and `LsmTimingInterp`. The block removes
# any slow DC bias on the IQ samples before it has a chance to
# propagate through the diff demod -> rotate -> slicer chain.
#
# Why this exists
# ---------------
# On the 2026-04-09..2026-04-11 on-target Clay County tests
# (NAC 0x8A1, LSM simulcast) the LSM PLL took 2-3 minutes to
# converge after each flash. During the transient the inner/outer
# dibit ratio ran ~60/40 instead of the ideal 50/50, putting ~5
# bit errors into every 48-bit sync window and collapsing the
# sync hit rate from the steady-state ~14/sec down to ~5/sec.
# After the transient settled the radio worked fine and the rate
# climbed back to ~14/sec, but those first ~2 minutes were
# essentially dead air.
#
# Tracing it back: the slicer in `LsmDemodLoop` slices the rotated
# differential-demod output's sign bits, so it's exquisitely
# sensitive to any DC bias on its input. The bias source isn't the
# slicer or the PLL -- it's the AD9361 IQ samples themselves,
# which carry a small slow-varying DC offset that the rest of the
# Maia chain doesn't currently strip on the LSM path. Two minutes
# is roughly how long the PLL/Gardner combination takes to "absorb"
# the bias on its own; once the slicer is fed unbiased IQ from the
# start, the loop locks immediately.
#
# This block fixes it. See doc/changes/030 ("PL port roadmap")
# Candidate 1 and doc/changes/031 (this phase) for the full
# motivation, and the PLL acquisition transient feedback memory
# for the on-target observations.
#
# Topology
# --------
# Standard one-pole leaky-integrator DC blocker:
#
#     dc[n] = (1 - alpha) * x[n] + alpha * dc[n-1]      # low-pass
#     y[n]  = x[n] - dc[n]                              # high-pass out
#
# This is mathematically equivalent (up to a constant scale factor
# of `alpha`) to the canonical "differentiator + leaky integrator"
# DC blocker `y[n] = x[n] - x[n-1] + alpha*y[n-1]`. We pick this
# form because it needs only one state register (no x[n-1] delay),
# and the (1 - alpha) factor is just an arithmetic right shift
# when alpha = 1 - 2^-K.
#
# We expose `alpha` only by its shift amount K (`alpha = 1 - 2^-K`),
# never as a free fixed-point constant. This keeps the update
# strictly to shifts and adds (no multiplier, no DSP) and gives a
# very clean stability story: the pole is at exactly `1 - 2^-K`,
# the time constant is `1 / (1 - alpha) = 2^K` samples, and the
# 3 dB cutoff is `f_s * (1 - alpha) / (2 * pi) = f_s / (2*pi*2^K)`.
#
# Default parameters (K = 7) at the LSM IQ rate of 31.25 kSPS:
#     alpha            = 1 - 2^-7    = 0.9921875
#     time constant    = 2^7 samples = ~4.1 ms
#     -3 dB cutoff     = ~38.9 Hz
#     settled in 5 tau = ~20 ms (well within the first symbol or two)
#
# This is a couple of orders of magnitude below any P25 LSM signal
# energy (the 4800 sym/s LSM tones live at +/- 600 Hz from carrier,
# and there's no useful signal energy below ~500 Hz at the IQ
# input), so the block doesn't eat anything we care about.
#
# Fixed-point format
# ------------------
# Input  `x_in`  : signed 16, Q1.15 (matches the LsmTimingInterp
#                  expected interface, which is what we splice into).
# State  `acc`   : signed (16 + K + 1) = 24-bit when K=7. We hold
#                  the dc estimate as `dc << K`, i.e. with K extra
#                  fractional bits, so the right-shift update has
#                  no quantisation creep -- the smallest update
#                  `(x - dc) >> K` is well-defined down to a single
#                  ULP of `x`. The +1 sign-margin bit absorbs the
#                  worst-case `(x_ext - acc)` difference (signed 25
#                  before the shift, signed 18 after).
# Output `y_out` : signed 16, Q1.15. Saturated -- in normal
#                  operation `|dc|` is small relative to `|x|` so
#                  `y` stays well within the input range, but on a
#                  pathological input (e.g. a step from -32768 to
#                  +32767 while the accumulator is loaded the other
#                  way) the subtraction can overflow signed 16, so
#                  we saturate explicitly. Costs ~2 LUTs.
#
# Bypass
# ------
# `enable_in` is sampled combinationally each cycle. When low, the
# output is `x_in` verbatim and the accumulator continues to update
# silently. The "continue to update" choice means a disable -> enable
# round-trip doesn't cause a step on the output: by the time the
# user re-enables the block, the accumulator already reflects the
# current DC. Costs nothing extra (the mux on the output is the
# whole bypass path).
#
# Reset behaviour: `acc` resets to 0, so on cold boot the first few
# samples see no DC removal until the integrator has had a chance
# to charge. This is exactly what we want -- if we pre-loaded the
# accumulator with a guess, the first sample's `y` would have a
# guaranteed step in it.
#
# Pipeline / latency
# ------------------
# One sync cycle. `y_out`, `strobe_out`, and the accumulator all
# update in lockstep on input strobes, matching the
# `LsmDecimator2` convention -- data and strobe travel together,
# both registered, holding their value between strobes. Off-strobe
# cycles do nothing (acc holds, y_out holds, strobe_out goes back
# to 0).
#
#   cycle N:    x_in valid + strobe_in high
#   cycle N+1:  y_out + strobe_out reflect the cycle-N input,
#               acc has been updated from the cycle-N x
#
# We deliberately mirror `LsmDecimator2`'s "data registered,
# strobe registered, both in lockstep" convention so the wiring
# into `LsmDemod` stays trivial -- the existing `re_in` ->
# `LsmTimingInterp.re_in` path is just one extra hop.
#
# Resource cost (Z7020)
# ---------------------
# Per instance: ~6 LUT (one shift-and-add update on signed 24, one
# subtractor for `y`, one saturator, one bypass mux), 24 FF (the
# accumulator), 0 DSP, 0 BRAM. Two instances per LSM channel = ~12
# LUT total. Negligible.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


# Default leaky-integrator pole shift. K=7 -> alpha = 0.9921875 ->
# ~39 Hz cutoff at 31.25 kSPS -> ~4 ms time constant.
DEFAULT_ALPHA_SHIFT = 7


class LsmDcBlocker(Elaboratable):
    """One-pole leaky-integrator DC blocker for a single signed sample stream.

    See module docstring for the full design discussion. Two of
    these are instantiated at the front of `LsmDemod` (one each for
    the I and Q sample streams).

    Parameters
    ----------
    width : int
        Bit width of the input/output samples (signed). Defaults
        to 16 to match the post-RRC IQ format that
        ``LsmTimingInterp`` consumes.
    alpha_shift : int
        Pole shift `K` such that ``alpha = 1 - 2**-K``. Larger K
        gives a slower DC tracker (more stable but takes longer to
        settle). Default 7 -> ~39 Hz cutoff at 31.25 kSPS, ~4 ms
        settling. Must be >= 1.

    Inputs (sync domain):
        x_in       : signed `width`  -- input sample
        strobe_in  : Signal()        -- one cycle per new sample
        enable_in  : Signal()        -- 1 to DC-block, 0 to bypass

    Outputs (sync domain):
        y_out      : signed `width`  -- DC-blocked sample (or `x_in`
                                        if bypassed)
        strobe_out : Signal()        -- one cycle per new output,
                                        registered (follows strobe_in
                                        by one tick)

    Debug taps:
        dc_dbg     : signed `width`  -- current DC estimate (top
                                        bits of the accumulator),
                                        useful for verifying that
                                        the integrator has actually
                                        converged on real captures.
    """

    def __init__(self, *, width=16, alpha_shift=DEFAULT_ALPHA_SHIFT):
        if alpha_shift < 1:
            raise ValueError(
                f"alpha_shift must be >= 1, got {alpha_shift!r}")
        self.width = width
        self.alpha_shift = alpha_shift

        # Accumulator holds `dc << alpha_shift` so the right-shift
        # update has no quantisation creep. +1 sign-margin bit.
        self.acc_width = width + alpha_shift + 1

        # ── Inputs ──────────────────────────────────────────────
        self.x_in = Signal(signed(width))
        self.strobe_in = Signal()
        self.enable_in = Signal(init=1)

        # ── Outputs ─────────────────────────────────────────────
        self.y_out = Signal(signed(width), reset_less=True)
        self.strobe_out = Signal()

        # ── Debug ───────────────────────────────────────────────
        self.dc_dbg = Signal(signed(width))

    def elaborate(self, platform):
        m = Module()

        K = self.alpha_shift
        W = self.width
        AW = self.acc_width

        # Accumulator: dc estimate scaled up by 2^K. Resets to 0
        # on cold boot.
        acc = Signal(signed(AW))

        # Sign-extend x_in into the accumulator's Q-format by
        # left-shifting by K. `x_ext` is signed (W + K) but we
        # widen it to `AW` for the subtraction.
        x_ext = Signal(signed(AW))
        m.d.comb += x_ext.eq(self.x_in << K)

        # Combinational DC estimate from the *current* (pre-update)
        # accumulator. Truncate the accumulator back down to the
        # input format. Used for `y` and the debug tap.
        dc = Signal(signed(W))
        m.d.comb += dc.eq(acc >> K)
        m.d.comb += self.dc_dbg.eq(dc)

        # Combinational `y_wide = x_in - dc`. Worst-case `x - dc`
        # ranges roughly +/- 2*(2^(W-1)), so it needs one extra
        # sign bit before the saturator.
        y_wide = Signal(signed(W + 1))
        m.d.comb += y_wide.eq(self.x_in - dc)

        sat_lo = -(1 << (W - 1))
        sat_hi = (1 << (W - 1)) - 1
        y_sat = Signal(signed(W))
        with m.If(y_wide > sat_hi):
            m.d.comb += y_sat.eq(sat_hi)
        with m.Elif(y_wide < sat_lo):
            m.d.comb += y_sat.eq(sat_lo)
        with m.Else():
            m.d.comb += y_sat.eq(y_wide)

        # Bypass mux on the registered output's data path: when
        # disabled, latch x_in straight through. We never freeze
        # the accumulator -- it keeps tracking quietly so a
        # disable -> enable round-trip doesn't cause a step.
        y_next = Signal(signed(W))
        m.d.comb += y_next.eq(Mux(self.enable_in, y_sat, self.x_in))

        # Leaky-integrator update on input strobes:
        #     acc <= acc + ((x_ext - acc) >>> K)
        # which is equivalent to:
        #     dc[n] = (1 - alpha) * x + alpha * dc[n-1]
        # with `alpha = 1 - 2^-K`. Lockstep with y_out and
        # strobe_out -- mirrors the LsmDecimator2 convention.
        diff = Signal(signed(AW + 1))
        m.d.comb += diff.eq(x_ext - acc)

        # Default: no output strobe.
        m.d.sync += self.strobe_out.eq(0)
        with m.If(self.strobe_in):
            m.d.sync += [
                acc.eq(acc + (diff >> K)),
                self.y_out.eq(y_next),
                self.strobe_out.eq(1),
            ]

        return m
