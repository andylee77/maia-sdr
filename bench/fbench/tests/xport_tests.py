"""xport.* tests: reference libiio capture, production-ring PRBS and lap tests."""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from ..analysis.ring import expected_lap_onset_ms, lap_onset, last_clean_stall, summarise
from ..errors import Inconclusive, PreconditionError, UsageError
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..stimulus import parse_number

BOARD_TIMED_READ = ("T0=$(date +%s.%N); iio_readdev -u local: -b {b} -s {n} {dev} voltage0 "
                    "voltage1 > /dev/null; RC=$?; T1=$(date +%s.%N); echo \"$RC $T0 $T1\"")


def _parse_timed(out: str) -> tuple[int, float, float, bool]:
    """Parse ``RC T0 T1``; returns (rc, t0, t1, nanosecond_resolution)."""
    parts = out.strip().split()
    if len(parts) < 3:
        raise PreconditionError(f"unexpected timing output: {out!r}")
    rc = int(parts[0])
    if "%N" in parts[1] or not re.fullmatch(r"\d+(\.\d+)?", parts[1]):
        raise PreconditionError("board `date` lacks %N (BusyBox FEATURE_DATE_NANO); "
                                "use -p where=host")
    return rc, float(parts[1]), float(parts[2]), "." in parts[1]


# ---------------------------------------------------------------------------
# xport.iio_capture
# ---------------------------------------------------------------------------


def analyze_iio_capture(a: AnalysisContext) -> Outcome:
    d = a.load_json("iio_capture.json")
    fs = float(d["fs_hz"])
    short, long_ = d["short"], d["long"]
    dn = long_["nsamples"] - short["nsamples"]
    dt = long_["elapsed_s"] - short["elapsed_s"]
    if dt <= 0:
        raise Inconclusive("non-increasing capture times; cannot derive the rate")
    rate = dn / dt
    err_pct = (rate / fs - 1.0) * 100.0
    a.metric("where", d["where"])
    a.metric("fs_hz", fs)
    a.metric("measured_rate_hz", round(rate, 2))
    a.metric("overhead_s", round(short["elapsed_s"] - short["nsamples"] / fs, 4))
    a.metric("rate_error_pct_abs", round(abs(err_pct), 5), max=float(a.params["tol_pct"]))
    hint = " (reader slower than the ADC: gaps/overflows)" if err_pct < -a.params["tol_pct"] \
        else ""
    return a.outcome(f"{d['where']} libiio capture: {rate:.1f} S/s vs {fs:.0f} "
                     f"({err_pct:+.4f} %){hint}")


