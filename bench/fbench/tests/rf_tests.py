"""rf.* tests (cabled, attenuated; the runner enforces the interlock).

Every tx,rx test records the direction and both transceivers (AD9361 vs
AD9363) and recommends running the reverse direction, so transceiver
differences separate from board differences.
"""

from __future__ import annotations

import json
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
from ..errors import FbenchError, Inconclusive, UsageError
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
