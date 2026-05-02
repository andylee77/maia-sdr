#!/usr/bin/env python3
"""Long-running bin <-> LCN correlator. Polls /api/traffic_bins and
/api/spectrum_wide at 2 Hz, identifies active Clay/Duval LCNs above
threshold each snapshot, and records which channelizer bins fire at
the same moment.

Output:
  - On-screen activity log (one line per snapshot showing active bins
    and active LCNs).
  - At end: a confidence-ranked bin -> LCN mapping table built by
    counting bin/LCN co-firings, weighted by snapshot uniqueness.

Usage:
    python tools/p25_bin_long_sweep.py [--seconds 60] [--rate 2]
                                       [--clay-thresh -75]
                                       [--duval-thresh -90]
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from dataclasses import dataclass, field

import numpy as np
import requests


# Reuse the LCN tables from p25_bin_overlay.
CLAY_LCN_HZ: dict[int, float] = {
    1:  855.2375e6,  2:  856.4375e6,  3:  857.2125e6,  4:  857.4375e6,
    5:  857.9875e6,  6:  858.4375e6,  7:  858.4625e6,  8:  858.9875e6,
    9:  859.4375e6, 10: 860.4375e6, 11: 860.9625e6,  # control
}

DUVAL_LCN_HZ: dict[int, float] = {
    1: 855.2125e6,  2: 855.4875e6,  3: 855.9625e6,  4: 855.9875e6,
    5: 854.9625e6,  6: 856.2125e6,  7: 856.2625e6,  8: 856.4625e6,
    9: 856.7125e6, 10: 856.7375e6, 11: 856.9375e6, 12: 856.9625e6,
   13: 856.9875e6, 14: 857.2375e6, 15: 857.4625e6, 16: 857.7125e6,
   17: 857.9375e6, 18: 857.9625e6, 19: 858.7125e6, 20: 858.9625e6,
   21: 859.4625e6, 22: 859.7125e6, 23: 859.9375e6, 24: 859.9625e6,
   25: 859.9875e6, 26: 860.4625e6, 27: 860.7125e6, 28: 860.9375e6,
}

# (system, lcn, freq_hz)
ALL_LCNS: list[tuple[str, int, float]] = (
    [("clay", lcn, hz) for lcn, hz in CLAY_LCN_HZ.items()]
    + [("duval", lcn, hz) for lcn, hz in DUVAL_LCN_HZ.items()]
)


def fetch(host: str, path: str) -> dict:
    r = requests.get(host.rstrip("/") + path, timeout=4.0)
    r.raise_for_status()
    return r.json()


def find_active_lcns(freqs_hz: np.ndarray, mag: np.ndarray,
                     clay_thresh: float, duval_thresh: float
                     ) -> list[tuple[str, int, float, float]]:
    """Return list of (system, lcn, freq, mag) for LCNs whose spectrum
    bin sits above the per-system threshold. Tighter ±15 kHz match so
    Duval's 25-kHz-spaced raster doesn't get confused with Clay's."""
    active: dict[tuple[str, int], tuple[float, float]] = {}
    for i in range(1, mag.size - 1):
        if mag[i] < mag[i - 1] or mag[i] < mag[i + 1]:
            continue
        f = float(freqs_hz[i])
        for system, lcn, hz in ALL_LCNS:
            d = abs(f - hz)
            if d > 15e3:
                continue
            thresh = clay_thresh if system == "clay" else duval_thresh
            if mag[i] < thresh:
                continue
            prev = active.get((system, lcn))
            if prev is None or float(mag[i]) > prev[1]:
                active[(system, lcn)] = (f, float(mag[i]))
            break
    return [(s, l, f, db) for (s, l), (f, db) in
            sorted(active.items(), key=lambda kv: -kv[1][1])]


