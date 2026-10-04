"""rf.* tests (cabled, attenuated; the runner enforces the interlock).

Every tx,rx test records the direction and both transceivers (AD9361 vs
AD9363) and recommends running the reverse direction, so transceiver
differences separate from board differences.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

import numpy as np

from ..analysis import sigmf
from ..analysis.tone import (
    ToneEstimate,
    estimate_tone,
    find_spurs,
    harmonic_aliases,
    linear_slope,
    match_spurs,
    noise_and_spurs,
    phase_continuity,
    ppm,
    spectrum_db,
)
from ..errors import FbenchError, Inconclusive, PreconditionError, UsageError
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..stimulus import (
    RX_LO_CHAN,
    ToneSource,
    capture_iq,
    check_transceiver,
    direction_metrics,
    load_capture,
    reverse_direction_note,
    rssi_db,
    rx_state,
    set_rx_gain,
)

ETH_CLOCKS_HZ = (25e6, 125e6)
CRYSTAL_FILE = "/mnt/jffs2/scanner/state/radio.json"  # the scanner's RadioState


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


def find_cw(iq: np.ndarray, fs: float, offset_hz: float) -> tuple[ToneEstimate, int]:
    """Strongest tone near ``+offset`` or ``-offset`` (spectral inversion tolerant)."""
    half = max(5e3, 0.02 * fs)
    pos = estimate_tone(iq, fs, search=(offset_hz, half))
    if offset_hz == 0:
        return pos, 1
    neg = estimate_tone(iq, fs, search=(-offset_hz, half))
    return (pos, 1) if pos.power_dbfs >= neg.power_dbfs else (neg, -1)


class RfPrep:
    """RX frequency plan for one run (restores what it changed)."""

    def __init__(self, ctx: TestContext, freq_param: str = "freq_hz") -> None:
        self.ctx = ctx
        self.tx, self.rx = ctx.roles["tx"], ctx.roles["rx"]
        st = rx_state(ctx, self.rx)
        self.fs = st.fs_hz
        self.rx_lo = st.lo_hz
        self.gain_mode, self.gain_db = st.gain_mode, st.gain_db
        self._restore_lo: float | None = None
        f_test = float(ctx.params.get(freq_param) or ctx.cfg.rf.test_freq_hz)
        if abs(st.lo_hz - f_test) > 1.0:
            if ctx.params.get("rx_retune"):
                ctx.iio_set(self.rx, ctx.cfg.iio.phy_device, "frequency", f"{int(f_test)}",
                            RX_LO_CHAN, True)
                self._restore_lo = st.lo_hz
                self.rx_lo = f_test
            else:
                ctx.warn(f"RX {self.rx} LO is {st.lo_hz / 1e6:.6f} MHz, not the test frequency "
                         f"{f_test / 1e6:.6f} MHz: testing at the RX LO (-p rx_retune=true "
                         "to retune)")
                f_test = st.lo_hz
        self.f_test = f_test
        check_transceiver(ctx, self.tx, f_test)
        check_transceiver(ctx, self.rx, f_test)
        direction_metrics(ctx)
        reverse_direction_note(ctx)

    def restore(self) -> None:
        if self._restore_lo is not None:
            try:
                self.ctx.iio_set(self.rx, self.ctx.cfg.iio.phy_device, "frequency",
                                 f"{int(self._restore_lo)}", RX_LO_CHAN, True)
            except FbenchError as exc:
                self.ctx.errors.append(f"could not restore RX LO: {exc.message}")


def _atten(ctx: Any) -> float:
    v = ctx.params["tx_atten_db"]
    return float(min(v)) if isinstance(v, list) else float(v)


# ---------------------------------------------------------------------------
# rf.cw_ppm
# ---------------------------------------------------------------------------


def _plot_xy(a: AnalysisContext, name: str, x: list[float], y: list[float], xlabel: str,
             ylabel: str, title: str) -> None:
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:  # pragma: no cover
        return
    fig, ax = plt.subplots(figsize=(7, 3.5))
    ax.plot(x, y, "o-")
    ax.set_xlabel(xlabel)
    ax.set_ylabel(ylabel)
    ax.set_title(title)
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(a.artifact_path(name), dpi=100)
    plt.close(fig)


def stored_ppm_cal(ctx: TestContext, unit: str) -> dict[str, Any]:
    """The scanner's stored crystal correction on ``unit``, read from flash (the
    scanner may be stopped).

    ``lo_ppm`` is the reference error it corrects (negative: reference low;
    the LO shift is ``-lo_ppm * lo``). No calibration means no correction.
    """
    try:
        rc, out, _ = ctx.services.ssh(unit).run(f"cat {CRYSTAL_FILE} 2>/dev/null", 15.0)
    except FbenchError as exc:
        return {"status": "unreadable", "error": exc.message}
    try:
        crystal = json.loads(out).get("crystal") if rc == 0 and out.strip() else None
        if not crystal:
            return {"status": "absent", "lo_ppm": 0.0}
        return {"status": "stored", "lo_ppm": float(crystal["ppm"]),
                "lo_shift_hz": crystal.get("lo_shift_hz"), "method": crystal.get("method"),
                "unix_ms": crystal.get("at_unix_ms")}
    except (ValueError, KeyError, TypeError) as exc:
        return {"status": "invalid", "error": str(exc)}


def _cal_check(a: AnalysisContext, d: dict[str, Any], measured: float, carrier: float) -> str:
    """Compare the measured TX-RX offset with the two stored corrections.

    Only the difference is checked: an error common to both corrections cancels.
    """
    cal = d.get("cal") or {}
    tx, rx = cal.get(d.get("tx_unit", "")), cal.get(d.get("rx_unit", ""))
    if not tx or not rx:
        return ""
    for u, c in ((d["tx_unit"], tx), (d["rx_unit"], rx)):
        a.metric(f"cal_{u}_ppm", round(c["lo_ppm"], 5) if "lo_ppm" in c else None)
        a.metric(f"cal_{u}_status", c["status"])
    if "lo_ppm" not in tx or "lo_ppm" not in rx:
        a.warn("stored ppm calibration unreadable on one unit: calibration check skipped")
        return ""
    predicted = tx["lo_ppm"] - rx["lo_ppm"]
    resid = measured - predicted
    a.metric("cal_predicted_ppm", round(predicted, 5))
    a.metric("cal_residual_ppm", round(resid, 5))
    if "stored" not in (tx["status"], rx["status"]):
        return ""  # nothing calibrated (e.g. factory images): report the offset only
    a.metric("cal_residual_ppm_abs", round(abs(resid), 5),
             max=float(a.params["max_cal_residual_ppm"]))
    return (f"; stored crystal corrections predict {predicted:+.4f} ppm, residual {resid:+.4f} ppm "
            f"({resid * carrier / 1e6:+.0f} Hz)")


def analyze_cw(a: AnalysisContext) -> Outcome:
    d = a.load_json("cw.json")
    carrier = float(d["f_test_hz"]) + float(d["offset_hz"])
    rows = []
    for cap in d["captures"]:
        iq, fs, _ = load_capture(a, cap["name"])
        est, sign = find_cw(iq, fs, float(d["offset_hz"]))
        err = sign * est.freq_hz - float(d["offset_hz"])
        rows.append({"t_s": cap["t_s"], "freq_hz": est.freq_hz, "err_hz": err,
                     "ppm": ppm(err, carrier), "snr_db": est.snr_db,
                     "power_dbfs": est.power_dbfs, "inverted": sign < 0})
    a.save_json("cw_estimates.json", rows)
    if not rows:
        raise Inconclusive("no captures")
    p = [r["ppm"] for r in rows]
    t = [r["t_s"] for r in rows]
    for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver", "rx_transceiver"):
        if k in d:
            a.metric(k, d[k])
    a.metric("ppm_mean", round(float(np.mean(p)), 5))
    a.metric("ppm_first", round(p[0], 5))
    a.metric("ppm_last", round(p[-1], 5))
    a.metric("snr_db_min", round(min(r["snr_db"] for r in rows), 2), min=10.0)
    a.metric("power_dbfs_mean", round(float(np.mean([r["power_dbfs"] for r in rows])), 2))
    a.metric("spectrum_inverted", any(r["inverted"] for r in rows))
    span = max(t) - min(t)
    a.metric("span_s", round(span, 1))
    drift = linear_slope(t, p) * 600.0 if len(rows) > 1 else None
    _plot_xy(a, "cw_ppm.png", t, p, "time (s)", "ppm (TX vs RX)",
             f"{d.get('direction', '')} CW offset")
    summary = (f"{d.get('direction', '')}: TX-RX reference offset {np.mean(p):+.4f} ppm "
               f"({np.mean(p) * carrier / 1e6:+.1f} Hz at {carrier / 1e6:.4f} MHz)")
    summary += _cal_check(a, d, float(np.mean(p)), carrier)
    if drift is None or span < float(a.params["min_span_s"]):
        a.metric("drift_ppm_per_10min", round(drift, 5) if drift is not None else None)
        return a.outcome(summary + f"; drift not assessed (span {span:.0f} s < "
                         f"{a.params['min_span_s']} s)", "inconclusive")
    a.metric("drift_ppm_per_10min", round(drift, 5))
    a.metric("drift_ppm_per_10min_abs", round(abs(drift), 5),
             max=float(a.params["max_drift_ppm_10min"]))
    return a.outcome(summary + f", drift {drift:+.4f} ppm/10 min over {span:.0f} s")


@bench_test(
    "rf.cw_ppm", tier=0, units="tx,rx", tx=True,
    params={"tx_atten_db": 40.0, "offset_hz": 200000.0, "span_s": 600.0, "interval_s": 60.0,
            "nsamples": 262144, "min_span_s": 300.0, "max_drift_ppm_10min": 0.5,
            "settle_s": 1.0, "stimulus": "auto", "capture": "auto", "freq_hz": 0.0,
            "rx_retune": False, "tx_port": "", "max_cal_residual_ppm": 0.2},
    description="CW frequency offset between the boards (both 40 MHz references) and drift "
                "over time. Tone at test_freq + offset_hz; ppm relative to the RF carrier. "
                "Cross-checks the difference of the scanner's stored crystal corrections.",
    pass_criteria="report ppm; drift < 0.5 ppm/10 min (inconclusive if span < min_span_s); "
                  "|measured - (cal_tx - cal_rx)| <= 0.2 ppm",
    artifacts=("cw.json", "cw_estimates.json", "cw_ppm.png", "cw_<n>.sigmf-*"),
    suites=("rf",), analyze=analyze_cw, duration_param="span_s",
)
def rf_cw_ppm(ctx: TestContext) -> Outcome:
    prep = RfPrep(ctx)
    src = ToneSource(ctx, prep.tx, str(ctx.params["stimulus"]))
    src.configure(prep.f_test, _atten(ctx))
    caps = []
    try:
        off = src.start(float(ctx.params["offset_hz"]))
        ctx.sleep(float(ctx.params["settle_s"]))
        interval = float(ctx.params["interval_s"])
        n_caps = max(1, int(float(ctx.params["span_s"]) // interval) + 1)
        t0 = ctx.services.monotonic()
        for i in range(n_caps):
            if i:
                ctx.sleep(max(0.0, t0 + i * interval - ctx.services.monotonic()))
            t = ctx.services.monotonic() - t0
            name = f"cw_{i:03d}"
            capture_iq(ctx, prep.rx, int(ctx.params["nsamples"]), name,
                       str(ctx.params["capture"]), prep.rx_lo, prep.fs)
            caps.append({"t_s": t, "name": name})
    finally:
        src.stop()
        prep.restore()
    cal = {u: stored_ppm_cal(ctx, u) for u in (prep.tx, prep.rx)}
    ctx.save_json("cw.json", {"f_test_hz": prep.f_test, "rx_lo_hz": prep.rx_lo, "fs_hz": prep.fs,
                              "offset_hz": off, "method": src.method, "captures": caps,
                              "cal": cal,
                              **{k: ctx.metrics[k] for k in ("direction", "tx_unit", "rx_unit",
                                                             "tx_transceiver", "rx_transceiver")}})
    return analyze_cw(ctx)


# ---------------------------------------------------------------------------
# rf.level_sweep
# ---------------------------------------------------------------------------


def analyze_level(a: AnalysisContext) -> Outcome:
    d = a.load_json("sweep.json")
    pts = []
    for p in d["points"]:
        iq, fs, _ = load_capture(a, p["name"])
        est, _ = find_cw(iq, fs, float(d["offset_hz"]))
        pts.append({"atten_db": p["atten_db"], "power_dbfs": est.power_dbfs,
                    "snr_db": est.snr_db, "clip_fraction": est.clip_fraction,
                    "rssi_db": p.get("rssi_db")})
    a.save_json("sweep_points.json", pts)
    for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver", "rx_transceiver"):
        if k in d:
            a.metric(k, d[k])
    use = [p for p in pts if p["clip_fraction"] < 1e-4 and p["snr_db"] > 15.0]
    a.metric("points_total", len(pts))
    a.metric("points_clipped", sum(1 for p in pts if p["clip_fraction"] >= 1e-4))
    if len(use) < 3:
        a.metric("points_used", len(use), min=3)
        raise Inconclusive(f"only {len(use)} unclipped points with SNR > 15 dB")
    a.metric("points_used", len(use), min=3)
    x = [-p["atten_db"] for p in use]
    y = [p["power_dbfs"] for p in use]
    slope = linear_slope(x, y)
    tol = float(a.params["slope_tol"])
    a.metric("slope_db_per_db", round(slope, 4), min=1.0 - tol, max=1.0 + tol)
    ordered = sorted(use, key=lambda p: -p["atten_db"])  # quiet -> loud
    steps = [ordered[i + 1]["power_dbfs"] - ordered[i]["power_dbfs"]
             for i in range(len(ordered) - 1)]
    worst = min(steps) if steps else 0.0
    a.metric("min_step_db", round(worst, 3))
    a.metric("monotonic", bool(worst >= -0.3), eq=True)
    rs = [(p["atten_db"], p["rssi_db"]) for p in use if p["rssi_db"] is not None]
    if len(rs) >= 2:
        a.metric("rssi_slope_db_per_db", round(linear_slope([-r[0] for r in rs],
                                                            [r[1] for r in rs]), 4))
    a.metric("max_power_dbfs", round(max(p["power_dbfs"] for p in pts), 2))
    _plot_xy(a, "level_sweep.png", [-p["atten_db"] for p in pts],
             [p["power_dbfs"] for p in pts], "-TX attenuation (dB)", "tone (dBFS)",
             f"{d.get('direction', '')} level sweep")
    return a.outcome(f"{d.get('direction', '')}: slope {slope:.3f} dB/dB over {len(use)} points, "
                     f"{'monotonic' if worst >= -0.3 else 'NOT monotonic'}")


@bench_test(
    "rf.level_sweep", tier=0, units="tx,rx", tx=True,
    params={"tx_atten_db": [70.0, 65.0, 60.0, 55.0, 50.0, 45.0, 40.0], "offset_hz": 200000.0,
            "nsamples": 65536, "rx_gain_mode": "manual", "rx_gain_db": 20.0, "slope_tol": 0.5,
            "settle_s": 0.5, "stimulus": "auto", "capture": "auto", "freq_hz": 0.0,
            "rx_retune": False, "tx_port": ""},
    description="TX attenuation sweep (quiet to loud): tone power, RSSI, clipping, linearity. "
                "The interlock uses the smallest attenuation of the sweep.",
    pass_criteria="monotonic, slope 1 dB/dB ±0.5",
    artifacts=("sweep.json", "sweep_points.json", "level_sweep.png"), suites=("rf",),
    analyze=analyze_level,
)
def rf_level_sweep(ctx: TestContext) -> Outcome:
    attens = sorted((float(x) for x in ctx.params["tx_atten_db"]), reverse=True)
    prep = RfPrep(ctx)
    src = ToneSource(ctx, prep.tx, str(ctx.params["stimulus"]))
    src.configure(prep.f_test, attens[0])
    points = []
    gain_changed = False
    try:
        if ctx.params["rx_gain_mode"]:
            set_rx_gain(ctx, prep.rx, str(ctx.params["rx_gain_mode"]),
                        float(ctx.params["rx_gain_db"]))
            gain_changed = True
        off = src.start(float(ctx.params["offset_hz"]))
        for i, atten in enumerate(attens):
            src.set_atten(atten)
            ctx.sleep(float(ctx.params["settle_s"]))
            name = f"lvl_{i:02d}"
            capture_iq(ctx, prep.rx, int(ctx.params["nsamples"]), name,
                       str(ctx.params["capture"]), prep.rx_lo, prep.fs)
            points.append({"atten_db": atten, "name": name, "rssi_db": rssi_db(ctx, prep.rx)})
    finally:
        src.stop()
        if gain_changed:
            try:
                set_rx_gain(ctx, prep.rx, prep.gain_mode, prep.gain_db)
            except FbenchError as exc:
                ctx.errors.append(f"could not restore RX gain: {exc.message}")
        prep.restore()
    ctx.save_json("sweep.json", {"offset_hz": off, "f_test_hz": prep.f_test, "points": points,
                                 "method": src.method,
                                 **{k: ctx.metrics[k] for k in ("direction", "tx_unit", "rx_unit",
                                                                "tx_transceiver",
                                                                "rx_transceiver")}})
    return analyze_level(ctx)


# ---------------------------------------------------------------------------
# rf.spur_scan
# ---------------------------------------------------------------------------


def analyze_spur(a: AnalysisContext) -> Outcome:
    d = a.load_json("spur.json")
    iq, fs, lo = load_capture(a, "spur")
    res = noise_and_spurs(iq, fs, float(a.params["threshold_db"]), int(a.params["nfft"]))
    tol = 3 * fs / int(a.params["nfft"])
    cands = harmonic_aliases(ETH_CLOCKS_HZ, float(lo or d.get("rx_lo_hz", 0)), fs, fs,
                             dc_guard_hz=tol)
    eth = match_spurs(res["spurs"], cands, tol)
    a.save_json("spurs.json", {**res, "eth_matches": eth})
    a.metric("unit", d.get("unit"))
    a.metric("transceiver", d.get("transceiver"))
    a.metric("rx_lo_hz", lo)
    a.metric("fs_hz", fs)
    a.metric("floor_dbfs_per_bin", round(res["floor_dbfs_per_bin"], 2))
    a.metric("spurs_count", len(res["spurs"]))
    a.metric("spurs_top", [{"freq_hz": round(s["freq_hz"], 1), "dbfs": round(s["dbfs"], 1)}
                           for s in res["spurs"][:10]])
    a.metric("eth_related_spurs", len(eth))
    top = res["spurs"][0] if res["spurs"] else None
    top_txt = f"strongest {top['freq_hz'] / 1e3:+.1f} kHz at {top['dbfs']:.1f} dBFS" if top \
        else "none"
    return a.outcome(f"{d.get('unit')} ({d.get('transceiver')}): floor "
                     f"{res['floor_dbfs_per_bin']:.1f} dBFS/bin, {len(res['spurs'])} spurs > "
                     f"floor+{a.params['threshold_db']} dB ({top_txt}), {len(eth)} at Ethernet "
                     "clock products", "pass")


@bench_test(
    "rf.spur_scan", tier=0, units="any",
    params={"nsamples": 1048576, "threshold_db": 10.0, "nfft": 8192, "tx_off_first": True},
    description="Noise floor and spurs with TX off/terminated at the current RX LO; flags lines "
                "at 25/125 MHz Ethernet clock products. Run on each unit.",
    pass_criteria="report",
    artifacts=("spur.json", "spurs.json", "spur.sigmf-*"), suites=("rf",),
    analyze=analyze_spur,
)
def rf_spur_scan(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    unit = ctx.cfg.unit(name)
    if ctx.params["tx_off_first"]:
        try:
            ctx.tx_off(name)
        except FbenchError as exc:
            ctx.warn(f"tx off before the scan failed: {exc.message}")
    st = rx_state(ctx, name)
    capture_iq(ctx, name, int(ctx.params["nsamples"]), "spur", "iio", st.lo_hz, st.fs_hz)
    ctx.save_json("spur.json", {"unit": name, "transceiver": unit.transceiver,
                                "rx_lo_hz": st.lo_hz, "fs_hz": st.fs_hz})
    return analyze_spur(ctx)


# ---------------------------------------------------------------------------
# rf.isolation
# ---------------------------------------------------------------------------


def _reference_gain(ref: str) -> float:
    try:
        return float(ref)
    except ValueError:
        pass
    path = Path(ref)
    res = path / "result.json" if path.is_dir() else path
    if not res.exists():
        raise UsageError(f"reference {ref!r} is neither a number nor a run dir with result.json")
    gain = json.loads(res.read_text(encoding="utf-8")).get("metrics", {}).get("path_gain_db")
    if gain is None:
        raise UsageError(f"{res} has no path_gain_db metric (run phase=cabled first)")
    return float(gain)


def analyze_isolation(a: AnalysisContext) -> Outcome:
    d = a.load_json("iso.json")
    iq, fs, _ = load_capture(a, "iso")
    est, _ = find_cw(iq, fs, float(d["offset_hz"]))
    gain = est.power_dbfs + float(d["atten_db"])
    for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver", "rx_transceiver"):
        if k in d:
            a.metric(k, d[k])
    a.metric("phase", d["phase"])
    a.metric("tone_power_dbfs", round(est.power_dbfs, 2))
    a.metric("tone_snr_db", round(est.snr_db, 2))
    a.metric("path_gain_db", round(gain, 2))
    if d["phase"] == "cabled":
        return a.outcome(
            f"reference path gain {gain:.1f} dB recorded (tone {est.power_dbfs:.1f} dBFS at "
            f"{d['atten_db']} dB atten). Now remove the cable, terminate both ports and run "
            f"`fbench run rf.isolation --tx {d.get('tx_unit')} --rx {d.get('rx_unit')} "
            f"-p phase=open -p reference=<this run dir>`", "inconclusive")
    ref = _reference_gain(str(a.params["reference"]))
    iso = ref - gain
    a.metric("reference_path_gain_db", ref)
    if est.snr_db < 6.0:
        a.metric("isolation_db_lower_bound", round(iso, 2))
        if iso >= float(a.params["min_isolation_db"]):
            a.metric("isolation_db", round(iso, 2), min=float(a.params["min_isolation_db"]))
            return a.outcome(f"leakage below the noise: isolation >= {iso:.1f} dB")
        return a.outcome(f"leakage below the noise but the bound ({iso:.1f} dB) is under "
                         f"{a.params['min_isolation_db']} dB: lower tx_atten for the open "
                         "phase", "inconclusive")
    a.metric("isolation_db", round(iso, 2), min=float(a.params["min_isolation_db"]))
    return a.outcome(f"isolation {iso:.1f} dB (cabled {ref:.1f} dB, open {gain:.1f} dB)")


@bench_test(
    "rf.isolation", tier=0, units="tx,rx", tx=True,
    params={"phase": "cabled", "reference": "", "tx_atten_db": 40.0, "offset_hz": 200000.0,
            "nsamples": 262144, "min_isolation_db": 60.0, "stimulus": "auto",
            "capture": "auto", "freq_hz": 0.0, "rx_retune": False, "tx_port": ""},
    description="Leakage with the cable removed, in two runs: phase=cabled records the path "
                "gain, phase=open (cable removed, ports terminated) compares against "
                "reference=<cabled run dir or dB>.",
    pass_criteria=">= 60 dB below the cabled level",
    artifacts=("iso.json", "iso.sigmf-*"), analyze=analyze_isolation,
)
def rf_isolation(ctx: TestContext) -> Outcome:
    phase = str(ctx.params["phase"])
    if phase not in ("cabled", "open"):
        raise UsageError("phase must be cabled or open")
    if phase == "open":
        _reference_gain(str(ctx.params["reference"]))  # validate before transmitting
        ctx.warn("open phase: TX port must be terminated (50 ohm), never an antenna")
    prep = RfPrep(ctx)
    src = ToneSource(ctx, prep.tx, str(ctx.params["stimulus"]))
    atten = _atten(ctx)
    src.configure(prep.f_test, atten)
    try:
        off = src.start(float(ctx.params["offset_hz"]))
        ctx.sleep(0.5)
        capture_iq(ctx, prep.rx, int(ctx.params["nsamples"]), "iso", str(ctx.params["capture"]),
                   prep.rx_lo, prep.fs)
    finally:
        src.stop()
        prep.restore()
    ctx.save_json("iso.json", {"phase": phase, "atten_db": atten, "offset_hz": off,
                               "f_test_hz": prep.f_test,
                               **{k: ctx.metrics[k] for k in ("direction", "tx_unit", "rx_unit",
                                                              "tx_transceiver",
                                                              "rx_transceiver")}})
    return analyze_isolation(ctx)


# ---------------------------------------------------------------------------
# rf.p25_replay
# ---------------------------------------------------------------------------

# The DAC's full scale on the float TX path is 2**14; backed off so filter overshoot can't clip.
P25_TX_FULL_SCALE = 2 ** 14 * 0.9
REPLAY_DIR = "/root/fbench_replay"  # rootfs: RAM without the tmpfs cap on Tezuka images
REPLAY_MEM_MARGIN_KB = 150 * 1024
CYCLIC_MAX_BYTES = 56 << 20  # one DMA buffer from the 64 MiB CMA pool


def _p25_snapshot(http: Any) -> dict[str, Any]:
    snap: dict[str, Any] = {}
    for path in ("/api/decoder_compare", "/api/stats", "/api/traffic"):
        snap[path] = http.get_json(path, None, 5.0)
    return snap


def _p25_counters(snap: dict[str, Any]) -> dict[str, float]:
    dc = (snap.get("/api/decoder_compare") or {}).get("ps_lsm", {}) or {}
    st = snap.get("/api/stats") or {}
    imbe = ((snap.get("/api/traffic") or {}).get("imbe") or {})
    return {
        "tsbk_crc_ok": float(dc.get("tsbk_crc_ok", 0)),
        "tsbk_block_attempts": float(dc.get("tsbk_block_attempts", 0)),
        "nid_decoded_ok": float(dc.get("nid_decoded_ok", 0)),
        "nid_attempts": float(dc.get("nid_attempts", 0)),
        "dibit_count": float(st.get("dibit_count", 0)),
        "ldu": float(imbe.get("ldu1_count", 0)) + float(imbe.get("ldu2_count", 0)),
        "hdu": float(imbe.get("hdu_count", 0)),
        "imbe_frames": float(imbe.get("imbe_frames_extracted", 0)),
    }


def _baseline_score(ref: str) -> float | None:
    if not ref:
        return None
    path = Path(ref)
    res = path / "result.json" if path.is_dir() else path
    if not res.exists():
        raise UsageError(f"baseline {ref!r} has no result.json")
    return json.loads(res.read_text(encoding="utf-8")).get("metrics", {}).get("score")


def _delivery_metrics(a: AnalysisContext, dd: dict[str, Any] | None) -> None:
    """p25-httpd >= 054 ``/api/dibit_delivery`` (absent on older builds)."""
    if not dd:
        return
    for ring in ("control", "traffic"):
        r = dd.get(ring) or {}
        c = r.get("counters") or {}
        a.metric(f"{ring}_delivery_mode", r.get("active_mode"))
        a.metric(f"{ring}_age_p99_ms", (r.get("age") or {}).get("p99_ms"))
        a.metric(f"{ring}_resyncs", c.get("resyncs"))
        a.metric(f"{ring}_phase_mismatches", c.get("phase_mismatches"))


def analyze_p25(a: AnalysisContext) -> Outcome:
    d = a.load_json("p25_replay.json")
    before, after = _p25_counters(d["before"]), _p25_counters(d["after"])
    secs = max(float(d["elapsed_s"]), 1e-9)
    delta = {k: after[k] - before[k] for k in after}
    for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver", "rx_transceiver"):
        if k in d:
            a.metric(k, d[k])
    a.metric("clip", d.get("clip"))
    a.metric("source", d.get("source"))
    a.metric("p25_build", d.get("build"))
    a.metric("seconds", round(secs, 1))
    rate = delta["tsbk_crc_ok"] / secs
    a.metric("crc_ok_per_s", round(rate, 3), min=float(a.params["min_crc_ok_per_s"]))
    a.metric("crc_ok_pct", round(100 * delta["tsbk_crc_ok"] / max(delta["tsbk_block_attempts"],
                                                                    1), 2))
    a.metric("nid_ok_pct", round(100 * delta["nid_decoded_ok"] / max(delta["nid_attempts"], 1), 2))
    a.metric("ldu_per_s", round(delta["ldu"] / secs, 3))
    a.metric("imbe_frames", delta["imbe_frames"])
    a.metric("score", round(rate, 3))
    tail = ""
    truth, loop_s = d.get("truth"), float(d.get("loop_s") or 0.0)
    if truth and loop_s > 0:
        loops = secs / loop_s
        a.metric("loops", round(loops, 2))
        a.metric("truth_transmissions", truth["transmissions"])
        a.metric("truth_imbe_per_loop", truth["imbe"])
        per_loop = delta["imbe_frames"] / loops
        a.metric("imbe_per_loop", round(per_loop, 1))
        a.metric("ldu_per_loop", round(delta["ldu"] / loops, 1))
        a.metric("hdu_per_loop", round(delta["hdu"] / loops, 2))
        if truth["imbe"]:
            pct = 100 * per_loop / truth["imbe"]
            a.metric("imbe_recovery_pct", round(pct, 1),
                     min=float(a.params["min_imbe_recovery_pct"]))
            tail = f", traffic {pct:.1f} % of SDRTrunk's {truth['imbe']} IMBE/loop"
        else:
            a.warn("ground truth has no IMBE frames in this window: traffic not scored")
        if any(c.get("straddles") for c in truth["calls"]):
            a.warn("a ground-truth transmission touches the clip window edge; the loop splice "
                   "cuts it, so the recovery ceiling is below 100 %")
        if abs(loops - round(loops)) > 0.05:
            a.warn(f"{loops:.2f} loops measured: use a whole number (-p loops=N) for an exact "
                   "per-loop count")
    elif d.get("truth_note"):
        a.warn(d["truth_note"])
    _delivery_metrics(a, d.get("delivery"))
    base = _baseline_score(str(a.params["baseline"]))
    if base:
        pct = (rate / base - 1) * 100
        a.metric("baseline_score", base)
        a.metric("score_vs_baseline_pct", round(pct, 2), min=-float(a.params["tolerance_pct"]))
    return a.outcome(f"{d.get('direction', '')} replay of {Path(str(d.get('clip'))).name}: "
                     f"{rate:.2f} CRC-ok TSBK/s, NID ok {a.metrics['nid_ok_pct']} %, "
                     f"{delta['ldu'] / secs:.2f} LDU/s" + tail +
                     (f" ({a.metrics['score_vs_baseline_pct']:+.1f} % vs baseline)" if base else ""))


def _size_cmd(path: str) -> str:
    """File size in bytes on a BusyBox board: no `stat`, and `wc -c` reads the whole
    file (18.8 s for a 448 MB clip), so take it from `ls -ln`."""
    return f"ls -ln {path} 2>/dev/null | awk '{{print $5}}'"


class BoardReplay:
    """A clip streamed gap-free from the TX board's own RAM.

    ``while cat clip; do :; done | iio_writedev`` runs in its own session on the
    board: no CMA limit on clip length (a cyclic buffer stops near 56 MB), no
    load on the host link or on the DUT, and the loop splice is the only
    discontinuity.
    """

    def __init__(self, ctx: TestContext, unit: str, block: int) -> None:
        self.ctx, self.unit, self.block = ctx, unit, block
        self.ssh = ctx.services.ssh(unit)
        self.remote = ""
        self.pidfile = f"{REPLAY_DIR}/stream.pid"
        self.log = f"{REPLAY_DIR}/stream.log"
        self.started = False

    def stage(self, local: Path, sha: str, size: int) -> bool:
        """Upload unless this clip is already on the board. Returns True if uploaded."""
        self.remote = f"{REPLAY_DIR}/clip_{sha[:16]}.cs16"
        _, out, _ = self.ssh.run(f"mkdir -p {REPLAY_DIR}; {_size_cmd(self.remote)}",
                                 15.0)
        if out.strip() == str(size):
            return False
        self.ssh.run(f"rm -f {REPLAY_DIR}/clip_*.cs16", 15.0)
        _, out, _ = self.ssh.run("grep MemAvailable /proc/meminfo", 15.0)
        m = re.search(r"(\d+)", out)
        if m and int(m.group(1)) - size // 1024 < REPLAY_MEM_MARGIN_KB:
            raise PreconditionError(
                f"clip ({size >> 20} MiB) does not fit in {self.unit}'s free RAM "
                f"({int(m.group(1)) >> 10} MiB available, {REPLAY_MEM_MARGIN_KB >> 10} MiB kept "
                "free): shorten clip_seconds", unit=self.unit)
        self.ctx.log.info("uploading %d MiB clip to %s:%s", size >> 20, self.unit, self.remote)
        self.ssh.put(local, self.remote, max(120.0, size / 2e6))
        _, out, _ = self.ssh.run(_size_cmd(self.remote), 15.0)
        if out.strip() and out.strip() != str(size):
            raise FbenchError(f"clip upload to {self.unit} truncated ({out.strip()} of {size} B)")
        return True

    def start(self) -> None:
        dev = self.ctx.cfg.iio.tx_device
        inner = (f"echo $$ > {self.pidfile}; while cat {self.remote}; do :; done | "
                 f"iio_writedev -u local: -b {self.block} {dev} voltage0 voltage1")
        self.ssh.run(f"rm -f {self.pidfile}; setsid sh -c '{inner}' > {self.log} 2>&1 "
                     "< /dev/null &", 15.0)
        self.started = True
        self.ctx.sleep(1.5)
        rc, out, _ = self.ssh.run(f"pgrep -x iio_writedev >/dev/null && echo streaming; "
                                  f"tail -3 {self.log} 2>/dev/null", 15.0)
        if "streaming" not in out and out.strip():
            raise FbenchError(f"replay stream on {self.unit} did not start: {out.strip()[:300]}")

    def stop(self) -> None:
        if not self.started:
            return
        self.ssh.run(f"P=$(cat {self.pidfile} 2>/dev/null); [ -n \"$P\" ] && "
                     f"kill -TERM -- -$P 2>/dev/null; sleep 1; pkill -f '[/]{REPLAY_DIR[1:]}/clip_'; "
                     f"pkill -x iio_writedev; rm -f {self.pidfile}; true", 20.0)
        # '[/]root/...' matches the cat loop but not this shell's own command line.
        self.started = False


def _replay_clip(ctx: TestContext) -> tuple[str, float, float]:
    """Clip path and window: the -p clip with its own window, else the [rf] default."""
    p = ctx.params
    if p["clip"]:
        return str(p["clip"]), float(p["clip_start_s"]), float(p["clip_seconds"]) or 10.0
    rf = ctx.cfg.rf
    start = float(p["clip_start_s"]) or rf.p25_clip_start_s
    return rf.p25_clip, start, float(p["clip_seconds"]) or rf.p25_clip_seconds or 10.0


def _replay_lo_trim(ctx: TestContext, tx: str, freq: float, own_clip: bool) -> tuple[float, str]:
    """TX LO offset that makes the replay look like the air to the recorder's reference.

    A clip carries its recorder's uncorrected reference error; replaying it from
    ``tx`` adds (ref_tx - ref_recorder). Shifting the TX LO by
    (ref_recorder - ref_tx) * f cancels that, so the DUT's stored off-air
    correction applies unchanged.
    """
    p = ctx.params
    if float(p["tx_lo_offset_hz"]):
        return float(p["tx_lo_offset_hz"]), "tx_lo_offset_hz"
    if str(p["lo_trim"]) == "off":
        return 0.0, "off"
    rec = str(p["clip_recorder"]) or ("" if own_clip else ctx.cfg.rf.p25_clip_recorder)
    if not rec:
        ctx.warn("clip recorder unknown (-p clip_recorder= or rf.p25_clip_recorder): TX LO "
                 "not trimmed, the DUT sees the clip's reference error plus the TX board's")
        return 0.0, "unknown recorder"
    if rec not in ctx.cfg.units:
        raise UsageError(f"clip_recorder {rec!r} is not a configured unit")
    ref_rec, ref_tx = ctx.cfg.unit(rec).ref_ppm, ctx.cfg.unit(tx).ref_ppm
    if ref_rec is None or ref_tx is None:
        ctx.warn(f"units.{rec}.ref_ppm / units.{tx}.ref_ppm not set: TX LO not trimmed")
        return 0.0, "ref_ppm unknown"
    return ((ref_rec - ref_tx) * 1e-6 * freq,
            f"ref_ppm {rec} {ref_rec:+.3f} - {tx} {ref_tx:+.3f}")


def _replay_truth(ctx: TestContext, clip: str, start_s: float,
                  secs: float) -> tuple[dict[str, Any] | None, str]:
    from ..analysis.p25_truth import clip_epoch, mbe_truth

    d = str(ctx.params["truth_dir"] or ctx.cfg.rf.p25_truth_dir)
    t = clip_epoch(clip)
    if not d:
        return None, "no ground truth (rf.p25_truth_dir / -p truth_dir): traffic not scored"
    if t is None:
        return None, "clip name has no capture time (<unix>_<freq>_<rate>_...): traffic not scored"
    try:
        return mbe_truth(d, t + start_s, t + start_s + secs), ""
    except FileNotFoundError as exc:
        return None, f"{exc}: traffic not scored"


def _prepare_clip(ctx: TestContext, clip: str, start_s: float, secs: float) -> dict[str, Any]:
    """Cut + scale the window once; reuse it while the source and window are unchanged."""
    src = Path(clip)
    out_dir = ctx.cfg.paths.state_dir / "replay"
    out = out_dir / f"{src.stem}_s{start_s:g}_d{secs:g}.cs16"
    side = out.with_suffix(".json")
    key = {"src": str(src.resolve()), "size": src.stat().st_size,
           "mtime": int(src.stat().st_mtime), "start_s": start_s, "seconds": secs,
           "rate_hz": float(ctx.params["rate_hz"]), "freq_hz": float(ctx.params["freq_hz"])}
    if out.exists() and side.exists():
        try:
            info = json.loads(side.read_text(encoding="utf-8"))
            if info.get("key") == key and out.stat().st_size == 4 * info["samples"]:
                return info
        except (OSError, ValueError, KeyError):
            pass
    for old in out_dir.glob("*.cs16") if out_dir.exists() else ():
        if old != out:
            old.unlink(missing_ok=True)
            old.with_suffix(".json").unlink(missing_ok=True)
    info = sigmf.prepare_replay(src, out, start_s=start_s, seconds=secs,
                                rate_hz=float(ctx.params["rate_hz"]) or None,
                                freq_hz=float(ctx.params["freq_hz"]) or None,
                                full_scale=P25_TX_FULL_SCALE)
    info["key"] = key
    side.write_text(json.dumps(info, indent=1), encoding="utf-8")
    return info


@bench_test(
    "rf.p25_replay", tier=0, units="tx,rx", tx=True,
    params={"clip": "", "clip_start_s": 0.0, "clip_seconds": 0.0, "tx_atten_db": 55.0,
            "seconds": 120.0, "loops": 0, "settle_s": 15.0, "freq_hz": 0.0, "rate_hz": 0.0,
            "source": "auto", "tx_lo_offset_hz": 0.0, "lo_trim": "auto", "clip_recorder": "",
            "tx_rf_bandwidth_hz": 0.0, "block_samples": 1048576, "truth_dir": "",
            "min_imbe_recovery_pct": 90.0, "reset_counters": True, "baseline": "",
            "tolerance_pct": 10.0, "min_crc_ok_per_s": 1.0, "tx_port": ""},
    description="Replay a P25 site clip (SigMF/.wav/.cs16) from the TX board into a DUT "
                "running p25-httpd. Tezuka TX boards stream it gap-free from their own RAM "
                "(source=board, the radio daemon stopped there); others take one cyclic libiio "
                "buffer. The TX LO is trimmed by the recorder/TX reference difference "
                "(units.*.ref_ppm), and traffic is scored against SDRTrunk's per-call .mbe "
                "decode of the same air (rf.p25_truth_dir). It reads p25-httpd's "
                "/api/decoder_compare, /api/stats and /api/traffic, which the scanner does not "
                "serve, so it is in no suite.",
    pass_criteria="TSBK CRC-ok >= min_crc_ok_per_s; IMBE recovery >= min_imbe_recovery_pct "
                  "of SDRTrunk (when ground truth exists); >= baseline - tolerance",
    artifacts=("p25_replay.json", "clip.json"), analyze=analyze_p25,
    duration_param="seconds",
)
def rf_p25_replay(ctx: TestContext) -> Outcome:
    from contextlib import ExitStack

    from ..runner import maintenance

    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    clip, start_s, secs = _replay_clip(ctx)
    if not clip or not Path(clip).exists():
        raise PreconditionError("no P25 clip: pass -p clip=<file> or set rf.p25_clip")
    ctx.require_image(rx, "p25")
    http = ctx.http(rx)
    system = http.get_json("/api/system", None, 5.0)
    source = str(ctx.params["source"])
    tx_image = ctx.caps(tx)["image"]
    if source == "auto":
        source = "board" if tx_image in ("p25", "hwval") else "cyclic"
    if source not in ("board", "cyclic"):
        raise UsageError(f"source must be auto|board|cyclic, not {source!r}")
    info = _prepare_clip(ctx, clip, start_s, secs)
    rate, freq, n = float(info["rate_hz"]), info["freq_hz"], int(info["samples"])
    if not freq:
        raise UsageError("clip centre frequency unknown: pass -p freq_hz=")
    if n == 0:
        raise UsageError("clip window is empty")
    local = Path(info["path"])
    if source == "cyclic" and 4 * n > CYCLIC_MAX_BYTES:
        raise PreconditionError(f"{4 * n >> 20} MiB is too long for one cyclic DMA buffer "
                                f"(<= {CYCLIC_MAX_BYTES >> 20} MiB): shorten clip_seconds or "
                                "use source=board")
    check_transceiver(ctx, tx, freq, rate)
    check_transceiver(ctx, rx, freq)
    direction_metrics(ctx)
    reverse_direction_note(ctx)
    trim, trim_from = _replay_lo_trim(ctx, tx, float(freq), bool(ctx.params["clip"]))
    truth, truth_note = _replay_truth(ctx, clip, start_s, n / rate)
    # Whole loops keep per-loop counts exact: -p loops=N, else `seconds` rounded to loops.
    loop_s = n / rate
    loops = int(ctx.params["loops"]) or max(1, round(float(ctx.params["seconds"]) / loop_s))
    seconds = loops * loop_s
    bw = float(ctx.params["tx_rf_bandwidth_hz"]) or min(
        max(1.25 * rate, 200e3), ctx.cfg.unit(tx).limits["rf_bw_max_hz"])
    tx_lo = int(round(float(freq) + trim))
    ctx.save_json("clip.json", {"path": clip, "start_s": start_s, "seconds": n / rate,
                                "rate_hz": rate, "freq_hz": freq, "samples": n,
                                "peak_counts": info["peak_counts"], "sha256": info["sha256"],
                                "source": source, "tx_lo_hz": tx_lo, "lo_trim_hz": trim,
                                "lo_trim_from": trim_from, "tx_rf_bandwidth_hz": bw,
                                "truth": truth})
    ctx.log.info("replay %s %.1f s from %s via %s, TX LO %d Hz (trim %+.1f Hz: %s)",
                 Path(clip).name, n / rate, tx, source, tx_lo, trim, trim_from)
    phy, atten = ctx.cfg.iio.phy_device, _atten(ctx)
    before = after = delivery = None
    elapsed = 0.0
    with ExitStack() as stack:
        if source == "board" and tx_image == "p25":
            # The TX rate change corrupts the TX board's own RX stream (rule 4).
            stack.enter_context(maintenance(ctx, tx))
        replay: BoardReplay | None = None
        handle = None
        try:
            # Rule 3: attenuation first, then LO/rate/bandwidth, then the source.
            ctx.iio_set(tx, phy, "hardwaregain", f"{-atten:g}", "voltage0", True, tx_ok=True)
            ctx.iio_set(tx, phy, "frequency", f"{tx_lo}", "altvoltage1", True)
            ctx.iio_set(tx, phy, "sampling_frequency", f"{int(rate)}", "voltage0", True)
            ctx.iio_set(tx, phy, "rf_bandwidth", f"{int(bw)}", "voltage0", True)
            ctx.mark_tx_active(tx)
            if source == "board":
                replay = BoardReplay(ctx, tx, int(ctx.params["block_samples"]))
                replay.stage(local, str(info["sha256"]), local.stat().st_size)
                replay.start()
            else:
                handle = ctx.services.iio(tx).start_cyclic_tx(
                    ctx.cfg.iio.tx_device, ["voltage0", "voltage1"], local, n)
            ctx.sleep(float(ctx.params["settle_s"]))
            if ctx.params["reset_counters"]:
                http.get_json("/api/decoder_reset", None, 5.0)
            before = _p25_snapshot(http)
            t0 = ctx.services.monotonic()
            ctx.sleep(seconds)
            after = _p25_snapshot(http)
            elapsed = ctx.services.monotonic() - t0
            try:
                delivery = http.get_json("/api/dibit_delivery", None, 5.0) or None
            except FbenchError:
                delivery = None  # p25-httpd before 054
        finally:
            if replay is not None:
                replay.stop()
            if handle is not None:
                handle.stop()
            try:
                ctx.tx_off(tx)  # before maintenance exit restarts the radio daemon on the TX board
            except FbenchError as exc:
                ctx.errors.append(f"tx off on {tx} after replay: {exc.message}")
    ctx.save_json("p25_replay.json", {"clip": clip, "source": source, "build": system.get("build"),
                                      "before": before, "after": after, "elapsed_s": elapsed,
                                      "loop_s": n / rate, "truth": truth,
                                      "truth_note": truth_note, "delivery": delivery,
                                      **{k: ctx.metrics[k] for k in ("direction", "tx_unit",
                                                                     "rx_unit", "tx_transceiver",
                                                                     "rx_transceiver")}})
    return analyze_p25(ctx)


# ---------------------------------------------------------------------------
# rf.refclk_eth (F19)
# ---------------------------------------------------------------------------

PHASES = ("idle", "down", "bounce", "load")


def _board_capture_cmd(dev: str, n: int, path: str) -> str:
    b = min(n, 1 << 22)
    return f"iio_readdev -u local: -b {b} -s {n} {dev} voltage0 voltage1 > {path}"


def _detached(cmd: str, done: str) -> str:
    inner = f"{cmd}; echo $? > {done}"
    return f"nohup sh -c '{inner}' >/dev/null 2>&1 &"


def _wait_done(ctx: TestContext, unit: str, done: str, timeout: float) -> int:
    """Poll for a detached command's exit file; tolerates the link being down."""
    deadline = ctx.services.monotonic() + timeout
    while ctx.services.monotonic() < deadline:
        try:
            rc, out, _ = ctx.services.ssh(unit).run(f"cat {done} 2>/dev/null", 10.0)
            if rc == 0 and out.strip():
                return int(out.strip())
        except FbenchError:
            pass  # link bouncing: retry
        ctx.sleep(1.0)
    raise FbenchError(f"detached capture on {unit} did not finish within {timeout:.0f} s")


