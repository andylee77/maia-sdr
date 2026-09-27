#!/usr/bin/env python3
"""
p25_ddc_filter_design.py -- P25DDC filter design, multi-preset sweep.

Designs the coefficients for the 3-stage DDC used by the Fishball P25
control + traffic chains, across a fixed table of sample-rate presets.
Every preset produces the same 50 kSPS DDC output (post-2026-05-03
retune; see FS_OUT below). The downstream LsmDecimator2 /2 takes the
50 kSPS to 25 kSPS, matching SDRTrunk's effective LSM front-end rate.
Only the AD9361 ADC rate and the per-stage decimation factors change
across presets.

The 8M preset is the original P25DDC v2 design (doc/changes/041) and
is bit-identical to what landed in 2026-04-15. All other presets are
parameterised extensions of the same design philosophy:

  * Stage 3 is always the critical anti-alias for LsmDecimator2's
    naive /2 fold-back. Passband 7.25 kHz (SDRTrunk baseband LPF
    edge), stopband 25 kHz (= output Nyquist at 50 kSPS), driven hard by a
    220 dB weight target so remez spends the full 256-tap budget on
    the steepest equiripple transition.

  * Stage 2 is a moderate anti-alias against stage 3's input
    Nyquist. Stopband = fs/(2*d1*d2) (= stage 2 output Nyquist),
    passband = min(60 kHz, stopband/4).

  * Stage 1 is a loose anti-alias against stage 2's input Nyquist.
    Stopband = fs/(2*d1), passband = min(200 kHz, stopband/5).

All coefficients are unit-DC-gain Q1.17 integers. No peak-rescaling.

Usage
-----
Regenerate the whole preset table:
    python tools/p25_ddc_filter_design.py \\
        --emit-rs p25-httpd/src/hardware/ddc_presets.rs

Analyse one preset in detail (adjacent-channel rejection table, DC
gain report, optional matplotlib plot):
    python tools/p25_ddc_filter_design.py --preset 8M --analyse
    python tools/p25_ddc_filter_design.py --preset 4M --analyse --plot

Rust consumers
--------------
The emitted `ddc_presets.rs` exposes a `DdcPreset` struct, one
`PRESET_<name>` const per feasible preset, and a `PRESETS` slice.
`p25-httpd/src/hardware/fpga.rs::configure_ddc()` takes a
`&DdcPreset` and loads the matching coefficient tables + per-stage
decimation + `operations_minus_one` into the HDL DDC coefficient RAM.

SPDX-License-Identifier: MIT
"""

from __future__ import annotations

import argparse
import math
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from scipy.signal import remez, freqz, kaiserord


# ── Global constants ──────────────────────────────────────────────

# 2026-05-03: dropped DDC output rate from 62.5 kSPS to 50 kSPS so
# the downstream LsmDecimator2 /2 lands at exactly 25 kSPS — matching
# SDRTrunk's LSM decoder front-end rate (sps = 25000/4800 ≈ 5.21).
# Per-preset (d1, d2, d3) factorizations are recomputed accordingly;
# stage 3 always carries the new /5 factor (smallest filter cost,
# cleanest transition band).
FS_OUT = 50_000            # = 10.42 samples/symbol pre-/2; 5.21 post-/2

# Fixed-point coefficient format (matches maia_hdl DDC):
#   18-bit signed, Q1.17. Max positive = 2^17 - 1 = 131071.
COEFF_WIDTH = 18
COEFF_FRAC = COEFF_WIDTH - 1                       # 17
COEFF_MAX = (1 << COEFF_FRAC) - 1                  # 131071
COEFF_MIN = -(1 << COEFF_FRAC)                     # -131072

# FIR4DSP / FIR2DSP RAM limits from maia_hdl/fir.py. The *usable*
# tap count depends on the decimation factor because fpga.rs's
# load_fir_*dsp() enforces `operations * decimation <= NUM_ADDR/2`
# (FIR4DSP) or `operations * decimation <= NUM_ADDR` (FIR2DSP).
# See max_taps_fir4dsp() / max_taps_fir2dsp() below.


