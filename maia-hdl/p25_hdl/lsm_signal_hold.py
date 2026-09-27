#
# Fishball P25 -- LSM "no signal" hold for the PLL and timing loops
#
# Turns the AGC's per-symbol idle-gate decision (`LsmAgc.gated_out`:
# this symbol's magnitude was below `mag_update_threshold`) into a
# hold flag with hysteresis:
#
#   - not held -> held after `enter_symbols` consecutive gated
#     symbols;
#   - held -> not held after `exit_symbols` consecutive non-gated
#     symbols;
#   - `reset_in` -> held, run counter cleared.
#
# `LsmDemodLoop` feeds `hold` to `LsmPllUpdate.hold_in` (accumulator
# frozen) and masks the Gardner correction strobe into
# `LsmTimingInterp`. See the "PLL/timing hold" comment at the top of
# lsm_demod_loop.py for the failure this prevents and the choice of
# the default counts.
#
# One small counter and a flag: a few LUTs/FFs, no DSP or BRAM.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class LsmSignalHold(Elaboratable):
    """Hysteresis on the AGC idle gate.

    Parameters
    ----------
    enter_symbols : int >= 1
        Consecutive gated symbols that start the hold.
    exit_symbols : int >= 1
        Consecutive non-gated symbols that end it.

    Inputs (sync domain):
        strobe_in : Signal()  one cycle per symbol (the AGC's
            ``decision_strobe_out``)
        gated_in  : Signal()  valid with ``strobe_in`` (the AGC's
            ``gated_out``)
        reset_in  : Signal()  one-cycle pulse: hold, clear the run

    Outputs (sync domain):
        hold_out : Signal(init=1)  registered; changes only on the
            cycle after ``strobe_in`` (or ``reset_in``)
    """

    def __init__(self, *, enter_symbols, exit_symbols):
        if enter_symbols < 1 or exit_symbols < 1:
            raise ValueError(
                f"enter_symbols and exit_symbols must be >= 1, got "
                f"{enter_symbols!r}, {exit_symbols!r}")
        self.enter_symbols = enter_symbols
        self.exit_symbols = exit_symbols

        self.strobe_in = Signal()
        self.gated_in = Signal()
        self.reset_in = Signal()
        self.hold_out = Signal(init=1)

    def elaborate(self, platform):
        m = Module()

        hold = self.hold_out
        run = Signal(range(max(self.enter_symbols, self.exit_symbols) + 1))

        with m.If(self.strobe_in):
            with m.If(hold):
                # Held: count consecutive symbols above the gate.
                with m.If(self.gated_in):
                    m.d.sync += run.eq(0)
                with m.Elif(run >= self.exit_symbols - 1):
                    m.d.sync += [hold.eq(0), run.eq(0)]
                with m.Else():
                    m.d.sync += run.eq(run + 1)
            with m.Else():
                # Tracking: count consecutive symbols below the gate.
                with m.If(~self.gated_in):
                    m.d.sync += run.eq(0)
                with m.Elif(run >= self.enter_symbols - 1):
                    m.d.sync += [hold.eq(1), run.eq(0)]
                with m.Else():
                    m.d.sync += run.eq(run + 1)

        with m.If(self.reset_in):
            m.d.sync += [hold.eq(1), run.eq(0)]

        return m
