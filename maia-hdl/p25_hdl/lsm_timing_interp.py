#
# Fishball P25 -- LSM symbol timing recovery + 4-way linear interp
#
# Phase 6E.4 of the LSM HDL port. Streaming-friendly extraction of
# the timing-recovery + linear-interp prefix of the Rust LSM demod
# loop in `p25-httpd/src/lsm/demod.rs`. AGC, differential demod,
# PLL rotation, slicer, Gardner TED, and PLL update are all
# deferred to 6E.5 / 6E.6 -- this block produces *just* the four
# fractional-sample-point IQ values per symbol decision and a
# strobe.
#
# Pipeline depth (Phase 6E.6e timing fix, doc/changes/022)
# --------------------------------------------------------
# 2 cycles from `strobe_in` (with a decision firing) to
# `decision_strobe`:
#
#   Cycle T   : strobe_in arrives, sp_dec subtraction + cur_int
#               extraction + cur_int-driven FIFO mux ALL fire
#               combinationally and the muxed FIFO entries +
#               cur_frac/mu_mid are latched into a stage 1 register
#               set. The FIFO shift and sample_point update also
#               fire on this cycle.
#   Cycle T+1 : the four lerps run combinationally from the stage
#               1 latched values (which are stable from flops, not
#               from a borrow-propagating subtractor chain), and
#               the lerp results + decision_strobe are latched
#               into the output registers.
#   Cycle T+2 : output registers and decision_strobe are visible
#               to downstream consumers.
#
# Why the 2-stage pipeline (vs the original 1-cycle form):
# ---------------------------------------------------------
# Phase 6E.6e CORDIC bake (commit 47c9cc5) exposed a 24-endpoint
# WNS = -0.444 ns intra-clock violation in this module's lerp
# datapath at 62.5 MHz (16 ns budget). The failing path was
# `sample_point_dbg_reg[12] -> SUB -> ADD -> cur_int -> DSP input
# mux -> DSP multiply -> output mux -> q_cur_out_reg`. Vivado
# fused `i_cur_3` and `i_cur_4` into a single DSP whose `a/b`
# inputs were muxed by cur_int, putting the cur_int decision
# logic INSIDE the DSP critical path -- ~17 ns from sample_point
# bit 12 to the q_cur_out flop, ~1 ns over budget.
#
# The 2-stage fix:
#   1. Pre-applies the cur_int mux on the FIFO entries in stage 1
#      (combinational), so stage 2 sees four DISTINCT lerps with
#      no shared multiplier. This naturally undoes the DSP fusion.
#   2. Latches the muxed FIFO entries + cur_frac/mu_mid into a
#      stage 1 register set, so stage 2's lerp inputs are flop
#      outputs (clean, ~0 ns clk-to-q) instead of CARRY-chain
#      outputs (~3 ns of borrow propagation).
#   3. The 2-stage pipeline drops the WNS comfortably back into
#      the green and also saves 2 DSPs (4 lerps in stage 2 vs
#      the original 6).
#
# The 1-extra-cycle latency is invisible downstream because all
# consumers gate on `decision_strobe`.
#
# Algorithm (port of `demod_lsm_with_state` lines 178-207, with the
# AGC scaling section short-circuited to gain = 1):
#
#   per input sample:
#       sample_point -= 1.0
#       if sample_point < 1.0:
#           # midpoint sample = lerp between buf[bp] and buf[bp+1]
#           i_mid = lerp(buf_i[bp],     buf_i[bp+1],     sample_point)
#           q_mid = lerp(buf_q[bp],     buf_q[bp+1],     sample_point)
#           # current symbol sample = lerp half_sps ahead of midpoint
#           ptr      = bp + sample_point + half_sps
#           offset   = floor(ptr)
#           residual = ptr - offset
#           i_cur = lerp(buf_i[offset], buf_i[offset+1], residual)
#           q_cur = lerp(buf_q[offset], buf_q[offset+1], residual)
#           emit (i_mid, q_mid, i_cur, q_cur, decision_strobe)
#           sample_point += sps
#
# Streaming reframe
# -----------------
# The Rust code walks `bp` through a static buffer and looks
# *forward* in the buffer for the half-symbol-ahead current sample.
# A streaming HDL implementation has only past samples, so we delay
# all decisions by `half_sps + 2` input samples and keep the recent
# samples in a small shift register, then index it backwards from
# the head.
#
# Concretely: a 6-deep IQ FIFO, where index 0 is the *newest*
# sample (just arrived) and index 5 is the "decision now" sample
# (matches Rust's `buf[bp]` after `bp += 1`). With `sps ≈ 6.51` and
# `half_sps ≈ 3.25`, the current-symbol lookahead lands at FIFO
# index 5 - {3 or 4} = {2 or 1}, comfortably inside the depth-6
# window.
#
# The `bp` index in the Rust loop is implicit in HDL: it's just
# "the current decision point", which is always at FIFO[5]. The
# only state we need is the FIFO contents and `sample_point`.
#
# Fixed-point format
# ------------------
# - I/Q samples: signed 16-bit, Q1.15 (matches the upstream RRC
#   output in 6E.3).
# - sample_point: signed 16-bit Q4.12. Range [-8, +8), ULP 2.4e-4.
#   Q4.12 gives 4 integer bits which is more than the [-2, +sps+2]
#   range that sample_point can ever take, plus 12 fractional bits
#   that match the precision needed for sub-sample timing
#   resolution (2.4e-4 sample is ~1.2 ns at 31.25 kSPS, far below
#   the noise floor of any P25 system).
# - Constants:
#       SPS_Q12      = round(31250 / 4800 * 4096) = 26667
#       HALF_SPS_Q12 = round(SPS_Q12 / 2)         = 13334
#       ONE_Q12      = 4096
#
# DSP cost
# --------
# 4 lerps × 1 multiply each = 4 DSP48E1 (parallel). With the 2-stage
# pipeline (Phase 6E.6e fix) the cur_int mux is pre-applied, so the
# stage 2 lerps are i_mid, q_mid, i_cur, q_cur -- four lerps total,
# four DSP48E1 blocks. (The original 1-cycle form had 6 lerps because
# i_cur_3 and i_cur_4 were both computed in parallel; pre-muxing
# saves 2 DSPs as a side effect of the timing fix.) Vivado packs
# the `(a + (b - a) * mu)` lerp form into a single DSP48E1 per
# channel using its pre-adder + multiply path.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


