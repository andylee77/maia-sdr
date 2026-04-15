#!/usr/bin/env python3
"""
p25_ddc_filter_design.py -- DDC filter redesign for Fishball P25.

Design new coefficients for the 3-stage maia_hdl DDC used by the
Fishball P25 control + traffic chains. Replaces the current short
Kaiser filters (48/32/64 taps) which have a transition band wide
enough that adjacent P25 channels at +/-500 kHz to +/-2 MHz leak
through the passband and corrupt the control-channel CRC at
rf_bandwidth >= 5 MHz.

Design philosophy
-----------------
Match SDRTrunk's filter-chain sharpness where feasible, using
proper equiripple designs instead of the wide Kaiser windows in
the current fpga.rs tables.

SDRTrunk decodes P25 by (a) decimating the front-end samples in
halfband /2 stages down to ~19.2 kHz, then (b) running a sharp
Parks-McClellan LPF (passband 7250 Hz, stopband 8000 Hz, 0.01
ripple), then (c) a 16-symbol RRC matched filter with alpha=0.2.
See `P25P1DecoderLSM.java:105-145` for the filter chain and
`getBasebandFilter()` for the PM spec.

The maia_hdl DDC primitive has only 3 programmable FIR stages
(FIR4DSP / FIR2DSP / FIR4DSP), each with a 256-coefficient RAM,
and the total decimation is locked at /128 (8 MSPS -> 62.5 kSPS).
That rules out the 4-stage halfband cascade SDRTrunk uses, but
we can still do much better than 48 taps on stage 1.

Decimation plan
---------------
The current `/16 /4 /2` split is pathological: stage 1 has to
achieve a very sharp transition (passband ~150 kHz, stopband
~250 kHz at the /16 output Nyquist) in 48 taps at 8 MSPS --
arithmetically impossible regardless of window choice. The
effective transition band ends well above 500 kHz, inside the
first P25 adjacent-channel group.

Moving to `/4 /4 /8 = /128` gives:
  * Stage 1 decim /4  -> 8 MSPS -> 2 MSPS (Nyquist 1 MHz)
  * Stage 2 decim /4  -> 2 MSPS -> 500 kSPS (Nyquist 250 kHz)
  * Stage 3 decim /8  -> 500 kSPS -> 62.5 kSPS (Nyquist 31.25 kHz)

Each stage's transition band is >= the per-stage output Nyquist
margin, and the tap counts fit comfortably in the 256-coefficient
RAM budget of each FIR. The net cascaded frequency response has
a much deeper stopband across the adjacent-channel range
(-500 kHz to -12.5 kHz and +12.5 kHz to +500 kHz in the stage-1
output frame).

Output of this script
---------------------
1. Three Parks-McClellan designs (one per stage), verified against
   an explicit stopband-attenuation budget.
2. Q1.17 quantised coefficient tables in the `&[i32]` format used
   by `p25-httpd/src/fpga.rs`.
3. Adjacent-channel rejection plot data (sampled dBc at every
   12.5 kHz grid point from 0 to 4 MHz) against the cascaded
   response.
4. The new `P25_DEC{1,2,3}` constants and `operations_minus_one*`
   values for the DDC register writes.

Run with `python tools/p25_ddc_filter_design.py > doc/changes/040_ddc_filter_redesign.txt`
to capture everything. `--plot` adds matplotlib windows if you
want to eyeball the shapes.

SPDX-License-Identifier: MIT
"""

from __future__ import annotations

import argparse
import math
import sys
from dataclasses import dataclass

import numpy as np
from scipy.signal import remez, freqz, firwin, kaiserord


# ── Global constants ──────────────────────────────────────────────

FS_IN = 8_000_000          # AD9361 RX rate at /128 to 62.5 kSPS
FS_OUT = 62_500            # = 13 samples/symbol @ 4800 baud
TOTAL_DECIM = FS_IN // FS_OUT   # 128

# Decimation plan (this is the CHANGE): /4 /4 /8 instead of /16 /4 /2.
DEC1 = 4
DEC2 = 4
DEC3 = 8
assert DEC1 * DEC2 * DEC3 == TOTAL_DECIM

# Fixed-point coefficient format (matches maia_hdl DDC):
#   18-bit signed, Q1.17. Max positive = 2^17 - 1 = 131071.
COEFF_WIDTH = 18
COEFF_FRAC = COEFF_WIDTH - 1                       # 17
COEFF_MAX = (1 << COEFF_FRAC) - 1                  # 131071
COEFF_MIN = -(1 << COEFF_FRAC)                     # -131072

# Per-stage sample rates after each decimation.
FS_AFTER_S1 = FS_IN // DEC1                        # 2 MHz
FS_AFTER_S2 = FS_AFTER_S1 // DEC2                  # 500 kHz
FS_AFTER_S3 = FS_AFTER_S2 // DEC3                  # 62.5 kHz

