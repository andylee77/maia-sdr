#
# Fishball P25 -- LSM PLL phase rotation
#
# Phase 6E.6c of the LSM HDL port. Streaming Amaranth port of the
# PLL rotation block in `p25-httpd/src/lsm/demod.rs` lines 240-249:
#
#     pll_i = pll.cos()
#     pll_q = pll.sin()
#     tmp = (i_demod * pll_i) - (q_demod * pll_q)
#     q_demod = (q_demod * pll_i) + (i_demod * pll_q)
#     i_demod = tmp
#
# This rotates the differentially-demodulated `(i, q)` symbol vector
# by the PLL angle so the slicer sees the carrier-corrected
# constellation. In hardware we drive both the midpoint and the
# current-symbol diff demod outputs through this same block.
#
# Implementation
# --------------
# 1024-entry sin/cos LUT covering pll values in [-2, +2] rad. Step
# is 3.9 mrad (~0.22 deg) -- the worst-case sin/cos quantisation
# error is ~0.004, or about 130 Q15 ULPs. Not exact, but well
# inside the LSM loop's tolerance budget: the PLL is clamped to
# +/- pi/3, the loop gain is 0.1, and any small bias from sin/cos
# rounding is absorbed by the integrator over a few symbols.
#
# Why not linear interpolation: it would push the precision down to
# sub-ULP at a cost of one extra multiply + one extra adder per
# rotation. We're not bottlenecked on accuracy here, so the simple
# direct-lookup form is fine. If the PLL ever ends up needing
# tighter precision (e.g. for off-air signals with severe LO drift)
# the LUT can be regenerated bigger or interpolation added later
# without changing the surrounding pipeline.
#
# LUT layout
# ----------
# Memory shape: 32 bits per entry, 1024 entries deep.
#   bits [15:0]  = cos(angle), Q1.15 signed
#   bits [31:16] = sin(angle), Q1.15 signed
# Address: bits [9:0] of `(pll + 16384) >> 5`. 16384 is the
#   centre-of-range offset (pll==0 -> index 512), 32 is the per-
#   entry step in Q2.13 units. The 1024-entry LUT covers pll values
#   in [-16384, 16384) Q2.13 = [-2, 2) rad. The PLL update block
#   clamps pll to +/- pi/3 ~= +/-1.047 rad, so the actual indices
#   used are roughly 244..780 -- about half the LUT is dead, but
#   the wider-than-needed range gives us room for any future
#   pll-clamp changes without needing to reload the LUT.
#
# Pipeline
# --------
# 3 sync cycles from `strobe_in` to `strobe_out`:
#   stage 1: latch (i, q, address)
#   stage 2: LUT data is registered out of the synchronous-read
#           port; latch into cos/sin signals
#   stage 3: 4 multiplies + 2 sums, shift right by 15 to rescale
#           the Q*.30 product back to Q*.15, latch outputs
#
# DSP48E1 cost
# ------------
# 4 DSPs per rotation. The LSM demod loop applies this twice (once
# to the midpoint diff demod and once to the current-symbol diff
# demod), so 8 DSPs total in the integrated demod loop. Or, if we
# share one block over two cycles, 4 DSPs and ~6 cycles of latency.
# For 6E.6c we keep it simple at 4 DSPs (one rotation per block).
# The integrating top-level in 6E.6d will instantiate two blocks.
#
# SPDX-License-Identifier: MIT
#

import math

from amaranth import *
from amaranth.lib.memory import Memory


# LUT geometry
LUT_DEPTH = 1024
LUT_ADDR_BITS = 10              # log2(LUT_DEPTH)
LUT_INDEX_OFFSET = LUT_DEPTH // 2  # so pll == 0 lands at index 512
LUT_INPUT_FRAC_BITS = 13        # PLL is Q2.13
LUT_STEP_BITS = 5               # 32 Q2.13 units per LUT step
# A bit of math: with 32-unit step in Q2.13, the LUT covers
# (LUT_DEPTH * 32) Q13 units = 32768 Q13 units = 4 rad. Centred
# at zero -> [-2, 2) rad.
assert LUT_DEPTH * (1 << LUT_STEP_BITS) == 1 << (LUT_ADDR_BITS + LUT_STEP_BITS)


