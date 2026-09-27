"""Interface eye analysis: pass grids -> windows, centres and heatmaps.

Conventions (see :mod:`fbench.agent` CONTRACT for the reply shapes):

- IDELAY scan: one row of 32 pass/fail flags per LVDS lane (tap 0..31).
  One 7-series IDELAY tap is 1/(32 x 2 x 200 MHz) = 78.125 ps.
- AD9361 scan: 16 x 16 grid, rows = clock delay, columns = data delay.
- 2-D scan: per lane, rows = AD9361 data delay, columns = IDELAY tap.
"""

from __future__ import annotations

from pathlib import Path
from typing import Sequence

import numpy as np

IDELAY_TAP_NS = 1.0 / (32 * 2 * 200e6) * 1e9  # 0.078125 ns


def longest_run(flags: Sequence[bool | int]) -> tuple[int, int, int]:
    """Longest run of truthy flags: ``(start, end_inclusive, length)``.

    Returns ``(-1, -1, 0)`` when nothing passes. Ties keep the first run.
    """
    best = (-1, -1, 0)
    start = None
    for i, f in enumerate(list(flags) + [0]):
        if f and start is None:
            start = i
        elif not f and start is not None:
            length = i - start
            if length > best[2]:
                best = (start, i - 1, length)
            start = None
    return best


def lane_windows(rows: Sequence[Sequence[int]], tap_ns: float = IDELAY_TAP_NS) -> list[dict]:
    """Window/centre per lane for an IDELAY scan."""
    out = []
    for lane, row in enumerate(rows):
        start, end, width = longest_run(row)
        out.append({
            "lane": lane,
            "start": start,
            "end": end,
            "width_taps": width,
            "width_ns": round(width * tap_ns, 4),
            "centre": (start + end) // 2 if width else None,
            "pass_taps": int(sum(1 for f in row if f)),
            "taps": len(row),
        })
    return out


def eye_summary(rows: Sequence[Sequence[int]], tap_ns: float = IDELAY_TAP_NS) -> dict:
    """Minimum window across lanes and a common centre tap.

    The common centre is the middle of the intersection of all lane windows
    when it is non-empty (one IDELAY value for every lane), else ``None``.
    """
    wins = lane_windows(rows, tap_ns)
    widths = [w["width_taps"] for w in wins]
    starts = [w["start"] for w in wins if w["width_taps"]]
    ends = [w["end"] for w in wins if w["width_taps"]]
    common = None
    common_width = 0
    if wins and len(starts) == len(wins):
        lo, hi = max(starts), min(ends)
        if hi >= lo:
            common, common_width = (lo + hi) // 2, hi - lo + 1
    return {
        "lanes": wins,
        "window_taps_min": min(widths) if widths else 0,
        "window_ns_min": round((min(widths) if widths else 0) * tap_ns, 4),
        "worst_lane": int(np.argmin(widths)) if widths else None,
        "centre_tap": common,
        "common_window_taps": common_width,
    }


def point_margin(grid: Sequence[Sequence[int]], row: int, col: int) -> dict:
    """Steps from ``(row, col)`` to the nearest failing cell along row/column.

    A direction that runs off the grid without meeting a failure is
    unbounded in that direction (reported as ``None`` and ignored in the
    minimum). ``margin`` is the smallest bounded distance, or ``None`` if the
    point is unbounded in every direction. A failing point has margin 0.
    """
    g = np.asarray(grid, dtype=bool)
    if not g[row, col]:
        return {"margin": 0, "left": 0, "right": 0, "up": 0, "down": 0}

    def walk(dr: int, dc: int) -> int | None:
        r, c, steps = row, col, 0
        while True:
            r, c = r + dr, c + dc
            if not (0 <= r < g.shape[0] and 0 <= c < g.shape[1]):
                return None
            steps += 1
            if not g[r, c]:
                return steps
    dirs = {"left": walk(0, -1), "right": walk(0, 1), "up": walk(-1, 0), "down": walk(1, 0)}
    bounded = [v for v in dirs.values() if v is not None]
    return {"margin": min(bounded) if bounded else None, **dirs}


def grid_summary(grid: Sequence[Sequence[int]], chosen: tuple[int, int] | None) -> dict:
    """Summary of an AD9361 clock x data delay grid."""
    g = np.asarray(grid, dtype=bool)
    out: dict = {
        "shape": list(g.shape),
        "pass_cells": int(g.sum()),
        "row_windows": [longest_run(r)[2] for r in g],
        "col_windows": [longest_run(c)[2] for c in g.T],
    }
    if chosen is not None:
        m = point_margin(g, chosen[0], chosen[1])
        out["chosen"] = {"clk": chosen[0], "data": chosen[1]}
        out["chosen_margin"] = m
    return out


def heatmap_png(grid: Sequence[Sequence[float]], path: Path, title: str, xlabel: str,
                ylabel: str, mark: tuple[int, int] | None = None) -> Path | None:
    """Render a pass/fail (or error-rate) grid. Returns None if matplotlib is missing."""
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:  # pragma: no cover - host dependent
        return None
    g = np.asarray(grid, dtype=float)
    fig, ax = plt.subplots(figsize=(max(4.0, g.shape[1] * 0.25 + 1.5),
                                    max(3.0, g.shape[0] * 0.3 + 1.2)))
    im = ax.imshow(g, cmap="RdYlGn", vmin=0, vmax=1, aspect="auto", origin="lower",
                   interpolation="nearest")
    ax.set_title(title)
    ax.set_xlabel(xlabel)
    ax.set_ylabel(ylabel)
    if mark is not None:
        ax.plot([mark[1]], [mark[0]], marker="x", color="black", markersize=10, mew=2)
    fig.colorbar(im, ax=ax, label="pass")
    fig.tight_layout()
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(path, dpi=110)
    plt.close(fig)
    return path