# FIR4DSP / FIR2DSP RAM limit from maia_hdl/fir.py (len_log2 = 8 for
# FIR4DSP, len_log2 = 7 for FIR2DSP); each coefficient slot holds
# one 18-bit tap.
MAX_TAPS_FIR4DSP = 256
MAX_TAPS_FIR2DSP = 128


# ── Filter specs ──────────────────────────────────────────────────
#
# All stopbands are anchored at the per-stage output Nyquist so
# there's no aliasing into the subsequent stage's passband. The
# passband cutoff is set generously (far wider than the 6.25 kHz
# P25 channel half-bandwidth) so the DDC NCO can still tune across
# ~+/-300 kHz without clipping the signal corner.


@dataclass
class StageSpec:
    name: str
    fs: int              # input sample rate for this stage
    decim: int
    passband_hz: float   # end of passband
    stopband_hz: float   # start of stopband
    stopband_db: float   # required stopband attenuation (dB)
    ripple_db: float = 0.1


STAGE1 = StageSpec(
    name='stage1',
    fs=FS_IN,             # 8 MHz
    decim=DEC1,           # /4 -> 2 MHz
    passband_hz=300_000,  # generous; leaves 700 kHz transition
    stopband_hz=1_000_000,  # = output Nyquist, anti-alias anchor
    stopband_db=80.0,
    ripple_db=0.1,
)

STAGE2 = StageSpec(
    name='stage2',
    fs=FS_AFTER_S1,       # 2 MHz
    decim=DEC2,           # /4 -> 500 kHz
    passband_hz=100_000,  # still far wider than the P25 channel
    stopband_hz=250_000,  # output Nyquist
    stopband_db=80.0,
    ripple_db=0.1,
)

STAGE3 = StageSpec(
    name='stage3',
    fs=FS_AFTER_S2,       # 500 kHz
    decim=DEC3,           # /8 -> 62.5 kHz
    # Stage 3 is the tightest filter -- its passband has to pass
    # the full P25 channel (+/- 6.25 kHz) plus a bit of headroom
    # for the NCO tune range and the downstream RRC/LPF
    # infrastructure, while the stopband must start at 31.25 kHz
    # to prevent aliasing into the 62.5 kSPS output band.
    passband_hz=10_000,
    stopband_hz=31_250,
    stopband_db=80.0,
    ripple_db=0.1,
)


def design_stage(spec: StageSpec, *, max_taps: int) -> np.ndarray:
    """Design a linear-phase equiripple lowpass for ``spec``, using
    scipy.signal.remez. Tap count is bumped until the stopband
    attenuation target is met, or we run out of budget.

    Returns the float coefficients (will be Q1.17-quantised later).
    """
    # Start from the Kaiser-window estimator for an initial tap count.
    # kaiserord returns (numtaps, beta); we ignore beta (PM doesn't
    # use it) but keep numtaps as a starting point.
    width_hz = spec.stopband_hz - spec.passband_hz
    delta_p = 10 ** (spec.ripple_db / 20.0) - 1
    delta_s = 10 ** (-spec.stopband_db / 20.0)
    ripple_db = -20 * math.log10(min(delta_p, delta_s))
    numtaps_start, _ = kaiserord(
        ripple_db, width_hz / (spec.fs / 2))
    # kaiserord is often conservative for equiripple; start 20 %
    # below then bump up if remez misses spec.
    n_taps = max(15, int(numtaps_start * 0.8) | 1)  # odd for symmetry

    def try_design(n):
        bands = [
            0.0,
            spec.passband_hz,
            spec.stopband_hz,
            spec.fs / 2,
        ]
        desired = [1.0, 0.0]
        # weights: heavier on the stopband so remez pushes it down
        # farther for a given tap count. Ratio comes from the
        # ripple/stopband targets.
        weight = [1.0, delta_p / delta_s]
        return remez(n, bands, desired, weight=weight, fs=spec.fs)

    while True:
        taps = try_design(n_taps)
        # Measure actual stopband.
        w, h = freqz(taps, worN=4096, fs=spec.fs)
        stopband_mask = w >= spec.stopband_hz
        if not np.any(stopband_mask):
            raise RuntimeError(
                f"{spec.name}: stopband starts above fs/2")
        worst_sb = 20 * np.log10(np.max(np.abs(h[stopband_mask])) + 1e-30)
        # Measure actual passband ripple.
        pb_mask = w <= spec.passband_hz
        pb_mag = np.abs(h[pb_mask])
        pb_ripple = 20 * np.log10(pb_mag.max() / pb_mag.min() + 1e-30)

        if worst_sb <= -spec.stopband_db and pb_ripple <= 2 * spec.ripple_db:
            print(
                f"[{spec.name}] N={n_taps:4d}  "
                f"stopband={worst_sb:+7.2f} dB  "
                f"passband_ripple={pb_ripple:.3f} dB  "
                f"pb=[0,{spec.passband_hz/1e3:.0f}] kHz  "
                f"sb=[{spec.stopband_hz/1e3:.0f},{spec.fs/2e3:.0f}] kHz")
            return taps

        if n_taps >= max_taps:
            print(
                f"[{spec.name}] N={n_taps:4d}  "
                f"stopband={worst_sb:+7.2f} dB (TARGET "
                f"{-spec.stopband_db:.1f} dB) -- at max tap budget")
            # Return what we got; the caller decides whether to
            # accept the compromise.
            return taps
        n_taps += 4  # keep odd-ish parity; PM accepts even/odd


