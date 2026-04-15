#!/usr/bin/env python3
"""
p25_ddc_filter_design.py -- P25DDC fork filter design, SDRTrunk-faithful.

Design the coefficients for the 3-stage DDC used by the Fishball P25
control + traffic chains. This is the P25DDC fork (v2) design --
tightens the Phase 10-prep filters to the SDRTrunk-equivalent stopband
depth and switches to a strict unit-DC-gain coefficient convention so
the peak-rescale quirk that cost a bake cycle on 2026-04-15 is gone.

Design philosophy
-----------------
Match SDRTrunk's DSP chain *where it actually matters for the DDC*.
The SDRTrunk P25 chain is, end to end:

  tuner rate
    -> ComplexPolyphaseChannelizerM2     (wideband channelization)
    -> DecimationFilterFactory /4 or /8  (half-band + Blackman cascades)
    -> Parks-McClellan baseband LPF      (pb=7250 Hz, sb=8000 Hz, 0.01 dB)
    -> RRC matched filter                (alpha=0.2, 16 symbol span)
    -> AGC -> Gardner TED -> PLL -> slicer

In our FPGA chain, the *downstream* LsmFir LPF (see
maia-hdl/p25_hdl/lsm_fir.py::LPF_TAPS_31250) is already an exact
replica of SDRTrunk's baseband Remez LPF (pb=7250 Hz, sb=8000 Hz,
31.25 kSPS rate), and the 105-tap LsmFir RRC is SDRTrunk's RRC
alpha=0.2 span=16. Those parts we already match.

The DDC's job, therefore, is **not** to be a second sharp baseband
LPF. Its job is to be a clean *anti-alias* decimator that feeds
LsmDecimator2 + LsmFir without contaminating the 0-15.625 kHz band
that the downstream chain cares about. There are two specific
constraints the Phase 10-prep filters violated:

1. **LsmDecimator2 is a NAIVE /2**
   (see maia-hdl/p25_hdl/lsm_decimator.py:10-15). It has no anti-alias
   filter of its own -- it just emits every other input sample. This
   means the DDC stage 3 MUST kill energy from 15.625 kHz to 31.25 kHz
   at its output, because anything in that range folds directly onto
   the desired 0-15.625 kHz band after the /2 decimation. The
   Phase 10-prep stage 3 was pb=10 kHz / sb=31.25 kHz, with cascade
   response only -25 dB at 25 kHz offset. A 25 kHz adjacent emitter
   aliases to -6.25 kHz after LsmDecimator2, right on top of the P25
   channel center. That is the root cause of the control-CRC gap and
   the traffic lock-on-retune failures tracked in
   project_p25_control_throughput_regression.md.

2. **Unit DC gain.**
   The Phase 10-prep tool rescaled coefficients to peak Q1.17 max
   (131071) to preserve the Maia DDC's ~17x/~5x/~8x per-stage DC
   gain profile. That rescaling is fragile -- it silently couples
   coefficient design and macc_trunc, and it bit us on the first
   Phase 10-prep flash where unit-gain remez filters starved the
   demod by ~700x. The new design emits unit-DC-gain coefficients
   (max tap < Q1.17 max, not equal to it) and computes a per-stage
   output_shift constant that the Rust side writes to a new HDL
   register. Coefficient design and gain/scale become orthogonal.

Decimation plan (unchanged from Phase 10-prep)
----------------------------------------------
  * Stage 1 FIR4DSP /4  -> 8 MSPS -> 2 MSPS       (Nyquist 1 MHz)
  * Stage 2 FIR2DSP /4  -> 2 MSPS -> 500 kSPS     (Nyquist 250 kHz)
  * Stage 3 FIR4DSP /8  -> 500 kSPS -> 62.5 kSPS  (Nyquist 31.25 kHz)
  * (downstream) LsmDecimator2 /2 -> 31.25 kSPS
  * (downstream) LsmFir LPF + RRC (SDRTrunk-equivalent)

Tap budget: FIR4DSP 256, FIR2DSP 128 (from maia_hdl/fir.py).
Phase 10-prep used 48/56/104 tap budgets. The v2 design spends
much more of the available budget on all three stages to get
deeper stopbands and tighter transitions -- especially stage 3,
which grows from 97 taps to ~240 taps, the maximum that fits in
FIR4DSP's 256-slot RAM after rounding to a multiple of /8.

Output of this script
---------------------
1. Three Parks-McClellan equiripple designs, one per stage, driven
   by 0.01 dB passband ripple and deep stopband targets.
2. A per-stage DC-gain analysis: cumulative DC gain, required
   output_shift, Q1.17 peak utilization (for sanity).
3. Cascaded frequency-response table across the fold-back band,
   with explicit callouts for the 15.625-31.25 kHz range that
   LsmDecimator2 folds back onto the P25 channel.
4. Q1.17 quantised coefficient tables in the `&[i32]` format used
   by `p25-httpd/src/fpga.rs`, with comments noting the new
   unit-DC-gain convention.
5. The `P25_DEC{1,2,3}` constants, per-stage `operations_minus_one`
   values, and new per-stage `P25_OUTPUT_SHIFT{1,2,3}` constants.

Run with `python tools/p25_ddc_filter_design.py > doc/changes/041_p25ddc_fork.txt`
to capture everything. `--plot` adds matplotlib windows for eyeball
verification.

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


# ── Filter specs (v2, SDRTrunk-faithful, tighter than Phase 10-prep) ─
#
# Each stage's stopband is anchored at its output Nyquist so
# nothing folds into the subsequent stage's passband. Ripple is
# tightened to 0.01 dB everywhere (SDRTrunk standard for Remez
# baseband designs), and stopband depth is pushed into -100 dB
# territory by spending far more of the available tap budget than
# Phase 10-prep did.
#
# STAGE 3 is the critical one. Phase 10-prep used 97 taps with
# pb=10 kHz / sb=31.25 kHz, giving only -25 dB rejection at 25 kHz
# offset -- and 25 kHz aliases to 6.25 kHz after LsmDecimator2's
# naive /2, landing right on the P25 channel center. The v2 design
# spends the full FIR4DSP 256-tap budget on a much sharper stage 3
# with a narrower passband (matching SDRTrunk's baseband LPF pb
# edge at 7.25 kHz) and a -110 dB stopband target. This makes the
# 15.625-31.25 kHz fold-back band much steeper, pushing mid-band
# rejection from -25 dB to >-60 dB at 25 kHz.


@dataclass
class StageSpec:
    name: str
    fs: int              # input sample rate for this stage
    decim: int
    passband_hz: float   # end of passband
    stopband_hz: float   # start of stopband
    stopband_db: float   # required stopband attenuation (dB)
    ripple_db: float = 0.01


STAGE1 = StageSpec(
    name='stage1',
    fs=FS_IN,              # 8 MHz
    decim=DEC1,            # /4 -> 2 MHz
    # Narrower passband than Phase 10-prep (was 300 kHz) -- still
    # leaves plenty of NCO tune range at the 8 MHz input rate.
    # stopband_db controls the remez weight ratio, NOT the final
    # depth. At max_taps=256 the actual depth comes out much
    # deeper than the target. Keep the target moderate (100 dB)
    # so the weight ratio stays numerically stable for scipy's
    # double-precision remez; any higher and remez returns
    # garbage at the full 256-tap budget (tested 2026-04-15).
    passband_hz=200_000,
    stopband_hz=1_000_000,  # = output Nyquist, anti-alias anchor
    stopband_db=100.0,
    ripple_db=0.01,
)

STAGE2 = StageSpec(
    name='stage2',
    fs=FS_AFTER_S1,        # 2 MHz
    decim=DEC2,            # /4 -> 500 kHz
    # Narrower passband than Phase 10-prep (was 100 kHz). Still
    # much wider than the P25 channel ±6.25 kHz. Same moderate
    # stopband_db treatment as stage 1 to keep remez's weight
    # ratio stable.
    passband_hz=60_000,
    stopband_hz=250_000,   # = output Nyquist
    stopband_db=100.0,
    ripple_db=0.01,
)

STAGE3 = StageSpec(
    name='stage3',
    fs=FS_AFTER_S2,        # 500 kHz
    decim=DEC3,            # /8 -> 62.5 kHz
    # Passband tightened to 7.25 kHz (exact match with SDRTrunk's
    # baseband LPF passband edge; the downstream LsmFir LPF then
    # cleans up the final 7.25-8.0 kHz knee). Stopband at input
    # Nyquist 31.25 kHz is the hard anti-alias constraint for /8
    # decimation. Stopband target is deliberately set deep
    # (-150 dB) so remez spends the *full* FIR4DSP 256-tap budget
    # on a very steep equiripple transition. The budget isn't
    # spent on achieving -150 dB stopband per se (we'd be happy
    # with -100); it's spent to drive mid-transition rejection at
    # 15-25 kHz down as far as the tap count allows, because that
    # is the band LsmDecimator2 folds onto the P25 channel.
    passband_hz=7_250,
    stopband_hz=31_250,
    # Stage 3 is the tight transition (4.8% of fs) and remez stays
    # numerically stable even at very high weight ratios. A very
    # deep stopband_db target drives remez to push the equiripple
    # mid-transition slope down more steeply, which is exactly
    # what we want at 25 kHz (the fold-back hotspot).
    stopband_db=220.0,
    ripple_db=0.01,
)


def design_stage(spec: StageSpec, *, max_taps: int) -> np.ndarray:
    """Design a linear-phase equiripple lowpass for ``spec``, using
    scipy.signal.remez. **Always spends the full tap budget** --
    returns the max_taps-tap design so we get the sharpest
    equiripple transition the coefficient RAM can hold.

    ``spec.stopband_db`` is only used to set the remez stopband/
    passband weight ratio. It is NOT used as an early-exit target;
    a tap-count-based loop would stop early on a loose spec and
    that leaves rejection on the table.

    Returns the float coefficients (unit DC gain; will be
    Q1.17-quantised later without any rescaling).
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
        # maxiter bump: tight 0.01 dB specs can take >40 iters to
        # converge on some tap counts; let scipy's default maxiter
        # handle most, but signal failure with a RuntimeError that
        # we catch below.
        try:
            return remez(n, bands, desired, weight=weight, fs=spec.fs)
        except ValueError:
            return None

    # Strategy: try remez at exactly max_taps first, because the
    # sharpest equiripple design for a given (pb, sb, ratio) is
    # always at the highest tap count that fits. If remez fails or
    # returns a garbage design at max_taps, fall back to the
    # nearest smaller tap count.
    #
    # Budget-aligned tap count rule: max_taps is a hard ceiling from
    # the FIR primitive's coefficient RAM (256 for FIR4DSP, 128 for
    # FIR2DSP). The polyphase-divisibility padding happens in
    # ensure_decim_divisible() after this function returns.
    #
    # "garbage design" = scipy.signal.remez sometimes returns
    # without raising an exception but with a filter whose passband
    # ripple is >> 1 dB and whose stopband is near 0 dB. This
    # happens on some extreme tap counts for specific band/weight
    # combinations and is not documented in scipy. We detect it by
    # evaluating the returned filter's response and rejecting any
    # design with passband ripple > 1 dB or stopband above -60 dB.

    def evaluate(taps_candidate):
        w, h = freqz(taps_candidate, worN=8192, fs=spec.fs)
        stopband_mask = w >= spec.stopband_hz
        if not np.any(stopband_mask):
            raise RuntimeError(
                f"{spec.name}: stopband starts above fs/2")
        worst_sb_local = 20 * np.log10(
            np.max(np.abs(h[stopband_mask])) + 1e-30)
        pb_mask = w <= spec.passband_hz
        pb_mag = np.abs(h[pb_mask])
        pb_ripple_local = 20 * np.log10(
            pb_mag.max() / pb_mag.min() + 1e-30)
        return worst_sb_local, pb_ripple_local

    attempt = max_taps
    # Step down by decim so every attempted length is polyphase-
    # compatible without any post-hoc padding.
    step = spec.decim
    min_attempt = max(15, 4 * spec.decim)
    while attempt >= min_attempt:
        taps = try_design(attempt)
        if taps is not None:
            worst_sb, pb_ripple = evaluate(taps)
            # Accept the design only if it is a real filter, not a
            # remez-returned-garbage artefact. Thresholds here are
            # intentionally loose -- any real equiripple design will
            # easily meet them; the check is only to catch bad
            # numerical behaviour at extreme tap counts.
            garbage = (pb_ripple > 1.0) or (worst_sb > -60)
            if garbage:
                print(
                    f"[{spec.name}] N={attempt:4d}  "
                    f"remez returned garbage (sb={worst_sb:+.1f} dB, "
                    f"pb_ripple={pb_ripple:.2f} dB), stepping down")
            else:
                print(
                    f"[{spec.name}] N={attempt:4d}  "
                    f"stopband={worst_sb:+7.2f} dB  "
                    f"passband_ripple={pb_ripple:.4f} dB  "
                    f"pb=[0,{spec.passband_hz/1e3:.2f}] kHz  "
                    f"sb=[{spec.stopband_hz/1e3:.2f},"
                    f"{spec.fs/2e3:.0f}] kHz")
                return taps
        else:
            print(
                f"[{spec.name}] N={attempt:4d}  remez did not "
                f"converge, stepping down")
        attempt -= step
    raise RuntimeError(
        f"{spec.name}: no valid design found from {max_taps} down "
        f"to {min_attempt}")


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


