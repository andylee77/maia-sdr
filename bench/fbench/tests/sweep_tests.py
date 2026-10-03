"""rf.freq_sweep: one cabled direction's RX and TX chains across the whole tuning range.

The RX LO steps from ``start_mhz`` to ``stop_mhz`` (``points_per_octave`` log steps, at most
``max_step_mhz`` apart, plus the AD9363's specified edges). The TX LO follows ``lo_offset_hz``
above it and the tone sits ``offset_hz`` above the TX LO, so every line of interest lands on its
own baseband frequency:

    tone          +(lo_offset + offset)
    TX LO leak    +lo_offset                dds and cyclic (pattern's tone is its LO)
    TX image      +(lo_offset - offset)     dds and cyclic
    RX image      -(lo_offset + offset)
    RX DC         0

Each stimulus method sweeps the same plan in turn, so the receive results repeat under
different generators and the transmit results separate by method. At every point the test
records both synthesizers' lock bits (AD9361 SPI 0x247 and 0x287), the LO read-backs, RSSI and
one capture. The TX quadrature calibration runs at every point (``tx_quad_cal``), so leakage
and image show what the chip calibrates at that frequency rather than when the driver last
calibrated.

One run is one direction. ``compare=<run dirs>`` overlays other runs of this test and plots
the level difference wherever two runs share the TX unit (the RX chains' difference) or the
RX unit (the TX chains' difference). That holds when the same cable and pads carried both runs.
"""

from __future__ import annotations

import json
import math
from itertools import combinations
from pathlib import Path
from typing import Any

import numpy as np

from .. import REPO_ROOT
from ..analysis.tone import estimate_tone, spectrum_db
from ..config import TRANSCEIVER_LIMITS
from ..errors import FbenchError, PreconditionError, SafetyRefusal, UsageError
from ..runner import AnalysisContext, Outcome, TestContext, bench_test, load_result
from ..stimulus import (
    RX_LO_CHAN,
    TX_LO_CHAN,
    ToneSource,
    capture_iq,
    direction_metrics,
    load_capture,
    parse_number,
    reverse_direction_note,
    rssi_db,
    rx_state,
    set_rx_gain,
)

METHODS = ("dds", "pattern", "cyclic")
RX_SYNTH_LOCK_REG, TX_SYNTH_LOCK_REG, VCO_LOCK = 0x247, 0x287, 0x02
LO_READBACK_TOL_HZ = 1e3
AT_FLOOR_DB = 10.0  # a line closer than this to the noise floor is only an upper bound
SPEC_EDGES_HZ = (TRANSCEIVER_LIMITS["AD9363"]["lo_min_hz"],
                 TRANSCEIVER_LIMITS["AD9363"]["lo_max_hz"])


# ---------------------------------------------------------------------------
# Plan and per-point measurement
# ---------------------------------------------------------------------------


def plan_freqs(start_hz: float, stop_hz: float, per_octave: int, max_step_hz: float,
               extra: tuple[float, ...] = ()) -> list[float]:
    """Log steps at the bottom, at most ``max_step_hz`` apart above, on a 100 kHz grid."""
    if not 0 < start_hz < stop_hz:
        raise UsageError("the sweep needs 0 < start < stop")
    if per_octave < 1 or max_step_hz <= 0:
        raise UsageError("points_per_octave must be >= 1 and max_step_mhz > 0")
    pts = [start_hz, stop_hz]
    f, ratio = start_hz, 2.0 ** (1.0 / per_octave)
    while True:
        f = min(f * ratio, f + max_step_hz)
        if f >= stop_hz:
            break
        pts.append(f)
    pts += [e for e in extra if start_hz < e < stop_hz]
    return sorted({round(p / 1e5) * 1e5 for p in pts})


def _peak_db(freqs: np.ndarray, pdb: np.ndarray, f: float, tol: float) -> float:
    m = np.abs(freqs - f) <= tol
    return float(pdb[m].max()) if m.any() else float("nan")