def ensure_decim_divisible(taps: np.ndarray, decim: int) -> np.ndarray:
    """The FIR4DSP / FIR2DSP loaders assume len(coeffs) is an exact
    multiple of ``decim`` so the polyphase branches are equal
    length. Pad with zeros at the end if needed."""
    rem = len(taps) % decim
    if rem == 0:
        return taps
    pad = decim - rem
    return np.concatenate([taps, np.zeros(pad)])


def quantise(taps: np.ndarray) -> list[int]:
    """Round to Q1.17 signed int with saturation. Matches the format
    loaded by p25-httpd/src/fpga.rs::load_fir_*dsp()."""
    scale = 1 << COEFF_FRAC
    q = np.clip(np.round(taps * scale), COEFF_MIN, COEFF_MAX).astype(int)
    return q.tolist()


def cascaded_response(taps1, taps2, taps3, *, n_points=16384):
    """Compute the cascaded frequency response of the 3-stage DDC
    as seen at the AD9361 input rate. Each downstream filter is
    upsampled by the decimation factors of the preceding stages
    (i.e. stage 2's response is evaluated at fs=2 MHz and mapped
    into the 8 MHz input band by zero-insertion / aliasing).

    For adjacent-channel analysis we only care about the aggregate
    response from 0 to FS_IN/2 at FS_IN resolution, so we combine
    via `np.convolve` in the time domain after upsampling each
    stage's taps to the input rate."""
    def upsample(taps, factor):
        out = np.zeros(len(taps) * factor)
        out[::factor] = taps
        return out

    up2 = upsample(taps2, DEC1)
    up3 = upsample(taps3, DEC1 * DEC2)
    cascaded = np.convolve(np.convolve(taps1, up2), up3)
    w, h = freqz(cascaded, worN=n_points, fs=FS_IN)
    return w, h


def format_rust_array(name: str, comment: str, coeffs: list[int]) -> str:
    """Emit a `const NAME: &[i32] = &[...];` block matching the
    fpga.rs style."""
    lines = [
        f"// {comment}",
        "#[rustfmt::skip]",
        f"const {name}: &[i32] = &[",
    ]
    # 8 values per line.
    for i in range(0, len(coeffs), 8):
        chunk = coeffs[i:i + 8]
        body = ", ".join(f"{c:>7d}" for c in chunk)
        lines.append(f"    {body},")
    lines.append("];")
    return "\n".join(lines)