def stage_dc_gain_q17(quantised: list[int]) -> int:
    """Return the DC gain of a quantised tap list in Q1.17 integer
    units (i.e. the sum of the raw integer taps). For a unit-DC-gain
    float filter the quantised sum is approximately ``1 << COEFF_FRAC``
    = 131072; deviations are second-order quantisation drift.
    """
    return int(sum(quantised))


def output_shift_for_gain(dc_gain_q17: int) -> int:
    """Given the DC gain of a single Q1.17 FIR stage (sum of int
    coefficients), return the number of right-shift bits the
    stage's MACC output needs to land back at a unit-gain scale.

    For a unit-DC-gain stage the DC gain is ~131072 = 1 << 17, so
    the shift is exactly 17. A well-designed unit-gain equiripple
    filter lands this almost exactly (within a handful of LSBs of
    131072). If the measured gain drifts significantly from 2^17
    it means the filter is not actually unit-DC-gain -- warn the
    caller rather than silently rounding to the wrong shift.
    """
    if dc_gain_q17 <= 0:
        raise ValueError(f"non-positive DC gain: {dc_gain_q17}")
    log2 = math.log2(dc_gain_q17)
    shift = int(round(log2))
    # The shift should be exactly 17 for a unit-gain Q1.17 filter.
    # Drift of more than 0.1 bit means the filter sum is not
    # approximately 1.0 -- this is a design bug, not a quantisation
    # rounding effect.
    if abs(log2 - shift) > 0.1:
        print(
            f"WARNING: non-unit DC gain {dc_gain_q17} "
            f"(log2 = {log2:.3f}) -- filter is not unit-gain")
    return shift


