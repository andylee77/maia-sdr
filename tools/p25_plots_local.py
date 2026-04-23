#!/usr/bin/env python3
"""
Local P25 diagnostic plots from the HDL pre-diff post-PLL ring.

Streams `/ws/iq?source=pre_diff` for N seconds and renders three
plots using the SAME captured data:

  constellation_<chain>.png  — 4-rail differential scatter (post-diff
                              quadrants at ±π/4, ±3π/4) with rails
                              marked
  eye_<chain>.png            — phase-domain eye (atan2(diff) * 4/π)
                              with 9-symbol overlay, upsampled 2→10 sps
                              by linear IQ interp, colour-cycled
                              traces, ±3 / ±1 decision rails
  diff_<chain>.png           — differential-phase histogram (deviation
                              Hz) with 240 bins over ±2400 Hz and rail
                              annotations at ±600, ±1800

Also writes summary.json with per-symbol counts, per-rail stdev,
clean fraction, and AGC cluster radius.

Tap note: the HDL ring delivers 2 sps (rotate_mid + rotate_cur,
interleaved). Differential demod takes consecutive `cur` samples
(the decision-time points). See debug.rs::pre_diff_sym_points and
differentiate() — this tool mirrors the same operation in Python.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import statistics
import struct
import sys
from collections import Counter

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

try:
    import websockets
except ImportError:
    print("pip install websockets", file=sys.stderr)
    sys.exit(1)


Q15_SCALE = 1.0 / 32768.0


async def stream(host: str, chain: str, seconds: float) -> np.ndarray:
    uri = f"ws://{host}/ws/iq?source=pre_diff&chain={chain}"
    print(f"connecting {uri}", file=sys.stderr)
    pairs = []
    async with websockets.connect(uri, max_size=None) as ws:
        deadline = asyncio.get_event_loop().time() + seconds
        try:
            while asyncio.get_event_loop().time() < deadline:
                remaining = deadline - asyncio.get_event_loop().time()
                if remaining <= 0:
                    break
                frame = await asyncio.wait_for(ws.recv(),
                                               timeout=remaining + 0.5)
                if isinstance(frame, str):
                    continue
                n = len(frame) // 4
                for k in range(n):
                    re, im = struct.unpack_from("<hh", frame, k * 4)
                    pairs.append((re, im))
        except asyncio.TimeoutError:
            pass
    if not pairs:
        raise SystemExit("no pre-diff data received")
    return np.array(pairs, dtype=np.int32).astype(np.float32) * Q15_SCALE


def sym_samples(arr: np.ndarray) -> np.ndarray:
    """Pick decision-time (odd-index) samples from the interleaved
    rotate_mid / rotate_cur pairs."""
    return arr[1::2]


def differentiate(syms: np.ndarray) -> np.ndarray:
    """d[n] = z[n] * conj(z[n-1])   (component-wise on the (re,im)
    representation). Returns array shape (N-1, 2)."""
    z = syms[:, 0] + 1j * syms[:, 1]
    d = z[1:] * np.conj(z[:-1])
    return np.stack([d.real, d.imag], axis=1)


def render_constellation(diff: np.ndarray, raw_syms: np.ndarray,
                         out_path: str, title: str,
                         n_display: int = 200) -> dict:
    """Traditional 4-dot LSM constellation: take the most recent
    `n_display` diff samples, project onto the unit circle (so
    |z|=1 regardless of AGC variation), and render with big dots
    at high alpha. On a clean signal you see four tight blobs at
    (±√2/2, ±√2/2). SDRTrunk-style."""

    # Use the most recent n_display diff samples. Fewer points
    # rendered with high alpha + larger markers => visible clusters
    # instead of an overlapping ring.
    # Gate out low-magnitude diff points. |d[n]| = |z[n]|·|z[n-1]|
    # is small when AGC is dipping or when a symbol straddles the
    # decision boundary. These produce meaningless phase angles
    # (origin noise spreading round the plot). Keep only diff points
    # with |d| >= 0.3 * mean |d| — that preserves clean decisions and
    # drops the "spoke through origin" artefact.
    mag_all = np.sqrt(diff[:, 0] ** 2 + diff[:, 1] ** 2)
    mean_mag = float(mag_all.mean()) if mag_all.size else 1.0
    keep = mag_all >= 0.3 * mean_mag
    clean = diff[keep]

    if clean.shape[0] > n_display:
        window = clean[-n_display:]
    else:
        window = clean

    # Phase-only projection onto the unit circle. Radial noise from
    # |d| variation is the largest single source of scatter-plot
    # ugliness; dropping it gives a clean 4-cluster view that reflects
    # the phase noise alone.
    phase = np.arctan2(window[:, 1], window[:, 0])
    xs = np.cos(phase)
    ys = np.sin(phase)

    # Per-cluster stats from the full (non-gated) capture.
    phase_all = np.arctan2(diff[:, 1], diff[:, 0])
    quad_stdev: dict[str, float] = {}
    cluster_var = 0.0
    weights = 0
    for name, ang in (("+pi/4", math.pi / 4),
                      ("+3pi/4", 3 * math.pi / 4),
                      ("-pi/4", -math.pi / 4),
                      ("-3pi/4", -3 * math.pi / 4)):
        mask = np.abs(phase_all - ang) < math.pi / 4
        if mask.any():
            # Cluster variance in IQ-on-unit-circle space.
            cx = float(np.cos(phase_all[mask]).mean())
            cy = float(np.sin(phase_all[mask]).mean())
            dx = np.cos(phase_all[mask]) - cx
            dy = np.sin(phase_all[mask]) - cy
            var = float(np.mean(dx * dx + dy * dy))
            quad_stdev[name] = float(np.std(phase_all[mask] - ang)
                                     * 180 / math.pi)
            cluster_var += var * mask.sum()
            weights += int(mask.sum())
    if weights:
        cluster_var /= weights

    fig, ax = plt.subplots(figsize=(5.8, 5.8), dpi=110)
    ax.set_facecolor("#0a0f1a")
    fig.patch.set_facecolor("#0a0f1a")
    ax.axhline(0, color="#243049", lw=0.6)
    ax.axvline(0, color="#243049", lw=0.6)
    for r in (0.5, 1.0, 1.5):
        ang = np.linspace(0, 2 * np.pi, 361)
        ax.plot(r * np.cos(ang), r * np.sin(ang),
                color="#1c2a42", lw=0.5)
    ax.scatter(xs, ys, s=22, color="#4fdb6f",
               alpha=0.55, linewidths=0)
    for ang in (math.pi / 4, 3 * math.pi / 4,
                -math.pi / 4, -3 * math.pi / 4):
        ax.plot([math.cos(ang)], [math.sin(ang)],
                marker="+", color="#ff9999", markersize=16, mew=1.8)
    ax.set_xlim(-1.8, 1.8)
    ax.set_ylim(-1.8, 1.8)
    ax.set_aspect("equal")
    ax.tick_params(colors="#9aa", labelsize=9)
    for spine in ax.spines.values():
        spine.set_color("#2a3a5a")

    ax.set_title(f"{title}\n"
                 f"n={len(xs)}  clustervar={cluster_var:.4f}  "
                 f"(total_syms={diff.shape[0]})",
                 color="#ddd", fontsize=10, loc="left")

    fig.tight_layout()
    fig.savefig(out_path, facecolor=fig.get_facecolor(), dpi=110)
    plt.close(fig)

    return dict(
        n_displayed=int(len(xs)),
        total_diff=int(diff.shape[0]),
        cluster_var=cluster_var,
        quad_angular_stdev_deg=quad_stdev,
        mean_mag=mean_mag,
    )


def render_eye(diff: np.ndarray, out_path: str, title: str) -> None:
    """Phase-domain eye: take the diff-phase trace at the symbol
    rate, upsample 2→10 sps, overlay 9-symbol windows."""
    # The diff array is 1-sample-per-symbol (one diff per decision
    # point). To build an eye we need multi-sample-per-symbol. Approach:
    # interpolate the diff in time between symbols, giving a smooth
    # phase trajectory. 10 sps via linear interp in IQ, then atan2.
    if len(diff) < 20:
        return
    upsample = 10
    n = len(diff)
    t0 = np.arange(n)
    t1 = np.linspace(0, n - 1, (n - 1) * upsample + 1)
    i_up = np.interp(t1, t0, diff[:, 0])
    q_up = np.interp(t1, t0, diff[:, 1])
    phase = np.arctan2(q_up, i_up) * (4.0 / np.pi)  # -> ±3 / ±1 rails

    window_syms = 9
    sps = upsample
    window_samples = sps * window_syms
    # OP25-Datascope density: ~40 traces is enough to see all
    # rail+transition combinations without turning into a fog. Step
    # the window by `window_syms` so traces don't overlap within a
    # single eye — each symbol contributes to one overlay only.
    traces = []
    step = window_syms
    for start_sym in range(0, n - window_syms, step):
        s = start_sym * sps
        e = s + window_samples
        if e > len(phase):
            break
        traces.append(phase[s:e])
    MAX_TRACES = 40
    if len(traces) > MAX_TRACES:
        # Take evenly-spaced sample across the capture so the overlay
        # covers the full window, not just the opening N symbols.
        idx = np.linspace(0, len(traces) - 1, MAX_TRACES).astype(int)
        traces = [traces[i] for i in idx]

    fig, ax = plt.subplots(figsize=(8.4, 5.4), dpi=110)
    ax.set_facecolor("#ffffff")
    fig.patch.set_facecolor("#ffffff")
    x = np.arange(window_samples) / sps
    cmap = plt.get_cmap("tab20")
    for k, tr in enumerate(traces):
        ax.plot(x, tr, color=cmap(k % 20), lw=1.0, alpha=0.9)
    for lvl in (+3, +1, -1, -3):
        ax.axhline(lvl, color="#bbb", lw=0.4, ls="-", zorder=0)
    ax.set_xlim(0, window_syms)
    ax.set_ylim(-4.0, 4.0)
    ax.set_xticks(range(window_syms + 1))
    ax.set_yticks([-4, -3, -2, -1, 0, 1, 2, 3, 4])
    ax.set_title(title, fontsize=11)
    ax.tick_params(labelsize=9)
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#ffffff", dpi=110)
    plt.close(fig)


def render_diff_hist(diff: np.ndarray, out_path: str, title: str) -> dict:
    """240-bin histogram of differential phase scaled to Hz. Expected
    peaks at ±600 Hz and ±1800 Hz."""
    dev_hz = np.arctan2(diff[:, 1], diff[:, 0]) * (600.0 / (math.pi / 4))
    bins = np.linspace(-2400, 2400, 241)
    counts, edges = np.histogram(dev_hz, bins=bins)

    fig, ax = plt.subplots(figsize=(9.0, 4.4), dpi=110)
    ax.set_facecolor("#1c1c1c")
    fig.patch.set_facecolor("#1c1c1c")
    centres = 0.5 * (edges[1:] + edges[:-1])
    ax.bar(centres, counts, width=(edges[1] - edges[0]),
           color="#4dd1ff", edgecolor="none")
    for rail, label in ((-1800, "−1800"), (-600, "−600"),
                        (+600, "+600"), (+1800, "+1800")):
        ax.axvline(rail, color="#ffdd55", lw=1.0, ls="--", alpha=0.8)
        ax.text(rail, counts.max() * 0.95, label,
                color="#ffdd55", ha="center", fontsize=8)
    ax.set_xlim(-2400, 2400)
    ax.set_xlabel("differential deviation (Hz)", color="#ddd")
    ax.set_ylabel("count", color="#ddd")
    ax.set_title(title, color="#ddd", fontsize=11)
    ax.tick_params(colors="#ddd", labelsize=8)
    for spine in ax.spines.values():
        spine.set_color("#444")
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#1c1c1c", dpi=110)
    plt.close(fig)

    # Per-rail stdev + clean fraction
    rails = {}
    for nom in (-1800, -600, +600, +1800):
        within = dev_hz[np.abs(dev_hz - nom) < 400]
        if within.size:
            rails[nom] = dict(
                count=int(within.size),
                mean_hz=float(within.mean()),
                stdev_hz=float(within.std()),
                clean_frac=float(np.mean(np.abs(within - nom) < 150)),
            )
    total = int(dev_hz.size)
    clean_total = sum(r["count"] * r["clean_frac"]
                      for r in rails.values())
    overall = clean_total / max(1, total)
    return dict(total=total, rails=rails, overall_clean_frac=overall)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--chain", choices=["control", "traffic"],
                    default="control")
    ap.add_argument("--duration", type=float, default=8.0)
    ap.add_argument("--outdir",
                    default="doc/diagnostics/2026-04-23/local_plots")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    raw = asyncio.run(stream(args.host, args.chain, args.duration))
    syms = sym_samples(raw)
    diff = differentiate(syms)
    print(f"captured {raw.shape[0]} interleaved samples "
          f"→ {syms.shape[0]} decision syms "
          f"→ {diff.shape[0]} diff points "
          f"({args.duration:.1f}s, chain={args.chain})", file=sys.stderr)

    base = args.chain
    const_path = os.path.join(args.outdir, f"constellation_{base}.png")
    eye_path   = os.path.join(args.outdir, f"eye_{base}.png")
    diff_path  = os.path.join(args.outdir, f"diff_{base}.png")

    const_stats = render_constellation(diff, syms, const_path,
        f"P25 LSM differential constellation  ({base}, n={diff.shape[0]})")
    render_eye(diff, eye_path,
        f"P25 LSM eye  ({base}, 9-sym overlay, {diff.shape[0]} diff syms)")
    diff_stats = render_diff_hist(diff, diff_path,
        f"P25 LSM differential deviation histogram  ({base})")

    # Per-rail + per-symbol stats from the diff-quadrant buckets
    q_buckets = Counter()
    for dr, di in diff:
        if dr > 0 and di > 0:   q_buckets["+1 (+π/4)"] += 1
        elif dr < 0 and di > 0: q_buckets["+3 (+3π/4)"] += 1
        elif dr < 0 and di < 0: q_buckets["-3 (-3π/4)"] += 1
        else:                    q_buckets["-1 (-π/4)"] += 1

    summary = dict(
        host=args.host, chain=args.chain, duration=args.duration,
        total_syms=int(syms.shape[0]),
        total_diff=int(diff.shape[0]),
        constellation=const_stats,
        distribution=diff_stats,
        quad_counts_sym={k: int(v) for k, v in q_buckets.items()},
    )
    with open(os.path.join(args.outdir, f"summary_{base}.json"), "w") as f:
        json.dump(summary, f, indent=2)

    # Pretty-print summary
    print(f"\nwrote {const_path}")
    print(f"wrote {eye_path}")
    print(f"wrote {diff_path}")
    print("\nDiff dibit distribution:")
    total = sum(q_buckets.values())
    for k in ("+1 (+π/4)", "-1 (-π/4)", "+3 (+3π/4)", "-3 (-3π/4)"):
        c = q_buckets[k]
        print(f"  {k:>12}: {c:5d}  ({100*c/total:5.1f}%)")
    print(f"\nPer-rail deviation stats:")
    for nom, r in diff_stats["rails"].items():
        print(f"  {nom:+5d} Hz: n={r['count']:4d}  "
              f"mean={r['mean_hz']:+7.1f}  stdev={r['stdev_hz']:6.1f}  "
              f"clean_frac={r['clean_frac']:.3f}")
    print(f"\nOverall clean_frac (|dev - nom| < 150 Hz): "
          f"{diff_stats['overall_clean_frac']:.3f}")
    print(f"AGC cluster mean |z|: {const_stats['mean_mag']:.3f}")


if __name__ == "__main__":
    main()
