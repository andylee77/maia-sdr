#!/usr/bin/env python3
"""
p25_iq_inspect.py — quick sanity-check on a wideband IQ capture from
/api/wideband_iq_capture (.cs16 raw interleaved i16 little-endian).

What it checks:

  * Sample count vs declared duration (8 MSPS expected).
  * DC offset and per-channel RMS (clipping/saturation, I/Q balance).
  * Top-N spectral peaks (FFT) — should match the in-band P25
    carriers of the active site.
  * Per-second power-floor stability (sample-rate drift / DMA gaps
    show as power dips between segments).

Usage:
  python tools/p25_iq_inspect.py /path/to/wb_iq_*.cs16 [--lo-mhz 858]
                                  [--peaks 20] [--fft 65536]

The --lo-mhz hint just labels the absolute frequencies in the peak
report; everything else works without it.
"""

import argparse
import os
import sys

import numpy as np


SAMPLE_RATE_HZ = 8_000_000
SAMPLE_BYTES = 4   # i16 I + i16 Q


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("path", help="path to .cs16 capture")
    ap.add_argument("--lo-mhz", type=float, default=None,
                    help="AD9361 RX LO in MHz (for absolute peak labels)")
    ap.add_argument("--peaks", type=int, default=20,
                    help="number of FFT peaks to report")
    ap.add_argument("--fft", type=int, default=65536,
                    help="FFT size for spectrum (default 65536, "
                         "~122 Hz/bin at 8 MSPS)")
    args = ap.parse_args()

    if not os.path.exists(args.path):
        print(f"file not found: {args.path}", file=sys.stderr)
        return 1

    size = os.path.getsize(args.path)
    n_samples = size // SAMPLE_BYTES
    duration_s = n_samples / SAMPLE_RATE_HZ
    print(f"=== file ===")
    print(f"  path        {args.path}")
    print(f"  size        {size:,} B")
    print(f"  samples     {n_samples:,} (= {duration_s:.4f} s @ 8 MSPS)")

    # Load as i16 then build complex64. Avoids float64 in case the
    # capture is huge.
    raw = np.fromfile(args.path, dtype=np.int16)
    raw = raw[: 2 * (raw.size // 2)]
    iq16 = raw.reshape(-1, 2)
    n = iq16.shape[0]
    if n == 0:
        print("EMPTY capture")
        return 1

    # Stats on the raw integer streams.
    i_mean = float(iq16[:, 0].mean())
    q_mean = float(iq16[:, 1].mean())
    i_rms = float(np.sqrt(np.mean(iq16[:, 0].astype(np.float64) ** 2)))
    q_rms = float(np.sqrt(np.mean(iq16[:, 1].astype(np.float64) ** 2)))
    i_max_abs = int(np.max(np.abs(iq16[:, 0])))
    q_max_abs = int(np.max(np.abs(iq16[:, 1])))
    # AD9361 12-bit signed → range [-2048, 2047]. Hitting 2047 is rare
    # but not crazy on a strong carrier; sustained near-rails is bad
    # (front-end overdriven).
    print()
    print("=== raw i16 stats ===")
    print(f"  I mean      {i_mean:+.2f}   Q mean      {q_mean:+.2f}")
    print(f"  I rms       {i_rms:7.2f}    Q rms       {q_rms:7.2f}")
    print(f"  I |max|     {i_max_abs:5d}      Q |max|     {q_max_abs:5d}"
          f"   (12-bit AD9361: rails at 2047)")
    if max(i_max_abs, q_max_abs) > 2047:
        print(f"  ! sample exceeds 12-bit signed range — sign-extension "
              f"or packing bug")
    if max(i_max_abs, q_max_abs) < 64:
        print(f"  ! samples very quiet (max abs {max(i_max_abs, q_max_abs)}) "
              f"— front-end may be muted or mistuned")
    iq_balance_db = 20 * np.log10(i_rms / q_rms) if q_rms > 0 else float("inf")
    print(f"  I/Q balance {iq_balance_db:+.2f} dB")
    if abs(iq_balance_db) > 0.5:
        print(f"  ! I/Q gain imbalance > 0.5 dB — verify packer ordering")

    # Build complex baseband (centered at LO).
    samples = iq16[:, 0].astype(np.float32) + 1j * iq16[:, 1].astype(np.float32)

    # Per-second segment stats — flag obvious DMA gaps.
    print()
    print("=== per-second segments ===")
    secs = int(duration_s)
    if secs >= 1:
        segsz = SAMPLE_RATE_HZ
        print(f"  {'sec':>4}  {'rms':>10}  {'pk_re':>6}  {'pk_im':>6}")
        for s in range(secs):
            seg = samples[s * segsz: (s + 1) * segsz]
            rms = float(np.sqrt(np.mean(np.abs(seg) ** 2)))
            pk_re = int(np.max(np.abs(seg.real)))
            pk_im = int(np.max(np.abs(seg.imag)))
            print(f"  {s:>4}  {rms:10.2f}  {pk_re:>6}  {pk_im:>6}")
    else:
        print("  (capture under 1 s — skipping)")

    # FFT peak hunt. Average abs(FFT) over multiple windows for stability.
    print()
    print("=== spectrum (averaged FFTs) ===")
    fft_n = args.fft
    if n < fft_n:
        print(f"  capture shorter than FFT size ({n} < {fft_n})")
        return 0
    n_avg = max(1, min(64, n // fft_n))
    win = np.hanning(fft_n).astype(np.float32)
    win_norm = win.sum()
    accum = np.zeros(fft_n, dtype=np.float64)
    for k in range(n_avg):
        seg = samples[k * fft_n: (k + 1) * fft_n] * win
        f = np.fft.fftshift(np.fft.fft(seg))
        accum += np.abs(f) ** 2
    accum /= n_avg
    psd = accum / (win_norm ** 2)
    # Convert to dB; floor at -200 to avoid -inf. Reference doesn't
    # matter — what we care about is the relative peak structure.
    psd_db = 10.0 * np.log10(psd + 1e-30)
    psd_db -= psd_db.max()  # peak = 0 dB

    bin_hz = SAMPLE_RATE_HZ / fft_n
    freqs_hz = (np.arange(fft_n) - fft_n // 2) * bin_hz

    # Estimate noise floor via median (robust to many narrow peaks).
    median_db = float(np.median(psd_db))
    pct90_db = float(np.percentile(psd_db, 90))
    print(f"  fft_size    {fft_n}  ({bin_hz:.1f} Hz/bin)")
    print(f"  averages    {n_avg}")
    print(f"  median      {median_db:+6.2f} dB     "
          f"90th pct  {pct90_db:+6.2f} dB     peak  0.00 dB")
    print(f"  span        ±{SAMPLE_RATE_HZ / 2 / 1e6:.2f} MHz")

    # Find peaks well above the median floor: simple "local max within
    # a 3-bin window AND >= median + 6 dB" filter, then sort by power.
    threshold_db = max(median_db + 6.0, -60.0)
    peak_candidates = []
    for i in range(2, fft_n - 2):
        v = psd_db[i]
        if v < threshold_db:
            continue
        if v >= psd_db[i-1] and v >= psd_db[i+1] and \
           v >= psd_db[i-2] and v >= psd_db[i+2]:
            peak_candidates.append((v, freqs_hz[i]))
    peak_candidates.sort(key=lambda x: -x[0])
    top = peak_candidates[: args.peaks]

    print()
    print(f"=== top {len(top)} spectral peaks (>= median + 6 dB) ===")
    if args.lo_mhz is not None:
        print(f"  {'rank':>4}  {'rel_dB':>7}  {'offset_kHz':>11}  "
              f"{'abs_MHz':>11}")
    else:
        print(f"  {'rank':>4}  {'rel_dB':>7}  {'offset_kHz':>11}")
    for rank, (db, hz) in enumerate(top, 1):
        line = f"  {rank:>4}  {db:+7.2f}  {hz/1e3:>11.1f}"
        if args.lo_mhz is not None:
            line += f"  {args.lo_mhz + hz/1e6:>11.4f}"
        print(line)

    print()
    print("=== verdict ===")
    issues = []
    if max(i_max_abs, q_max_abs) > 2047:
        issues.append("samples outside 12-bit signed range")
    if max(i_max_abs, q_max_abs) < 64:
        issues.append("very low signal level")
    if abs(iq_balance_db) > 0.5:
        issues.append(f"I/Q gain imbalance ({iq_balance_db:+.2f} dB)")
    if abs(i_mean) > 50 or abs(q_mean) > 50:
        issues.append(f"large DC offset (I={i_mean:+.1f}, Q={q_mean:+.1f})")
    if len(top) < 1:
        issues.append("no spectral peaks above threshold — band quiet?")
    if not issues:
        print("  capture looks healthy.")
    else:
        for it in issues:
            print(f"  ! {it}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