def _refclk_capture(ctx: TestContext, unit: str, phase: str, n: int, fs: float, lo: float,
                    during: Any = None) -> None:
    remote = f"/tmp/fbench_{ctx.run_id}_{phase}.cs16"
    done = remote + ".done"
    ssh = ctx.services.ssh(unit)
    ssh.run(f"rm -f {remote} {done}", 15.0)
    ssh.run(_detached(_board_capture_cmd(ctx.cfg.iio.rx_device, n, remote), done), 15.0)
    if during is not None:
        during()
    rc = _wait_done(ctx, unit, done, n / fs * 3 + 60)
    if rc != 0:
        raise FbenchError(f"iio_readdev on {unit} exited {rc} during phase {phase}")
    local = ctx.artifact_path(f"refclk_{phase}.sigmf-data")
    ssh.get(remote, local, 300.0)
    ssh.run(f"rm -f {remote} {done}", 15.0)
    sigmf.write(local, local.read_bytes(), fs, lo, "ci16_le", f"rf.refclk_eth {phase} on {unit}")
    ctx.artifact_path(f"refclk_{phase}.sigmf-meta")


def analyze_refclk(a: AnalysisContext) -> Outcome:
    d = a.load_json("refclk.json")
    offset = float(d["offset_hz"])
    carrier = float(d["f_test_hz"]) + offset
    nfft = int(a.params["nfft"])
    per: dict[str, dict[str, Any]] = {}
    for phase in d["phases"]:
        iq, fs, lo = load_capture(a, f"refclk_{phase}")
        head = iq[: min(len(iq), 1 << 20)]
        est, sign = find_cw(head, fs, offset)
        pc = phase_continuity(iq, fs, est.freq_hz, float(a.params["block_s"]),
                              float(a.params["step_threshold_deg"]))
        freqs, pdb = spectrum_db(iq, fs, nfft)
        main_db = est.power_dbfs
        spurs = find_spurs(pdb, freqs, est.freq_hz, 8 * fs / nfft,
                           float(np.median(pdb)) + 10.0, main_db, 40)
        cands = harmonic_aliases(ETH_CLOCKS_HZ, float(lo or d["rx_lo_hz"]), fs, fs,
                                 dc_guard_hz=3 * fs / nfft)
        eth = [s for s in match_spurs(spurs, cands, 3 * fs / nfft)
               if s["dbc"] > float(a.params["max_spur_dbc"])]
        per[phase] = {"ppm": ppm(sign * pc["freq_hz"] - offset, carrier), "snr_db": est.snr_db,
                      "phase_steps": pc["n_steps"], "max_step_deg": pc["max_step_deg"],
                      "steps": pc["steps"], "residual_rms_deg": pc["residual_rms_deg"],
                      "spurs": spurs, "eth_spurs": eth}
    a.save_json("refclk_phases.json", per)
    for k in ("direction", "tx_unit", "rx_unit", "tx_transceiver", "rx_transceiver"):
        if k in d:
            a.metric(k, d[k])
    a.metric("phases", list(per))
    for p, r in per.items():
        a.metric(f"ppm_{p}", round(r["ppm"], 5))
        a.metric(f"phase_steps_{p}", r["phase_steps"])
    if "idle" not in per:
        raise Inconclusive("idle reference phase missing")
    shift = max(abs(r["ppm"] - per["idle"]["ppm"]) for r in per.values())
    a.metric("max_freq_shift_ppm", round(shift, 5), max=float(a.params["max_shift_ppm"]))
    a.metric("max_phase_step_deg", round(max(r["max_step_deg"] for r in per.values()), 2))
    a.metric("phase_steps_total", sum(r["phase_steps"] for r in per.values()), max=0)
    a.metric("eth_spurs", sum(len(r["eth_spurs"]) for r in per.values()), max=0)
    load_only = []
    if "load" in per:
        tol = 3 * float(d.get("fs_hz", 1)) / nfft
        idle_f = [s["freq_hz"] for s in per["idle"]["spurs"]]
        load_only = [s for s in per["load"]["spurs"]
                     if s["dbc"] > float(a.params["max_spur_dbc"])
                     and all(abs(s["freq_hz"] - f) > tol for f in idle_f)]
        a.metric("load_only_spurs", len(load_only), max=0)
    for skipped in d.get("skipped", []):
        a.warn(f"phase skipped: {skipped}")
    return a.outcome(
        f"{d.get('direction', '')}: phases {', '.join(per)}; max CW shift {shift:.4f} ppm, "
        f"{a.metrics['phase_steps_total']} phase steps > {a.params['step_threshold_deg']} deg, "
        f"{a.metrics['eth_spurs']} Ethernet-clock spurs > {a.params['max_spur_dbc']} dBc, "
        f"{len(load_only)} load-only lines")


