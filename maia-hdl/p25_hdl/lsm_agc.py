#
# Fishball P25 -- LSM per-symbol AGC
#
# Streaming fixed-point port of the AGC in SDRTrunk's
# `P25P1DemodulatorLSM.java` lines 157-172. Runs at the symbol rate
# (`LsmTimingInterp.decision_strobe` = 4800 Hz), on the four
# timing-interpolated samples produced by LsmTimingInterp
# (`i_mid_out`, `q_mid_out`, `i_cur_out`, `q_cur_out`). Inserted
# between LsmTimingInterp and LsmDiffDemodSlicer inside
# LsmDemodLoop so the slicer + PLL update see amplitude-normalised
# IQ regardless of upstream gain drift.
#
# Why we need this
# ----------------
# The slicer in `LsmDiffDemodSlicer` hard-slices the sign bits of
# the rotated per-symbol I/Q values. Any DC / gain drift that pushes
# the constellation off-centre or scales it down to the noise floor
# corrupts the dibit decisions before the PLL has a chance to
# settle. Phase 8's runtime reset fixed the carrier transient; this
# module fixes the amplitude transient. Between them, a retune
# followed by a clean first-symbol decision is now possible.
#
# SDRTrunk reference algorithm (float)
# ------------------------------------
# Lines 157-172 of P25P1DemodulatorLSM.java, reproduced verbatim:
#
#     magnitude = sqrt(iCurrent^2 + qCurrent^2)
#     if (magnitude > 0 && !isInfinite(magnitude)) {
#         requiredGain = constrain(OBJECTIVE_MAGNITUDE / magnitude, 500);
#         sampleGain += (requiredGain - sampleGain) * 0.05f;
#         sampleGain = min(sampleGain, requiredGain);
#         sampleGain = min(sampleGain, 500);
#     }
#     iMiddle *= sampleGain;  qMiddle *= sampleGain;
#     iCurrent *= sampleGain; qCurrent *= sampleGain;
#
# where `OBJECTIVE_MAGNITUDE = 1.0f`.
#
# Fixed-point translation — NO APPROXIMATIONS
# -------------------------------------------
# We match every constant in SDRTrunk's code to the precision of
# the chosen Q-format:
#
#   * `OBJECTIVE_MAGNITUDE = 1.0` -> `TARGET_RAW = 1 << SAMPLE_FRAC`
#       (= 32768 for Q1.15). No approximation.
#   * `500` cap -> `GAIN_MAX = 500 * 2**GAIN_FRAC` (= 1,024,000 for
#       Q9.11). Exact.
#   * `0.05f` IIR lerp coefficient -> multiplicand
#       `ALPHA_Q20 = round(0.05 * 2**20) = 52429`.
#     Step is computed as `(diff * ALPHA_Q20) >> 20` which matches
#     0.05 to ~5 parts per million. The alternative shift-only
#     `diff >> 4` is 1/16 = 0.0625 (+25 % relative error) — we
#     explicitly reject that shortcut because the lerp time constant
#     sets the AGC's settling behaviour on-target and the user
#     requested SDRTrunk-faithful dynamics.
#   * `magnitude > 0` skip condition -> `mag_reg != 0`. No arbitrary
#       threshold.
#   * `!Float.isInfinite(magnitude)` -> trivially satisfied in
#       fixed-point: the squared-sum has a finite 33-bit range.
#
# Q-formats
# ---------
# Input samples (i_*_in, q_*_in): signed 16-bit Q1.15 (matches
# LsmTimingInterp output).
#
# Gain register: unsigned 20-bit Q9.11. 11 fractional bits give an
# ULP of ~5e-4, well below SDRTrunk's float working precision for
# gains in the 1..500 range. 9 integer bits cover the 500 cap.
# Init: `GAIN_INIT = 1 << GAIN_FRAC` (= 2048 = 1.0).
#
# Magnitude path:
#     sq = i_cur*i_cur + q_cur*q_cur             Q2.30 signed 33-bit
#     mag = isqrt(sq)                            Q1.15 unsigned 17-bit
#
# Division:
#     req_gain_Q11 = TARGET_NUMERATOR / mag_Q15
#     TARGET_NUMERATOR = TARGET_RAW << GAIN_FRAC
#                      = 2**SAMPLE_FRAC * 2**GAIN_FRAC
#                      = 2**(SAMPLE_FRAC + GAIN_FRAC)
#                      = 2**26 = 67_108_864
#     Derivation: `req_gain_float = target_float / mag_float`, and
#     `req_gain_Q11 = req_gain_float * 2**GAIN_FRAC`. Substituting
#     raw fixed-point (`target_float = TARGET_RAW / 2**SAMPLE_FRAC`,
#     `mag_float = mag_raw / 2**SAMPLE_FRAC`) yields
#     `req_gain_Q11 = TARGET_RAW * 2**GAIN_FRAC / mag_raw`.
#
# IIR lerp (asymmetric):
#     diff = req_clamped - gain                  signed 21-bit
#     step = (diff * ALPHA_Q20) >> 20            signed 18-bit
#     new_gain = gain + step
#     new_gain = min(new_gain, req_clamped)      asymmetric cap
#     new_gain = min(new_gain, GAIN_MAX)
#     new_gain = max(new_gain, GAIN_MIN)
#
# Apply gain to the four interpolated samples:
#     i_mid_out = saturate((i_mid_in * gain) >> 11, 16 bits)
#     q_mid_out = ...
#     i_cur_out = ...
#     q_cur_out = ...
#
# State machine
# -------------
#     IDLE       -> on decision_strobe_in: latch i_mid/q_mid/i_cur/q_cur,
#                   compute sq combinationally (i_cur*i_cur +
#                   q_cur*q_cur), latch sq, go to SQRT_INIT (or
#                   BYPASS if enable_in=0).
#     BYPASS     -> emit inputs verbatim, strobe, return to IDLE.
#     SQRT_INIT  -> seed sqrt state, go to SQRT_ITER.
#     SQRT_ITER  -> 17 non-restoring sqrt iterations (one per cycle).
#     DIV_INIT   -> latch magnitude. If mag == 0, skip to APPLY
#                   without touching gain (SDRTrunk's `if (magnitude
#                   > 0)` branch). Otherwise seed the divider, go to
#                   DIV_ITER.
#     DIV_ITER   -> 26 restoring division iterations.
#     UPDATE     -> clamp req_gain to GAIN_MAX, exact 0.05 lerp,
#                   asymmetric min clamps, latch gain.
#     APPLY      -> compute 4 output samples via gain multiply,
#                   saturate, latch, emit decision_strobe_out,
#                   return to IDLE.
#
# Every state includes an explicit `with m.If(self.reset_in)` check
# that forces `m.next = "IDLE"` and clears the persistent gain
# register back to GAIN_INIT. This is the only way to cancel an
# in-flight sqrt/divide from outside the FSM context. The checks
# are written as overrides on top of each state's normal body,
# relying on Amaranth's last-assignment-wins semantics in
# `m.d.sync` and FSM next-state arbitration.
#
# Per-symbol latency from decision_strobe_in to decision_strobe_out:
#     1 (IDLE->SQRT_INIT)
#   + 1 (SQRT_INIT)
#   + 17 (SQRT_ITER)
#   + 1 (DIV_INIT)
#   + 26 (DIV_ITER)
#   + 1 (UPDATE)
#   + 1 (APPLY)
#   = 48 sync cycles.
# At 62.5 MHz sync and 4800 symbols/s the symbol period is ~13000
# cycles, so the 48-cycle latency is invisible to the symbol budget.
#
# Resource estimate (Z7020)
# -------------------------
# 2 DSP48E1 for sq = i*i + q*q (parallel, single cycle)
# 1 DSP48E1 for diff * ALPHA_Q20 in UPDATE
# 4 DSP48E1 for the final gain multiplies in APPLY (parallel)
# 0 DSP for sqrt (shift / compare / subtract)
# 0 DSP for div (shift / compare / subtract)
# Total per LSM chain: 7 DSP48. Both chains: 14 DSP48. Well under
# the available Z7020 budget.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


