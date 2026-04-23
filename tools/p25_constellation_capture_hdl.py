#!/usr/bin/env python3
"""
Capture constellation snapshots from the Phase 10.7 HDL post-PLL ring.

Streams /ws/iq?source=post_pll (9.6 kSPS, 2 samples per symbol — mid+sym
interleaved) from the board, windows into N-second blocks, computes the
same per-block metrics as `p25_constellation_capture.py` (radius_mean,
cluster_var_mean, angle_std, PLL-lock proxy), and writes PNGs plus a
summary.jsonl so the breathing pattern on the HDL ring can be compared
directly with the /api/constellation (software-PLL) path.

Why this tool exists: `/api/constellation` runs a fresh software Gardner
+ PLL over each 33 ms snapshot, so the per-snapshot PLL never fully
settles and the reported cluster variance breathes 8x across a 30 s
window even on a healthy signal. The HDL post-PLL ring is continuously
locked, so breathing measured here reflects *real* signal variance.
"""
from __future__ import annotations

import argparse
import asyncio
import datetime as dt
import json
import math
import os
import statistics
import struct
import sys

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

try:
    import websockets
except ImportError:
    print("websockets package required: pip install websockets", file=sys.stderr)
    sys.exit(1)


# Match the firmware scaling in p25-httpd/src/httpd/api/debug.rs
# (rotate output is Q1.13; peak normalisation is done per-frame).
Q13_SCALE = 1.0 / 8192.0


def cluster_metrics(i: np.ndarray, q: np.ndarray) -> dict:
    """Per-quadrant cluster stats mirroring tools/p25_constellation_capture.py."""
    quads = [(i > 0) & (q > 0), (i > 0) & (q < 0),
             (i < 0) & (q < 0), (i < 0) & (q > 0)]
    vars_ = []
    radii = []
    for mask in quads:
        pts_r = i[mask]
        pts_i = q[mask]
        if pts_r.size < 3:
            continue
        cx, cy = pts_r.mean(), pts_i.mean()
        d2 = (pts_r - cx) ** 2 + (pts_i - cy) ** 2
        vars_.append(float(d2.mean()))
        radii.append(math.hypot(cx, cy))
    radius_mean = float(np.mean(radii)) if radii else 0.0
    cluster_var_mean = float(np.mean(vars_)) if vars_ else 0.0
    ang = np.arctan2(q, i)
    ang_std = float(ang.std())
    return dict(radius_mean=radius_mean,
                cluster_var_mean=cluster_var_mean,
                angle_std=ang_std,
                n=int(i.size))


def render_constellation(i: np.ndarray, q: np.ndarray,
                         out_path: str, meta: dict) -> None:
    fig, ax = plt.subplots(figsize=(4.5, 4.5), dpi=110)
    ax.scatter(i, q, s=4, alpha=0.35, c="#4cf")
    circle = plt.Circle((0, 0), 1.0, fill=False, color="#345", lw=0.8)
    ax.add_patch(circle)
    for ix, iy in [(1, 1), (1, -1), (-1, -1), (-1, 1)]:
        ax.plot(ix, iy, marker="x", color="#888", markersize=8, markeredgewidth=1.4)
    ax.axhline(0, color="#223", lw=0.6)
    ax.axvline(0, color="#223", lw=0.6)
    ax.set_xlim(-1.6, 1.6)
    ax.set_ylim(-1.6, 1.6)
    ax.set_aspect("equal")
    ax.set_facecolor("#0a0f1c")
    title = (f"t={meta['t']}s  n={meta['n']}  "
             f"radius={meta['radius_mean']:.3f}  "
             f"cvar={meta['cluster_var_mean']:.3f}")
    ax.set_title(title, fontsize=9, color="#aab")
    ax.tick_params(colors="#aab", labelsize=8)
    for spine in ax.spines.values():
        spine.set_color("#223")
    fig.patch.set_facecolor("#0a0f1c")
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#0a0f1c", dpi=110)
    plt.close(fig)


def render_eye(both: np.ndarray, out_path: str, meta: dict) -> None:
    """P25 / OP25 Datascope-style eye: overlay many symbol periods of
    the deviation-equivalent waveform, rails at ±3 / ±1.

    The HDL post-PLL ring outputs post-diff-demod (rotated) complex
    samples z[n] = z_pre[n] * conj(z_pre[n-1]). For clean CQPSK/LSM or
    C4FM, arg(z[n]) takes one of four values at symbol instants:
      +π/4 → +1    +3π/4 → +3
      −π/4 → −1    −3π/4 → −3
    This phase angle IS the P25 symbol-deviation signal; scaling by
    4/π lands the rails exactly on ±1 and ±3 (three eye openings).

    `both` is the interleaved (mid, sym, mid, sym, ...) series at
    2 samples per symbol.
    """
    SPS = 2
    OVERLAY = SPS * 2  # 2 symbols per overlay trace

    # arg(z) for every sample → scale to ±3 / ±1 rails.
    phase = np.arctan2(both[:, 1], both[:, 0]) * (4.0 / np.pi)

    fig, ax = plt.subplots(figsize=(7.0, 4.0), dpi=110)
    x = np.arange(OVERLAY) / (OVERLAY - 1)
    for start in range(0, phase.size - OVERLAY, SPS):
        ax.plot(x, phase[start:start + OVERLAY],
                color="#4cf", lw=0.4, alpha=0.12)
    # P25 decision-level rails.
    for lvl, style in ((+3, "--"), (+1, "--"), (-1, "--"), (-3, "--")):
        ax.axhline(lvl, color="#678", lw=0.7, ls=style)
    ax.axhline(0, color="#223", lw=0.5)
    ax.set_ylabel("symbol deviation", color="#aab")
    ax.set_ylim(-4.0, 4.0)
    ax.set_yticks([-3, -1, 0, 1, 3])
    ax.set_xlabel("symbol periods (2-sps overlay)", color="#aab")
    ax.set_facecolor("#0a0f1c")
    ax.tick_params(colors="#aab", labelsize=9)
    for spine in ax.spines.values():
        spine.set_color("#223")
    title = (f"eye t={meta['t']}s  n={meta['n']}  "
             f"cvar={meta['cluster_var_mean']:.3f}  "
             f"SPS=2 (HDL post-PLL, atan2·4/π)")
    ax.set_title(title, fontsize=9, color="#aab")
    fig.patch.set_facecolor("#0a0f1c")
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#0a0f1c", dpi=110)
    plt.close(fig)