@bench_test(
    "rf.refclk_eth", tier=0, units="tx,rx", tx=True,
    params={"tx_atten_db": 40.0, "offset_hz": 200000.0, "rx_rate_hz": 0.0,
            "capture_s": 0.5, "bounce_s": 2.0, "bounce_pad_s": 1.5, "load_mb": 256,
            "port": 5202,
            "block_s": 0.001, "step_threshold_deg": 10.0, "max_spur_dbc": -80.0,
            "max_shift_ppm": 0.05, "nfft": 65536, "stimulus": "auto", "freq_hz": 0.0,
            "rx_retune": False, "tx_port": ""},
    description="Does the RX unit's Ethernet disturb its 40 MHz reference? CW captured on the "
                "RX board (iio_readdev to /tmp, pulled afterwards) with eth0 idle, down (only "
                "units not managed over eth0), bounced mid-capture (detached, self-restoring) "
                "and under agent net-send load. rx_rate_hz=0 keeps the RX rate; setting it "
                "enters maintenance mode on the RX unit for the run (the scanner stopped).",
    pass_criteria="no phase steps > 10 deg, no Ethernet-correlated spurs above -80 dBc, CW "
                  "shift between phases < 0.05 ppm",
    artifacts=("refclk.json", "refclk_phases.json", "refclk_<phase>.sigmf-*"), suites=("rf",),
    analyze=analyze_refclk,
)
def rf_refclk_eth(ctx: TestContext) -> Outcome:
    from contextlib import ExitStack

    from ..runner import maintenance

    rx = ctx.roles["rx"]
    st0 = rx_state(ctx, rx)
    want_fs = float(ctx.params["rx_rate_hz"]) or st0.fs_hz
    with ExitStack() as stack:
        if abs(want_fs - st0.fs_hz) > 1:
            # A rate change breaks the scanner's assumptions: stop it for the run.
            stack.enter_context(maintenance(ctx, rx))
        return _refclk_run(ctx, st0, want_fs)


