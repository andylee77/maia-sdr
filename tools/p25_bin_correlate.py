#!/usr/bin/env python3
"""Time-series cross-correlation between per-LCN wideband power and
per-channelizer-bin power. For each known LCN, samples the wideband
spectrum at the LCN frequency over time. For each of 64 channelizer
bins, samples bin energy over time. Then computes Pearson correlation
between every (LCN, bin) pair. The bin with the highest correlation
to a given LCN's time series IS the bin that LCN maps to — no
threshold tuning needed.

Usage:
    python tools/p25_bin_correlate.py [--seconds 120] [--rate 4]
"""

from __future__ import annotations

import argparse
import sys
import time

import numpy as np
import requests


CLAY_LCN_HZ: dict[int, float] = {
    1:  855.2375e6,  2:  856.4375e6,  3:  857.2125e6,  4:  857.4375e6,
    5:  857.9875e6,  6:  858.4375e6,  7:  858.4625e6,  8:  858.9875e6,
    9:  859.4375e6, 10: 860.4375e6, 11: 860.9625e6,
}

DUVAL_LCN_HZ: dict[int, float] = {
    2: 855.4875e6,                                  # control
    14: 857.2375e6, 16: 857.7125e6, 19: 858.7125e6,
    21: 859.4625e6, 26: 860.4625e6, 27: 860.7125e6,
}


def fetch(host: str, path: str) -> dict:
    r = requests.get(host.rstrip("/") + path, timeout=4.0)
    r.raise_for_status()
    return r.json()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="http://192.168.2.1:8080")
    ap.add_argument("--seconds", type=float, default=120.0)
    ap.add_argument("--rate", type=float, default=4.0)
    ap.add_argument("--fft", type=int, default=4096)
    ap.add_argument("--clay-only", action="store_true",
                    help="Only correlate against Clay LCNs (skip Duval)")
    args = ap.parse_args()

    targets: list[tuple[str, int, float]] = (
        [("clay", l, h) for l, h in CLAY_LCN_HZ.items()]
        + ([] if args.clay_only
           else [("duval", l, h) for l, h in DUVAL_LCN_HZ.items()])
    )

    period = 1.0 / args.rate
    n_target = len(targets)

    bin_series: list[list[int]] = [[] for _ in range(64)]
    lcn_series: list[list[float]] = [[] for _ in range(n_target)]
    t_start = time.time()
    print(f"Sampling {n_target} LCNs + 64 bins at {args.rate} Hz "
          f"for {args.seconds:.0f}s...")

    while time.time() - t_start < args.seconds:
        loop_t = time.time()
        try:
            spec = fetch(args.host,
                         f"/api/spectrum_wide?fft_size={args.fft}")
            bins = fetch(args.host, "/api/traffic_bins")
        except Exception as e:
            print(f"  fetch err: {e}", file=sys.stderr)
            time.sleep(period)
            continue

        mag = np.array(spec["mag_db"], dtype=np.float64)
        center = float(spec["center_hz"])
        span = float(spec["span_hz"])
        n = mag.size
        freqs = center + (np.arange(n) - n / 2) * (span / n)
        # Pull mag at each LCN's freq (linear interpolation between
        # the two nearest FFT bins).
        for ti, (_, _, hz) in enumerate(targets):
            idx_f = (hz - freqs[0]) / (span / n)
            i0 = int(idx_f)
            if i0 < 0 or i0 >= n - 1:
                lcn_series[ti].append(-200.0)
                continue
            frac = idx_f - i0
            interp = mag[i0] * (1 - frac) + mag[i0 + 1] * frac
            lcn_series[ti].append(float(interp))

        be = bins["bin_energies"]
        for k in range(64):
            bin_series[k].append(int(be[k]))

        elapsed = time.time() - loop_t
        if elapsed < period:
            time.sleep(period - elapsed)

    n_samp = len(bin_series[0])
    print(f"Captured {n_samp} samples over {time.time()-t_start:.1f}s.")
    print()

    # Convert wideband mag dB to linear power so correlation lines up
    # with bin_energy (which is |x|^2 IIR, also linear-ish).
    lcn_power = [10 ** (np.array(s) / 10.0) for s in lcn_series]
    bin_power = [np.array(s, dtype=np.float64) for s in bin_series]

    # Pearson correlation per (LCN, bin).
    def pearson(a: np.ndarray, b: np.ndarray) -> float:
        if a.std() < 1e-30 or b.std() < 1e-30:
            return 0.0
        return float(((a - a.mean()) * (b - b.mean())).mean()
                     / (a.std() * b.std()))

    print(f"=== LCN -> best-matching bins (top 3 Pearson correlation) ===")
    print(f"{'system':>5} {'LCN':>4} {'freq (MHz)':>10}  "
          f"{'best bin (corr)':>20}  {'2nd':>14}  {'3rd':>14}  "
          f"{'mean dB':>8}")
    rows: list[tuple] = []
    for ti, (sys, lcn, hz) in enumerate(targets):
        corrs = [(k, pearson(lcn_power[ti], bin_power[k]))
                 for k in range(64)]
        corrs.sort(key=lambda kv: -kv[1])
        top3 = corrs[:3]
        mean_db = float(np.mean(lcn_series[ti]))
        rows.append((sys, lcn, hz, top3, mean_db))
        s = " ".join([f"b{k}({c:+.2f})" for k, c in top3])
        print(f"{sys:>5} {lcn:>4} {hz/1e6:>10.4f}  "
              f"b{top3[0][0]:2d}({top3[0][1]:+.3f})       "
              f"b{top3[1][0]:2d}({top3[1][1]:+.2f})    "
              f"b{top3[2][0]:2d}({top3[2][1]:+.2f})    "
              f"{mean_db:>+6.1f}")

    print()
    # Confidence-ranked unique mapping. Pick each LCN's strongest bin
    # only if correlation > 0.3, else mark "low confidence".
    print(f"=== Empirical bin <- LCN mapping (correlation > 0.3) ===")
    used_bins: dict[int, tuple[str, int, float]] = {}
    for sys, lcn, hz, top3, mean_db in rows:
        bestk, bestc = top3[0]
        if bestc < 0.3:
            continue
        existing = used_bins.get(bestk)
        if existing is None or bestc > existing[2]:
            used_bins[bestk] = (sys, lcn, bestc)
    for k in sorted(used_bins):
        sys, lcn, c = used_bins[k]
        hz = (CLAY_LCN_HZ if sys == "clay" else DUVAL_LCN_HZ)[lcn]
        offset_khz = (hz - center) / 1e3
        ifb = round(offset_khz / 125)
        print(f"  bin {k:2d} <- {sys:5s} LCN{lcn:2d}  "
              f"{hz/1e6:.4f} MHz  offset {offset_khz:+.0f} kHz  "
              f"input_freq_bin {ifb:+d}  corr={c:.3f}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