@dataclass
class SnapshotLog:
    t_offset: float
    active_bins: dict[int, int]                       # bin -> energy
    active_lcns: list[tuple[str, int, float, float]]  # (sys,lcn,f,db)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="http://192.168.2.1:8080")
    ap.add_argument("--seconds", type=float, default=60.0)
    ap.add_argument("--rate", type=float, default=2.0)
    ap.add_argument("--fft", type=int, default=4096)
    ap.add_argument("--clay-thresh", type=float, default=-75.0)
    ap.add_argument("--duval-thresh", type=float, default=-92.0)
    ap.add_argument("--bin-min-energy", type=int, default=2000)
    args = ap.parse_args()

    t_start = time.time()
    period = 1.0 / args.rate
    snaps: list[SnapshotLog] = []
    print(f"Sampling every {period*1000:.0f} ms for {args.seconds:.0f}s "
          f"(~{int(args.seconds * args.rate)} snapshots).")
    print(f"Clay threshold: {args.clay_thresh} dB. "
          f"Duval threshold: {args.duval_thresh} dB. "
          f"Bin min energy: {args.bin_min_energy}")
    print()

    last_loop = time.time()
    while time.time() - t_start < args.seconds:
        loop_t = time.time()
        try:
            spec = fetch(args.host, f"/api/spectrum_wide?fft_size={args.fft}")
            bins = fetch(args.host, "/api/traffic_bins")
        except Exception as e:
            print(f"  fetch error: {e}", file=sys.stderr)
            time.sleep(period)
            continue
        mag = np.array(spec["mag_db"], dtype=np.float64)
        center = float(spec["center_hz"])
        span = float(spec["span_hz"])
        n = mag.size
        freqs = center + (np.arange(n) - n / 2) * (span / n)
        active_lcns = find_active_lcns(
            freqs, mag, args.clay_thresh, args.duval_thresh)
        be = np.array(bins["bin_energies"], dtype=np.int64)
        active_bins = {int(k): int(be[k])
                       for k in np.where(be >= args.bin_min_energy)[0]}
        snap = SnapshotLog(
            t_offset=loop_t - t_start,
            active_bins=active_bins,
            active_lcns=active_lcns,
        )
        snaps.append(snap)
        bin_strs = ",".join(f"b{k}={v}" for k, v in
                            sorted(active_bins.items(), key=lambda kv: -kv[1]))
        lcn_strs = ",".join(f"{s[0].upper()}{l}({db:.0f}dB)"
                            for s, l, _, db in active_lcns[:8])
        print(f"  t={snap.t_offset:5.1f}s  bins[{bin_strs or '-'}]  "
              f"lcns[{lcn_strs or '-'}]")
        elapsed = time.time() - loop_t
        if elapsed < period:
            time.sleep(period - elapsed)

    print()
    print(f"Captured {len(snaps)} snapshots over {time.time()-t_start:.1f}s.")
    print()

    # Co-firing analysis. For each bin, count how many snapshots it was
    # active. For each LCN, same. Build a contingency-like table.
    bin_count: dict[int, int] = {}
    lcn_count: dict[tuple[str, int], int] = {}
    cofire: dict[tuple[int, str, int], int] = {}
    for snap in snaps:
        for k in snap.active_bins:
            bin_count[k] = bin_count.get(k, 0) + 1
        for sys, lcn, _, _ in snap.active_lcns:
            lcn_count[(sys, lcn)] = lcn_count.get((sys, lcn), 0) + 1
        for k in snap.active_bins:
            for sys, lcn, _, _ in snap.active_lcns:
                cofire[(k, sys, lcn)] = cofire.get((k, sys, lcn), 0) + 1

    print(f"=== Bin activity (snapshots seen active) ===")
    for k in sorted(bin_count):
        print(f"  bin {k:2d}: {bin_count[k]} / {len(snaps)} snaps "
              f"({100*bin_count[k]/len(snaps):.0f}%)")

    print()
    print(f"=== LCN activity ===")
    for (sys, lcn), n in sorted(lcn_count.items(),
                                key=lambda kv: (-kv[1], kv[0])):
        hz = CLAY_LCN_HZ.get(lcn) if sys == "clay" else DUVAL_LCN_HZ.get(lcn)
        print(f"  {sys:5s} LCN{lcn:2d} ({hz/1e6:.4f} MHz): {n} / {len(snaps)} "
              f"({100*n/len(snaps):.0f}%)")

    # Best mapping: for each bin, find the LCN with highest fraction
    # of co-firing relative to LCN's activity. (Specificity: how often
    # the bin lit when this LCN was active.)
    print()
    print(f"=== Bin -> LCN best match (specificity = cofire/lcn_active) ===")
    for k in sorted(bin_count):
        cands: list[tuple[float, str, int, int]] = []
        for (kk, sys, lcn), c in cofire.items():
            if kk != k:
                continue
            la = lcn_count[(sys, lcn)]
            if la == 0:
                continue
            spec = c / la
            cands.append((spec, sys, lcn, c))
        cands.sort(reverse=True)
        if not cands:
            continue
        top = cands[:5]
        s = ", ".join(f"{sys[0].upper()}{lcn}={c}/{lcn_count[(sys,lcn)]}"
                      f"({sp*100:.0f}%)"
                      for sp, sys, lcn, c in top)
        print(f"  bin {k:2d}: [{s}]")

    return 0


if __name__ == "__main__":
    sys.exit(main())