# Symbol rate vs sample rate constants. Hard-coded to the P25
# 31.25 kSPS / 4800 sym-per-sec ratio used by the LSM chain.
P25_LSM_SAMPLE_RATE_HZ = 31_250
P25_SYMBOL_RATE_HZ = 4_800

# Q4.12 fixed-point representation of the timing constants.
# Q4.12 puts the fractional point at bit 12.
SAMPLE_POINT_FRAC_BITS = 12
ONE_Q12 = 1 << SAMPLE_POINT_FRAC_BITS  # 4096
SPS_Q12 = round(P25_LSM_SAMPLE_RATE_HZ / P25_SYMBOL_RATE_HZ * ONE_Q12)
HALF_SPS_Q12 = round(SPS_Q12 / 2)
# Sanity: SPS_Q12 / ONE_Q12 ≈ 6.5104, HALF_SPS_Q12 / ONE_Q12 ≈ 3.2552.
assert 26000 < SPS_Q12 < 27000
assert 13000 < HALF_SPS_Q12 < 14000


class LsmTimingInterp(Elaboratable):
    """Timing recovery + 4-way fractional-sample linear interpolator.

    See module docstring for the algorithm and fixed-point rationale.

    Parameters
    ----------
    iq_width : int
        Width of input and interpolated output IQ samples (Q1.{w-1}).
        Default 16.
    fifo_depth : int
        Lookahead window depth. Must be >= ceil(half_sps) + 2 = 6 for
        the default 31.25/4800 sps. Default 8 to leave a tiny safety
        margin and round to a nicer power of 2 for index arithmetic.

    Inputs (sync domain):
        re_in, im_in : signed iq_width   IQ sample
        strobe_in    : Signal()          one cycle per new input sample

    Outputs (sync domain):
        i_mid_out, q_mid_out : signed iq_width  midpoint sample
        i_cur_out, q_cur_out : signed iq_width  current-symbol sample
        decision_strobe      : Signal()         one cycle per symbol
            decision; the four `*_out` signals are valid only on this
            cycle (registered, then held until the next decision).
    """

    def __init__(self, *, iq_width=16, fifo_depth=8):
        if fifo_depth < 6:
            raise ValueError(
                f"fifo_depth {fifo_depth} too small for "
                f"half_sps lookahead (need >= 6)")
        self.iq_width = iq_width
        self.fifo_depth = fifo_depth

        # Fixed integer position in the FIFO that corresponds to the
        # Rust loop's "current decision point" (`buf[bp]` after `bp +=
        # 1`). One-ahead lookups use BP_INDEX - 1 (the slot one
        # closer to the head), and the half-sps-ahead lookups use
        # BP_INDEX - {3 or 4}.
        self.BP_INDEX = fifo_depth - 3  # = 5 for depth 8
        # Sanity: with sps=6.51, the integer part of (sample_point +
        # half_sps) is 3 or 4, so the smallest valid index we read
        # is BP_INDEX - 4 = 1. fifo[0] is the newest sample (one
        # input ahead of bp). fifo[BP_INDEX] is bp.
        # Required: BP_INDEX - 4 >= 0  AND  BP_INDEX <= depth - 1.
        assert self.BP_INDEX - 4 >= 0
        assert self.BP_INDEX <= fifo_depth - 1

        # ── Inputs ──────────────────────────────────────────────
        self.re_in = Signal(signed(iq_width))
        self.im_in = Signal(signed(iq_width))
        self.strobe_in = Signal()

        # Timing adjustment from Gardner TED (Phase 6E.6a). When
        # `timing_adj_strobe_in` is asserted, `timing_adj_in` is
        # added to `sample_point` -- this is the feedback path from
        # the demod loop's timing-error detector. Both default to
        # 0/0 so the standalone 6E.4 tests don't need to drive
        # them, and the timing recovery just runs open-loop.
        self.timing_adj_in = Signal(signed(iq_width))
        self.timing_adj_strobe_in = Signal()

        # Phase 8A: runtime reset. One-cycle pulse clears the IQ
        # lookahead FIFO and rewinds `sample_point` to its warmup
        # init, so the timing recovery acts like cold-start on the
        # next input sample.
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.i_mid_out = Signal(signed(iq_width), reset_less=True)
        self.q_mid_out = Signal(signed(iq_width), reset_less=True)
        self.i_cur_out = Signal(signed(iq_width), reset_less=True)
        self.q_cur_out = Signal(signed(iq_width), reset_less=True)
        self.decision_strobe = Signal()

        # Useful for downstream debug / future Gardner hookup -- expose
        # the current sample_point value (Q5.12 in 18-bit signed,
        # widened from 16-bit so the warmup-offset init fits).
        self.sample_point_dbg = Signal(signed(18), reset_less=True)

    @staticmethod
    def _lerp(m, name, a, b, mu_q12, out_width):
        """Compute `a + (b - a) * mu` in Q1.{out_width-1} fixed point.

        ``a`` and ``b`` are signed (out_width)-bit Q1.(out_width-1) values.
        ``mu_q12`` is an unsigned 12-bit Q0.12 value in [0, 1).
        Returns a fresh signed (out_width)-bit Signal holding the lerp.
        """
        # diff = b - a, signed (out_width+1) to avoid overflow on
        # the worst case b=+max, a=-max.
        diff = Signal(signed(out_width + 1), name=f"{name}_diff")
        m.d.comb += diff.eq(b - a)

        # diff * mu, signed (out_width + 1 + 12). For 16-bit IQ
        # in/out: 17 + 12 = 29 bits. Vivado packs this into one
        # DSP48E1.
        prod = Signal(signed(out_width + 1 + 12), name=f"{name}_prod")
        # mu_q12 is unsigned but Amaranth needs both operands signed
        # for the mul to be signed -- zero-extend by Cat'ing a 0.
        mu_signed = Signal(signed(13), name=f"{name}_mu")
        m.d.comb += mu_signed.eq(mu_q12)
        m.d.comb += prod.eq(diff * mu_signed)

        # Rescale by 2^12 to drop the mu fractional bits, and add a.
        # The shift is arithmetic right (signed). One extra bit of
        # headroom guards the +a sum.
        scaled = Signal(signed(out_width + 2), name=f"{name}_scaled")
        m.d.comb += scaled.eq((prod >> SAMPLE_POINT_FRAC_BITS) + a)

        # Saturate scaled back to out_width signed.
        result = Signal(signed(out_width), name=f"{name}_lerp")
        max_val = (1 << (out_width - 1)) - 1
        min_val = -(1 << (out_width - 1))
        with m.If(scaled > max_val):
            m.d.comb += result.eq(max_val)
        with m.Elif(scaled < min_val):
            m.d.comb += result.eq(min_val)
        with m.Else():
            m.d.comb += result.eq(scaled)
        return result

    def elaborate(self, platform):
        m = Module()
        N = self.fifo_depth
        BP = self.BP_INDEX
        W = self.iq_width

        # ── IQ shift register / lookahead FIFO ──────────────────
        # fifo_re[0] = newest sample (one input AHEAD of bp).
        # fifo_re[BP] = bp (the "decision now" sample, matching
        #               Rust's `buf_i[bp]` after `bp += 1`).
        # fifo_re[BP - k] = k samples ahead of bp.
        fifo_re = Array([
            Signal(signed(W), reset_less=True, name=f"fifo_re_{i}")
            for i in range(N)
        ])
        fifo_im = Array([
            Signal(signed(W), reset_less=True, name=f"fifo_im_{i}")
            for i in range(N)
        ])

        # ── sample_point register (Q5.12 signed, 18-bit) ────────
        # Initialised to sps + (BP_INDEX + 2) periods so the FIRST
        # decision fires only after the lookahead FIFO has been
        # populated with enough samples to match the Rust loop's
        # pre-loaded-buffer access pattern.
        #
        # Why the offset: in Rust's `demod_lsm_with_state`, the
        # buffer is fully loaded before the loop starts, so the
        # very first iteration that satisfies `sample_point < 1.0`
        # (which happens at `bp = ceil(sps) = 7`) accesses
        # `buf[bp]` and `buf[bp+1]` -- samples that have been "seen"
        # for several iterations already.
        #
        # In streaming HDL the FIFO is filled incrementally as
        # `strobe_in` arrives. For pre-shift `fifo[BP_INDEX]` to
        # equal `in[bp_first]` at the first decision, we need at
        # least `bp_first + BP_INDEX + 2` strobes to have been
        # received (the +2 covers the pre-shift offset). Bumping
        # the initial sample_point by `(BP_INDEX + 2) * ONE_Q12`
        # delays the first decision by exactly that many strobes
        # without changing the steady-state cadence -- the post-
        # decision `sample_point += SPS_Q12` add-back uses the
        # natural sps period, so subsequent decisions fire at the
        # same ~6-7 strobe interval as Rust.
        #
        # Width: 18 bits (Q5.12) instead of the 16 bits that would
        # be sufficient for the steady-state range [-2, sps+2].
        # The init value `SPS_Q12 + 7*ONE_Q12 = 55339` exceeds the
        # signed-16 max (32767), which would silently wrap to a
        # large negative number and fire a decision on the very
        # first strobe with garbage FIFO content. With 18 bits the
        # range is +/-32 in Q4.12 -- comfortable headroom for the
        # warmup init AND for any future increase to BP_INDEX.
        warmup_offset = (self.BP_INDEX + 2) * ONE_Q12
        sample_point_init = SPS_Q12 + warmup_offset
        sample_point = Signal(signed(18), init=sample_point_init)
        m.d.comb += self.sample_point_dbg.eq(sample_point)

        # ── Stage 1 latch register set ──────────────────────────
        # Latched on the cycle that a decision is detected; used
        # by stage 2 (next cycle) as the lerp inputs. See module
        # docstring for the rationale.
        s1_active = Signal(reset_less=True)
        s1_mu_mid = Signal(unsigned(SAMPLE_POINT_FRAC_BITS),
                           reset_less=True, name="s1_mu_mid")
        s1_cur_frac = Signal(unsigned(SAMPLE_POINT_FRAC_BITS),
                             reset_less=True, name="s1_cur_frac")
        # FIFO snapshots for the four stage-2 lerps. The cur_int
        # mux is pre-applied in stage 1 (combinationally on
        # fifo_re/fifo_im), so stage 2 sees four DISTINCT inputs
        # rather than a cur_int-controlled DSP-input mux.
        s1_a_mid_re = Signal(signed(W), reset_less=True,
                             name="s1_a_mid_re")
        s1_b_mid_re = Signal(signed(W), reset_less=True,
                             name="s1_b_mid_re")
        s1_a_mid_im = Signal(signed(W), reset_less=True,
                             name="s1_a_mid_im")
        s1_b_mid_im = Signal(signed(W), reset_less=True,
                             name="s1_b_mid_im")
        s1_a_cur_re = Signal(signed(W), reset_less=True,
                             name="s1_a_cur_re")
        s1_b_cur_re = Signal(signed(W), reset_less=True,
                             name="s1_b_cur_re")
        s1_a_cur_im = Signal(signed(W), reset_less=True,
                             name="s1_a_cur_im")
        s1_b_cur_im = Signal(signed(W), reset_less=True,
                             name="s1_b_cur_im")

        # Default: no strobes fire this cycle.
        m.d.sync += self.decision_strobe.eq(0)
        m.d.sync += s1_active.eq(0)

        with m.If(self.strobe_in):
            # ── Shift the lookahead FIFO ────────────────────────
            # Newest sample lands at index 0; everything else
            # moves up by one (slot 1 <- slot 0, slot 2 <- slot 1,
            # ...). The shift takes effect on the next clock
            # edge, so the FIFO reads inside this `with` block
            # see the PRE-shift values -- which is exactly what
            # the lerp wants (matches Rust's `buf[bp]`).
            for i in range(N - 1, 0, -1):
                m.d.sync += [
                    fifo_re[i].eq(fifo_re[i - 1]),
                    fifo_im[i].eq(fifo_im[i - 1]),
                ]
            m.d.sync += [
                fifo_re[0].eq(self.re_in),
                fifo_im[0].eq(self.im_in),
            ]

            # ── Decrement sample_point ──────────────────────────
            # The "next" sample_point value -- compute it once and
            # use both for the threshold comparison and as the
            # base for the post-decision sps add-back. 18-bit
            # signed to match the widened sample_point register.
            sp_dec = Signal(signed(18))
            m.d.comb += sp_dec.eq(sample_point - ONE_Q12)

            with m.If(sp_dec >= ONE_Q12):
                # No symbol decision yet -- just commit the new
                # sample_point and wait for the next input.
                m.d.sync += sample_point.eq(sp_dec)
            with m.Else():
                # Symbol decision fires this cycle. Stage 1 will
                # latch the FIFO snapshot + lerp control signals;
                # stage 2 (next cycle) will run the lerps.
                #
                # NOTE: the FIFO contents we snapshot are the
                # *pre-shift* values, because the m.d.sync shift
                # above hasn't taken effect yet (synchronous
                # assignments are applied at the next clock edge).
                # The new sample we just received is *not* in the
                # FIFO yet -- but it doesn't need to be. The Rust
                # loop's `buf[bp]` corresponds to fifo[BP] of the
                # pre-shift FIFO, and `buf[bp+1]` is fifo[BP - 1]
                # of the pre-shift FIFO. The current-input sample
                # stays out of the lerp entirely until it has been
                # shifted in.
                #
                # Build mu_mid = sp_dec[0:12] (the fractional
                # part). sp_dec is in [0, 1) at this branch so its
                # integer part is 0 and the unsigned low 12 bits
                # are exactly the lerp coefficient.
                mu_mid = Signal(unsigned(SAMPLE_POINT_FRAC_BITS),
                                name="mu_mid")
                m.d.comb += mu_mid.eq(sp_dec[:SAMPLE_POINT_FRAC_BITS])

                # ── Current-symbol sample lookup ────────────────
                # ptr   = sp_dec + HALF_SPS_Q12  (still Q4.12)
                # int   = ptr >> 12  ∈ {3, 4}
                # frac  = ptr[:12]               (lerp residual)
                ptr = Signal(signed(16), name="cur_ptr")
                m.d.comb += ptr.eq(sp_dec + HALF_SPS_Q12)
                cur_int = Signal(unsigned(4), name="cur_int")
                cur_frac = Signal(unsigned(SAMPLE_POINT_FRAC_BITS),
                                  name="cur_frac")
                m.d.comb += [
                    cur_int.eq(ptr[SAMPLE_POINT_FRAC_BITS:
                                   SAMPLE_POINT_FRAC_BITS + 4]),
                    cur_frac.eq(ptr[:SAMPLE_POINT_FRAC_BITS]),
                ]

                # Pre-apply the cur_int mux on the FIFO entries.
                # This combinational mux is the only place
                # cur_int (and therefore sample_point[12]) flows
                # in the timing-critical direction; latching the
                # muxed result into stage 1 keeps the path short
                # (FIFO flop -> mux LUT -> stage 1 flop) and
                # leaves stage 2 reading from clean flop outputs.
                a_cur_re = Signal(signed(W), name="a_cur_re_mux")
                b_cur_re = Signal(signed(W), name="b_cur_re_mux")
                a_cur_im = Signal(signed(W), name="a_cur_im_mux")
                b_cur_im = Signal(signed(W), name="b_cur_im_mux")
                with m.If(cur_int == 3):
                    m.d.comb += [
                        a_cur_re.eq(fifo_re[BP - 3]),
                        b_cur_re.eq(fifo_re[BP - 4]),
                        a_cur_im.eq(fifo_im[BP - 3]),
                        b_cur_im.eq(fifo_im[BP - 4]),
                    ]
                with m.Else():
                    # cur_int == 4 (the only other possible value
                    # given ptr's range [HALF_SPS_Q12, ONE_Q12 +
                    # HALF_SPS_Q12) = [13334, 17430)).
                    m.d.comb += [
                        a_cur_re.eq(fifo_re[BP - 4]),
                        b_cur_re.eq(fifo_re[BP - 5]),
                        a_cur_im.eq(fifo_im[BP - 4]),
                        b_cur_im.eq(fifo_im[BP - 5]),
                    ]

                # ── Latch stage 1 + advance sample_point ────────
                m.d.sync += [
                    s1_active.eq(1),
                    s1_mu_mid.eq(mu_mid),
                    s1_cur_frac.eq(cur_frac),
                    s1_a_mid_re.eq(fifo_re[BP]),
                    s1_b_mid_re.eq(fifo_re[BP - 1]),
                    s1_a_mid_im.eq(fifo_im[BP]),
                    s1_b_mid_im.eq(fifo_im[BP - 1]),
                    s1_a_cur_re.eq(a_cur_re),
                    s1_b_cur_re.eq(b_cur_re),
                    s1_a_cur_im.eq(a_cur_im),
                    s1_b_cur_im.eq(b_cur_im),
                    # Schedule the next decision: add SPS_Q12 to
                    # the post-decrement value.
                    sample_point.eq(sp_dec + SPS_Q12),
                ]

        # ── Stage 2: lerps from stage 1 snapshot ────────────────
        # Fires on the cycle after a decision was latched. The
        # four lerps run combinationally from stage 1 flop outputs
        # (clean inputs, no borrow-propagating subtractor chain
        # or DSP-input mux), and the lerp results + decision_strobe
        # are latched into the output registers.
        with m.If(s1_active):
            i_mid = self._lerp(
                m, "i_mid", s1_a_mid_re, s1_b_mid_re, s1_mu_mid, W)
            q_mid = self._lerp(
                m, "q_mid", s1_a_mid_im, s1_b_mid_im, s1_mu_mid, W)
            i_cur = self._lerp(
                m, "i_cur", s1_a_cur_re, s1_b_cur_re, s1_cur_frac, W)
            q_cur = self._lerp(
                m, "q_cur", s1_a_cur_im, s1_b_cur_im, s1_cur_frac, W)

            m.d.sync += [
                self.i_mid_out.eq(i_mid),
                self.q_mid_out.eq(q_mid),
                self.i_cur_out.eq(i_cur),
                self.q_cur_out.eq(q_cur),
                self.decision_strobe.eq(1),
            ]

        # ── Gardner TED feedback (Phase 6E.6a) ──────────────────
        # When the loop's timing-error detector asserts
        # `timing_adj_strobe_in`, add `timing_adj_in` to
        # sample_point. This is wired *outside* the strobe_in
        # handler so the feedback can land independently of the
        # input sample stream -- Gardner fires its strobe a few
        # cycles after each symbol decision, which is between
        # input samples in the steady state.
        #
        # Race-with-strobe-in note: if both `strobe_in` and
        # `timing_adj_strobe_in` happen on the same cycle, the
        # m.d.sync writes from the strobe_in branch take precedence
        # (later assignment wins inside the same `with` block, and
        # the timing_adj path is in a separate `with` so the
        # `Elif` chain above already covered it). To make the
        # ordering deterministic, the timing_adj branch is gated
        # off when strobe_in is also active -- the next idle cycle
        # will pick it up. The two strobes are at 31.25 kSPS and
        # 4800 sym/s respectively, so the collision rate is
        # vanishingly low and a 1-cycle delay there is harmless.
        with m.If(self.timing_adj_strobe_in & ~self.strobe_in):
            m.d.sync += sample_point.eq(sample_point + self.timing_adj_in)

        # ── Phase 8A runtime reset override ─────────────────────
        # Rewind the timing-recovery state so the next cold-start
        # acquires from scratch on the new carrier. Clears the IQ
        # lookahead FIFO, the stage-1 latches, and rewinds
        # `sample_point` to its warmup init. Last-assignment-wins in
        # `m.d.sync` makes this an override of any update fired by
        # the strobe_in / timing_adj_strobe_in branches above.
        with m.If(self.reset_in):
            m.d.sync += [
                sample_point.eq(sample_point_init),
                self.decision_strobe.eq(0),
                s1_active.eq(0),
                s1_mu_mid.eq(0),
                s1_cur_frac.eq(0),
                s1_a_mid_re.eq(0),
                s1_b_mid_re.eq(0),
                s1_a_mid_im.eq(0),
                s1_b_mid_im.eq(0),
                s1_a_cur_re.eq(0),
                s1_b_cur_re.eq(0),
                s1_a_cur_im.eq(0),
                s1_b_cur_im.eq(0),
                self.i_mid_out.eq(0),
                self.q_mid_out.eq(0),
                self.i_cur_out.eq(0),
                self.q_cur_out.eq(0),
            ]
            for i in range(N):
                m.d.sync += [
                    fifo_re[i].eq(0),
                    fifo_im[i].eq(0),
                ]

        return m