@bench_test(
    "xport.iio_capture", tier=0, units="any",
    params={"where": "board", "seconds_short": 2.0, "seconds_long": 20.0, "tol_pct": 0.01,
            "buffer_samples": 1048576},
    description="Reference libiio capture: sample count vs wall clock, differential (long "
                "minus short capture cancels start-up overhead). where=board runs iio_readdev "
                "on the unit, where=host over the network (USB-limited).",
    pass_criteria="rate within 0.01 % of sampling_frequency",
    artifacts=("iio_capture.json",), suites=("transport",), analyze=analyze_iio_capture,
    duration_param="seconds_long",
)
def xport_iio_capture(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    fs = parse_number(ctx.iio_get(name, ctx.cfg.iio.phy_device, "sampling_frequency",
                                  "voltage0", False))
    where = str(ctx.params["where"])
    runs: dict[str, dict[str, Any]] = {}
    for label, secs in (("short", float(ctx.params["seconds_short"])),
                        ("long", float(ctx.params["seconds_long"]))):
        n = int(round(fs * secs))
        if where == "board":
            cmd = BOARD_TIMED_READ.format(b=int(ctx.params["buffer_samples"]), n=n,
                                          dev=ctx.cfg.iio.rx_device)
            rc, out, err = ctx.services.ssh(name).run(cmd, secs * 3 + 30)
            prc, t0, t1, _ = _parse_timed(out)
            if prc != 0:
                raise PreconditionError(f"iio_readdev failed on {name}: {err.strip()[:200]}")
            elapsed = t1 - t0
        elif where == "host":
            tmp = ctx.artifacts_dir / f"_host_{label}.cs16"
            t0 = ctx.services.monotonic()
            ctx.services.iio(name).capture(ctx.cfg.iio.rx_device, ["voltage0", "voltage1"], n,
                                           tmp, timeout=secs * 3 + 30)
            elapsed = ctx.services.monotonic() - t0
            Path(tmp).unlink(missing_ok=True)
        else:
            raise UsageError("where must be board or host")
        runs[label] = {"nsamples": n, "elapsed_s": elapsed}
    ctx.save_json("iio_capture.json", {"where": where, "fs_hz": fs, **runs})
    return analyze_iio_capture(ctx)


# ---------------------------------------------------------------------------
# xport.p25_ring_prbs / xport.p25_ring_lap
# ---------------------------------------------------------------------------
#
# The agent drives the stimulus itself (``--bist prbs|tone``, restored on
# exit) and, in maintenance mode, enables the wideband DMA (``--enable``) and
# releases ``sdr_reset`` if p25-httpd left it asserted (``--release-reset``).


def _ring_kwargs(ctx: TestContext) -> dict[str, Any]:
    return {"bist": str(ctx.params["bist"]) or None, "enable": bool(ctx.params["enable"]),
            "release_reset": bool(ctx.params["release_reset"])}


def analyze_ring_prbs(a: AnalysisContext) -> Outcome:
    s = summarise(a.load_json("ring_check.json"))
    a.save_json("ring_summary.json", s)
    a.metric("bytes_checked", s["bytes_checked"])
    a.metric("seconds", s["seconds"])
    a.metric("counts", s["counts"])
    if s["bytes_checked"] <= 0:
        raise Inconclusive("ring checker verified no data")
    a.metric("lost_bytes", s["lost_bytes"], max=0)
    a.metric("anomalies_total", s["total"], max=0)
    if s.get("periodicity", {}).get("periodic"):
        a.warn(f"anomalies are periodic: {s['periodicity']['period_s']:.2f} s")
    return a.outcome(f"production ring, {s['bytes_checked'] / 1e6:.1f} MB checked in "
                     f"{s['seconds']:g} s: {s['total']} anomalies "
                     f"({', '.join(s['classes_seen']) or 'none'})")


@bench_test(
    "xport.p25_ring_prbs", tier=0, units="any", maintenance=True,
    params={"seconds": 60.0, "ring": "p25-wideband", "pattern": "pn0fn", "bist": "prbs",
            "enable": True, "release_reset": True},
    description="AD9361 BIST PRBS (or tone: -p bist=tone -p pattern=tone) through the "
                "PRODUCTION wideband ring, read by the agent's checker; anomaly classes.",
    pass_criteria="0 anomalies at nominal reader timing",
    artifacts=("ring_check.json", "ring_summary.json"), suites=("transport",),
    requires=("p25 image",), analyze=analyze_ring_prbs, duration_param="seconds",
)
def xport_p25_ring_prbs(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.require_image(name, "p25")
    rep = ctx.agent.ring_check(name, str(ctx.params["ring"]), str(ctx.params["pattern"]),
                               float(ctx.params["seconds"]), **_ring_kwargs(ctx))
    ctx.save_json("ring_check.json", rep)
    return analyze_ring_prbs(ctx)


def lap_points(d: dict[str, Any]) -> list[tuple[float, float]]:
    """(stall_ms, lost) points from one multi-stall run or several single-stall runs."""
    points: list[tuple[float, float]] = []
    for run in d.get("runs", []):
        stalls = run.get("stalls")
        if stalls:
            for st in stalls:
                after = st.get("anomalies_after") or {}
                lost = float(st.get("lost_units_after") or 0) or float(
                    after.get("lap", 0) + after.get("word_gap", 0) + after.get("torn", 0))
                points.append((float(st["stall_ms"]), lost))
        else:
            s = summarise(run)
            lost = s["lost_bytes"] or (s["counts"].get("lap", 0) + s["counts"].get("word_gap", 0))
            points.append((float(run["stall_ms"]), float(lost)))
    # A stall value repeated during one run: keep the worst observation.
    worst: dict[float, float] = {}
    for stall, lost in points:
        worst[stall] = max(worst.get(stall, 0.0), lost)
    return sorted(worst.items())


def analyze_ring_lap(a: AnalysisContext) -> Outcome:
    d = a.load_json("ring_lap.json")
    points = lap_points(d)
    geom: dict[str, Any] = {}
    for run in d["runs"]:
        for key in ("num_buffers", "subbuf_period_ms", "lap_threshold_ms"):
            if key in run and key not in geom:
                geom[key] = run[key]
    a.metric("points", [{"stall_ms": s, "lost": l} for s, l in points])
    if not points:
        raise Inconclusive("no stall observations")
    if "num_buffers" not in geom or "subbuf_period_ms" not in geom:
        raise Inconclusive("agent did not report ring geometry (num_buffers, subbuf_period_ms)")
    expected = expected_lap_onset_ms(int(geom["num_buffers"]), float(geom["subbuf_period_ms"]))
    if geom.get("lap_threshold_ms") is not None:
        a.metric("agent_lap_threshold_ms", geom["lap_threshold_ms"])
    onset = lap_onset(points)
    clean = last_clean_stall(points)
    a.metric("expected_onset_ms", round(expected, 2))
    a.metric("observed_onset_ms", onset)
    a.metric("last_clean_stall_ms", clean)
    if onset is None:
        if max(s for s, _ in points) < expected:
            raise Inconclusive(f"no loss up to {max(s for s, _ in points):g} ms; stall sweep "
                               f"must exceed the expected onset {expected:.0f} ms")
        a.metric("onset_error_frac", None, max=float(a.params["tolerance_frac"]))
        return a.outcome(f"no loss even beyond (N-1)xT_buf = {expected:.0f} ms: model "
                         "disagrees with the production ring", "fail")
    err = abs(onset - expected) / expected
    a.metric("onset_error_frac", round(err, 3), max=float(a.params["tolerance_frac"]))
    return a.outcome(f"loss onset between {clean} and {onset} ms stall; model (N-1)xT_buf = "
                     f"{expected:.0f} ms (lap-blind reader confirmed)")


@bench_test(
    "xport.p25_ring_lap", tier=0, units="any", maintenance=True,
    params={"seconds": 30.0, "ring": "p25-wideband", "pattern": "pn0fn", "bist": "prbs",
            "enable": True, "release_reset": True,
            "stall_ms": [100, 200, 300, 400, 450, 500, 550, 600, 800], "tolerance_frac": 0.25},
    description="Production ring with injected reader stalls (one agent run, --stall-ms list, "
                "one stall every 1.5 s): proves lap blindness and measures the loss threshold.",
    pass_criteria="loss onset at (N-1) x T_buf (within tolerance_frac)",
    artifacts=("ring_lap.json",), suites=("transport",), requires=("p25 image",),
    analyze=analyze_ring_lap, duration_param="seconds",
)
def xport_p25_ring_lap(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.require_image(name, "p25")
    stalls = [int(s) for s in ctx.params["stall_ms"]]
    # Enough time for every stall at the agent's default 1.5 s spacing.
    seconds = max(float(ctx.params["seconds"]), 1.5 * len(stalls) + 3.0)
    rep = ctx.agent.ring_check(name, str(ctx.params["ring"]), str(ctx.params["pattern"]),
                               seconds, stalls, **_ring_kwargs(ctx))
    ctx.save_json("ring_lap.json", {"runs": [rep]})
    return analyze_ring_lap(ctx)