def measure_point(iq: np.ndarray, fs: float, lo_offset_hz: float, offset_hz: float,
                  search_hz: float, tx_lines: bool, spur_threshold_db: float) -> dict[str, Any]:
    """The tone and the lines at known places relative to it, from one capture.

    The tone is the strongest of its candidate places: ``sign`` is -1 when the RX spectrum is
    inverted, ``tx_sideband`` -1 when the generator put it below its LO (``tx_lines`` methods
    only). Line levels come from the same tone-normalised spectrum as the tone's peak, so a
    ``*_dbc`` is a ratio within one capture.
    """
    best = None
    for s_tx in ((1, -1) if tx_lines else (1,)):
        expect = lo_offset_hz + s_tx * offset_hz
        for s_rx in (1, -1):
            est = estimate_tone(iq, fs, search=(s_rx * expect, search_hz))
            if best is None or est.power_dbfs > best[0].power_dbfs:
                best = (est, s_rx, s_tx, expect)
    est, s_rx, s_tx, expect = best  # type: ignore[misc]
    n = len(iq)
    freqs, pdb = spectrum_db(iq, fs, n)
    tol = 4 * fs / n
    floor = float(np.median(pdb))
    t = est.freq_hz
    tone_pk = _peak_db(freqs, pdb, t, tol)
    out: dict[str, Any] = {"tone_hz": t, "sign": s_rx, "tx_sideband": s_tx,
                           "expect_hz": expect, "level_dbfs": est.power_dbfs,
                           "snr_db": est.snr_db, "clip": est.clip_fraction,
                           "floor_dbfs_bin": floor}
    lines = {"rx_image": -t, "dc": 0.0}
    if tx_lines:
        lines["tx_lo"] = t - s_rx * s_tx * offset_hz
        lines["tx_image"] = t - 2 * s_rx * s_tx * offset_hz
    for name, f in lines.items():
        lvl = _peak_db(freqs, pdb, f, tol)
        if name == "dc":
            out["dc_dbfs"] = lvl
            continue
        out[f"{name}_dbc"] = lvl - tone_pk
        out[f"{name}_at_floor"] = bool(lvl - floor < AT_FLOOR_DB)
    guard = (np.abs(freqs - t) <= max(50e3, 0.01 * fs)) | (np.abs(freqs) > 0.45 * fs)
    for name, f in lines.items():
        guard |= np.abs(freqs - f) <= (max(10e3, 2 * tol) if name == "dc" else 2 * tol)
    out["spur_dbc"], out["spur_hz"] = None, None
    idx = np.flatnonzero(~guard)
    if idx.size:
        k = int(idx[np.argmax(pdb[idx])])
        if pdb[k] - floor >= spur_threshold_db:
            out["spur_dbc"], out["spur_hz"] = float(pdb[k] - tone_pk), float(freqs[k])
    return out


def _search_hz(f_hz: float, max_ref_ppm: float) -> float:
    """Half-width of the tone search: the two references may differ by ``max_ref_ppm``."""
    return 5e3 + max_ref_ppm * 1e-6 * f_hz


# ---------------------------------------------------------------------------
# Analysis
# ---------------------------------------------------------------------------


def _in_spec(d: dict[str, Any], unit: str, f: float) -> bool:
    u = d["units"][unit]
    return bool(u["lo_min_hz"] <= f <= u["lo_max_hz"])


def _point_row(a: AnalysisContext, d: dict[str, Any], method: str, run: dict[str, Any],
               pt: dict[str, Any]) -> dict[str, Any]:
    p = a.params
    f, tx_lo = float(pt["f_hz"]), float(pt["tx_lo_hz"])
    lo_off, off = float(d["lo_offset_hz"]), float(run["offset_hz"])
    row: dict[str, Any] = {
        "method": method, "i": pt["i"], "f_mhz": round(f / 1e6, 4),
        "rx_in_spec": _in_spec(d, d["rx_unit"], f), "tx_in_spec": _in_spec(d, d["tx_unit"], tx_lo),
        "error": pt.get("error"), "cal_error": pt.get("cal_error"),
        "rx_locked": pt.get("rx_locked"), "tx_locked": pt.get("tx_locked"),
        "rssi_db": pt.get("rssi_db"), "found": False,
    }
    rb = ((pt.get("rx_lo_rb_hz"), f), (pt.get("tx_lo_rb_hz"), tx_lo))
    row["lo_readback_ok"] = None if any(x is None for x, _ in rb) else \
        all(abs(float(x) - want) <= LO_READBACK_TOL_HZ for x, want in rb)
    if pt.get("error"):
        return row
    iq, fs, _ = load_capture(a, pt["name"])
    m = measure_point(iq, fs, lo_off, off, _search_hz(f + lo_off + off, float(p["max_ref_ppm"])),
                      not run["tone_at_tx_lo"], float(p["spur_threshold_db"]))
    row.update(m)
    row["ppm"] = (m["tone_hz"] * m["sign"] - m["expect_hz"]) / (f + m["expect_hz"]) * 1e6
    row["found"] = bool(m["snr_db"] >= float(p["min_snr_db"]))
    return row


def _lo_residuals(rows: list[dict[str, Any]], p: dict[str, Any]) -> None:
    """Each point's reference offset against its neighbours' (a running median follows the
    references' slow drift); a jump marks an LO that did not land where it was set."""
    for method in dict.fromkeys(r["method"] for r in rows):
        found = [r for r in rows if r["method"] == method and r["found"]]
        ppms = [r["ppm"] for r in found]
        for k, r in enumerate(found):
            res = r["ppm"] - float(np.median(ppms[max(0, k - 3):k + 4]))
            r["lo_residual_ppm"] = res
            r["lo_error"] = bool(abs(res) > float(p["max_lo_error_ppm"]) and
                                 abs(res) * r["f_mhz"] > float(p["max_lo_error_hz"]))


def _worst(rows: list[dict[str, Any]], key: str) -> tuple[float | None, float | None]:
    """Highest value of ``key`` (for dBc, the worst) and where."""
    vals = [(r[key], r["f_mhz"]) for r in rows
            if r.get(key) is not None and np.isfinite(r[key])]
    if not vals:
        return None, None
    v, f = max(vals)
    return round(float(v), 2), f


def _mhz(rows: list[dict[str, Any]]) -> list[float]:
    return sorted({r["f_mhz"] for r in rows})


