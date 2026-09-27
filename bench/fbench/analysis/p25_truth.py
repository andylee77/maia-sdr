"""Ground truth for P25 replays from SDRTrunk's per-call ``.mbe`` recordings.

SDRTrunk writes one JSON ``.mbe`` per transmission (``frames[]`` with epoch-ms
``time`` and IMBE ``hex``), named ``YYYYMMDD_HHMMSS_<freq>_<n>_<to>_<from>.mbe``
in local time. A wideband capture ``<unix>_<freq>_<rate>_baseband.wav`` of the
same air lets a replay window be scored against what SDRTrunk decoded live.
"""

from __future__ import annotations

import json
import re
import time
from pathlib import Path
from typing import Any

from .sigmf import SDRTRUNK_NAME

MBE_NAME = re.compile(r"^(\d{8})_(\d{6})_(\d+)_(\d+)_(\d+)_(\d+)\.mbe$")
EDGE_S = 0.5  # a transmission this close to a window edge may be split by decode latency


def clip_epoch(path: str | Path) -> float | None:
    """Capture start (unix s) from an SDRTrunk baseband file name."""
    m = SDRTRUNK_NAME.match(Path(path).name)
    return float(m.group(1)) if m else None


def _name_epoch(m: re.Match[str]) -> float:
    return time.mktime(time.strptime(m.group(1) + m.group(2), "%Y%m%d%H%M%S"))


def mbe_truth(truth_dir: str | Path, t0: float, t1: float,
              lead_s: float = 600.0) -> dict[str, Any]:
    """IMBE frames SDRTrunk decoded with air time in ``[t0, t1)`` (unix s).

    Files are pre-filtered by the local-time stamp in their name (a recording
    starts at most ``lead_s`` before its first frame in the window).
    """
    root = Path(truth_dir)
    if not root.is_dir():
        raise FileNotFoundError(f"truth dir {root} not found")
    calls: list[dict[str, Any]] = []
    for f in sorted(root.glob("*.mbe")):
        m = MBE_NAME.match(f.name)
        if not m:
            continue
        start = _name_epoch(m)
        if start < t0 - lead_s or start >= t1 + 2.0:
            continue
        try:
            doc = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        times = [fr["time"] / 1000.0 for fr in doc.get("frames", []) if "time" in fr]
        inside = [t for t in times if t0 <= t < t1]
        if not inside:
            continue
        calls.append({
            "file": f.name, "freq_hz": int(m.group(3)), "to": doc.get("to"),
            "from": doc.get("from"), "encrypted": bool(doc.get("encrypted")),
            "frames": len(inside), "frames_total": len(times),
            "first_s": round(min(inside) - t0, 3), "last_s": round(max(inside) - t0, 3),
            "straddles": len(inside) < len(times) or min(inside) - t0 < EDGE_S
            or t1 - max(inside) < EDGE_S,
        })
    return {"t0": t0, "t1": t1, "imbe": sum(c["frames"] for c in calls),
            "imbe_clear": sum(c["frames"] for c in calls if not c["encrypted"]),
            "transmissions": len(calls), "calls": calls}