# ── Fixed-point constants ─────────────────────────────────────────

SAMPLE_WIDTH = 16
SAMPLE_FRAC = 15

# Gain register format: Q9.11, unsigned 20 bits.
GAIN_WIDTH = 20
GAIN_FRAC = 11

# gain = 1.0
GAIN_INIT = 1 << GAIN_FRAC                       # 2048

# SDRTrunk's `500` cap on sampleGain.
GAIN_MAX_FLOAT = 500.0
GAIN_MAX = int(GAIN_MAX_FLOAT * (1 << GAIN_FRAC))  # 1_024_000

# Floor the gain at 1 ULP so a saturated input that pulls
# required_gain to zero doesn't lock the AGC at zero. This is an
# HDL-specific guard; SDRTrunk's float AGC can't hit zero because
# of the `> 0` magnitude skip, but the HDL divider can emit 0 if
# the numerator is fully consumed.
GAIN_MIN = 1

# SDRTrunk's OBJECTIVE_MAGNITUDE = 1.0f in Q1.15.
TARGET_RAW = 1 << SAMPLE_FRAC                     # 32768

# Divider numerator: TARGET_RAW * 2**GAIN_FRAC. See docstring.
TARGET_NUMERATOR = TARGET_RAW << GAIN_FRAC        # 2**26