async def stream(host: str, chain: str, seconds: float,
                 outdir: str, window_s: float) -> list[dict]:
    uri = f"ws://{host}/ws/iq?source=post_pll&chain={chain}"
    print(f"connecting {uri}", file=sys.stderr)
    pairs: list[tuple[int, int]] = []
    hello = None
    async with websockets.connect(uri, max_size=None) as ws:
        deadline = asyncio.get_event_loop().time() + seconds
        try:
            while asyncio.get_event_loop().time() < deadline:
                remaining = deadline - asyncio.get_event_loop().time()
                if remaining <= 0:
                    break
                frame = await asyncio.wait_for(ws.recv(), timeout=remaining + 0.5)
                if isinstance(frame, str):
                    if hello is None:
                        hello = frame
                        print(f"hello: {hello}", file=sys.stderr)
                    continue
                n = len(frame) // 4
                for k in range(n):
                    re, im = struct.unpack_from("<hh", frame, k * 4)
                    pairs.append((re, im))
        except asyncio.TimeoutError:
            print("ws recv timed out (end of capture)", file=sys.stderr)
    if not pairs:
        raise SystemExit("no data received on /ws/iq?source=post_pll")
    arr = np.array(pairs, dtype=np.int32).astype(np.float32) * Q13_SCALE
    # Odd-indexed samples are rotate_sym (decision-time); even are rotate_mid.
    sym = arr[1::2]
    print(f"collected {arr.shape[0]} samples → {sym.shape[0]} symbol-time points",
          file=sys.stderr)

    # Window into blocks of window_s seconds = window_s * 4800 symbols each.
    # The eye needs the full mid+sym series, so compute the raw-samples-
    # per-block at 9600 sps = 2 * 4800.
    syms_per_block = int(window_s * 4800)
    samples_per_block = 2 * syms_per_block
    os.makedirs(outdir, exist_ok=True)
    rows = []
    n_blocks = min(sym.shape[0] // syms_per_block,
                   arr.shape[0] // samples_per_block)
    for b in range(n_blocks):
        s = b * syms_per_block
        e = s + syms_per_block
        sblock = sym[s:e]
        i = sblock[:, 0]
        q = sblock[:, 1]
        # Normalise by mean-magnitude (NOT peak): the HDL post-PLL stream
        # has occasional outlier peaks that throw off peak normalisation,
        # collapsing the observed clusters to ~0.22. Mean |z| is a
        # stable estimate of cluster radius, so clusters land at ~1
        # regardless of outliers. peak_raw_q13 is still reported.
        mags = np.hypot(i, q)
        peak = float(mags.max() or 1.0)
        norm = float(mags.mean() or 1.0)
        in_ = i / norm
        qn = q / norm
        m = cluster_metrics(in_, qn)
        row = {
            "t": round(b * window_s, 2),
            "n": m["n"],
            "radius_mean": m["radius_mean"],
            "cluster_var_mean": m["cluster_var_mean"],
            "angle_std": m["angle_std"],
            "peak_raw_q13": peak,
        }
        rows.append(row)
        tag = f"{b:03d}_t{int(row['t']):04d}"
        render_constellation(in_, qn,
                             os.path.join(outdir, f"const_{tag}.png"),
                             dict(t=row["t"], **m))
        rs = b * samples_per_block
        re_ = rs + samples_per_block
        eye_block = arr[rs:re_]
        render_eye(eye_block,
                   os.path.join(outdir, f"eye_{tag}.png"),
                   dict(t=row["t"], **m))
    with open(os.path.join(outdir, "summary.jsonl"), "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    return rows


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--chain", choices=["control", "traffic"], default="control")
    ap.add_argument("--duration", type=float, default=30.0)
    ap.add_argument("--window", type=float, default=1.0,
                    help="seconds per block (default 1.0)")
    ap.add_argument("--outdir", default="constellation_hdl_capture")
    args = ap.parse_args()
    rows = asyncio.run(stream(args.host, args.chain,
                              args.duration, args.outdir, args.window))
    print(f"\nwrote {len(rows)} snapshots + summary.jsonl to {args.outdir}")
    print(f"\n{'t_s':>5} {'n':>5} {'radius':>7} {'cvar':>7} {'angstd':>7}")
    for r in rows:
        print(f"{r['t']:>5} {r['n']:>5} "
              f"{r['radius_mean']:>7.3f} {r['cluster_var_mean']:>7.3f} "
              f"{r['angle_std']:>7.3f}")


if __name__ == "__main__":
    main()