def analyze_sweep(a: AnalysisContext) -> Outcome:
    d = a.load_json("sweep.json")
    p = a.params
    rows = [_point_row(a, d, m, run, pt) for m, run in d["methods"].items()
            for pt in run["points"]]
    _lo_residuals(rows, p)
    a.save_json("sweep_points.json", rows)
    for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver", "rx_transceiver"):
        a.metric(k, d.get(k))
    plan = d["plan_hz"]
    a.metric("methods", list(d["methods"]))
    a.metric("points_planned", len(plan))
    a.metric("range_mhz", [round(plan[0] / 1e6, 1), round(plan[-1] / 1e6, 1)])
    a.metric("fs_hz", d["fs_hz"])
    ok = [r for r in rows if not r["error"]]
    a.metric("points_measured", len(ok), min=1)
    checks = {
        "point_errors": [r for r in rows if r["error"]],
        "rx_unlocked": [r for r in ok if r["rx_locked"] is False],
        "tx_unlocked": [r for r in ok if r["tx_locked"] is False],
        "lo_readback_mismatch": [r for r in ok if r["lo_readback_ok"] is False],
        "tone_missing": [r for r in ok if not r["found"]],
        "lo_errors": [r for r in ok if r.get("lo_error")],
    }
    for name, bad in checks.items():
        a.metric(name, len(bad), max=0)
        a.metric(f"{name}_mhz", _mhz(bad))
    a.metric("rx_lock_read", any(r["rx_locked"] is not None for r in ok))
    a.metric("tx_lock_read", any(r["tx_locked"] is not None for r in ok))
    a.metric("clipped", sum(1 for r in ok if r.get("clip", 0) >= 1e-4), max=0, severity="warn")
    a.metric("cal_errors_mhz", _mhz([r for r in ok if r["cal_error"]]))
    found = [r for r in ok if r["found"]]
    if found:
        a.metric("ref_offset_ppm_median", round(float(np.median([r["ppm"] for r in found])), 4))
        a.metric("tx_sideband_inverted_mhz", _mhz([r for r in found if r["tx_sideband"] < 0]))
    for m in d["methods"]:
        mr = [r for r in found if r["method"] == m]
        if not mr:
            continue
        low = min(mr, key=lambda r: r["snr_db"])
        a.metric(f"{m}_snr_db_min", round(low["snr_db"], 2))
        a.metric(f"{m}_snr_db_min_mhz", low["f_mhz"])
        a.metric(f"{m}_level_dbfs_range", [round(min(r["level_dbfs"] for r in mr), 2),
                                           round(max(r["level_dbfs"] for r in mr), 2)])
        for key, spec in (("rx_image_dbc", "rx_in_spec"), ("spur_dbc", "rx_in_spec"),
                          ("tx_lo_dbc", "tx_in_spec"), ("tx_image_dbc", "tx_in_spec")):
            for region, sel in (("in_spec", True), ("out_of_spec", False)):
                v, f = _worst([r for r in mr if r[spec] is sel], key)
                if v is not None:
                    a.metric(f"{m}_{key}_worst_{region}", v)
                    a.metric(f"{m}_{key}_worst_{region}_mhz", f)
    _plot_run(a, d, rows)
    summary = _summary(a, d, checks)
    if p["compare"]:
        summary += _compare(a, d, rows)
    return a.outcome(summary)


def _summary(a: AnalysisContext, d: dict[str, Any],
             checks: dict[str, list[dict[str, Any]]]) -> str:
    plan = d["plan_hz"]
    head = (f"{d['direction']} ({d['tx_transceiver']} TX, {d['rx_transceiver']} RX), "
            f"{len(plan)} points {plan[0] / 1e6:g}-{plan[-1] / 1e6:g} MHz x "
            f"{'/'.join(d['methods'])}")
    bad = {k: _mhz(v) for k, v in checks.items() if v}
    if bad:
        return head + ": " + "; ".join(
            f"{k.replace('_', ' ')} at {', '.join(f'{f:g}' for f in v[:6])}"
            f"{' ...' if len(v) > 6 else ''} MHz" for k, v in bad.items())
    tail = []
    for m in d["methods"]:
        snr = a.metrics.get(f"{m}_snr_db_min")
        if snr is not None:
            tail.append(f"{m} SNR >= {snr:.0f} dB (lowest at "
                        f"{a.metrics[f'{m}_snr_db_min_mhz']:g} MHz)")
        for region in ("in_spec", "out_of_spec"):
            v = a.metrics.get(f"{m}_rx_image_dbc_worst_{region}")
            if v is not None:
                tail.append(f"{m} RX image <= {v:.0f} dBc {region.replace('_', ' ')}")
    return head + ": every point tuned, locked and carried the tone; " + ", ".join(tail)


# ---------------------------------------------------------------------------
# Comparison across runs
# ---------------------------------------------------------------------------


def _resolve_run(ref: str, here: Path) -> Path | None:
    p = Path(ref)
    for cand in (p, REPO_ROOT / p, here.parent / p):
        if (cand / "result.json").exists():
            return cand
    return None