def _refclk_run(ctx: TestContext, st0: Any, want_fs: float) -> Outcome:
    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    rx_unit = ctx.cfg.unit(rx)
    phy = ctx.cfg.iio.phy_device
    rate_changed = abs(want_fs - st0.fs_hz) > 1
    if rate_changed:
        ctx.iio_set(rx, phy, "sampling_frequency", f"{int(want_fs)}", "voltage0", False)
    prep = RfPrep(ctx)
    fs = prep.fs if not rate_changed else want_fs
    src = ToneSource(ctx, tx, str(ctx.params["stimulus"]))
    src.configure(prep.f_test, _atten(ctx))
    phases: list[str] = []
    skipped: list[str] = []
    eth_managed = bool(rx_unit.via)
    n = int(float(ctx.params["capture_s"]) * fs)
    try:
        off = src.start(float(ctx.params["offset_hz"]))
        ctx.sleep(1.0)
        _refclk_capture(ctx, rx, "idle", n, fs, prep.rx_lo)
        phases.append("idle")
        if eth_managed:
            skipped.append(f"down: {rx} is managed over eth0 (via {rx_unit.via})")
        else:
            ctx.services.ssh(rx).run("ip link set eth0 down", 15.0)
            try:
                _refclk_capture(ctx, rx, "down", n, fs, prep.rx_lo)
                phases.append("down")
            finally:
                ctx.services.ssh(rx).run("ip link set eth0 up", 15.0)
        bounce = float(ctx.params["bounce_s"])
        nb = int((bounce + float(ctx.params["bounce_pad_s"])) * fs)
        bounce_cmd = (f"nohup sh -c 'sleep 1; ip link set eth0 down; sleep {bounce:g}; "
                      "ip link set eth0 up' >/dev/null 2>&1 &")
        _refclk_capture(ctx, rx, "bounce", nb, fs, prep.rx_lo,
                        during=lambda: ctx.services.ssh(rx).run(bounce_cmd, 15.0))
        phases.append("bounce")
        ctx.sleep(5.0)  # link renegotiation before the load phase
        if ctx.has_agent(rx) and ctx.has_agent(tx):
            peer_ip = ctx.cfg.unit(tx).eth_ip or ctx.cfg.unit(tx).host
            port, mb = int(ctx.params["port"]), int(ctx.params["load_mb"])
            server = ctx.agent.net_serve(tx, port, mb)
            try:
                ctx.sleep(1.0)
                _refclk_capture(ctx, rx, "load", n, fs, prep.rx_lo,
                                during=lambda: ctx.agent.spawn(rx, ["net", "send", "--host",
                                                                    peer_ip, "--port", str(port),
                                                                    "--mb", str(mb)]))
                phases.append("load")
            finally:
                server.kill()
        else:
            skipped.append("load: needs the agent on both units")
    finally:
        src.stop()
        prep.restore()
        if rate_changed:
            try:
                ctx.iio_set(rx, phy, "sampling_frequency", f"{int(st0.fs_hz)}", "voltage0", False)
            except FbenchError as exc:
                ctx.errors.append(f"could not restore RX rate: {exc.message}")
    ctx.save_json("refclk.json", {"f_test_hz": prep.f_test, "rx_lo_hz": prep.rx_lo, "fs_hz": fs,
                                  "offset_hz": off, "phases": phases, "skipped": skipped,
                                  "method": src.method,
                                  **{k: ctx.metrics[k] for k in ("direction", "tx_unit",
                                                                 "rx_unit", "tx_transceiver",
                                                                 "rx_transceiver")}})
    return analyze_refclk(ctx)
