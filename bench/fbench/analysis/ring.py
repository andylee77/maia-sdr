"""Summaries of the agent's ring-checker anomaly reports.

Anomaly classes (design doc 5.3 ``ring check``): ``lap`` (reader lapped by
the writer), ``torn`` (sub-buffer overwritten while being copied),
``word_gap`` (missing words in the pattern), ``stale_line`` (cache line of
old data: coherency), ``splice`` (data from a previous session/epoch),
``bit_error`` (pattern mismatch without a gap).
"""

from __future__ import annotations

from typing import Any, Sequence

from .periodicity import detect_periodicity

ANOMALY_CLASSES: tuple[str, ...] = ("lap", "torn", "word_gap", "stale_line", "splice",
                                    "bit_error")


def summarise(reply: dict[str, Any]) -> dict[str, Any]:
    """Normalise a ``ring check`` reply into counts, rates and timing."""
    anomalies = list(reply.get("anomalies", []) or [])
    counts = {c: 0 for c in ANOMALY_CLASSES}
    reported = dict(reply.get("counts", {}) or {})
    for c in ANOMALY_CLASSES:
        counts[c] = int(reported.get(c, 0))
    if not reported:
        for a in anomalies:
            cls = str(a.get("class", "unknown"))
            counts[cls] = counts.get(cls, 0) + 1
    extra = {k: int(v) for k, v in reported.items() if k not in counts}
    counts.update(extra)
    total = sum(counts.values())
    times = [float(a["t"]) for a in anomalies if "t" in a]
    seconds = float(reply.get("seconds", 0) or 0)
    out: dict[str, Any] = {
        "total": total,
        "counts": counts,
        "bytes_checked": int(reply.get("bytes_checked", 0) or 0),
        "lost_bytes": int(reply.get("lost_bytes", 0) or 0),
        "subbuffers": int(reply.get("subbuffers", 0) or 0),
        "seconds": seconds,
        "rate_per_hour": (total / seconds * 3600.0) if seconds > 0 else None,
        "first_t": min(times) if times else None,
        "last_t": max(times) if times else None,
        "classes_seen": sorted(c for c, v in counts.items() if v),
    }
    if len(times) >= 5:
        out["periodicity"] = detect_periodicity(times).to_dict()
    return out


def expected_lap_onset_ms(num_buffers: int, subbuf_period_ms: float) -> float:
    """Reader stall above which a lap loses data: (N - 1) x T_buf."""
    return (num_buffers - 1) * subbuf_period_ms


def lap_onset(points: Sequence[tuple[float, float]]) -> float | None:
    """Smallest stall (ms) that showed loss, from ``(stall_ms, lost)`` points."""
    lossy = sorted(s for s, lost in points if lost > 0)
    return lossy[0] if lossy else None


def last_clean_stall(points: Sequence[tuple[float, float]]) -> float | None:
    clean = sorted(s for s, lost in points if lost <= 0)
    onset = lap_onset(points)
    if onset is not None:
        clean = [s for s in clean if s < onset]
    return clean[-1] if clean else None