def _load_run(path: Path) -> dict[str, Any] | None:
    if load_result(path).get("test") != "rf.freq_sweep":
        return None
    try:
        d = json.loads((path / "artifacts" / "sweep.json").read_text(encoding="utf-8"))
        rows = json.loads((path / "artifacts" / "sweep_points.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return {"dir": path.as_posix(), "d": d, "rows": rows}


def _levels(rows: list[dict[str, Any]], method: str) -> dict[float, float]:
    return {r["f_mhz"]: r["level_dbfs"] for r in rows
            if r["method"] == method and r.get("found") and not r.get("error")}


def _comparable(runs: list[dict[str, Any]]) -> list[str]:
    """Differences between runs that make their absolute levels incomparable."""
    notes = []
    for key in ("fs_hz", "rx_gain_db", "tx_atten_db", "lo_offset_hz", "offset_hz", "nsamples"):
        vals = {str(r["d"].get(key)) for r in runs}
        if len(vals) > 1:
            notes.append(f"{key} differs between the runs ({', '.join(sorted(vals))})")
    pads = {str((r["d"].get("link") or {}).get("pad_db")) for r in runs}
    if len(pads) > 1:
        notes.append(f"the runs went through different pads ({', '.join(sorted(pads))} dB): "
                     "level differences include the pads")
    return notes


def _pair_stats(diff: dict[float, float], units: list[dict[str, Any]]) -> dict[str, Any]:
    fs = sorted(diff)
    v = np.array([diff[f] for f in fs])
    out: dict[str, Any] = {"points": len(fs), "median_db": round(float(np.median(v)), 2),
                           "min_db": round(float(v.min()), 2), "max_db": round(float(v.max()), 2)}
    lo = max(u["lo_min_hz"] for u in units) / 1e6
    hi = min(u["lo_max_hz"] for u in units) / 1e6
    inside = [diff[f] for f in fs if lo <= f <= hi]
    outside = [diff[f] for f in fs if not lo <= f <= hi]
    if outside:
        out["spec_mhz"] = [lo, hi]
        out["median_in_spec_db"] = round(float(np.median(inside)), 2) if inside else None
        out["median_out_of_spec_db"] = round(float(np.median(outside)), 2)
    return out


def _pair_kind(d1: dict[str, Any], d2: dict[str, Any]
               ) -> tuple[str, str, list[dict[str, Any]]] | None:
    """What a level difference between two runs measures, its label and the units it spans."""
    if d1["tx_unit"] == d2["tx_unit"] and d1["rx_unit"] != d2["rx_unit"]:
        return ("rx", f"RX {d2['rx_unit']} - RX {d1['rx_unit']} (TX {d1['tx_unit']})",
                [d1["units"][d1["rx_unit"]], d2["units"][d2["rx_unit"]]])
    if d1["rx_unit"] == d2["rx_unit"] and d1["tx_unit"] != d2["tx_unit"]:
        return ("tx", f"TX {d2['tx_unit']} - TX {d1['tx_unit']} (RX {d1['rx_unit']})",
                [d1["units"][d1["tx_unit"]], d2["units"][d2["tx_unit"]]])
    if d1["tx_unit"] == d2["rx_unit"] and d1["rx_unit"] == d2["tx_unit"]:
        return ("reverse", f"{d2['direction']} - {d1['direction']}", list(d1["units"].values()))
    if d1["direction"] == d2["direction"]:
        return ("repeat", f"{d2['direction']} repeat - first", list(d1["units"].values()))
    return None


def _compare(a: AnalysisContext, d: dict[str, Any], rows: list[dict[str, Any]]) -> str:
    runs = [{"dir": a.run_dir.as_posix(), "d": d, "rows": rows}]
    for ref in a.params["compare"]:
        path = _resolve_run(str(ref), a.run_dir)
        run = _load_run(path) if path else None
        if run is None:
            a.warn(f"compare: {ref} is not an analysed rf.freq_sweep run dir: skipped")
            continue
        if Path(run["dir"]).resolve() != a.run_dir.resolve():
            runs.append(run)
    for note in _comparable(runs):
        a.warn(f"compare: {note}")
    runs.sort(key=lambda r: (r["d"]["tx_unit"], r["d"]["rx_unit"]))
    pairs = []
    for r1, r2 in combinations(runs, 2):
        kind = _pair_kind(r1["d"], r2["d"])
        if kind is None:
            continue
        for m in [m for m in r1["d"]["methods"] if m in r2["d"]["methods"]]:
            l1, l2 = _levels(r1["rows"], m), _levels(r2["rows"], m)
            diff = {f: l2[f] - l1[f] for f in l1 if f in l2}
            if diff:
                pairs.append({"kind": kind[0], "label": f"{kind[1]}, {m}", "method": m,
                              "runs": [r1["dir"], r2["dir"]], **_pair_stats(diff, kind[2]),
                              "diff_db": {f"{f:g}": round(v, 3) for f, v in sorted(diff.items())}})
    a.save_json("compare.json", {
        "runs": [{"dir": r["dir"], "direction": r["d"]["direction"],
                  "tx_transceiver": r["d"]["tx_transceiver"],
                  "rx_transceiver": r["d"]["rx_transceiver"], "link": r["d"].get("link")}
                 for r in runs],
        "pairs": pairs})
    a.metric("compare_runs", [r["d"]["direction"] for r in runs])
    a.metric("compare_pairs", [{k: v for k, v in pr.items() if k not in ("diff_db", "runs")}
                               for pr in pairs])
    _plot_compare(a, runs, pairs)
    key = [pr for pr in pairs if pr["kind"] in ("rx", "tx")]
    if not key:
        return f"; compared with {len(runs) - 1} run(s): no pair shares a TX or an RX unit"
    return "; " + ", ".join(f"{pr['label']}: median {pr['median_db']:+.1f} dB" for pr in key)


# ---------------------------------------------------------------------------
# Plots
# ---------------------------------------------------------------------------


def _pyplot() -> Any:
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:  # pragma: no cover
        return None
    return plt


def _legend(ax: Any, size: int = 8) -> None:
    if ax.get_legend_handles_labels()[0]:
        ax.legend(fontsize=size)


def _shade_out_of_spec(ax: Any, unit: dict[str, Any], lo_mhz: float, hi_mhz: float) -> None:
    lo, hi = unit["lo_min_hz"] / 1e6, unit["lo_max_hz"] / 1e6
    if lo > lo_mhz:
        ax.axvspan(lo_mhz, lo, color="0.88", zorder=0)
    if hi < hi_mhz:
        ax.axvspan(hi, hi_mhz, color="0.88", zorder=0)


def _series(rows: list[dict[str, Any]], method: str, key: str) -> tuple[list[float], list[float]]:
    sel = [r for r in rows if r["method"] == method and r.get("found") and
           r.get(key) is not None and np.isfinite(r[key])]
    return [r["f_mhz"] for r in sel], [r[key] for r in sel]


def _plot_run(a: AnalysisContext, d: dict[str, Any], rows: list[dict[str, Any]]) -> None:
    plt = _pyplot()
    if plt is None:
        return
    lo_mhz, hi_mhz = d["plan_hz"][0] / 1e6, d["plan_hz"][-1] / 1e6
    rx_u, tx_u = d["units"][d["rx_unit"]], d["units"][d["tx_unit"]]
    bad = [r for r in rows if r["error"] or r["rx_locked"] is False or r["tx_locked"] is False]
    title = f"{d['direction']} ({d['tx_transceiver']} TX -> {d['rx_transceiver']} RX)"

    fig, axes = plt.subplots(2, 1, figsize=(10, 7), sharex=True)
    for ax, key, ylabel in ((axes[0], "level_dbfs", "tone (dBFS)"),
                            (axes[1], "snr_db", "SNR (dB)")):
        _shade_out_of_spec(ax, rx_u, lo_mhz, hi_mhz)
        for m in d["methods"]:
            ax.plot(*_series(rows, m, key), ".-", label=m)
        if bad:
            ax.plot([r["f_mhz"] for r in bad], [ax.get_ylim()[0]] * len(bad), "rx",
                    label="error or unlocked")
        ax.set_ylabel(ylabel)
        ax.grid(True, which="both", alpha=0.3)
    axes[0].set_title(f"{title}: RX gain {d['rx_gain_db']:g} dB, TX atten "
                      f"{d['tx_atten_db']:g} dB (grey: outside the RX unit's spec)")
    _legend(axes[0])
    axes[1].set_xscale("log")
    axes[1].set_xlabel("RX LO (MHz)")
    fig.tight_layout()
    fig.savefig(a.artifact_path("freq_sweep_level.png"), dpi=100)
    plt.close(fig)

    fig, axes = plt.subplots(2, 2, figsize=(12, 7), sharex=True)
    panels = ((axes[0][0], "rx_image_dbc", "RX image (dBc)", rx_u),
              (axes[0][1], "spur_dbc", "strongest spur (dBc)", rx_u),
              (axes[1][0], "tx_lo_dbc", "TX LO leakage (dBc)", tx_u),
              (axes[1][1], "tx_image_dbc", "TX image (dBc)", tx_u))
    for ax, key, ylabel, unit in panels:
        _shade_out_of_spec(ax, unit, lo_mhz, hi_mhz)
        for m in d["methods"]:
            f, v = _series(rows, m, key)
            if f:
                ax.plot(f, v, ".-", label=m)
        ax.set_ylabel(ylabel)
        ax.set_xscale("log")
        ax.grid(True, which="both", alpha=0.3)
    _legend(axes[0][0])
    for ax in axes[1]:
        ax.set_xlabel("RX LO (MHz)")
    fig.suptitle(f"{title}: lines relative to the tone (grey: outside that side's spec; lines "
                 f"within {AT_FLOOR_DB:g} dB of the noise are upper bounds)", fontsize=10)
    fig.tight_layout()
    fig.savefig(a.artifact_path("freq_sweep_lines.png"), dpi=100)
    plt.close(fig)


def _plot_compare(a: AnalysisContext, runs: list[dict[str, Any]],
                  pairs: list[dict[str, Any]]) -> None:
    plt = _pyplot()
    if plt is None:
        return
    fig, ax = plt.subplots(figsize=(10, 5))
    for r in runs:
        for m in r["d"]["methods"]:
            ax.plot(*_series(r["rows"], m, "level_dbfs"), ".-",
                    label=f"{r['d']['direction']} {m}")
    ax.set_xscale("log")
    ax.set_xlabel("RX LO (MHz)")
    ax.set_ylabel("tone (dBFS)")
    ax.set_title("RX level, every run")
    ax.grid(True, which="both", alpha=0.3)
    _legend(ax)
    fig.tight_layout()
    fig.savefig(a.artifact_path("compare_level.png"), dpi=100)
    plt.close(fig)

    if pairs:
        fig, ax = plt.subplots(figsize=(10, 5))
        for pr in pairs:
            ax.plot([float(k) for k in pr["diff_db"]], list(pr["diff_db"].values()), ".-",
                    label=pr["label"])
            for edge in pr.get("spec_mhz", ()):
                ax.axvline(edge, color="0.6", ls="--", lw=0.8)
        ax.axhline(0, color="k", lw=0.8)
        ax.set_xscale("log")
        ax.set_xlabel("RX LO (MHz)")
        ax.set_ylabel("level difference (dB)")
        ax.set_title("Same TX: RX difference; same RX: TX difference (dashed: spec edges)")
        ax.grid(True, which="both", alpha=0.3)
        _legend(ax)
        fig.tight_layout()
        fig.savefig(a.artifact_path("compare_diff.png"), dpi=100)
        plt.close(fig)

    fig, axes = plt.subplots(1, 2, figsize=(12, 4.5), sharex=True)
    for r in runs:
        d = r["d"]
        for m in d["methods"]:
            axes[0].plot(*_series(r["rows"], m, "rx_image_dbc"), ".-",
                         label=f"RX {d['rx_unit']} ({d['rx_transceiver']}) via {d['tx_unit']} {m}")
            f, v = _series(r["rows"], m, "tx_lo_dbc")
            if f:
                axes[1].plot(f, v, ".-", label=f"TX {d['tx_unit']} ({d['tx_transceiver']}) {m}")
    for ax, ylabel in ((axes[0], "RX image (dBc)"), (axes[1], "TX LO leakage (dBc)")):
        ax.set_xscale("log")
        ax.set_xlabel("RX LO (MHz)")
        ax.set_ylabel(ylabel)
        ax.grid(True, which="both", alpha=0.3)
        _legend(ax, 7)
    fig.tight_layout()
    fig.savefig(a.artifact_path("compare_lines.png"), dpi=100)
    plt.close(fig)


# ---------------------------------------------------------------------------
# Acquisition
# ---------------------------------------------------------------------------


def _methods(ctx: TestContext, tx: str, wanted: list[str]) -> list[str]:
    supported = []
    if ctx.caps(tx)["image"] in ("hwval", "factory"):
        supported.append("dds")
    if ctx.has_agent(tx):
        supported.append("pattern")
    supported.append("cyclic")
    if wanted == ["all"]:
        return supported
    bad = [m for m in wanted if m not in METHODS]
    if bad:
        raise UsageError(f"stimuli must be all or a list of {'|'.join(METHODS)}, not {bad}")
    missing = [m for m in wanted if m not in supported]
    if missing:
        raise PreconditionError(f"unit {tx} cannot generate {missing} (it supports "
                                f"{supported}: dds needs the hwval or factory image, pattern "
                                "the agent)", unit=tx)
    return [m for m in METHODS if m in wanted]


def _plan(ctx: TestContext, lo_off: float, off: float) -> list[float]:
    p = ctx.params
    if p["freqs_mhz"]:
        return sorted({float(f) * 1e6 for f in p["freqs_mhz"]})
    # The TX LO and the tone sit above the RX LO: the last point leaves room for them.
    stop = math.floor((float(p["stop_mhz"]) * 1e6 - lo_off - off) / 1e5) * 1e5
    return plan_freqs(float(p["start_mhz"]) * 1e6, stop, int(p["points_per_octave"]),
                      float(p["max_step_mhz"]) * 1e6, SPEC_EDGES_HZ)


def _optional(ctx: TestContext, unit: str, attr: str, chan: str | None,
              output: bool) -> str | None:
    try:
        return ctx.iio_get(unit, ctx.cfg.iio.phy_device, attr, chan, output).strip()
    except FbenchError:
        return None


def _settings(ctx: TestContext, tx: str, rx: str) -> dict[str, Any]:
    """The calibration and tracking state the run measured under."""
    return {
        "rx": {k: _optional(ctx, rx, k, "voltage0", False)
               for k in ("rf_bandwidth", "quadrature_tracking_en", "rf_dc_offset_tracking_en",
                         "bb_dc_offset_tracking_en", "rf_port_select")},
        "rx_calib_mode": _optional(ctx, rx, "calib_mode", None, False),
        "tx": {k: _optional(ctx, tx, k, "voltage0", True)
               for k in ("rf_bandwidth", "rf_port_select")},
        "tx_calib_mode": _optional(ctx, tx, "calib_mode", None, False),
    }


def _lock_bits(ctx: TestContext, rx: str, tx: str) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for role, unit, reg in (("rx", rx, RX_SYNTH_LOCK_REG), ("tx", tx, TX_SYNTH_LOCK_REG)):
        if ctx.has_agent(unit):
            v = ctx.agent.spi_read(unit, reg)
            out[f"{role}_lock_reg"] = v
            out[f"{role}_locked"] = bool(v & VCO_LOCK)
    return out


def _reachable(ctx: TestContext, unit: str) -> bool:
    try:
        ctx.iio_get(unit, ctx.cfg.iio.phy_device, "frequency", RX_LO_CHAN, True)
        return True
    except FbenchError:
        return False


def _sweep_method(ctx: TestContext, method: str, plan: list[float], lo_off: float,
                  fs: float, run: dict[str, Any]) -> None:
    """One stimulus method across the plan into ``run`` (kept as it fills, so an aborted
    sweep still records what it measured); a point that fails is recorded, not fatal."""
    p = ctx.params
    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    phy = ctx.cfg.iio.phy_device
    gain, n, settle = float(p["rx_gain_db"]), int(p["nsamples"]), float(p["settle_s"])
    tone_at_lo = run["tone_at_tx_lo"]
    points = run["points"]
    src = ToneSource(ctx, tx, method)
    src.configure(plan[0] + lo_off, float(p["tx_atten_db"]))
    try:
        actual = run["offset_hz"] = src.start(run["offset_hz"])
        for i, f in enumerate(plan):
            tx_lo = f + lo_off + (actual if tone_at_lo else 0.0)
            pt: dict[str, Any] = {"i": i, "f_hz": f, "tx_lo_hz": tx_lo,
                                  "name": f"sw_{method}_{i:03d}"}
            try:
                ctx.iio_set(rx, phy, "frequency", f"{int(round(f))}", RX_LO_CHAN, True)
                ctx.iio_set(tx, phy, "frequency", f"{int(round(tx_lo))}", TX_LO_CHAN, True)
                ctx.iio_set(rx, phy, "hardwaregain", f"{gain:g}", "voltage0", False)
                if p["tx_quad_cal"] and not tone_at_lo:
                    try:
                        ctx.iio_set(tx, phy, "calib_mode", "tx_quad")
                    except FbenchError as exc:
                        pt["cal_error"] = exc.message
                ctx.sleep(settle)
                if p["lock_check"]:
                    pt.update(_lock_bits(ctx, rx, tx))
                pt["rx_lo_rb_hz"] = parse_number(ctx.iio_get(rx, phy, "frequency", RX_LO_CHAN,
                                                             True))
                pt["tx_lo_rb_hz"] = parse_number(ctx.iio_get(tx, phy, "frequency", TX_LO_CHAN,
                                                             True))
                capture_iq(ctx, rx, n, pt["name"], "iio", f, fs)
                pt["rssi_db"] = rssi_db(ctx, rx)
            except SafetyRefusal:
                raise
            except FbenchError as exc:
                pt["error"] = exc.message
                ctx.log.warning("%s at %.1f MHz: %s", method, f / 1e6, exc.message)
                for unit in dict.fromkeys((rx, tx)):
                    if not _reachable(ctx, unit):
                        points.append(pt)
                        raise FbenchError(f"unit {unit} stopped answering during the {method} "
                                          f"sweep at {f / 1e6:.1f} MHz: {exc.message}") from exc
            points.append(pt)
    finally:
        src.stop()


def _restore(ctx: TestContext, rx: str, tx: str, st: Any, tx_lo0: float,
             rates0: dict[str, float]) -> None:
    phy = ctx.cfg.iio.phy_device
    steps = [(u, "sampling_frequency", f"{int(r)}", "voltage0", False) for u, r in rates0.items()]
    steps += [(rx, "frequency", f"{int(st.lo_hz)}", RX_LO_CHAN, True),
              (tx, "frequency", f"{int(tx_lo0)}", TX_LO_CHAN, True)]
    for unit, attr, value, chan, output in steps:
        try:
            ctx.iio_set(unit, phy, attr, value, chan, output)
        except FbenchError as exc:
            ctx.errors.append(f"could not restore {unit} {attr}: {exc.message}")
    try:
        set_rx_gain(ctx, rx, st.gain_mode, st.gain_db)
    except FbenchError as exc:
        ctx.errors.append(f"could not restore {rx} RX gain: {exc.message}")


@bench_test(
    "rf.freq_sweep", tier=0, units="tx,rx", maintenance=True, tx=True,
    params={"start_mhz": 70.0, "stop_mhz": 6000.0, "points_per_octave": 8,
            "max_step_mhz": 100.0, "freqs_mhz": [], "stimuli": ["all"], "tx_atten_db": 30.0,
            "rx_gain_db": 40.0, "lo_offset_hz": 1000000.0, "offset_hz": 500000.0,
            "nsamples": 65536, "settle_s": 0.2, "tx_quad_cal": True, "lock_check": True,
            "rate_hz": 0.0, "min_snr_db": 10.0, "max_ref_ppm": 5.0, "spur_threshold_db": 20.0,
            "max_lo_error_ppm": 0.05, "max_lo_error_hz": 100.0, "compare": [], "tx_port": ""},
    description="One cabled direction across the tuning range (default 70 MHz-6 GHz, 84 "
                "points): the RX LO steps, the TX LO follows lo_offset_hz above it, and each "
                "stimulus method (dds, pattern, cyclic; stimuli=all takes every one the TX "
                "unit supports) sweeps in turn at fixed RX gain. Per point: synthesizer lock "
                "bits on both units, LO read-back, tone level and SNR, the TX vs RX reference "
                "offset, RX image, TX LO leakage and TX image (after a TX quadrature "
                "calibration), the strongest spur, RSSI. Both units in maintenance mode; an "
                "estimated 3 s per point per method. compare=<run dirs> overlays other runs "
                "and plots RX "
                "(same TX) and TX (same RX) differences; --tx A --rx A is a self loop.",
    pass_criteria="every point tunes, both synthesizers lock, the LOs read back as set, the "
                  "tone is found (SNR >= min_snr_db) and its frequency stays on the reference "
                  "offset (residual within max_lo_error_ppm or max_lo_error_hz); the rest is "
                  "reported",
    artifacts=("sweep.json", "sweep_points.json", "freq_sweep_level.png",
               "freq_sweep_lines.png", "compare.json", "compare_level.png", "compare_diff.png",
               "compare_lines.png", "sw_<method>_<n>.sigmf-*"),
    analyze=analyze_sweep,
)
def rf_freq_sweep(ctx: TestContext) -> Outcome:
    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    p = ctx.params
    phy = ctx.cfg.iio.phy_device
    lo_off, off = float(p["lo_offset_hz"]), float(p["offset_hz"])
    if not 0 < off < lo_off - 50e3:
        raise UsageError("need 0 < offset_hz < lo_offset_hz - 50 kHz, so the TX image stays "
                         "clear of DC and of the TX LO leakage")
    methods = _methods(ctx, tx, [str(m) for m in p["stimuli"]])
    plan = _plan(ctx, lo_off, off)
    direction_metrics(ctx)
    if tx != rx:
        reverse_direction_note(ctx)
    st = rx_state(ctx, rx)
    tx_lo0 = parse_number(ctx.iio_get(tx, phy, "frequency", TX_LO_CHAN, True))
    fs, rate = st.fs_hz, float(p["rate_hz"])
    rates0: dict[str, float] = {}
    try:
        if rate and abs(rate - fs) > 1:
            for unit in dict.fromkeys((rx, tx)):
                rates0[unit] = parse_number(ctx.iio_get(unit, phy, "sampling_frequency",
                                                        "voltage0", False))
                ctx.iio_set(unit, phy, "sampling_frequency", f"{int(rate)}", "voltage0", False)
            fs = rate
        tx_fs = parse_number(ctx.iio_get(tx, phy, "sampling_frequency", "voltage0", True))
        if lo_off + off > 0.4 * min(fs, tx_fs):
            raise UsageError(f"lo_offset_hz + offset_hz must stay below 0.4 x the sample rate "
                             f"({min(fs, tx_fs) / 1e6:g} MSPS)")
        units = {}
        for u in dict.fromkeys((tx, rx)):
            lim = ctx.cfg.unit(u).limits
            units[u] = {"transceiver": ctx.cfg.unit(u).transceiver,
                        "lo_min_hz": lim["lo_min_hz"], "lo_max_hz": lim["lo_max_hz"]}
            outside = sum(1 for f in plan if not lim["lo_min_hz"] <= f <= lim["lo_max_hz"])
            if outside:
                ctx.warn(f"unit {u} ({units[u]['transceiver']}): {outside} of {len(plan)} "
                         f"points lie outside its specified {lim['lo_min_hz'] / 1e6:g}-"
                         f"{lim['lo_max_hz'] / 1e6:g} MHz")
        if p["lock_check"]:
            for u in dict.fromkeys((rx, tx)):
                if not ctx.has_agent(u):
                    ctx.warn(f"unit {u} has no agent: its synthesizer lock is not read")
        raw: dict[str, Any] = {
            **{k: ctx.metrics[k] for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver",
                                           "rx_transceiver")},
            "units": units, "fs_hz": fs, "tx_fs_hz": tx_fs, "lo_offset_hz": lo_off,
            "offset_hz": off, "rx_gain_db": float(p["rx_gain_db"]),
            "tx_atten_db": float(p["tx_atten_db"]), "nsamples": int(p["nsamples"]),
            "plan_hz": plan, "link": ctx.budget.to_dict() if ctx.budget else None,
            "settings": _settings(ctx, tx, rx), "methods": {},
        }
        set_rx_gain(ctx, rx, "manual", float(p["rx_gain_db"]))
        try:
            for m in methods:
                raw["methods"][m] = {"offset_hz": off, "tone_at_tx_lo": m == "pattern",
                                     "points": []}
                _sweep_method(ctx, m, plan, lo_off, fs, raw["methods"][m])
        finally:
            ctx.save_json("sweep.json", raw)
    finally:
        _restore(ctx, rx, tx, st, tx_lo0, rates0)
    return analyze_sweep(ctx)