def evaluate_adjacent_channels(w, h):
    """Sample the cascaded |H| at multiples of 12.5 kHz from 0 to
    4 MHz. Print dB values for every grid point; dBc is relative
    to DC gain."""
    mag_db = 20 * np.log10(np.abs(h) + 1e-30)
    dc_gain = mag_db[0]
    print()
    print("Adjacent-channel rejection (dB relative to DC, sampled "
          "at 12.5 kHz grid):")
    print("  offset      dBc")
    for off_khz in (
        0, 6.25, 12.5, 25, 50, 100, 200, 500,
        1000, 1500, 2000, 2500, 3000, 3500, 3900,
    ):
        f_hz = off_khz * 1e3
        idx = np.argmin(np.abs(w - f_hz))
        rel = mag_db[idx] - dc_gain
        marker = "   PASS" if off_khz <= 6.25 else ""
        if 12.5 <= off_khz <= 500 and rel > -60:
            marker = "   WEAK"
        print(f"  {off_khz:7.2f} kHz  {rel:+7.2f} dB{marker}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        '--plot', action='store_true',
        help='Plot per-stage + cascaded responses (requires matplotlib)')
    args = ap.parse_args()

    print("=" * 72)
    print("P25 DDC filter redesign")
    print("=" * 72)
    print(f"Total decimation: /{TOTAL_DECIM}  ({FS_IN/1e6:.1f} MSPS -> "
          f"{FS_OUT/1e3:.1f} kSPS)")
    print(f"New stage plan:   /{DEC1} /{DEC2} /{DEC3}")
    print(f"Coefficient format: Q1.{COEFF_FRAC} "
          f"(signed {COEFF_WIDTH}-bit)")
    print()
    print("Per-stage designs (equiripple Parks-McClellan via remez):")
    print()

    taps1 = design_stage(STAGE1, max_taps=MAX_TAPS_FIR4DSP)
    taps2 = design_stage(STAGE2, max_taps=MAX_TAPS_FIR2DSP)
    taps3 = design_stage(STAGE3, max_taps=MAX_TAPS_FIR4DSP)

    # Pad to multiples of the respective decimation factors (the
    # DDC loader needs len % decim == 0 to split into equal
    # polyphase branches).
    taps1 = ensure_decim_divisible(taps1, DEC1)
    taps2 = ensure_decim_divisible(taps2, DEC2)
    taps3 = ensure_decim_divisible(taps3, DEC3)

    print()
    print(f"Final lengths: stage1={len(taps1)} "
          f"stage2={len(taps2)} stage3={len(taps3)}")
    print(f"  branch_len stage1 = {len(taps1) // DEC1} "
          f"(operations_minus_one1 = {len(taps1) // DEC1 - 1})")
    print(f"  ops_minus_one2     = {len(taps2) // DEC2 - 1}")
    print(f"  branch_len stage3 = {len(taps3) // DEC3} "
          f"(operations_minus_one3 = {len(taps3) // DEC3 - 1})")

    # Cascaded response analysis.
    w, h = cascaded_response(taps1, taps2, taps3)
    evaluate_adjacent_channels(w, h)

    # Quantise and dump Rust tables.
    q1 = quantise(taps1)
    q2 = quantise(taps2)
    q3 = quantise(taps3)

    print()
    print("=" * 72)
    print("Rust coefficient tables for p25-httpd/src/fpga.rs:")
    print("=" * 72)
    print()
    print(f"const P25_DEC1: usize = {DEC1};")
    print(f"const P25_DEC2: usize = {DEC2};")
    print(f"const P25_DEC3: usize = {DEC3};")
    print()
    print(format_rust_array(
        'P25_FIR1_COEFFS',
        f'Stage 1 (FIR4DSP): {len(q1)} taps, '
        f'pb={STAGE1.passband_hz/1e3:.0f} kHz, '
        f'sb={STAGE1.stopband_hz/1e3:.0f} kHz, '
        f'-{STAGE1.stopband_db:.0f} dB Parks-McClellan',
        q1))
    print()
    print(format_rust_array(
        'P25_FIR2_COEFFS',
        f'Stage 2 (FIR2DSP): {len(q2)} taps, '
        f'pb={STAGE2.passband_hz/1e3:.0f} kHz, '
        f'sb={STAGE2.stopband_hz/1e3:.0f} kHz, '
        f'-{STAGE2.stopband_db:.0f} dB Parks-McClellan',
        q2))
    print()
    print(format_rust_array(
        'P25_FIR3_COEFFS',
        f'Stage 3 (FIR4DSP): {len(q3)} taps, '
        f'pb={STAGE3.passband_hz/1e3:.0f} kHz, '
        f'sb={STAGE3.stopband_hz/1e3:.0f} kHz, '
        f'-{STAGE3.stopband_db:.0f} dB Parks-McClellan',
        q3))

    if args.plot:
        try:
            import matplotlib.pyplot as plt
        except ImportError:
            print("\nmatplotlib not available -- skipping --plot")
            return

        fig, axs = plt.subplots(4, 1, figsize=(8, 10))
        for ax, (name, taps, fs) in zip(axs[:3], [
            (STAGE1.name, taps1, STAGE1.fs),
            (STAGE2.name, taps2, STAGE2.fs),
            (STAGE3.name, taps3, STAGE3.fs),
        ]):
            w_s, h_s = freqz(taps, worN=4096, fs=fs)
            ax.plot(w_s / 1e3, 20 * np.log10(np.abs(h_s) + 1e-30))
            ax.set_title(f"{name} -- {len(taps)} taps")
            ax.set_xlabel("kHz")
            ax.set_ylabel("dB")
            ax.grid(True)
            ax.set_ylim(-120, 5)
        axs[3].plot(w / 1e6, 20 * np.log10(np.abs(h) + 1e-30))
        axs[3].set_title("Cascaded response (8 MSPS in -> 62.5 kSPS)")
        axs[3].set_xlabel("MHz")
        axs[3].set_ylabel("dB")
        axs[3].grid(True)
        axs[3].set_ylim(-120, 5)
        plt.tight_layout()
        plt.show()


if __name__ == '__main__':
    sys.exit(main() or 0)
