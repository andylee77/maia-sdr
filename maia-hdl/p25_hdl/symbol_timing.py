#
# Fishball P25 - Symbol Timing Recovery
#
# Gardner timing error detector + PI loop filter + symbol-rate slicer.
# Operates at ~13 samples/symbol (62.5 kSPS / 4800 sym/sec).
#
# Gardner TED: e[k] ≈ sign(x[k-1/2]) * (x[k] - x[k-1])  (on diff_im)
# Loop filter: PI controller adjusting integer NCO step ±1
# Output: 2-bit dibit (4-level symbol decision) + strobe
#
# Symbol-rate differential (for the slicer):
#   diff = sym[k] * conj(sym[k-1])
#   diff_re = re[k]*re[k-1] + im[k]*im[k-1]
#   diff_im = im[k]*re[k-1] - re[k]*im[k-1]
#   dibit = (sign(diff_re), sign(diff_im))     (4 quadrants)
#
# Cost: 4 DSP48E1 (16x16 multiplies for the symbol-rate differential)
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class SymbolTimingRecovery(Elaboratable):
    """Gardner-based symbol timing recovery + symbol-rate slicer for P25
    C4FM and LSM.

    Architecture:
        - Decimating counter (integer NCO) at samples_per_symbol
        - Gardner TED on diff_im (the FM cross-product) — works for both
          C4FM (where diff_im = instantaneous frequency) and LSM (where
          diff_im is the imaginary part of the per-sample differential
          and still has a zero-crossing at symbol boundaries)
        - PI loop filter adjusts the NCO step ±1
        - Symbol slicer: at the symbol decision point, hold the latched
          previous symbol's (re, im), compute the SYMBOL-RATE differential
          z[k] * conj(z[k-1]) combinationally, and take the two sign
          bits as the dibit. This is the same approach used by SDRTrunk's
          P25P1DemodulatorLSM.toDibit and works for both C4FM and LSM.

    Why symbol-rate (not sample-rate) differential for slicing:
        At sample rate, the per-sample phase change is small (~symbol
        phase change / sps), so cos(small_angle) ≈ +1 and the real part
        of the differential is *always* positive. The slicer LSB is
        stuck and only 2 of the 4 dibit values appear. By computing the
        differential between successive *symbols* (samples 1 symbol
        apart), the phase change is the actual P25 symbol angle (±π/4
        or ±3π/4), the real part flips sign for the outer ±3 symbols,
        and all four dibit values are produced.

    Inputs (sync domain):
        re_in, im_in: 16-bit signed post-DDC IQ samples (used for the
            symbol-rate differential — captured at the symbol point)
        diff_im_in: 18-bit signed FM cross-product (used by Gardner TED
            and as a sanity discriminator; same as before)
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

        # Inputs
        # Raw post-DDC IQ samples (used for the symbol-rate differential)
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        # FM cross-product from C4FMDemod (used by Gardner TED)
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

        # ── Symbol-rate differential (for the slicer) ────────────────
        # Latch the IQ samples at every symbol decision point. The held
        # value becomes the "previous symbol" for the next decision.
        # The combinational differential is then:
        #   sym_diff_re = re_in*sym_re_prev + im_in*sym_im_prev
        #   sym_diff_im = im_in*sym_re_prev - re_in*sym_im_prev
        # Inferred to 4 DSP48E1 multiplies (16x16 -> 32-bit each).
        sym_re_prev = Signal(signed(16), reset_less=True)
        sym_im_prev = Signal(signed(16), reset_less=True)
        sym_diff_re = Signal(signed(34))
        sym_diff_im = Signal(signed(34))
        m.d.comb += [
            sym_diff_re.eq(
                self.re_in * sym_re_prev + self.im_in * sym_im_prev),
            sym_diff_im.eq(
                self.im_in * sym_re_prev - self.re_in * sym_im_prev),
        ]

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

                # Symbol slicer: sign bits of the SYMBOL-RATE differential
                # sym[k] * conj(sym[k-1]). Matches SDRTrunk LSM/C4FM
                # toDibit (P25P1DemodulatorLSM.toDibit, P25 TIA-102.BAAA):
                #   sym_diff_re > 0, sym_diff_im > 0  -> +1 -> dibit 00 (0)
                #   sym_diff_re < 0, sym_diff_im > 0  -> +3 -> dibit 01 (1)
                #   sym_diff_re > 0, sym_diff_im < 0  -> -1 -> dibit 10 (2)
                #   sym_diff_re < 0, sym_diff_im < 0  -> -3 -> dibit 11 (3)
                # dibit_lsb = (sym_diff_re < 0), dibit_msb = (sym_diff_im < 0)
                # In Amaranth Cat(a, b), 'a' is the LSB.
                m.d.sync += [
                    self.dibit_out.eq(
                        Cat(sym_diff_re[-1], sym_diff_im[-1])),
                    # Update held previous symbol for the NEXT decision.
                    sym_re_prev.eq(self.re_in),
                    sym_im_prev.eq(self.im_in),
                ]

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
