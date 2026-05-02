#
# Fishball P25 -- PerTargetDDC HDL tests (M2A)
#
# Single end-to-end test: drive a complex tone at the input,
# program the NCO to shift it to DC, decimate /4, and verify the
# output magnitude is much larger than that of an unrelated NCO
# setting (off-target case).
#
# The point isn't bit-exact — the FIR rounding + mixer cmult
# rounding diverge by a few LSB from the model. We're checking
# that the chain WORKS: NCO mixes, FIR filters, decimator outputs
# at the right rate.
#
# SPDX-License-Identifier: MIT
#

import unittest

import numpy as np
from amaranth import *

from p25_hdl.per_target_ddc import PerTargetDDC, design_decim_fir
from .amaranth_sim import AmaranthSim
from .common_edge import CommonEdgeTb


class TestPerTargetDDC(AmaranthSim):

    def test_tone_to_dc_via_nco(self):
        coeffs = design_decim_fir(n_taps=32, decimation=4, fs_in_hz=125000)
        ddc = PerTargetDDC(fir_coeffs=coeffs, decimation=4,
                           width_in=16, width_out=16, nco_width=24)
        domain_3x = ddc._3x
        self.dut = CommonEdgeTb(ddc, [(domain_3x, 3, 'common_edge_3x')])

        # Drive a complex tone at +20 kHz (relative to fs_in=125 ksps).
        # Set NCO to -20 kHz to shift it to DC. Output samples should
        # then be near-constant (DC) — the decim FIR averages the
        # near-DC samples.
        fs_in = 125_000
        f_tone = 20_000
        # Mixer.frequency is the frequency that gets shifted DOWN
        # to DC. To bring a +f_tone tone to DC, set NCO = +f_tone.
        nco_norm = f_tone / fs_in       # cycles/sample at fs_in
        nco_freq = int(round(nco_norm * (1 << 24)))

        n_inputs = 256
        cycles_per_strobe = 8     # >> mixer + FIR latency
        amp = 6000
        ws = []
        for n in range(n_inputs):
            theta = 2 * np.pi * f_tone * n / fs_in
            ws.append((int(round(amp * np.cos(theta))),
                       int(round(amp * np.sin(theta)))))

        async def bench(ctx):
            ctx.set(ddc.enable, 1)
            ctx.set(ddc.nco_freq, nco_freq)
            outputs = []
            for n in range(n_inputs):
                re, im = ws[n]
                ctx.set(ddc.re_in, re)
                ctx.set(ddc.im_in, im)
                ctx.set(ddc.strobe_in, 1)
                await ctx.tick()
                ctx.set(ddc.strobe_in, 0)
                for _ in range(cycles_per_strobe - 1):
                    if ctx.get(ddc.strobe_out):
                        outputs.append((ctx.get(ddc.re_out),
                                        ctx.get(ddc.im_out)))
                    await ctx.tick()
            # Skip the first few outputs (filter ramp-up).
            steady = outputs[8:]
            assert len(steady) > 4, f'too few outputs ({len(outputs)})'
            mags = [r * r + i * i for r, i in steady]
            mean_mag = np.mean(mags)
            # After mixing to DC + lowpass + decim, mean amplitude
            # should be on the order of `amp` (full tone power
            # passed through). Allow ~3 dB slop for the FIR's
            # passband ripple.
            expected_mag = amp * amp
            ratio = mean_mag / expected_mag
            assert ratio > 0.25, (
                f'on-tone output too weak: mean_mag={mean_mag}, '
                f'expected~{expected_mag}, ratio={ratio:.3f}')

        self.simulate(bench, named_clocks={domain_3x: 4e-9})

    def test_off_tone_attenuated(self):
        """Drive a tone past the FIR cutoff; output should be tiny."""
        coeffs = design_decim_fir(n_taps=32, decimation=4, fs_in_hz=125000)
        ddc = PerTargetDDC(fir_coeffs=coeffs, decimation=4,
                           width_in=16, width_out=16, nco_width=24)
        domain_3x = ddc._3x
        self.dut = CommonEdgeTb(ddc, [(domain_3x, 3, 'common_edge_3x')])

        # Drive a tone at +50 kHz. NCO = 0 leaves it at +50 kHz at
        # the FIR input. FIR cutoff is 0.4 * 31.25 / 2 = 6.25 kHz —
        # so +50 kHz should be deep in the stopband.
        # Actually FIR cutoff was set as 0.4*fs_out=0.4*31250 =
        # 12.5 kHz. Tone at +50 kHz is well past cutoff, so it
        # should aliasdownward but the prototype kills it.
        fs_in = 125_000
        f_tone = 50_000
        nco_freq = 0
        n_inputs = 256
        cycles_per_strobe = 8
        amp = 6000

        async def bench(ctx):
            ctx.set(ddc.enable, 1)
            ctx.set(ddc.nco_freq, nco_freq)
            outputs = []
            for n in range(n_inputs):
                theta = 2 * np.pi * f_tone * n / fs_in
                ctx.set(ddc.re_in, int(round(amp * np.cos(theta))))
                ctx.set(ddc.im_in, int(round(amp * np.sin(theta))))
                ctx.set(ddc.strobe_in, 1)
                await ctx.tick()
                ctx.set(ddc.strobe_in, 0)
                for _ in range(cycles_per_strobe - 1):
                    if ctx.get(ddc.strobe_out):
                        outputs.append((ctx.get(ddc.re_out),
                                        ctx.get(ddc.im_out)))
                    await ctx.tick()
            steady = outputs[8:]
            mags = [r * r + i * i for r, i in steady]
            mean_mag = np.mean(mags) if mags else 0
            attenuation_db = (10 * np.log10(mean_mag / (amp * amp))
                              if mean_mag > 0 else -100)
            assert attenuation_db < -20, (
                f'off-tone not attenuated enough: '
                f'mean_mag={mean_mag}, attenuation={attenuation_db:.1f} dB '
                f'(expected < -20)')

        self.simulate(bench, named_clocks={domain_3x: 4e-9})


if __name__ == '__main__':
    unittest.main()
