#!/usr/bin/env python3
"""Capture wideband spectrum + channelizer bin energies and plot them
together to determine the empirical bin->freq mapping.

Usage:
    python tools/p25_bin_overlay.py [--host http://192.168.2.1:8080]
                                    [--out spectrum_bins.png]
                                    [--fft 4096]

Top panel: wideband spectrum (4096 bins) — single curve, frequency on x.
Bottom panel: channelizer bins (64 bars) at every candidate
location predicted by three different mapping hypotheses, color-coded.
The hypothesis whose bars line up with wideband peaks is the correct
mapping for this build of HDL.

Requires: numpy, matplotlib, requests (`pip install matplotlib requests`).
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import requests
import matplotlib.pyplot as plt


def fetch(host: str, path: str) -> dict:
    r = requests.get(host.rstrip("/") + path, timeout=5.0)
    r.raise_for_status()
    return r.json()


# Clay County (Florida) P25 system — operator-supplied LCN -> frequency
# table. Lets us correlate the wideband spectrum peaks to a known
# carrier instead of guessing which signal is which.
CLAY_LCN_HZ: dict[int, float] = {
    1:  855.2375e6,
    2:  856.4375e6,
    3:  857.2125e6,
    4:  857.4375e6,
    5:  857.9875e6,
    6:  858.4375e6,
    7:  858.4625e6,
    8:  858.9875e6,
    9:  859.4375e6,
    10: 860.4375e6,
    11: 860.9625e6,  # control channel
}

# Duval County / Jacksonville City P25 system — also visible in the
# Clay band. On internal antenna these sit -95 to -80 dB; with the
# external antenna they can rival Clay levels. Useful as additional
# reference carriers for the bin->freq mapping diagnosis.
DUVAL_LCN_HZ: dict[int, float] = {
    1:  855.2125e6,
    2:  855.4875e6,
    3:  855.9625e6,
    4:  855.9875e6,
    5:  854.9625e6,
    6:  856.2125e6,
    7:  856.2625e6,
    8:  856.4625e6,
    9:  856.7125e6,
    10: 856.7375e6,
    11: 856.9375e6,
    12: 856.9625e6,
    13: 856.9875e6,
    14: 857.2375e6,
    15: 857.4625e6,
    16: 857.7125e6,
    17: 857.9375e6,
    18: 857.9625e6,
    19: 858.7125e6,
    20: 858.9625e6,
    21: 859.4625e6,
    22: 859.7125e6,
    23: 859.9375e6,
    24: 859.9625e6,
    25: 859.9875e6,
    26: 860.4625e6,
    27: 860.7125e6,
    28: 860.9375e6,
}

# Combined (system, lcn) -> freq table for matching peaks against
# either system. System tag lets us color-code in the plot and
# disambiguate when Clay and Duval LCNs sit close in frequency.
ALL_LCNS: list[tuple[str, int, float]] = (
    [("clay", lcn, hz) for lcn, hz in CLAY_LCN_HZ.items()]
    + [("duval", lcn, hz) for lcn, hz in DUVAL_LCN_HZ.items()]
)


def find_lcn_peaks(freqs_hz: np.ndarray, mag: np.ndarray,
                   threshold_db: float = -85.0,
                   ) -> list[tuple[str, int, float, float]]:
    """Find local-maximum peaks above `threshold_db` and snap each to
    the nearest known (Clay or Duval) LCN within +/- 25 kHz. Returns a
    list of (system, lcn, freq_hz, mag_db) tuples sorted by mag desc.
    """
    seen: dict[tuple[str, int], tuple[float, float]] = {}
    above = mag > threshold_db
    # Find local maxima above threshold
    for i in range(1, mag.size - 1):
        if not above[i]:
            continue
        if mag[i] < mag[i - 1] or mag[i] < mag[i + 1]:
            continue
        f = float(freqs_hz[i])
        # Snap to nearest known LCN within +/- 25 kHz (tighter than the
        # 50 kHz Clay-only path because Duval's grid bumps right up
        # against Clay's).
        best: tuple[str, int] | None = None
        best_dist = 25e3
        for system, lcn, lcn_hz in ALL_LCNS:
            d = abs(f - lcn_hz)
            if d < best_dist:
                best_dist = d
                best = (system, lcn)
        if best is None:
            continue
        prev = seen.get(best)
        if prev is None or float(mag[i]) > prev[1]:
            seen[best] = (f, float(mag[i]))
    return [(system, lcn, f, db)
            for (system, lcn), (f, db) in
            sorted(seen.items(), key=lambda kv: -kv[1][1])]


def input_freq_bin(target_hz: float, lo_hz: float, fs_hz: float, m: int) -> int:
    """Compute the *input* freq bin (signed in [-M/2, M/2)) for a target."""
    raw = round((target_hz - lo_hz) / fs_hz * m)
    # Wrap into [-M/2, M/2)
    if raw >= m // 2:
        raw -= m
    elif raw < -m // 2:
        raw += m
    return raw


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="http://192.168.2.1:8080")
    ap.add_argument("--fft", type=int, default=4096,
                    help="wideband FFT size (max ~16384)")
    ap.add_argument("--out", type=Path, default=Path("spectrum_bins.png"))
    ap.add_argument("--show", action="store_true",
                    help="open the plot window in addition to saving")
    ap.add_argument("--snapshots", type=int, default=1,
                    help="capture N snapshots ~250 ms apart and accumulate "
                         "bin->LCN observations into a single mapping table")
    ap.add_argument("--clay-thresh-db", type=float, default=-75.0,
                    help="threshold (dB) for a peak to count as Clay; "
                         "default -75 to bias against Duval (max -80)")
    args = ap.parse_args()

    t_pre = time.time()
    # Co-occurrence keyed on (channelizer_bin, system, lcn).
    cooc: dict[tuple[int, str, int], int] = {}
    last_spec = None
    last_bins = None
    for snap in range(max(1, args.snapshots)):
        spec_i = fetch(args.host, f"/api/spectrum_wide?fft_size={args.fft}")
        bins_i = fetch(args.host, "/api/traffic_bins")
        last_spec, last_bins = spec_i, bins_i
        mag_i = np.array(spec_i["mag_db"], dtype=np.float64)
        center_i = float(spec_i["center_hz"])
        span_i = float(spec_i["span_hz"])
        n_i = mag_i.size
        freqs_i = center_i + (np.arange(n_i) - n_i / 2) * (span_i / n_i)
        peaks = find_lcn_peaks(freqs_i, mag_i, args.clay_thresh_db)
        active = [(system, lcn) for system, lcn, _, _ in peaks]
        be_i = np.array(bins_i["bin_energies"], dtype=np.float64)
        nz_i = [int(k) for k in np.where(be_i > 0)[0]]
        for k in nz_i:
            for system, lcn in active:
                cooc[(k, system, lcn)] = cooc.get((k, system, lcn), 0) + 1
        if args.snapshots > 1:
            label_strs = [f"{s[0][0].upper()}{s[1]}" for s in active]
            print(f"snap {snap+1}/{args.snapshots}: bins {nz_i}  "
                  f"active {label_strs}")
            if snap + 1 < args.snapshots:
                time.sleep(0.25)
    spec = last_spec
    bins = last_bins
    t_post = time.time()
    print(f"fetched {args.snapshots} snapshots in {t_post-t_pre:.2f} s")

    if not spec.get("ok", True):
        print("spectrum_wide returned not-ok:", spec, file=sys.stderr)
        return 1
    if not bins.get("ok", True):
        print("traffic_bins returned not-ok:", bins, file=sys.stderr)
        return 1

    mag = np.array(spec["mag_db"], dtype=np.float64)
    center_hz = float(spec["center_hz"])
    span_hz = float(spec["span_hz"])
    n = mag.size
    # Centered FFT order: idx 0 = -span/2, idx N-1 = +span/2 - bin
    freqs_hz = center_hz + (np.arange(n) - n / 2) * (span_hz / n)
    freqs_mhz = freqs_hz * 1e-6

    energies = np.array(bins["bin_energies"], dtype=np.float64)
    M = energies.size
    bin_rate_hz = span_hz / M

    nz = np.where(energies > 0)[0]
    print(f"non-zero channelizer bins: {nz.tolist()}")
    print(f"  energies: {[int(energies[b]) for b in nz]}")
    print(f"channelizer M={M}, bin spacing = {bin_rate_hz/1e3:.1f} kHz")
    print(f"AD9361 LO = {center_hz/1e6:.4f} MHz, span = {span_hz/1e6:.2f} MHz")

    # Three candidate freq mappings for channelizer bin k.
    # 'natural'  : bin k -> +k * fs/M (k < M/2 = positive offset, k >= M/2 = -((M-k)*fs/M))
    # 'inverted' : bin k -> -k * fs/M (M2A model, peak_bin = -input_freq_bin mod M)
    # 'shifted'  : bin k -> (k - M/2) * fs/M (centered FFT-shift convention)
    def freq_natural(k):
        signed = k if k < M / 2 else k - M
        return center_hz + signed * bin_rate_hz

    def freq_inverted(k):
        signed = k if k < M / 2 else k - M
        return center_hz - signed * bin_rate_hz

    def freq_shifted(k):
        return center_hz + (k - M / 2) * bin_rate_hz

    # M2B 2026-05-02: closed-form HDL permutation derived by deep-dive
    # of polyphase_channelizer.py. PS_bin K corresponds to input freq
    # bin = -bit_reverse((bit_reverse(K) - L) mod M)  (signed)  with
    # L=10 (R22 FFT pipeline lag observed empirically). Verified
    # against bin 16=Clay3, 21=Clay5, 39=Clay7, 61=Clay11 (control).
    HDL_FFT_LAG = 10

    def _bitrev6(x: int) -> int:
        y = 0
        for i in range(6):
            y |= ((x >> i) & 1) << (5 - i)
        return y

    def freq_actual(k):
        n_natural = _bitrev6((_bitrev6(k) - HDL_FFT_LAG) % int(M))
        # input_freq_bin = -n_natural (signed in [-M/2, M/2))
        signed_in = -n_natural
        if signed_in < -int(M) // 2:
            signed_in += int(M)
        elif signed_in >= int(M) // 2:
            signed_in -= int(M)
        return center_hz + signed_in * bin_rate_hz

    fig, (ax_spec, ax_bins) = plt.subplots(
        2, 1, figsize=(20, 10), sharex=True,
        gridspec_kw={"height_ratios": [3, 2]})

    # Top: wideband spectrum
    ax_spec.plot(freqs_mhz, mag, color="#3a8fd1", linewidth=0.7,
                 label="wideband HDL FFT")
    ax_spec.set_ylabel("Magnitude (dB)")
    ax_spec.set_title(
        f"Wideband + channelizer overlay — LO={center_hz/1e6:.4f} MHz "
        f"span={span_hz/1e6:.2f} MHz   build={fetch(args.host, '/api/system').get('build','?')}"
    )
    ax_spec.grid(True, alpha=0.3)

    # Mark Clay County LCN frequencies in orange so we can identify
    # which spectrum peaks correspond to which carriers.
    mag_min = float(np.min(mag))
    for lcn, hz in CLAY_LCN_HZ.items():
        f_mhz = hz / 1e6
        if not (freqs_mhz[0] <= f_mhz <= freqs_mhz[-1]):
            continue
        ax_spec.axvline(f_mhz, color="#ff7f00", alpha=0.55,
                        linestyle=":", linewidth=1.0)
        ax_spec.text(f_mhz, mag_min + 2,
                     f"LCN{lcn}\n{f_mhz:.4f}",
                     color="#cc6600", fontsize=7,
                     ha="center", va="bottom")
        ax_bins.axvline(f_mhz, color="#ff7f00", alpha=0.35,
                        linestyle=":", linewidth=1.0)

    # Mark non-zero bins under each candidate mapping. "actual" is the
    # closed-form HDL permutation we derived; natural/inverted/shifted
    # are kept for visual comparison so it's obvious why they're wrong.
    colors = {
        "actual":   "#ffd92f",
        "natural":  "#e41a1c",
        "inverted": "#4daf4a",
        "shifted":  "#984ea3",
    }
    mag_max = float(np.max(mag))
    for k in nz:
        e = int(energies[k])
        for label, fn in [("actual", freq_actual),
                          ("natural", freq_natural),
                          ("inverted", freq_inverted),
                          ("shifted", freq_shifted)]:
            f_mhz = fn(k) * 1e-6
            ls = "-" if label == "actual" else "--"
            lw = 1.6 if label == "actual" else 1.0
            ax_spec.axvline(f_mhz, color=colors[label], alpha=0.55,
                            linestyle=ls, linewidth=lw)
            ax_spec.text(
                f_mhz,
                mag_max - 2 - 4 * list(colors).index(label),
                f"b{k}={e} ({label})",
                color=colors[label], fontsize=7,
                rotation=90, va="top", ha="right",
            )
    # Custom legend for hypothesis colors
    handles = [plt.Line2D([0], [0], color=c, linestyle="--", label=l)
               for l, c in colors.items()]
    ax_spec.legend(handles=handles, loc="lower right", fontsize=8)

    # Bottom: ALL 64 channelizer bin boxes laid across the spectrum,
    # one row per mapping hypothesis. Each box spans its bin's
    # 125 kHz slice and is color-shaded by raw energy. Bin number is
    # printed inside every box so non-zero bins pop visually.
    import matplotlib.colors as mcolors
    import matplotlib.patches as mpatches
    e_max = max(1.0, float(np.max(energies)))
    cmap = plt.cm.viridis

    def draw_row(ax, fn, y_lo, y_hi, label_color, label):
        for k in range(M):
            f_center = fn(k) * 1e-6
            f_left = f_center - (bin_rate_hz / 2) * 1e-6
            f_right = f_center + (bin_rate_hz / 2) * 1e-6
            e = float(energies[k])
            shade = cmap(min(0.95, e / e_max)) if e > 0 else (0.12, 0.12, 0.16, 1.0)
            rect = mpatches.Rectangle(
                (f_left, y_lo), f_right - f_left, y_hi - y_lo,
                facecolor=shade, edgecolor=label_color,
                linewidth=0.4, alpha=0.85)
            ax.add_patch(rect)
            txt_color = "#ffffff" if e > 0.5 * e_max else "#cccccc"
            text_lines = f"{k}"
            if e > 0:
                text_lines = f"{k}\n{int(e)}"
            ax.text(
                f_center, (y_lo + y_hi) / 2, text_lines,
                fontsize=6, ha="center", va="center", color=txt_color,
                fontweight="bold" if e > 0 else "normal")
        # Row label on the left edge.
        ax.text(freqs_mhz[0] - 0.05, (y_lo + y_hi) / 2, label,
                fontsize=8, ha="right", va="center", color=label_color,
                fontweight="bold")

    # Four rows stacked — "actual" on top (the derived closed-form
    # HDL mapping that lit boxes should align with wideband peaks),
    # then natural/inverted/shifted for comparison so it's obvious
    # why those three were wrong.
    draw_row(ax_bins, freq_actual,   y_lo=3.0, y_hi=4.0,
             label_color=colors["actual"],   label="actual (HDL)")
    draw_row(ax_bins, freq_natural,  y_lo=2.0, y_hi=3.0,
             label_color=colors["natural"],  label="natural")
    draw_row(ax_bins, freq_inverted, y_lo=1.0, y_hi=2.0,
             label_color=colors["inverted"], label="inverted")
    draw_row(ax_bins, freq_shifted,  y_lo=0.0, y_hi=1.0,
             label_color=colors["shifted"],  label="shifted")

    ax_bins.set_xlabel("Frequency (MHz)")
    ax_bins.set_yticks([0.5, 1.5, 2.5, 3.5])
    ax_bins.set_yticklabels(["shifted", "inverted", "natural", "actual"])
    ax_bins.set_xlim(freqs_mhz[0], freqs_mhz[-1])
    ax_bins.set_ylim(0, 4)
    ax_bins.grid(True, alpha=0.3, axis="x")

    # Console summary too — handy when running headless.
    print()
    print("=== Per-bin (non-zero) — predicted frequency under each hypothesis ===")
    print(f"{'bin':>4} {'energy':>8}  {'natural':>10}  {'inverted':>10}  {'shifted':>10}")
    for k in nz:
        print(f"{int(k):4d} {int(energies[k]):8d}  "
              f"{freq_natural(k)/1e6:>8.4f} M  "
              f"{freq_inverted(k)/1e6:>8.4f} M  "
              f"{freq_shifted(k)/1e6:>8.4f} M")

    # Empirical bin->LCN co-occurrence accumulator. cooc keys are
    # (bin, system, lcn) since both Clay and Duval get tracked.
    if cooc:
        print()
        print("=== Empirical bin->LCN mapping (co-occurrence counts) ===")
        bin_to_lcns: dict[int, list[tuple[str, int, int]]] = {}
        for (k, system, lcn), n in cooc.items():
            bin_to_lcns.setdefault(k, []).append((system, lcn, n))
        for k in sorted(bin_to_lcns):
            cands = sorted(bin_to_lcns[k], key=lambda t: -t[2])
            cand_str = ", ".join(
                f"{system[0].upper()}{lcn}={n}"
                for system, lcn, n in cands[:5]
            )
            print(f"  bin {k:2d} -> [{cand_str}]")
        # Best-guess unique mapping: each (system,LCN) keeps only top bin.
        best: dict[tuple[str, int], tuple[int, int]] = {}
        for (k, system, lcn), n in cooc.items():
            prev = best.get((system, lcn))
            if prev is None or n > prev[1]:
                best[(system, lcn)] = (k, n)
        if best:
            print()
            print("=== Best-guess LCN -> bin (top-co-occurrence) ===")
            print(f"{'sys':>5} {'LCN':>4}  {'freq (MHz)':>10}  "
                  f"{'input_freq_bin':>14}  {'empirical bin':>14}  "
                  f"{'count':>6}")
            for (system, lcn) in sorted(best):
                k, n = best[(system, lcn)]
                table = CLAY_LCN_HZ if system == "clay" else DUVAL_LCN_HZ
                hz = table[lcn]
                ifb = input_freq_bin(hz, center_hz, span_hz, M)
                print(f"{system:>5} {lcn:4d}  {hz/1e6:>10.4f}  "
                      f"{ifb:>+14d}  {k:>14d}  {n:>6d}")

    fig.tight_layout()
    out = args.out.resolve()
    fig.savefig(out, dpi=140)
    print(f"saved plot to {out}")
    if args.show:
        plt.show()
    return 0


if __name__ == "__main__":
    sys.exit(main())
