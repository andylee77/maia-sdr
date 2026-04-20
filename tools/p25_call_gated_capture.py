#!/usr/bin/env python3
"""Capture traffic-chain constellation + eye snapshots ONLY during active calls.

Polls `/api/traffic` for `current_tg != 0` AND a fresh LDU (last_imbe_secs_ago
below a threshold), then captures `/api/constellation?chain=traffic`. Writes
one summary.jsonl line + one PNG per snapshot tagged with the active TG so the
locked-vs-idle split is unambiguous in post-analysis.

Gate logic:
- `current_tg != 0`             → follower has a grant
- `last_imbe_secs_ago < 1.0`    → IMBEs are still arriving (mid-call, not hold)
- else skip the tick

Usage:
    python tools/p25_call_gated_capture.py \\
        --host 192.168.2.1:8080 --minutes 3 \\
        --outdir doc/diagnostics/2026-04-19/call_gated
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sys
import time
import urllib.error
import urllib.request

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def fetch(host: str, path: str, timeout: float = 3.0):
    url = f"http://{host}{path}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
        print(f"  fetch fail ({path}): {e}", file=sys.stderr)
        return None


def cluster_metrics(i: np.ndarray, q: np.ndarray) -> dict:
    if len(i) == 0:
        return {"n": 0, "cluster_var_mean": None}
    angles = np.arctan2(q, i)
    quads = [
        (angles > 0) & (angles < np.pi / 2),
        (angles >= np.pi / 2) & (angles <= np.pi),
        (angles > -np.pi) & (angles < -np.pi / 2),
        (angles >= -np.pi / 2) & (angles <= 0),
    ]
    variances = []
    for m in quads:
        if not m.any():
            continue
        xs, ys = i[m], q[m]
        cx, cy = xs.mean(), ys.mean()
        variances.append(float(((xs - cx) ** 2 + (ys - cy) ** 2).mean()))
    return {
        "n": int(len(i)),
        "cluster_var_mean": float(np.mean(variances)) if variances else None,
        "radius_mean": float(np.hypot(i, q).mean()),
    }


def render_png(out_path, snap, title, metrics):
    i = np.asarray(snap.get("i") or [])
    q = np.asarray(snap.get("q") or [])
    fig, ax = plt.subplots(figsize=(5.5, 5.5), dpi=120)
    ax.set_facecolor("#111")
    ax.scatter(i, q, s=8, c="#4e7", alpha=0.8)
    ref = 0.7071
    for rx, ry in [(+ref, +ref), (-ref, +ref), (-ref, -ref), (+ref, -ref)]:
        ax.plot(rx, ry, "+", color="#f88", markersize=14, mew=2)
    for r in (0.5, 1.0, 1.5):
        ax.add_patch(plt.Circle((0, 0), r, fill=False, color="#333", lw=0.5))
    ax.set_xlim(-1.8, 1.8)
    ax.set_ylim(-1.8, 1.8)
    ax.set_aspect("equal")
    ax.set_xlabel("I", color="#888")
    ax.set_ylabel("Q", color="#888")
    ax.tick_params(colors="#888")
    ax.set_title(title, color="#ddd", fontsize=10, loc="left")
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#111")
    plt.close(fig)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--minutes", type=float, default=3.0)
    ap.add_argument("--interval", type=float, default=0.75, help="poll period (s)")
    ap.add_argument("--max-imbe-age", type=float, default=1.0,
                    help="gate: last_imbe_secs_ago must be < this")
    ap.add_argument("--outdir",
                    default="doc/diagnostics/2026-04-19/call_gated")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    summary_path = os.path.join(args.outdir, "summary.jsonl")

    start = time.monotonic()
    deadline = start + args.minutes * 60
    captured = 0
    skipped_idle = 0
    skipped_stale = 0

    with open(summary_path, "a", encoding="utf-8") as summary_fh:
        while time.monotonic() < deadline:
            now = time.monotonic()
            trf = fetch(args.host, "/api/traffic")
            if trf is None:
                time.sleep(args.interval)
                continue
            cur_tg = trf.get("current_talkgroup") or 0
            if cur_tg == 0:
                skipped_idle += 1
                time.sleep(args.interval)
                continue
            imbe = trf.get("imbe") or {}
            last_imbe = imbe.get("last_imbe_secs_ago")
            if last_imbe is None or last_imbe > args.max_imbe_age:
                skipped_stale += 1
                time.sleep(args.interval)
                continue

            # Active call + fresh IMBEs. Capture traffic constellation.
            snap = fetch(args.host, "/api/constellation?chain=traffic")
            if snap is None:
                time.sleep(args.interval)
                continue
            i = np.asarray(snap.get("i") or [])
            q = np.asarray(snap.get("q") or [])
            if len(i) == 0:
                time.sleep(args.interval)
                continue
            metrics = cluster_metrics(i, q)

            ts = dt.datetime.now().strftime("%H%M%S")
            pll = snap.get("pll_final") or 0.0
            tfin = snap.get("timing_final") or 0.0
            fname = f"{ts}_tg{cur_tg}_p{pll:+.2f}_cv{metrics['cluster_var_mean']:.3f}.png".replace(
                ".", "_", 2
            )
            # restore trailing .png
            if not fname.endswith(".png"):
                fname = fname + ".png"
            out_path = os.path.join(args.outdir, fname)
            title = (f"traffic {ts}  tg={cur_tg}\n"
                     f"n={metrics['n']} pll={pll:+.3f} tmg={tfin:+.3f} "
                     f"cv={metrics['cluster_var_mean']:.4f} imbe_age={last_imbe:.2f}s")
            render_png(out_path, snap, title, metrics)

            rec = {
                "ts": ts,
                "tg": cur_tg,
                "png": fname,
                "last_imbe_secs_ago": last_imbe,
                "n": metrics["n"],
                "cluster_var_mean": metrics["cluster_var_mean"],
                "pll_final": pll,
                "timing_final": tfin,
                "radius_mean": metrics.get("radius_mean"),
                "hdu_count": imbe.get("hdu_count"),
                "ldu1_count": imbe.get("ldu1_count"),
                "ldu2_count": imbe.get("ldu2_count"),
                "vocoder_frames_silent_suppressed":
                    imbe.get("vocoder_frames_silent_suppressed"),
            }
            summary_fh.write(json.dumps(rec) + "\n")
            summary_fh.flush()
            captured += 1
            print(
                f"  [{ts}] tg={cur_tg} cv={metrics['cluster_var_mean']:.4f} "
                f"pll={pll:+.3f} imbe_age={last_imbe:.2f}s"
            )
            time.sleep(args.interval)

    print(
        f"done. captured={captured} skipped_idle={skipped_idle} "
        f"skipped_stale={skipped_stale} outdir={args.outdir}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