def max_taps_fir4dsp(decim: int) -> int:
    """Largest tap count a FIR4DSP stage can hold for a given
    decimation. Folded polyphase layout needs
    `ceil(branch_len/2) * decim <= 128` where
    `branch_len = ceil(taps/decim)`. We constrain branch_len to be
    EVEN so the folding math is exact, giving
    `max_taps = 2 * (128 // decim) * decim`.
    """
    return 2 * (128 // decim) * decim


def max_taps_fir2dsp(decim: int) -> int:
    """Largest tap count a FIR2DSP stage can hold for a given
    decimation. No folding; `operations = branch_len`, constraint is
    `branch_len * decim <= 128`.
    """
    return (128 // decim) * decim


# ── Preset table ──────────────────────────────────────────────────
#
# (name, sample_rate_hz, d1, d2, d3). Every entry must satisfy
# d1*d2*d3 = sample_rate_hz / FS_OUT  with FS_OUT = 50 kSPS.
#
# 2026-05-03 retune: every preset now ends in d3=5. Stage 3's input
# rate (and therefore its filter sharpness budget) is unchanged
# vs the prior 62.5 kSPS table — only the decim factor moves from
# 4 (or 8 for 8M) to 5, and stage 3's stopband moves from 31.25 kHz
# to 25 kHz (the new fs_out/2). The previous bit-identical-to-2026
# -04-15 8M preset is intentionally retired; the new design is
# validated by the same `rejection_25k_db` build-time threshold.

PRESETS = [
    ("2M",   2_000_000,  2, 4, 5),
    ("3M",   3_000_000,  3, 4, 5),
    ("4M",   4_000_000,  4, 4, 5),
    ("5M",   5_000_000,  5, 4, 5),
    ("6M",   6_000_000,  6, 4, 5),
    ("7M",   7_000_000,  7, 4, 5),
    ("8M",   8_000_000,  8, 4, 5),
    ("9M",   9_000_000,  9, 4, 5),
    ("10M", 10_000_000, 10, 4, 5),
    ("12M", 12_000_000, 12, 4, 5),
    ("16M", 16_000_000, 16, 4, 5),
]


@dataclass
class StageSpec:
    name: str
    fs: int
    decim: int
    passband_hz: float
    stopband_hz: float
    stopband_db: float
    ripple_db: float = 0.01


def stages_for_preset(fs: int, d1: int, d2: int, d3: int):
    """Return (stage1, stage2, stage3) StageSpecs for a given preset.

    Design rules (see module docstring). For the (fs=8e6, d1=d2=4,
    d3=8) case these reproduce the original P25DDC v2 specs exactly.
    """
    # Stage 1 is the widest passband / loosest transition.
    stage1_sb = fs // (2 * d1)
    stage1_pb = min(200_000, stage1_sb // 5)
    stage1 = StageSpec(
        name='stage1',
        fs=fs,
        decim=d1,
        passband_hz=stage1_pb,
        stopband_hz=stage1_sb,
        stopband_db=100.0,
    )

    # Stage 2 anchors at its own output Nyquist.
    stage2_fs = fs // d1
    stage2_sb = stage2_fs // (2 * d2)
    stage2_pb = min(60_000, stage2_sb // 4)
    stage2 = StageSpec(
        name='stage2',
        fs=stage2_fs,
        decim=d2,
        passband_hz=stage2_pb,
        stopband_hz=stage2_sb,
        stopband_db=100.0,
    )

    # Stage 3 is the critical anti-alias for LsmDecimator2's fold-back.
    # Passband + stopband are ABSOLUTE frequencies (same across all
    # presets); only the input rate changes.
    # 2026-05-03: stopband moved from 31_250 (old fs_out/2 @ 62.5 kSPS)
    # to 25_000 (new fs_out/2 @ 50 kSPS) so the LsmDecimator2 /2 lands
    # at 25 kSPS without fold-back.
    stage3_fs = stage2_fs // d2
    stage3 = StageSpec(
        name='stage3',
        fs=stage3_fs,
        decim=d3,
        passband_hz=7_250,
        stopband_hz=25_000,
        stopband_db=220.0,
    )
    return stage1, stage2, stage3


# ── Filter design primitives (unchanged from v2) ──────────────────

def design_stage(spec: StageSpec, *, max_taps: int):
    """Design a linear-phase equiripple lowpass for ``spec``, spending
    the full tap budget. ``spec.stopband_db`` is only used to set the
    remez passband/stopband weight ratio, not as an early-exit target.
    Returns (taps_float, info_dict) or (None, info_dict_with_error).
    """
    width_hz = spec.stopband_hz - spec.passband_hz
    delta_p = 10 ** (spec.ripple_db / 20.0) - 1
    delta_s = 10 ** (-spec.stopband_db / 20.0)
    ripple_db = -20 * math.log10(min(delta_p, delta_s))
    numtaps_start, _ = kaiserord(ripple_db, width_hz / (spec.fs / 2))
    n_taps = max(15, int(numtaps_start * 0.8) | 1)

    def try_design(n):
        bands = [0.0, spec.passband_hz, spec.stopband_hz, spec.fs / 2]
        desired = [1.0, 0.0]
        weight = [1.0, delta_p / delta_s]
        try:
            return remez(n, bands, desired, weight=weight, fs=spec.fs)
        except ValueError:
            return None

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
    step = spec.decim
    min_attempt = max(15, 4 * spec.decim)
    while attempt >= min_attempt:
        taps = try_design(attempt)
        if taps is not None:
            try:
                worst_sb, pb_ripple = evaluate(taps)
            except RuntimeError as e:
                return None, {"error": str(e)}
            garbage = (pb_ripple > 1.0) or (worst_sb > -60)
            if not garbage:
                return taps, {
                    "n_taps": attempt,
                    "stopband_db": float(worst_sb),
                    "passband_ripple_db": float(pb_ripple),
                }
        attempt -= step
    return None, {
        "error": f"no valid design from {max_taps} down to {min_attempt}",
    }


def ensure_decim_divisible(taps: np.ndarray, decim: int):
    rem = len(taps) % decim
    if rem == 0:
        return taps
    pad = decim - rem
    return np.concatenate([taps, np.zeros(pad)])


def quantise(taps: np.ndarray):
    scale = 1 << COEFF_FRAC
    q = np.clip(
        np.round(taps * scale), COEFF_MIN, COEFF_MAX,
    ).astype(int)
    return q.tolist()


def peak_q17_utilization(quantised: list[int]) -> float:
    peak = max(abs(c) for c in quantised)
    return peak / COEFF_MAX


def cascaded_response(taps1, taps2, taps3, d1, d2, fs_in, n_points=16384):
    """Cascaded frequency response at the AD9361 input rate."""
    def upsample(taps, factor):
        out = np.zeros(len(taps) * factor)
        out[::factor] = taps
        return out

    up2 = upsample(taps2, d1)
    up3 = upsample(taps3, d1 * d2)
    cascaded = np.convolve(np.convolve(taps1, up2), up3)
    w, h = freqz(cascaded, worN=n_points, fs=fs_in)
    return w, h


def evaluate_adjacent_channels(w, h):
    mag_db = 20 * np.log10(np.abs(h) + 1e-30)
    dc_gain = mag_db[0]
    print()
    print("Cascaded rejection (dB relative to DC):")
    print("  offset        dBc    note")
    grid = [
        (0.00, 'PASS'),
        (6.25, 'PASS'),
        (10.00, 'pre-fold'),
        (15.625, 'FOLD start'),
        (18.75, 'FOLD -> 12.5 kHz'),
        (20.00, 'FOLD -> 11.25 kHz'),
        (25.00, 'FOLD -> 6.25 kHz'),
        (28.00, 'FOLD -> 3.25 kHz'),
        (31.25, 'FOLD edge'),
        (50.00, 'stage3 sb'),
        (100.0, 'stage3 sb'),
        (200.0, 'stage2 sb'),
        (500.0, 'stage2 sb'),
        (1_000.0, 'stage1 sb'),
        (2_000.0, 'stage1 sb'),
    ]
    for off_khz, note in grid:
        f_hz = off_khz * 1e3
        if f_hz > w.max():
            continue
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


# ── Per-preset design pipeline ────────────────────────────────────

def design_preset(name, fs, d1, d2, d3, *, verbose=False):
    """Design all three FIR stages for a preset. Returns a dict with
    quantised coefficients and per-stage metrics, or None if any
    stage is infeasible within the tap budget.
    """
    stage1, stage2, stage3 = stages_for_preset(fs, d1, d2, d3)
    taps1, info1 = design_stage(stage1, max_taps=max_taps_fir4dsp(d1))
    taps2, info2 = design_stage(stage2, max_taps=max_taps_fir2dsp(d2))
    taps3, info3 = design_stage(stage3, max_taps=max_taps_fir4dsp(d3))
    if verbose:
        for s, info in [(stage1, info1), (stage2, info2), (stage3, info3)]:
            if "error" in info:
                print(f"[{name}/{s.name}] FAILED: {info['error']}")
            else:
                print(
                    f"[{name}/{s.name}] N={info['n_taps']:3d} "
                    f"stopband={info['stopband_db']:+7.2f} dB  "
                    f"pb={s.passband_hz/1e3:.2f} kHz "
                    f"sb={s.stopband_hz/1e3:.2f} kHz"
                )
    if taps1 is None or taps2 is None or taps3 is None:
        return None
    taps1 = ensure_decim_divisible(taps1, d1)
    taps2 = ensure_decim_divisible(taps2, d2)
    taps3 = ensure_decim_divisible(taps3, d3)
    q1, q2, q3 = quantise(taps1), quantise(taps2), quantise(taps3)

    # Adjacent-channel rejection at 25 kHz (the LsmDecimator2 fold-back
    # hotspot) must stay below -55 dB. This is the single hardest
    # constraint; if we fail it the preset is not safe to ship.
    w, h = cascaded_response(taps1, taps2, taps3, d1, d2, fs)
    mag_db = 20 * np.log10(np.abs(h) + 1e-30)
    dc_gain = mag_db[0]
    idx_25k = np.argmin(np.abs(w - 25_000))
    rejection_25k_db = float(mag_db[idx_25k] - dc_gain)

    feasible = rejection_25k_db <= -55.0
    return {
        "name": name,
        "fs": fs,
        "d1": d1, "d2": d2, "d3": d3,
        "q1": q1, "q2": q2, "q3": q3,
        "taps1_float": taps1, "taps2_float": taps2, "taps3_float": taps3,
        "stopband_db_s1": info1.get("stopband_db"),
        "stopband_db_s2": info2.get("stopband_db"),
        "stopband_db_s3": info3.get("stopband_db"),
        "peak_util_s1": peak_q17_utilization(q1),
        "peak_util_s2": peak_q17_utilization(q2),
        "peak_util_s3": peak_q17_utilization(q3),
        "rejection_25k_db": rejection_25k_db,
        "feasible": feasible,
    }


# ── Rust emission ─────────────────────────────────────────────────

def format_coeff_block(name: str, coeffs: list[int]) -> str:
    lines = [f"#[rustfmt::skip]", f"const {name}: &[i32] = &["]
    for i in range(0, len(coeffs), 8):
        chunk = coeffs[i:i + 8]
        body = ", ".join(f"{c:>7d}" for c in chunk)
        lines.append(f"    {body},")
    lines.append("];")
    return "\n".join(lines)


def emit_rust_module(presets_data: list[dict]) -> str:
    """Build the full ddc_presets.rs file contents."""
    out: list[str] = []
    out.append("// SPDX-License-Identifier: MIT")
    out.append("//")
    out.append("// AUTO-GENERATED by tools/p25_ddc_filter_design.py.")
    out.append("// DO NOT EDIT BY HAND. Rerun the script to regenerate.")
    out.append("//")
    out.append("// P25DDC v2 multi-preset coefficient tables. Every preset")
    out.append("// produces 50 kSPS at the DDC output so the downstream")
    out.append("// LsmDecimator2 /2 + LsmFir(LPF_TAPS_25K, RRC_TAPS_25K) chain")
    out.append("// is identical across presets and lands at 25 kSPS at the")
    out.append("// LSM front end (matching SDRTrunk's effective LSM rate).")
    out.append("// 2026-05-03 retune; the prior 62.5 kSPS bit-identical-2026")
    out.append("// -04-15 8M preset is intentionally retired.")
    out.append("")
    out.append("#![allow(dead_code)]")
    out.append("")
    out.append("/// One DDC preset: AD9361 sample-rate choice + matching")
    out.append("/// 3-stage FIR coefficient tables + decimation factors.")
    out.append("/// `sample_rate_hz / (decim1 * decim2 * decim3)` is always")
    out.append("/// 50 000 Hz by construction (post-2026-05-03 retune).")
    out.append("pub struct DdcPreset {")
    out.append("    pub name: &'static str,")
    out.append("    pub sample_rate_hz: u32,")
    out.append("    pub rf_bandwidth_hz: u32,")
    out.append("    pub decim1: usize,")
    out.append("    pub decim2: usize,")
    out.append("    pub decim3: usize,")
    out.append("    pub fir1_coeffs: &'static [i32],")
    out.append("    pub fir2_coeffs: &'static [i32],")
    out.append("    pub fir3_coeffs: &'static [i32],")
    out.append("    /// Worst-case cascaded rejection at 25 kHz offset from the")
    out.append("    /// channel center (the LsmDecimator2 fold-back hotspot).")
    out.append("    /// Ships below -55 dB by build-time enforcement.")
    out.append("    pub rejection_25k_db: f32,")
    out.append("}")
    out.append("")
    out.append("/// Total DDC decimation for this preset.")
    out.append("impl DdcPreset {")
    out.append("    pub const fn total_decim(&self) -> usize {")
    out.append("        self.decim1 * self.decim2 * self.decim3")
    out.append("    }")
    out.append("    /// Half of the addressable NCO window (= sample_rate/2).")
    out.append("    pub const fn nco_half_window_hz(&self) -> u32 {")
    out.append("        self.sample_rate_hz / 2")
    out.append("    }")
    out.append("}")
    out.append("")
    for p in presets_data:
        n = p["name"]
        out.append(f"// ── {n} preset ─────────────────────────────────")
        out.append(
            f"// {p['fs']/1e6:.3f} MSPS → 50 kSPS "
            f"(/{p['d1']}/{p['d2']}/{p['d3']} = /{p['d1']*p['d2']*p['d3']})"
        )
        out.append(
            f"// stage stopbands: "
            f"s1={p['stopband_db_s1']:+.1f} dB, "
            f"s2={p['stopband_db_s2']:+.1f} dB, "
            f"s3={p['stopband_db_s3']:+.1f} dB"
        )
        out.append(
            f"// cascaded 25 kHz rejection: "
            f"{p['rejection_25k_db']:+.1f} dB (fold-back hotspot)"
        )
        out.append("")
        out.append(format_coeff_block(f"P25_FIR1_{n}", p["q1"]))
        out.append("")
        out.append(format_coeff_block(f"P25_FIR2_{n}", p["q2"]))
        out.append("")
        out.append(format_coeff_block(f"P25_FIR3_{n}", p["q3"]))
        out.append("")
        out.append(f"pub const PRESET_{n}: DdcPreset = DdcPreset {{")
        out.append(f"    name: \"{n}\",")
        out.append(f"    sample_rate_hz: {p['fs']},")
        # rf_bandwidth = sample_rate by default (AD9361 filter width
        # should track the DDC input rate).
        out.append(f"    rf_bandwidth_hz: {p['fs']},")
        out.append(f"    decim1: {p['d1']},")
        out.append(f"    decim2: {p['d2']},")
        out.append(f"    decim3: {p['d3']},")
        out.append(f"    fir1_coeffs: P25_FIR1_{n},")
        out.append(f"    fir2_coeffs: P25_FIR2_{n},")
        out.append(f"    fir3_coeffs: P25_FIR3_{n},")
        out.append(f"    rejection_25k_db: {p['rejection_25k_db']:.2f},")
        out.append("};")
        out.append("")
    out.append("/// All feasible presets, in ascending sample-rate order.")
    out.append("pub const PRESETS: &[&DdcPreset] = &[")
    for p in presets_data:
        out.append(f"    &PRESET_{p['name']},")
    out.append("];")
    out.append("")
    out.append("/// Default preset at boot / when CLI doesn't override.")
    out.append("/// Matches the validated 2026-04-15 configuration.")
    out.append("pub const DEFAULT_PRESET: &DdcPreset = &PRESET_8M;")
    out.append("")
    out.append("/// Look up a preset by its public name (`\"2M\"`, `\"8M\"`, …).")
    out.append("pub fn find_preset(name: &str) -> Option<&'static DdcPreset> {")
    out.append("    PRESETS.iter().copied().find(|p| p.name == name)")
    out.append("}")
    out.append("")
    return "\n".join(out)


# ── Main ──────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        '--preset', type=str, default=None,
        help='Design only this preset (e.g. "8M"). Implies --analyse.')
    ap.add_argument(
        '--analyse', action='store_true',
        help='Print per-stage metrics, cascaded response table, '
             'and Rust coefficient dumps for each designed preset.')
    ap.add_argument(
        '--emit-rs', type=str, default=None,
        help='Write the full preset table to this Rust source file.')
    ap.add_argument(
        '--plot', action='store_true',
        help='Matplotlib windows for the selected preset (needs --preset).')
    args = ap.parse_args()

    if args.preset is not None:
        selected = [p for p in PRESETS if p[0] == args.preset]
        if not selected:
            names = ', '.join(n for n, *_ in PRESETS)
            print(f"unknown preset '{args.preset}'. Known: {names}",
                  file=sys.stderr)
            return 2
        args.analyse = True
    else:
        selected = PRESETS

    print("=" * 72)
    print("P25DDC v2 preset design")
    print("=" * 72)

    designed: list[dict] = []
    dropped: list[tuple[str, str]] = []
    for name, fs, d1, d2, d3 in selected:
        assert d1 * d2 * d3 * FS_OUT == fs, (
            f"preset {name}: d1*d2*d3 ({d1*d2*d3}) != fs/FS_OUT "
            f"({fs // FS_OUT})"
        )
        print()
        print(f"-- preset {name}: {fs/1e6:.3f} MSPS, /{d1}/{d2}/{d3} "
              f"-> {FS_OUT/1e3:.1f} kSPS")
        p = design_preset(name, fs, d1, d2, d3, verbose=True)
        if p is None:
            dropped.append((name, "stage design infeasible"))
            continue
        if not p["feasible"]:
            dropped.append((
                name,
                f"25 kHz rejection {p['rejection_25k_db']:+.2f} dB "
                f"above -55 dB threshold",
            ))
            print(
                f"    DROP: cascaded 25 kHz rejection "
                f"{p['rejection_25k_db']:+.2f} dB is too shallow."
            )
            continue
        print(
            f"    taps: s1={len(p['q1'])} s2={len(p['q2'])} s3={len(p['q3'])} "
            f"  peak util s1={p['peak_util_s1']*100:.1f}% "
            f"s2={p['peak_util_s2']*100:.1f}% s3={p['peak_util_s3']*100:.1f}%"
        )
        print(
            f"    cascaded rejection @ 25 kHz: "
            f"{p['rejection_25k_db']:+.2f} dB"
        )
        designed.append(p)
        if args.analyse:
            w, h = cascaded_response(
                p["taps1_float"], p["taps2_float"], p["taps3_float"],
                d1, d2, fs,
            )
            evaluate_adjacent_channels(w, h)

    print()
    print("=" * 72)
    print(f"Designed {len(designed)} preset(s); dropped {len(dropped)}.")
    for name, why in dropped:
        print(f"  DROPPED {name}: {why}")

    if args.emit_rs:
        rs_path = Path(args.emit_rs)
        rs_path.parent.mkdir(parents=True, exist_ok=True)
        rs_path.write_text(emit_rust_module(designed), encoding="utf-8")
        print()
        print(f"Wrote {len(designed)} preset(s) to {rs_path}.")

    if args.plot and args.preset:
        try:
            import matplotlib.pyplot as plt
        except ImportError:
            print("\nmatplotlib not available -- skipping --plot")
            return 0
        p = designed[0]
        fig, axs = plt.subplots(4, 1, figsize=(8, 10))
        for ax, (stage_name, taps, fs) in zip(axs[:3], [
            ("stage1", p["taps1_float"], p["fs"]),
            ("stage2", p["taps2_float"], p["fs"] // p["d1"]),
            ("stage3", p["taps3_float"], p["fs"] // (p["d1"] * p["d2"])),
        ]):
            w_s, h_s = freqz(taps, worN=4096, fs=fs)
            ax.plot(w_s / 1e3, 20 * np.log10(np.abs(h_s) + 1e-30))
            ax.set_title(f"{p['name']} {stage_name} -- {len(taps)} taps")
            ax.set_xlabel("kHz")
            ax.set_ylabel("dB")
            ax.grid(True)
            ax.set_ylim(-120, 5)
        w, h = cascaded_response(
            p["taps1_float"], p["taps2_float"], p["taps3_float"],
            p["d1"], p["d2"], p["fs"],
        )
        axs[3].plot(w / 1e6, 20 * np.log10(np.abs(h) + 1e-30))
        axs[3].set_title(
            f"{p['name']} cascaded ({p['fs']/1e6:.1f} MSPS → 50 kSPS)")
        axs[3].set_xlabel("MHz")
        axs[3].set_ylabel("dB")
        axs[3].grid(True)
        axs[3].set_ylim(-120, 5)
        plt.tight_layout()
        plt.show()

    return 0


if __name__ == '__main__':
    sys.exit(main())
