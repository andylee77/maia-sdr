"""Detect periodic events (e.g. a ~10 s dropout) in a list of timestamps.

Two complementary views:

1. **Phase coherence (Rayleigh statistic / event-train spectrum).** For a
   trial period P, ``R(P) = |mean(exp(2*pi*i*t_k/P))|``. Strictly periodic
   events give R ~ 1 even when occurrences are missing; random events give
   R ~ 1/sqrt(n). R is evaluated on a fine frequency grid (this is the
   magnitude of the Fourier transform of the impulse train), the maximum is
   taken, and the fundamental is chosen as the lowest-frequency peak within
   90 % of the maximum (harmonics k/P also score high).
2. **Interval histogram.** The fraction of inter-event intervals that are an
   integer multiple (1..3) of P within ``rel_tol``.

Significance: with n events and M trial frequencies, ``z = n R^2`` exceeds
``ln(M) + 5`` for random data with probability < ~0.7 %.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Sequence

import numpy as np


@dataclass
class PeriodicityResult:
    periodic: bool
    period_s: float | None
    rayleigh_r: float
    z: float
    z_threshold: float
    n_events: int
    interval_match_fraction: float
    median_interval_s: float | None
    candidates: list[dict] = field(default_factory=list)

    def to_dict(self) -> dict:
        return asdict(self)


def _rayleigh(times: np.ndarray, freqs: np.ndarray, chunk: int = 2048) -> np.ndarray:
    out = np.empty(len(freqs))
    for s in range(0, len(freqs), chunk):
        f = freqs[s:s + chunk, None]
        out[s:s + chunk] = np.abs(np.exp(2j * np.pi * f * times[None, :]).mean(axis=1))
    return out


def interval_match_fraction(intervals: np.ndarray, period: float, rel_tol: float = 0.05,
                            max_multiple: int = 3) -> float:
    if len(intervals) == 0 or period <= 0:
        return 0.0
    ratio = intervals / period
    k = np.clip(np.round(ratio), 1, max_multiple)
    ok = (np.abs(ratio - k) <= rel_tol * k) & (ratio >= 0.5) & (ratio <= max_multiple + 0.5)
    return float(np.mean(ok))


def detect_periodicity(times: Sequence[float], min_events: int = 5,
                       min_period_s: float = 0.5, max_period_s: float | None = None,
                       rel_tol: float = 0.05, r_threshold: float = 0.5,
                       oversample: int = 8) -> PeriodicityResult:
    """Look for a dominant period in event ``times`` (seconds)."""
    t = np.sort(np.asarray(times, dtype=float))
    n = len(t)
    intervals = np.diff(t)
    median = float(np.median(intervals)) if len(intervals) else None
    empty = PeriodicityResult(False, None, 0.0, 0.0, 0.0, n, 0.0, median)
    if n < min_events:
        return empty
    span = float(t[-1] - t[0])
    if span <= 0:
        return empty
    max_p = max_period_s if max_period_s is not None else span / 2.0
    if max_p <= min_period_s:
        return empty
    f_lo, f_hi = 1.0 / max_p, 1.0 / min_period_s
    df = 1.0 / (oversample * span)
    freqs = np.arange(f_lo, f_hi + df, df)
    if len(freqs) > 400_000:  # keep it bounded for very long soaks
        freqs = np.linspace(f_lo, f_hi, 400_000)
    t0 = t - t[0]
    r = _rayleigh(t0, freqs)
    i_max = int(np.argmax(r))
    r_max = float(r[i_max])
    # Fundamental: lowest frequency whose local peak reaches 90 % of the max.
    cand_idx = [i for i in range(1, len(r) - 1)
                if r[i] >= r[i - 1] and r[i] >= r[i + 1] and r[i] >= 0.9 * r_max]
    if not cand_idx:
        cand_idx = [i_max]
    fund = min(cand_idx, key=lambda i: freqs[i])
    period = 1.0 / float(freqs[fund])
    r_fund = float(r[fund])
    z = n * r_fund ** 2
    z_thr = float(np.log(len(freqs)) + 5.0)
    match = interval_match_fraction(intervals, period, rel_tol)
    top = np.argsort(r)[::-1][:5]
    candidates = [{"period_s": float(1.0 / freqs[i]), "rayleigh_r": float(r[i])} for i in top]
    periodic = bool(r_fund >= r_threshold and z >= z_thr)
    return PeriodicityResult(periodic, period, r_fund, float(z), z_thr, n, match, median,
                             candidates)
