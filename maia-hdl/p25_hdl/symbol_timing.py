#
# Fishball P25 - Symbol Timing Recovery
#
# Gardner timing error detector + PI loop filter + interpolator.
# Operates at 10 samples/symbol (48 kSPS / 4800 sym/sec).
#
# Gardner TED: e[k] = (x[k] - x[k-1]) * x[k-1/2]
# Loop filter: PI controller (BW ~48 Hz, damping 0.707)
# Output: 2-bit dibit (4-level symbol decision) + strobe
#
# Cost: 2-3 DSP48E1
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class SymbolTimingRecovery(Elaboratable):
    """Gardner-based symbol timing recovery for P25 C4FM and LSM

    Architecture:
        - Decimating counter (integer NCO) at ~13 samp/sym
        - Gardner TED on diff_im (the FM cross-product) — works for both
          C4FM (where diff_im = instantaneous frequency) and LSM (where
          diff_im is the imaginary part of the differential vector and
          still has a zero-crossing at symbol boundaries)
        - PI loop filter adjusts the NCO step
        - Symbol slicer: sign bits of (diff_re, diff_im) -> 2-bit dibit
          This works for both C4FM and LSM/CQPSK as confirmed by the
          SDRTrunk implementation (P25P1DemodulatorLSM.toDibit). The
          differential product z[n]*conj(z[n-1]) lies in one of 4
          quadrants matching the P25 dibit set.

    Inputs (sync domain):
        diff_re_in, diff_im_in: 18-bit signed differential product
        strobe_in: sample valid strobe

    Outputs (sync domain):
        dibit_out: 2-bit symbol decision
        symbol_strobe: symbol decision strobe (4800 Hz)
    """
    # PI loop filter gains (fixed-point, 16 fractional bits)
    KP = 185
    KI = 1

    def __init__(self, samples_per_symbol=10):
        self.samples_per_symbol = samples_per_symbol

        # Inputs (differential demodulator outputs)
        self.diff_re_in = Signal(signed(18))
        self.diff_im_in = Signal(signed(18))
        self.strobe_in = Signal()

        # Legacy alias: disc_in == diff_im_in (FM cross-product)
        self.disc_in = self.diff_im_in

        # Outputs
        self.dibit_out = Signal(2)
        self.symbol_strobe = Signal()

    def elaborate(self, platform):
        m = Module()
        sps = self.samples_per_symbol

        # ── Sample counter (NCO-like decimator) ──────────────────────
        # Counter counts down from (sps-1) to 0. When it wraps -> symbol strobe.
        # Midpoint at sps//2.
        # The loop filter adjusts the reload value by ±1 to shift timing.
        counter = Signal(range(sps + 2), reset=sps - 1)
        midpoint = sps // 2

        at_symbol = Signal()   # counter reached 0 -> symbol decision point
        at_midpoint = Signal()  # counter at midpoint -> Gardner midpoint sample
        m.d.comb += [
            at_symbol.eq(counter == 0),
            at_midpoint.eq(counter == midpoint),
        ]

        # ── Sample registers for Gardner TED ─────────────────────────
        # x_curr: sample at symbol point (current)
        # x_prev: sample at previous symbol point
        # x_mid:  sample at midpoint between prev and current symbol
        # All on diff_im (the FM-style discriminator) — Gardner TED zero
        # crossings work the same way for both C4FM and LSM here.
        x_curr = Signal(signed(18), reset_less=True)
        x_prev = Signal(signed(18), reset_less=True)
        x_mid = Signal(signed(18), reset_less=True)

        # ── Gardner TED + PI loop filter ─────────────────────────────
        # Error: e = (x_curr - x_prev) * x_mid
        # This is computed when at_symbol fires.
        # The error drives a PI loop filter whose output adjusts the
        # NCO counter reload value.

        # Loop filter integrator (signed, 32-bit accumulator)
        integrator = Signal(signed(32), reset_less=True)
        # Loop filter output: proportional + integral term
        loop_out = Signal(signed(32), reset_less=True)
        # Timing adjustment: sign of loop output -> advance/retard by 1
        timing_adj = Signal(signed(2), reset_less=True)

        with m.If(self.strobe_in):
            # Capture midpoint sample (on diff_im for Gardner TED)
            with m.If(at_midpoint):
                m.d.sync += x_mid.eq(self.diff_im_in)

            # Symbol strobe: compute TED, update loop, output dibit
            with m.If(at_symbol):
                m.d.sync += [
                    x_curr.eq(self.diff_im_in),
                    x_prev.eq(x_curr),
                    self.symbol_strobe.eq(1),
                ]

                # Gardner TED: e = (diff_im_in - x_prev) * x_mid
                # Approximated as sign(x_mid) * (diff_im_in - x_prev)
                diff = Signal(signed(19), reset_less=True)
                m.d.comb += diff.eq(self.diff_im_in - x_prev)
                error = Signal(signed(19), reset_less=True)
                with m.If(x_mid[-1]):  # x_mid negative
                    m.d.comb += error.eq(-diff)
                with m.Else():
                    m.d.comb += error.eq(diff)

                # PI loop filter
                # integral += Ki * error
                # loop_out = Kp * error + integral
                ki_error = Signal(signed(32), reset_less=True)
                kp_error = Signal(signed(32), reset_less=True)
                m.d.comb += [
                    ki_error.eq(error * self.KI),
                    kp_error.eq(error * self.KP),
                ]
                new_integrator = Signal(signed(32), reset_less=True)
                m.d.comb += new_integrator.eq(integrator + ki_error)
                m.d.sync += [
                    integrator.eq(new_integrator),
                    loop_out.eq(kp_error + new_integrator),
                ]

                # Symbol slicer: sign bits of (diff_re, diff_im) at the
                # symbol point. Matches SDRTrunk LSM/C4FM toDibit
                # (P25P1DemodulatorLSM.toDibit, P25 TIA-102.BAAA):
                #   diff_re > 0, diff_im > 0  -> +1 -> dibit 00 (0)
                #   diff_re < 0, diff_im > 0  -> +3 -> dibit 01 (1)
                #   diff_re > 0, diff_im < 0  -> -1 -> dibit 10 (2)
                #   diff_re < 0, diff_im < 0  -> -3 -> dibit 11 (3)
                # So dibit_lsb = (diff_re < 0) and dibit_msb = (diff_im < 0)
                # In Amaranth Cat(a, b), 'a' is the LSB.
                m.d.sync += self.dibit_out.eq(
                    Cat(self.diff_re_in[-1], self.diff_im_in[-1]))

                # Timing adjustment from loop filter: if loop_out > threshold
                # advance by 1 sample, if < -threshold retard by 1
                # Use bit 16 as threshold (= 1.0 in Q16 fixed-point)
                with m.If(loop_out > (1 << 16)):
                    m.d.sync += timing_adj.eq(-1)  # advance (shorter period)
                with m.Elif(loop_out < -(1 << 16)):
                    m.d.sync += timing_adj.eq(1)   # retard (longer period)
                with m.Else():
                    m.d.sync += timing_adj.eq(0)

            with m.Else():
                m.d.sync += self.symbol_strobe.eq(0)

            # Counter update
            with m.If(at_symbol):
                # Reload with adjustment
                m.d.sync += counter.eq(sps - 1 + timing_adj)
            with m.Else():
                m.d.sync += counter.eq(counter - 1)

        with m.Else():
            m.d.sync += self.symbol_strobe.eq(0)

        return m
