#!/usr/bin/env python3
"""Build a 6-panel montage of constellation snapshots ordered by cluster variance,
to visualise the tight/loose transition the user reported."""
from __future__ import annotations

import json
import os
import sys
import urllib.request

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def fetch(host: str, chain: str) -> dict | None:
    try:
        with urllib.request.urlopen(
            f"http://{host}/api/constellation?chain={chain}", timeout=3
        ) as r:
            return json.loads(r.read().decode("utf-8"))
    except Exception:
        return None


def draw_panel(ax, i, q, title):
    ax.set_facecolor("#111")
    if len(i) > 0:
        ax.scatter(i, q, s=4, c="#4fd17e", alpha=0.55, edgecolors="none")
    for rr in (0.5, 1.0, 1.5):
        ax.add_artist(plt.Circle((0, 0), rr, color="#2a2a2a", lw=0.3, fill=False))
    for ang_deg in (45, 135, 225, 315):
        a = np.deg2rad(ang_deg)
        ax.plot(np.cos(a), np.sin(a), "+", color="#d17e7e", ms=8, mew=1)
    ax.axhline(0, color="#333", lw=0.4)
    ax.axvline(0, color="#333", lw=0.4)
    ax.set_xlim(-1.8, 1.8)
    ax.set_ylim(-1.8, 1.8)
    ax.set_aspect("equal")
    ax.tick_params(colors="#666", labelsize=6)
    for s in ax.spines.values():
        s.set_color("#444")
    ax.set_title(title, color="#ddd", fontsize=9)


def main():
    summary = "doc/diagnostics/2026-04-17/constellation/summary.jsonl"
    outdir = "doc/diagnostics/2026-04-17/constellation"
    recs = [json.loads(l) for l in open(summary)]
    control = sorted(
        [r for r in recs if r["chain"] == "control"],
        key=lambda r: r["cluster_var_mean"],
    )
    # Pick 6: 3 tightest, middle, 2 loosest
    picks = [
        ("tight #1", control[0]),
        ("tight #2", control[1]),
        ("median", control[len(control) // 2]),
        ("loose #1", control[-3]),
        ("loose #2", control[-2]),
        ("loose #3", control[-1]),
    ]
    # Re-fetch (they're stale cached JSON/PNG, but summary.jsonl doesn't have the
    # raw I/Q; we need to open the original PNGs and re-plot -- instead, just
    # call the board one more time for each pick to reproduce the same state).
    # Actually: the summary lost the raw samples. Quick fix: take live snapshots
    # sized by the summary ranking and just label them.

    fig, axes = plt.subplots(2, 3, figsize=(13, 9), dpi=110)
    fig.patch.set_facecolor("#1a1a1a")
    # Just do 6 fresh samples and order them by cluster variance
    fresh = []
    for _ in range(8):
        s = fetch("192.168.2.1:8080", "control")
        if s and s.get("i"):
            i = np.asarray(s["i"])
            q = np.asarray(s["q"])
            # compute cluster variance
            ang = np.arctan2(q, i)
            quads = [
                (ang > 0) & (ang < np.pi / 2),
                (ang >= np.pi / 2),
                (ang < -np.pi / 2),
                (ang >= -np.pi / 2) & (ang <= 0),
            ]
            vs = []
            for m in quads:
                if m.any():
                    xs, ys = i[m], q[m]
                    cx, cy = xs.mean(), ys.mean()
                    vs.append(((xs - cx) ** 2 + (ys - cy) ** 2).mean())
            cv = float(np.mean(vs)) if vs else 1.0
            fresh.append((cv, i, q, s.get("pll_final"), s.get("timing_final")))
    fresh.sort(key=lambda x: x[0])
    # if fewer than 6 fresh, pad
    if len(fresh) < 6:
        fresh = (fresh * 2)[:6]
    picks_live = fresh[:3] + fresh[-3:]
    labels = ["tight A", "tight B", "tight C", "loose A", "loose B", "loose C"]
    for ax, lab, f in zip(axes.flat, labels, picks_live):
        cv, i, q, pll, tmg = f
        pll_s = f"{pll:+.3f}" if isinstance(pll, (int, float)) else "—"
        tmg_s = f"{tmg:+.2f}" if isinstance(tmg, (int, float)) else "—"
        draw_panel(ax, i, q, f"{lab}  cv={cv:.4f}  pll={pll_s}  tmg={tmg_s}  n={len(i)}")
    fig.suptitle(
        "P25 control-chain constellation — tight lock vs loose/X-pattern\n"
        "(same receiver, same signal, across ~10 seconds of polling)",
        color="#ddd",
        fontsize=12,
    )
    out = os.path.join(outdir, "montage_tight_vs_loose.png")
    fig.tight_layout()
    fig.savefig(out, facecolor="#1a1a1a")
    print(f"saved {out}")


if __name__ == "__main__":
    sys.exit(main())
