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
    """Gardner-based symbol timing recovery for P25 C4FM

    Architecture:
        - Decimating counter (integer NCO) at ~10 samp/sym
        - Gardner TED: e[k] = (x[k] - x[k-1]) * x[k-half]
        - PI loop filter adjusts the NCO step
        - 4-level symbol slicer -> 2-bit dibit

    The NCO accumulates a fractional phase. When it wraps, that's a symbol
    strobe. The midpoint sample is taken at counter = sps/2. This is a
    "decimate and Gardner" approach — simpler than a full interpolating
    recovery, but sufficient for the 10 samp/sym ratio of P25.

    Inputs (sync domain):
        disc_in: 18-bit signed discriminator samples
        strobe_in: sample valid strobe

    Outputs (sync domain):
        dibit_out: 2-bit symbol decision (4FSK level)
        symbol_strobe: symbol decision strobe (4800 Hz)
    """
    # PI loop filter gains (fixed-point, 16 fractional bits)
    # BW ~48 Hz at 48 kSPS, damping ~0.707
    # Kp = 4*zeta*BnT / (1 + 2*zeta*BnT) ≈ 0.00283
    # Ki = 4*(BnT)^2 / (1 + 2*zeta*BnT)^2 ≈ 0.00000401
    # Scale for fixed-point: Kp*2^16 ≈ 185, Ki*2^16 ≈ 0.26 -> use 1 minimum
    KP = 185   # proportional gain (Q0.16 as integer)
    KI = 1     # integral gain (Q0.16 as integer)

    def __init__(self, samples_per_symbol=10):
        self.samples_per_symbol = samples_per_symbol

        # Inputs
        self.disc_in = Signal(signed(18))
        self.strobe_in = Signal()

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
            # Capture midpoint sample
            with m.If(at_midpoint):
                m.d.sync += x_mid.eq(self.disc_in)

            # Symbol strobe: compute TED, update loop, output dibit
            with m.If(at_symbol):
                m.d.sync += [
                    x_curr.eq(self.disc_in),
                    x_prev.eq(x_curr),
                    self.symbol_strobe.eq(1),
                ]

                # Gardner TED: e = (disc_in - x_prev) * x_mid
                # (disc_in is x_curr at this point)
                # Approximate by sign(x_mid) * (disc_in - x_prev) to avoid
                # a full multiply and keep the error bounded
                diff = Signal(signed(19), reset_less=True)
                m.d.comb += diff.eq(self.disc_in - x_prev)
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

                # Symbol slicer (4-level -> 2-bit dibit)
                # P25 C4FM deviation levels: +3, +1, -1, -3
                # Thresholds at 0, +2*step, -2*step (normalized)
                # With 18-bit disc output, use simple comparison
                self._slicer(m, self.disc_in)

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

    def _slicer(self, m, sample):
        """4-level symbol slicer -> 2-bit dibit

        P25 C4FM deviation mapping (TIA-102.BAAA):
          +1800 Hz -> dibit 01 (level +3)
          +600 Hz  -> dibit 00 (level +1)
          -600 Hz  -> dibit 10 (level -1)
          -1800 Hz -> dibit 11 (level -3)

        Decision boundaries at 0 and ±(2*step).
        """
        # sample is signed 18-bit; thresholds are at ~1/3 and ~2/3 of max
        # For the discriminator, +3 and -3 are ~3x the +1/-1 levels.
        # Threshold between ±3 and ±1 is at ±2*unit.
        # With arbitrary scaling, just use sign bit + magnitude comparison.
        # Threshold at half of expected max deviation
        # (If max disc output for ±1800 Hz = X, threshold = X/2 ~ 2/3*X)
        # Use simple quarter-range: compare abs(sample) vs (max/2)
        # For 18-bit signed, we use bit 16 as the threshold (≈ 1/4 of max)
        with m.If(sample >= 0):
            with m.If(sample[15]):  # above mid-threshold -> +3 -> dibit 01
                m.d.sync += self.dibit_out.eq(0b01)
            with m.Else():          # below mid-threshold -> +1 -> dibit 00
                m.d.sync += self.dibit_out.eq(0b00)
        with m.Else():
            with m.If(~sample[15]):  # magnitude above threshold -> -3 -> dibit 11
                m.d.sync += self.dibit_out.eq(0b11)
            with m.Else():           # magnitude below threshold -> -1 -> dibit 10
                m.d.sync += self.dibit_out.eq(0b10)
