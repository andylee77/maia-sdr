#!/usr/bin/env python3
"""
Poll /api/constellation at ~1 Hz, save JSON + render PNG for each snapshot.

Each PNG is tagged with timestamp + pll + timing in the filename so interesting
states (X-pattern PLL excursions, cluster tightness) can be filtered afterwards.

Also writes a summary.jsonl with one line per snapshot so post-analysis can
pick out interesting frames without re-parsing the PNGs.
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


def fetch(host: str, chain: str, timeout: float = 3.0) -> dict | None:
    url = f"http://{host}/api/constellation?chain={chain}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
        print(f"  fetch fail ({chain}): {e}", file=sys.stderr)
        return None


def cluster_metrics(i: np.ndarray, q: np.ndarray) -> dict:
    """Sort points into 4 quadrants and compute a tightness metric per cluster."""
    if len(i) == 0:
        return {"n": 0, "cluster_var_mean": None, "radius_mean": None}

    mags = np.hypot(i, q)
    angles = np.arctan2(q, i)

    quads = [
        (angles > 0) & (angles < np.pi / 2),
        (angles >= np.pi / 2) & (angles <= np.pi),
        (angles > -np.pi) & (angles < -np.pi / 2),
        (angles >= -np.pi / 2) & (angles <= 0),
    ]
    variances = []
    centroids = []
    for m in quads:
        if not m.any():
            continue
        xs, ys = i[m], q[m]
        cx, cy = xs.mean(), ys.mean()
        centroids.append((cx, cy))
        d2 = (xs - cx) ** 2 + (ys - cy) ** 2
        variances.append(float(d2.mean()))

    return {
        "n": int(len(i)),
        "cluster_var_mean": float(np.mean(variances)) if variances else None,
        "radius_mean": float(mags.mean()),
        "radius_std": float(mags.std()),
        "angle_std": float(angles.std()),
        "centroids": centroids,
    }


def render_png(
    out_path: str, snapshot: dict, chain: str, metrics: dict, timestamp: str
) -> None:
    i = np.asarray(snapshot.get("i") or [])
    q = np.asarray(snapshot.get("q") or [])

    fig, ax = plt.subplots(figsize=(5.5, 5.5), dpi=120)
    ax.set_facecolor("#111")

    if len(i) > 0:
        ax.scatter(
            i,
            q,
            s=6,
            c="#4fd17e",
            alpha=0.55,
            edgecolors="none",
        )

    r = 1.8
    ax.axhline(0, color="#333", lw=0.5, zorder=0)
    ax.axvline(0, color="#333", lw=0.5, zorder=0)
    for rr in (0.5, 1.0, 1.5):
        circle = plt.Circle((0, 0), rr, color="#2a2a2a", lw=0.4, fill=False, zorder=0)
        ax.add_artist(circle)

    # Expected LSM cluster centers at ±π/4, ±3π/4 at unit radius
    for ang_deg in (45, 135, 225, 315):
        ang = np.deg2rad(ang_deg)
        ax.plot(np.cos(ang), np.sin(ang), "+", color="#d17e7e", ms=12, mew=1.2)

    ax.set_xlim(-r, r)
    ax.set_ylim(-r, r)
    ax.set_aspect("equal")
    ax.set_xlabel("I", color="#aaa", fontsize=9)
    ax.set_ylabel("Q", color="#aaa", fontsize=9)
    ax.tick_params(colors="#777", labelsize=8)
    for s in ax.spines.values():
        s.set_color("#444")

    pll = snapshot.get("pll_final")
    tmg = snapshot.get("timing_final")
    cv = metrics.get("cluster_var_mean")
    pll_str = f"{pll:.3f}" if isinstance(pll, (int, float)) else "—"
    tmg_str = f"{tmg:.3f}" if isinstance(tmg, (int, float)) else "—"
    cv_str = f"{cv:.4f}" if isinstance(cv, (int, float)) else "—"

    title = (
        f"{chain}  {timestamp}\n"
        f"n={metrics['n']}  pll={pll_str}  tmg={tmg_str}  clustervar={cv_str}"
    )
    ax.set_title(title, color="#ddd", fontsize=10, loc="left")
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#111")
    plt.close(fig)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument(
        "--chain",
        choices=["control", "traffic", "both"],
        default="both",
    )
    ap.add_argument("--interval", type=float, default=1.5, help="seconds between polls")
    ap.add_argument("--duration", type=float, default=90.0, help="total seconds to run")
    ap.add_argument(
        "--outdir",
        default="doc/diagnostics/2026-04-17/constellation",
    )
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    summary_path = os.path.join(args.outdir, "summary.jsonl")

    chains = ["control", "traffic"] if args.chain == "both" else [args.chain]
    start = time.monotonic()
    captured = 0
    with open(summary_path, "a", encoding="utf-8") as summary_fh:
        while time.monotonic() - start < args.duration:
            ts = dt.datetime.now().strftime("%H%M%S")
            for chain in chains:
                snap = fetch(args.host, chain)
                if snap is None:
                    continue
                i = np.asarray(snap.get("i") or [])
                q = np.asarray(snap.get("q") or [])
                if len(i) == 0:
                    # traffic chain often returns empty when idle
                    continue
                metrics = cluster_metrics(i, q)
                pll = snap.get("pll_final")
                pll_tag = (
                    f"p{pll:+.2f}".replace(".", "_")
                    if isinstance(pll, (int, float))
                    else "p_"
                )
                fname = f"{ts}_{chain}_{pll_tag}.png"
                path = os.path.join(args.outdir, fname)
                render_png(path, snap, chain, metrics, ts)

                rec = {
                    "ts": ts,
                    "chain": chain,
                    "png": fname,
                    "n": metrics["n"],
                    "pll_final": pll,
                    "timing_final": snap.get("timing_final"),
                    "cluster_var_mean": metrics.get("cluster_var_mean"),
                    "radius_mean": metrics.get("radius_mean"),
                    "angle_std": metrics.get("angle_std"),
                }
                summary_fh.write(json.dumps(rec) + "\n")
                summary_fh.flush()
                captured += 1
                print(
                    f"  [{ts}] {chain} n={metrics['n']} "
                    f"pll={pll} cv={metrics['cluster_var_mean']}"
                )
            time.sleep(args.interval)
    print(f"done. captured={captured} outdir={args.outdir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