# Exact 0.05 in Q20.
ALPHA_FLOAT = 0.05
ALPHA_SHIFT = 20
ALPHA_Q = int(round(ALPHA_FLOAT * (1 << ALPHA_SHIFT)))  # 52429
# Sanity check: ALPHA_Q / 2**20 should be within 5 ppm of 0.05.
assert abs(ALPHA_Q / (1 << ALPHA_SHIFT) - ALPHA_FLOAT) < 1e-5

# Squared-sum (Q2.30): 16-bit signed * 16-bit signed = 32-bit,
# sum of two products = 33-bit signed (in practice always >= 0).
SQ_WIDTH = 2 * SAMPLE_WIDTH + 1                   # 33

# Integer sqrt: 2 bits of input consumed per iteration, 17 iters
# for 33-bit input. Output is Q1.15 unsigned, 17 bits.
SQRT_ITERS = (SQ_WIDTH + 1) // 2                  # 17
MAG_WIDTH = SQRT_ITERS                            # 17

# Restoring divider.
#
# Numerator width = 28 bits so TARGET_NUMERATOR (= 2^26) sits with
# two zero-pad bits above it, which lets the restoring algorithm
# consume the whole padded value cleanly starting from bit 27.
#
# DIV_ITERS = 28 so every numerator bit produces one quotient bit.
# For the worst-case `mag_reg == 1` the correct quotient is 2^26,
# which requires bit 26 of the quotient register -- impossible with
# a shorter iteration count. The UPDATE clamp downstream catches
# any overshoot above GAIN_MAX (~2^20) and saturates.
DIV_NUM_WIDTH = 28                                # holds TARGET_NUMERATOR
DIV_ITERS = 28