def peak_q17_utilization(quantised: list[int]) -> float:
    """Return the largest |tap| as a fraction of the Q1.17 range.
    For unit-DC-gain designs this is typically 0.3-0.6 (well below
    saturation), which is the *point* -- we no longer force the
    peak tap to exactly 131071 like the Phase 10-prep rescale did.
    """
    peak = max(abs(c) for c in quantised)
    return peak / COEFF_MAX


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
    """Sample the cascaded |H| at critical offsets and print a table
    of rejection in dB relative to DC. Marks:
      PASS   -- in the P25 channel ±6.25 kHz, should be ~0 dB
      FOLD   -- in the LsmDecimator2 fold-back band 15.625-31.25 kHz;
                these aliases land onto the P25 channel after the
                downstream /2, so they must be ≤-60 dB ideally
      WEAK   -- in the adjacent-channel region that should be
                deeply rejected but isn't
    """
    mag_db = 20 * np.log10(np.abs(h) + 1e-30)
    dc_gain = mag_db[0]
    print()
    print("Cascaded rejection (dB relative to DC):")
    print("  offset        dBc    note")
    grid = [
        # P25 channel itself
        (0.00, 'PASS'),
        (6.25, 'PASS'),
        # The key -- LsmDecimator2 fold-back band. Everything in
        # 15.625..31.25 kHz folds onto 0..15.625 kHz after /2, and
        # the first few kHz of that fold lands ON the P25 channel.
        (10.00, 'pre-fold'),
        (15.625, 'FOLD start'),
        (18.75, 'FOLD -> 12.5 kHz'),
        (20.00, 'FOLD -> 11.25 kHz'),
        (25.00, 'FOLD -> 6.25 kHz'),
        (28.00, 'FOLD -> 3.25 kHz'),
        (31.25, 'FOLD edge'),
        # Beyond fold-back, into stage-3 stopband proper
        (50.00, 'stage3 sb'),
        (100.0, 'stage3 sb'),
        (200.0, 'stage2 sb'),
        (500.0, 'stage2 sb'),
        (1_000.0, 'stage1 sb'),
        (2_000.0, 'stage1 sb'),
        (3_500.0, 'stage1 sb'),
    ]
    for off_khz, note in grid:
        f_hz = off_khz * 1e3
        idx = np.argmin(np.abs(w - f_hz))
        rel = mag_db[idx] - dc_gain
        marker = ''
        if note.startswith('PASS'):
            marker = '   PASS'
        elif note.startswith('FOLD'):
            marker = '   FOLD' if rel > -60 else '   FOLD-ok'
        elif rel > -60:
            marker = '   WEAK'
        print(f"  {off_khz:8.3f} kHz  {rel:+8.2f}  {note}{marker}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        '--plot', action='store_true',
        help='Plot per-stage + cascaded responses (requires matplotlib)')
    args = ap.parse_args()

    print("=" * 72)
    print("P25DDC fork filter design (v2, SDRTrunk-faithful, unit DC gain)")
    print("=" * 72)
    print(f"Total decimation: /{TOTAL_DECIM}  ({FS_IN/1e6:.1f} MSPS -> "
          f"{FS_OUT/1e3:.1f} kSPS)")
    print(f"Stage plan:       /{DEC1} /{DEC2} /{DEC3}  "
          f"(FIR4DSP / FIR2DSP / FIR4DSP)")
    print(f"Coefficient format: Q1.{COEFF_FRAC} "
          f"(signed {COEFF_WIDTH}-bit), unit DC gain")
    print(f"Ripple target:      0.01 dB passband (SDRTrunk standard)")
    print()
    print("Per-stage equiripple Parks-McClellan designs:")
    print()

    taps1 = design_stage(STAGE1, max_taps=MAX_TAPS_FIR4DSP)
    taps2 = design_stage(STAGE2, max_taps=MAX_TAPS_FIR2DSP)
    taps3 = design_stage(STAGE3, max_taps=MAX_TAPS_FIR4DSP)

    # NOTE: Phase 10-prep rescaled coefficients so the peak tap
    # landed at Q1.17 max (131071), implicitly baking a non-unit
    # per-stage DC gain into the filter shape. v2 keeps unit DC
    # gain -- the per-stage output_shift register (computed below)
    # becomes the explicit gain knob instead of a hidden coupling
    # between coefficient values and macc_trunc. Exactly one bake
    # cycle (2026-04-15) was lost to that coupling; don't repeat.

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

    # Quantise first so the DC-gain analysis reports what actually
    # lands in the coefficient RAM, not the float ideal.
    q1 = quantise(taps1)
    q2 = quantise(taps2)
    q3 = quantise(taps3)

    # ── Per-stage DC-gain analysis ─────────────────────────────────
    # For a unit-DC-gain float filter, sum(taps) = 1.0 and the
    # Q1.17 quantised sum is ~131072 = 1 << 17. The output_shift
    # is then log2(sum) ≈ 17 for each stage, and the cascaded
    # output gain is 1.0.
    g1 = stage_dc_gain_q17(q1)
    g2 = stage_dc_gain_q17(q2)
    g3 = stage_dc_gain_q17(q3)
    s1 = output_shift_for_gain(g1)
    s2 = output_shift_for_gain(g2)
    s3 = output_shift_for_gain(g3)
    print()
    print("Per-stage DC gain (unit-gain target is 131072 = 1<<17):")
    print(f"  stage1 DC gain = {g1:>7d}  "
          f"(shift={s1}, peak_util={peak_q17_utilization(q1)*100:5.1f}%)")
    print(f"  stage2 DC gain = {g2:>7d}  "
          f"(shift={s2}, peak_util={peak_q17_utilization(q2)*100:5.1f}%)")
    print(f"  stage3 DC gain = {g3:>7d}  "
          f"(shift={s3}, peak_util={peak_q17_utilization(q3)*100:5.1f}%)")
    cascaded_gain_float = (g1 * g2 * g3) / float(1 << (3 * COEFF_FRAC))
    print(f"  cascaded DC gain (linear, post output_shift): "
          f"{cascaded_gain_float:.4f}")

    # Cascaded response analysis.
    w, h = cascaded_response(taps1, taps2, taps3)
    evaluate_adjacent_channels(w, h)

    print()
    print("=" * 72)
    print("Rust constants and coefficient tables for p25-httpd/src/fpga.rs:")
    print("=" * 72)
    print()
    print(f"// P25DDC fork v2: SDRTrunk-faithful, unit DC gain.")
    print(f"const P25_DEC1: usize = {DEC1};")
    print(f"const P25_DEC2: usize = {DEC2};")
    print(f"const P25_DEC3: usize = {DEC3};")
    print()
    print(f"// Per-stage output_shift (derived from Q1.17 DC gain).")
    print(f"// Load into the new ddc_output_shift{{1,2,3}} register")
    print(f"// fields when they land in p25_top.py.")
    print(f"const P25_OUTPUT_SHIFT1: u32 = {s1};  "
          f"// stage1 DC gain = {g1}")
    print(f"const P25_OUTPUT_SHIFT2: u32 = {s2};  "
          f"// stage2 DC gain = {g2}")
    print(f"const P25_OUTPUT_SHIFT3: u32 = {s3};  "
          f"// stage3 DC gain = {g3}")
    print()
    print(format_rust_array(
        'P25_FIR1_COEFFS',
        f'Stage 1 (FIR4DSP): {len(q1)} taps, '
        f'pb={STAGE1.passband_hz/1e3:.0f} kHz, '
        f'sb={STAGE1.stopband_hz/1e3:.0f} kHz, '
        f'-{STAGE1.stopband_db:.0f} dB PM equiripple, unit DC gain',
        q1))
    print()
    print(format_rust_array(
        'P25_FIR2_COEFFS',
        f'Stage 2 (FIR2DSP): {len(q2)} taps, '
        f'pb={STAGE2.passband_hz/1e3:.0f} kHz, '
        f'sb={STAGE2.stopband_hz/1e3:.0f} kHz, '
        f'-{STAGE2.stopband_db:.0f} dB PM equiripple, unit DC gain',
        q2))
    print()
    print(format_rust_array(
        'P25_FIR3_COEFFS',
        f'Stage 3 (FIR4DSP): {len(q3)} taps, '
        f'pb={STAGE3.passband_hz/1e3:.2f} kHz, '
        f'sb={STAGE3.stopband_hz/1e3:.2f} kHz, '
        f'-{STAGE3.stopband_db:.0f} dB PM equiripple, unit DC gain',
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