def _build_sin_cos_lut():
    """Pre-compute the 1024-entry packed sin/cos LUT.

    Entry k contains `(sin << 16) | cos` of the angle
    `(k - 512) * (32 / 2^13)` rad. Both sin and cos are
    quantised to Q1.15 signed and packed as unsigned 16-bit
    halves of a 32-bit word.
    """
    out = []
    for k in range(LUT_DEPTH):
        angle = (k - LUT_INDEX_OFFSET) * (
            (1 << LUT_STEP_BITS) / (1 << LUT_INPUT_FRAC_BITS)
        )
        c = int(round(math.cos(angle) * (1 << 15)))
        s = int(round(math.sin(angle) * (1 << 15)))
        c = max(-32768, min(32767, c))
        s = max(-32768, min(32767, s))
        c_u = c & 0xFFFF
        s_u = s & 0xFFFF
        out.append((s_u << 16) | c_u)
    return out


_SIN_COS_LUT = _build_sin_cos_lut()
assert len(_SIN_COS_LUT) == LUT_DEPTH


class LsmPllRotate(Elaboratable):
    """Single complex rotation by an angle from the PLL register.

    Parameters
    ----------
    iq_width : int
        Width of input/output IQ samples. Default 18 (Q3.15,
        matches LsmDiffDemodSlicer outputs).
    pll_width : int
        Width of pll input. Default 16 (Q2.13, matches
        LsmPllUpdate output).

    Inputs (sync domain):
        i_in, q_in : signed iq_width
        pll_in     : signed pll_width  Q2.13
        strobe_in  : Signal()  one cycle when (i_in, q_in) is valid

    Outputs (sync domain):
        i_out, q_out : signed iq_width  Q3.15  rotated samples
        strobe_out   : Signal()  one cycle, 3 sync clocks after
            strobe_in, when (i_out, q_out) is valid
    """

    def __init__(self, *, iq_width=18, pll_width=16):
        self.iw = iq_width
        self.pw = pll_width

        self.i_in = Signal(signed(iq_width))
        self.q_in = Signal(signed(iq_width))
        self.pll_in = Signal(signed(pll_width))
        self.strobe_in = Signal()

        self.i_out = Signal(signed(iq_width), reset_less=True)
        self.q_out = Signal(signed(iq_width), reset_less=True)
        self.strobe_out = Signal()

    def elaborate(self, platform):
        m = Module()

        # ── LUT ─────────────────────────────────────────────────
        # 1024 x 32-bit LUT, synchronous-read port (Memory.read_port
        # registers data with 1 cycle latency).
        m.submodules.lut = lut = Memory(
            shape=32, depth=LUT_DEPTH, init=_SIN_COS_LUT)
        rdport = lut.read_port()

        # ── Stage 1: compute LUT address from pll, latch (i, q) ──
        #
        # addr = ((pll + 16384) >> 5) & 0x3FF
        # Equivalently: take bits [14:5] of (pll + 16384). The +&
        # masking handles values past the LUT range by wrapping --
        # the PLL clamp at +/- pi/3 keeps us inside the LUT
        # comfortably, but the wrap is the safe behaviour for any
        # out-of-range stray.
        addr_offset = (1 << (LUT_INPUT_FRAC_BITS + 1))  # 16384
        pll_plus_offset = Signal(signed(self.pw + 1))
        m.d.comb += pll_plus_offset.eq(self.pll_in + addr_offset)
        addr = pll_plus_offset[LUT_STEP_BITS:LUT_STEP_BITS + LUT_ADDR_BITS]

        i_q1 = Signal(signed(self.iw), reset_less=True)
        q_q1 = Signal(signed(self.iw), reset_less=True)
        stage1_strobe = Signal()

        m.d.comb += rdport.addr.eq(addr)

        with m.If(self.strobe_in):
            m.d.sync += [
                i_q1.eq(self.i_in),
                q_q1.eq(self.q_in),
                stage1_strobe.eq(1),
            ]
        with m.Else():
            m.d.sync += stage1_strobe.eq(0)

        # ── Stage 2: LUT data is now available (1-cycle latency).
        # Unpack into signed cos/sin and latch (i, q) one more
        # cycle to align with the LUT data.
        cos_sig = Signal(signed(16), reset_less=True)
        sin_sig = Signal(signed(16), reset_less=True)
        i_q2 = Signal(signed(self.iw), reset_less=True)
        q_q2 = Signal(signed(self.iw), reset_less=True)
        stage2_strobe = Signal()

        # rdport.data is unsigned 32-bit; reinterpret each half as
        # signed 16-bit by passing through a sign-extension Signal.
        cos_unsigned = rdport.data[:16]
        sin_unsigned = rdport.data[16:32]
        cos_signed = Signal(signed(16))
        sin_signed = Signal(signed(16))
        m.d.comb += [
            cos_signed.eq(cos_unsigned.as_signed()),
            sin_signed.eq(sin_unsigned.as_signed()),
        ]

        with m.If(stage1_strobe):
            m.d.sync += [
                cos_sig.eq(cos_signed),
                sin_sig.eq(sin_signed),
                i_q2.eq(i_q1),
                q_q2.eq(q_q1),
                stage2_strobe.eq(1),
            ]
        with m.Else():
            m.d.sync += stage2_strobe.eq(0)

        # ── Stage 3: complex rotation, shift back to Q3.15 ──────
        #
        #   i_out = i*cos - q*sin
        #   q_out = i*sin + q*cos
        #
        # Each multiply: signed(iw) * signed(16) -> signed(iw + 16)
        # Sum: one extra bit of headroom -> signed(iw + 17)
        # Shift right by 15 to rescale (cos/sin are Q1.15, the
        # input is Q3.15, the product is Q4.30, we want Q3.15
        # back -> shift right by 15).
        prod_w = self.iw + 16
        sum_w = prod_w + 1

        i_rot_full = Signal(signed(sum_w), reset_less=True)
        q_rot_full = Signal(signed(sum_w), reset_less=True)
        stage3_strobe = Signal()

        with m.If(stage2_strobe):
            m.d.sync += [
                i_rot_full.eq(i_q2 * cos_sig - q_q2 * sin_sig),
                q_rot_full.eq(i_q2 * sin_sig + q_q2 * cos_sig),
                stage3_strobe.eq(1),
            ]
        with m.Else():
            m.d.sync += stage3_strobe.eq(0)

        # Round-to-nearest shift right by 15.
        round_bias = Const(1 << 14, signed(sum_w))
        i_rot_scaled = (i_rot_full + round_bias) >> 15
        q_rot_scaled = (q_rot_full + round_bias) >> 15

        # Saturate to iw bits.
        out_max = (1 << (self.iw - 1)) - 1
        out_min = -(1 << (self.iw - 1))

        m.d.sync += self.strobe_out.eq(0)
        with m.If(stage3_strobe):
            with m.If(i_rot_scaled > out_max):
                m.d.sync += self.i_out.eq(out_max)
            with m.Elif(i_rot_scaled < out_min):
                m.d.sync += self.i_out.eq(out_min)
            with m.Else():
                m.d.sync += self.i_out.eq(i_rot_scaled)
            with m.If(q_rot_scaled > out_max):
                m.d.sync += self.q_out.eq(out_max)
            with m.Elif(q_rot_scaled < out_min):
                m.d.sync += self.q_out.eq(out_min)
            with m.Else():
                m.d.sync += self.q_out.eq(q_rot_scaled)
            m.d.sync += self.strobe_out.eq(1)

        return m