class LsmAgc(Elaboratable):
    """Per-symbol automatic gain control for the LSM demod loop.

    Runs in lockstep with `LsmTimingInterp.decision_strobe`,
    producing amplitude-normalised ``i_*_out``/``q_*_out`` in the
    same Q1.15 format as the inputs. See module-level docstring for
    the full algorithm, fixed-point format, and the SDRTrunk
    reference.

    Inputs (sync domain):
        i_mid_in, q_mid_in  : signed 16   Q1.15
        i_cur_in, q_cur_in  : signed 16   Q1.15
        decision_strobe_in  : Signal()    one cycle per symbol
        enable_in           : Signal(init=1)
            high = AGC runs; low = pass-through.
        reset_in            : Signal()
            one-cycle pulse: `gain <- GAIN_INIT`, FSM -> IDLE.

    Outputs (sync domain, registered):
        i_mid_out, q_mid_out : signed 16  Q1.15
        i_cur_out, q_cur_out : signed 16  Q1.15
        decision_strobe_out  : Signal()   one cycle per completed
            symbol (~48 sync cycles after decision_strobe_in)

    Debug taps:
        gain_dbg  : unsigned 16  Q9.7 truncation of gain register
            (bit 4 .. bit 19 of the Q9.11 raw gain). Range 0..500.
        mag_dbg   : unsigned 16  most recent L2 magnitude in Q1.15
            (top 16 of the 17-bit sqrt output)
    """

    def __init__(self):
        # ── Inputs ──────────────────────────────────────────────
        self.i_mid_in = Signal(signed(SAMPLE_WIDTH))
        self.q_mid_in = Signal(signed(SAMPLE_WIDTH))
        self.i_cur_in = Signal(signed(SAMPLE_WIDTH))
        self.q_cur_in = Signal(signed(SAMPLE_WIDTH))
        self.decision_strobe_in = Signal()
        self.enable_in = Signal(init=1)
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.i_mid_out = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        self.q_mid_out = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        self.i_cur_out = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        self.q_cur_out = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        self.decision_strobe_out = Signal()

        # ── Debug taps ──────────────────────────────────────────
        self.gain_dbg = Signal(16, reset_less=True)
        self.mag_dbg = Signal(16, reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # ── Latched operand registers ───────────────────────────
        i_mid_q = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        q_mid_q = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        i_cur_q = Signal(signed(SAMPLE_WIDTH), reset_less=True)
        q_cur_q = Signal(signed(SAMPLE_WIDTH), reset_less=True)

        # Squared-sum register (Q2.30, 33-bit signed).
        sq_reg = Signal(signed(SQ_WIDTH), reset_less=True)

        # ── Persistent gain register (Q9.11, init 1.0) ──────────
        gain = Signal(GAIN_WIDTH, init=GAIN_INIT, reset_less=True)

        # ── Integer sqrt state ──────────────────────────────────
        # 2-bit-at-a-time non-restoring sqrt. Per-iteration:
        #   rem = (rem << 2) | top2(shift_reg)
        #   test = (root << 2) | 1
        #   if rem >= test:
        #       rem -= test
        #       root = (root << 1) | 1
        #   else:
        #       root = root << 1
        sqrt_rem = Signal(SQ_WIDTH + 2, reset_less=True)
        sqrt_root = Signal(MAG_WIDTH + 1, reset_less=True)
        sqrt_shift_reg = Signal(SQ_WIDTH + 2, reset_less=True)
        sqrt_counter = Signal(range(SQRT_ITERS + 1), reset_less=True)

        # Final magnitude register (Q1.15 unsigned, 17-bit).
        mag_reg = Signal(MAG_WIDTH, reset_less=True)

        # ── Restoring divider state ─────────────────────────────
        div_rem = Signal(DIV_NUM_WIDTH + 1, reset_less=True)
        div_num = Signal(DIV_NUM_WIDTH, reset_less=True)
        div_quot = Signal(DIV_ITERS, reset_less=True)
        div_counter = Signal(range(DIV_ITERS + 1), reset_less=True)

        # ── Main FSM ────────────────────────────────────────────
        m.d.sync += self.decision_strobe_out.eq(0)

        with m.FSM(init="IDLE"):

            with m.State("IDLE"):
                with m.If(self.reset_in):
                    m.next = "IDLE"
                with m.Elif(self.decision_strobe_in):
                    m.d.sync += [
                        i_mid_q.eq(self.i_mid_in),
                        q_mid_q.eq(self.q_mid_in),
                        i_cur_q.eq(self.i_cur_in),
                        q_cur_q.eq(self.q_cur_in),
                    ]
                    # sq = i_cur^2 + q_cur^2, combinational from the
                    # *incoming* (unregistered) values so it lands
                    # in sq_reg at the same edge as the operand
                    # register. Saves one state vs doing it in SQR.
                    sq = Signal(signed(SQ_WIDTH))
                    m.d.comb += sq.eq(
                        self.i_cur_in * self.i_cur_in
                        + self.q_cur_in * self.q_cur_in
                    )
                    m.d.sync += sq_reg.eq(sq)

                    with m.If(self.enable_in):
                        m.next = "SQRT_INIT"
                    with m.Else():
                        m.next = "BYPASS"

            with m.State("BYPASS"):
                # AGC disabled: emit inputs verbatim and strobe.
                m.d.sync += [
                    self.i_mid_out.eq(i_mid_q),
                    self.q_mid_out.eq(q_mid_q),
                    self.i_cur_out.eq(i_cur_q),
                    self.q_cur_out.eq(q_cur_q),
                    self.decision_strobe_out.eq(1),
                ]
                m.next = "IDLE"
                with m.If(self.reset_in):
                    m.next = "IDLE"

            with m.State("SQRT_INIT"):
                m.d.sync += [
                    sqrt_rem.eq(0),
                    sqrt_root.eq(0),
                    # Pad the 33-bit sq to an even width (34) and
                    # seat the top 2 bits at the MSB end so the
                    # 17-iteration loop consumes them first.
                    sqrt_shift_reg.eq(sq_reg.as_unsigned() << 1),
                    sqrt_counter.eq(0),
                ]
                m.next = "SQRT_ITER"
                with m.If(self.reset_in):
                    m.next = "IDLE"

            with m.State("SQRT_ITER"):
                # Digit-by-digit 2-bit-at-a-time integer sqrt.
                # Per step:
                #   rem_new = (rem << 2) | top2(shift_reg)
                #   test    = (root << 2) | 1       (= 4*root + 1)
                #   if rem_new >= test:
                #       rem  = rem_new - test
                #       root = (root << 1) | 1
                #   else:
                #       root = root << 1
                # The `4*root + 1` form is the standard digit-recurrence
                # derivation: the next root is r' = 2r + d with d in
                # {0,1}, so the new contribution to subtract is
                # (r')^2 - (2r)^2 = 4r + 1 when d=1.
                top2 = sqrt_shift_reg[-2:]
                rem_new = Cat(top2, sqrt_rem[:SQ_WIDTH]).as_unsigned()
                # test_val = (root << 2) | 1:
                #   bit 0 = 1, bit 1 = 0, bits 2.. = root[0..MAG_WIDTH-1]
                test_val = Cat(
                    Const(1, 1),
                    Const(0, 1),
                    sqrt_root[:MAG_WIDTH]).as_unsigned()
                cmp_diff = Signal(SQ_WIDTH + 3)
                m.d.comb += cmp_diff.eq(rem_new - test_val)

                with m.If(cmp_diff[-1] == 0):  # non-negative
                    m.d.sync += [
                        sqrt_rem.eq(cmp_diff),
                        sqrt_root.eq(
                            Cat(Const(1, 1), sqrt_root[:MAG_WIDTH])),
                    ]
                with m.Else():
                    m.d.sync += [
                        sqrt_rem.eq(rem_new),
                        sqrt_root.eq(
                            Cat(Const(0, 1), sqrt_root[:MAG_WIDTH])),
                    ]

                m.d.sync += [
                    sqrt_shift_reg.eq(sqrt_shift_reg << 2),
                    sqrt_counter.eq(sqrt_counter + 1),
                ]

                with m.If(sqrt_counter == SQRT_ITERS - 1):
                    m.next = "DIV_INIT"
                with m.If(self.reset_in):
                    m.next = "IDLE"

            with m.State("DIV_INIT"):
                mag_val = sqrt_root[:MAG_WIDTH]
                m.d.sync += [
                    mag_reg.eq(mag_val),
                    # Take the LOW 16 bits of the 17-bit sqrt output
                    # (not the top 16 — that would hide bit 15 of a
                    # near-unit magnitude behind a right-shift). For
                    # Q1.15 inputs the sqrt range is [0, sqrt(2)]
                    # which maxes at ~46340 raw, fitting cleanly in
                    # unsigned 16 bits. `mag_dbg` is therefore a
                    # faithful Q1.15-ish view of L2 magnitude.
                    self.mag_dbg.eq(mag_val[:16]),
                ]
                # SDRTrunk: `if (magnitude > 0 && !isInfinite(...))`.
                # `!isInfinite` is trivially true in fixed-point.
                # `> 0` maps to `mag_val != 0`.
                with m.If(mag_val == 0):
                    m.next = "APPLY"
                with m.Else():
                    m.d.sync += [
                        div_num.eq(
                            Const(TARGET_NUMERATOR, DIV_NUM_WIDTH)),
                        div_rem.eq(0),
                        div_quot.eq(0),
                        div_counter.eq(0),
                    ]
                    m.next = "DIV_ITER"
                with m.If(self.reset_in):
                    m.next = "IDLE"

            with m.State("DIV_ITER"):
                top_num_bit = div_num[-1]
                rem_shifted = Cat(
                    top_num_bit, div_rem[:DIV_NUM_WIDTH]).as_unsigned()
                rem_diff = Signal(DIV_NUM_WIDTH + 2)
                m.d.comb += rem_diff.eq(rem_shifted - mag_reg)

                with m.If(rem_diff[-1] == 0):
                    m.d.sync += [
                        div_rem.eq(rem_diff),
                        div_quot.eq(
                            Cat(Const(1, 1), div_quot[:DIV_ITERS - 1])),
                    ]
                with m.Else():
                    m.d.sync += [
                        div_rem.eq(rem_shifted),
                        div_quot.eq(
                            Cat(Const(0, 1), div_quot[:DIV_ITERS - 1])),
                    ]

                m.d.sync += [
                    div_num.eq(div_num << 1),
                    div_counter.eq(div_counter + 1),
                ]

                with m.If(div_counter == DIV_ITERS - 1):
                    m.next = "UPDATE"
                with m.If(self.reset_in):
                    m.next = "IDLE"

            with m.State("UPDATE"):
                # Clamp req_gain to GAIN_MAX (= SDRTrunk's 500).
                raw_req = div_quot.as_unsigned()
                req_clamped = Signal(GAIN_WIDTH)
                with m.If(raw_req > GAIN_MAX):
                    m.d.comb += req_clamped.eq(GAIN_MAX)
                with m.Else():
                    m.d.comb += req_clamped.eq(raw_req)

                # Exact 0.05 lerp: step = (req - gain) * ALPHA_Q / 2^20.
                diff = Signal(signed(GAIN_WIDTH + 1))
                m.d.comb += diff.eq(
                    req_clamped.as_signed() - gain.as_signed())

                # 21-bit signed * 17-bit signed = 38-bit signed.
                # ALPHA_Q is a 17-bit positive constant; treat as
                # signed(18) so Amaranth doesn't widen the diff side.
                alpha_const = Const(ALPHA_Q, signed(18))
                step_wide = Signal(signed(GAIN_WIDTH + 1 + 18))
                m.d.comb += step_wide.eq(diff * alpha_const)
                step = Signal(signed(GAIN_WIDTH + 1))
                m.d.comb += step.eq(step_wide >> ALPHA_SHIFT)

                lerp_val = Signal(signed(GAIN_WIDTH + 2))
                m.d.comb += lerp_val.eq(gain.as_signed() + step)

                # Asymmetric clamps.
                gain_next = Signal(GAIN_WIDTH)
                clamp_asymm = Signal(signed(GAIN_WIDTH + 2))
                with m.If(lerp_val > req_clamped.as_signed()):
                    m.d.comb += clamp_asymm.eq(req_clamped.as_signed())
                with m.Else():
                    m.d.comb += clamp_asymm.eq(lerp_val)

                with m.If(clamp_asymm > GAIN_MAX):
                    m.d.comb += gain_next.eq(GAIN_MAX)
                with m.Elif(clamp_asymm < GAIN_MIN):
                    m.d.comb += gain_next.eq(GAIN_MIN)
                with m.Else():
                    m.d.comb += gain_next.eq(clamp_asymm)

                m.d.sync += [
                    gain.eq(gain_next),
                    # Expose Q9.7 truncation (= gain / 16) so the
                    # register-bank debug field fits in 16 bits and
                    # still covers 0..500.
                    self.gain_dbg.eq(gain_next[GAIN_FRAC - 7:GAIN_FRAC + 9]),
                ]
                m.next = "APPLY"
                with m.If(self.reset_in):
                    m.next = "IDLE"

            with m.State("APPLY"):
                # Apply `gain` (Q9.11) to the four latched samples
                # (Q1.15). Product is Q10.26; shift right by 11 to
                # Q10.15, saturate to Q1.15 (signed 16).
                gain_s = gain.as_signed()

                def scale_one(x):
                    prod = Signal(
                        signed(SAMPLE_WIDTH + GAIN_WIDTH + 1))
                    m.d.comb += prod.eq(x * gain_s)
                    scaled = prod >> GAIN_FRAC

                    sat = Signal(signed(SAMPLE_WIDTH))
                    sat_hi = (1 << (SAMPLE_WIDTH - 1)) - 1
                    sat_lo = -(1 << (SAMPLE_WIDTH - 1))
                    with m.If(scaled > sat_hi):
                        m.d.comb += sat.eq(sat_hi)
                    with m.Elif(scaled < sat_lo):
                        m.d.comb += sat.eq(sat_lo)
                    with m.Else():
                        m.d.comb += sat.eq(scaled)
                    return sat

                m.d.sync += [
                    self.i_mid_out.eq(scale_one(i_mid_q)),
                    self.q_mid_out.eq(scale_one(q_mid_q)),
                    self.i_cur_out.eq(scale_one(i_cur_q)),
                    self.q_cur_out.eq(scale_one(q_cur_q)),
                    self.decision_strobe_out.eq(1),
                ]
                m.next = "IDLE"
                with m.If(self.reset_in):
                    m.next = "IDLE"

        # ── Phase 8A runtime reset persistent-state override ────
        # The FSM `next`-state arbitration above handles the
        # control flow side of reset. This block handles the data
        # side: clears the persistent gain register back to
        # GAIN_INIT and the debug taps to zero on a single reset
        # pulse. Last-assignment-wins in `m.d.sync` makes this an
        # override of any normal-path update that fired in the
        # same cycle.
        with m.If(self.reset_in):
            m.d.sync += [
                gain.eq(GAIN_INIT),
                self.decision_strobe_out.eq(0),
                self.gain_dbg.eq(0),
                self.mag_dbg.eq(0),
            ]

        return m
