#
# Fishball P25 -- SignalEnergy HDL tests
#
# Part of the 2026-04-30 channelizer rewrite.
#
# Two deterministic tests for `SignalEnergy`:
#
#   1. Step response — drive one channel from zero to a constant
#      magnitude; verify the IIR settles toward |x|^2 with the
#      expected time constant (~1024 samples at alpha_log2=10).
#   2. Threshold compare — set per-channel thresholds, drive
#      different magnitudes, verify `energy_present` bits track
#      the steady-state energy vs threshold relationship.
#
# Sim ticks are cheap (one MACC update per channel per epoch); we
# can run a few thousand epochs without blowing the budget.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *

from p25_hdl.signal_energy import SignalEnergy
from .amaranth_sim import AmaranthSim


class TestSignalEnergy(AmaranthSim):

    def test_step_response_settles(self):
        """Drive channel 0 with constant |x|^2 = R; energy converges to R."""
        M = 8                          # smaller M for faster sim
        width_in = 16
        alpha = 6                       # ~64-sample TC for sim speed
        dut = SignalEnergy(M=M, width_in=width_in,
                           alpha_log2_default=alpha)
        self.dut = dut

        re_val = 1000
        im_val = 500
        target_energy = re_val * re_val + im_val * im_val

        async def bench(ctx):
            # Drive channel 0 only; others stay at zero. Provide
            # M strobes worth of warm-up zeros to push the IIR off
            # its reset state.
            n_epochs = 1000
            ctx.set(dut.alpha_log2, alpha)
            ctx.set(dut.bin_re[0], re_val)
            ctx.set(dut.bin_im[0], im_val)
            for _ in range(n_epochs):
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                # Wait M+5 cycles for the round-robin sweep to
                # finish before next strobe.
                for _ in range(M + 5):
                    await ctx.tick()
            # After many time-constants the energy should be within
            # a few % of target (IIR steady-state error tracks
            # alpha-shift round-off).
            ch0_energy = ctx.get(dut.energy[0])
            err_pct = abs(ch0_energy - target_energy) / target_energy
            assert err_pct < 0.05, (
                f'channel 0 energy {ch0_energy} far from target '
                f'{target_energy} (err {err_pct:.2%})')
            # Other channels stayed at zero: their energy should
            # be at most a few alpha-shift LSBs.
            ch1_energy = ctx.get(dut.energy[1])
            assert ch1_energy < 4, (
                f'channel 1 leaked energy {ch1_energy} despite zero input')

        self.simulate(bench)

    def test_energy_present_threshold(self):
        """Threshold compare flips with energy state."""
        M = 4
        alpha = 6
        dut = SignalEnergy(M=M, alpha_log2_default=alpha)
        self.dut = dut

        async def bench(ctx):
            ctx.set(dut.alpha_log2, alpha)
            # Channel 0 above threshold, channel 1 below.
            ctx.set(dut.bin_re[0], 5000)
            ctx.set(dut.bin_im[0], 0)
            ctx.set(dut.bin_re[1], 100)
            ctx.set(dut.bin_im[1], 0)
            ctx.set(dut.threshold[0], 1_000_000)   # 5000^2 = 2.5e7
            ctx.set(dut.threshold[1], 1_000_000)   # 100^2 = 1e4 < threshold
            ctx.set(dut.threshold[2], 1)
            ctx.set(dut.threshold[3], 1)
            for _ in range(2000):
                ctx.set(dut.strobe_in, 1)
                await ctx.tick()
                ctx.set(dut.strobe_in, 0)
                for _ in range(M + 5):
                    await ctx.tick()
            present = ctx.get(dut.energy_present)
            assert present & 1, 'channel 0 should be present (high energy)'
            assert not (present & 2), \
                'channel 1 should NOT be present (energy < threshold)'

        self.simulate(bench)


if __name__ == '__main__':
    unittest.main()
