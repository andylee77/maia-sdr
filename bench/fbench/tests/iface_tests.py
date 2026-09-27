"""iface.* tests: LVDS clock, PRBS soak, eye scans, TX link, FPGA loopback."""

from __future__ import annotations

from typing import Any

from ..analysis.eye import eye_summary, grid_summary, heatmap_png, longest_run
from ..errors import Inconclusive
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..stimulus import parse_number

CLK_FREQ_LSB_HZ = 100e6 / 65536  # up_clock_mon: count x 100 MHz / 2^16
ADC_CLK_FREQ_OFF = 0x0054
ADC_CLK_RATIO_OFF = 0x0058


def adc_reg_read(ctx: TestContext, unit: str, name: str, offset: int) -> int:
    """ADC core register via the agent, else libiio debugfs direct_reg_access."""
    if ctx.has_agent(unit):
        return ctx.agent.reg_read(unit, "adi_adc", name)
    iio = ctx.services.iio(unit)
    dev = ctx.cfg.iio.rx_device
    # cf-ad9361-lpc forwards addresses without bit 31 to the AD9361 over SPI;
    # bit 31 (the pcore flag) selects the axi_ad9361 core's own registers.
    iio.attr_set(dev, "direct_reg_access", hex(0x8000_0000 | offset), debug=True)
    return int(iio.attr_get(dev, "direct_reg_access", debug=True).strip(), 0)


def _rates(ctx: TestContext, unit: str) -> list[int]:
    rates = [int(r) for r in ctx.params.get("rates_hz", [])]
    if rates:
        return rates
    fs = parse_number(ctx.iio_get(unit, ctx.cfg.iio.phy_device, "sampling_frequency",
                                  "voltage0", False))
    return [int(fs)]


# ---------------------------------------------------------------------------
# iface.clk_freq
# ---------------------------------------------------------------------------


def analyze_clk(a: AnalysisContext) -> Outcome:
    d = a.load_json("clk.json")
    measured = d["clk_freq"] * CLK_FREQ_LSB_HZ
    expected = d["fs_hz"] * float(a.params["clk_mult"])
    err = measured - expected
    a.metric("clk_freq_count", d["clk_freq"])
    a.metric("clk_ratio", d.get("clk_ratio"))
    a.metric("fs_hz", d["fs_hz"])
    a.metric("measured_hz", round(measured, 1))
    a.metric("expected_hz", expected)
    a.metric("implied_mult", round(measured / d["fs_hz"], 4) if d["fs_hz"] else None)
    a.metric("error_hz_abs", round(abs(err), 1), max=float(a.params["tol_hz"]))
    return a.outcome(f"interface clock {measured / 1e6:.4f} MHz vs expected "
                     f"{expected / 1e6:.4f} MHz ({a.params['clk_mult']} x fs), error {err:+.0f} Hz")


@bench_test(
    "iface.clk_freq", tier=0, units="any",
    params={"clk_mult": 2.0, "tol_hz": 1526.0},
    description="axi_ad9361 CLK_FREQ (interface clock, 1.526 kHz/count) vs clk_mult x fs. "
                "Uses the agent, or libiio direct_reg_access on factory firmware.",
    pass_criteria="within one CLK_FREQ count (1.526 kHz) of clk_mult x fs",
    artifacts=("clk.json",), suites=("smoke", "interface"), analyze=analyze_clk,
)
def iface_clk_freq(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    fs = parse_number(ctx.iio_get(name, ctx.cfg.iio.phy_device, "sampling_frequency",
                                  "voltage0", False))
    clk = adc_reg_read(ctx, name, "CLK_FREQ", ADC_CLK_FREQ_OFF)
    ratio = adc_reg_read(ctx, name, "CLK_RATIO", ADC_CLK_RATIO_OFF)
    ctx.save_json("clk.json", {"clk_freq": clk, "clk_ratio": ratio, "fs_hz": fs})
    return analyze_clk(ctx)


# ---------------------------------------------------------------------------
# iface.prbs_soak
# ---------------------------------------------------------------------------


def analyze_prbs(a: AnalysisContext) -> Outcome:
    d = a.load_json("prbs.json")
    total = 0
    parts = []
    for run in d["runs"]:
        ei = int(run.get("error_intervals", 0))
        total += ei
        a.metric(f"error_intervals_{int(run['rate_hz'])}", ei)
        parts.append(f"{run['rate_hz'] / 1e6:g} MSPS: {ei} in {run.get('seconds')} s")
    a.metric("error_intervals_total", total, max=0)
    return a.outcome("AD9361 BIST PRBS error intervals — " + "; ".join(parts))


@bench_test(
    "iface.prbs_soak", tier=0, units="any", maintenance=True,
    params={"seconds": 60.0, "poll_ms": 100, "rates_hz": []},
    description="AD9361 BIST PRBS through LVDS; ADI PN-monitor error intervals over time per "
                "rate (empty rates_hz = current rate).",
    pass_criteria="0 error intervals",
    artifacts=("prbs.json",), suites=("interface", "soak"), analyze=analyze_prbs,
    duration_param="seconds",
)
def iface_prbs_soak(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    runs = []
    for rate in _rates(ctx, name):
        rep = ctx.agent.prbs_soak(name, float(ctx.params["seconds"]), int(ctx.params["poll_ms"]),
                                  rate if ctx.params["rates_hz"] else None)
        runs.append({"rate_hz": rate, **rep})
    ctx.save_json("prbs.json", {"runs": runs})
    return analyze_prbs(ctx)


# ---------------------------------------------------------------------------
# iface.eye_ad9361
# ---------------------------------------------------------------------------


def analyze_eye_ad9361(a: AnalysisContext) -> Outcome:
    d = a.load_json("eye_ad9361.json")
    worst = None
    parts = []
    for scan in d["scans"]:
        rate = int(scan["rate_hz"])
        chosen = scan.get("chosen") or {}
        pt = (int(chosen["clk"]), int(chosen["data"])) if chosen else None
        s = grid_summary(scan["grid"], pt)
        m = (s.get("chosen_margin") or {}).get("margin")
        margin = 16 if m is None and pt is not None else m
        a.metric(f"margin_steps_{rate}", margin)
        a.metric(f"pass_cells_{rate}", s["pass_cells"])
        heatmap_png(scan["grid"], a.artifact_path(f"eye_ad9361_{rate}.png"),
                    f"AD9361 delay eye {rate / 1e6:g} MSPS", "data delay", "clock delay", pt)
        if margin is not None:
            worst = margin if worst is None else min(worst, margin)
        parts.append(f"{rate / 1e6:g} MSPS chosen {pt} margin {margin}")
    if worst is None:
        raise Inconclusive("no chosen delay point reported")
    a.metric("margin_steps_min", worst, min=int(a.params["min_margin"]))
    return a.outcome("; ".join(parts))


@bench_test(
    "iface.eye_ad9361", tier=0, units="any", maintenance=True,
    params={"rates_hz": [61440000], "dwell_ms": 10, "min_margin": 3},
    description="AD9361 clock/data delay 16x16 PRBS pass grid per rate; margin of the "
                "boot-chosen delays.",
    pass_criteria="chosen point >= 3 steps from the window edge",
    artifacts=("eye_ad9361.json", "eye_ad9361_<rate>.png"), suites=("interface",),
    analyze=analyze_eye_ad9361,
)
def iface_eye_ad9361(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    scans = []
    for rate in _rates(ctx, name):
        rep = ctx.agent.eyescan(name, "ad9361", rate, int(ctx.params["dwell_ms"]))
        scans.append({"rate_hz": rate, **rep})
    ctx.save_json("eye_ad9361.json", {"scans": scans})
    return analyze_eye_ad9361(ctx)


# ---------------------------------------------------------------------------
# iface.eye_idelay
# ---------------------------------------------------------------------------


def _lane_rows_2d(scan: dict[str, Any]) -> tuple[list[list[int]], list[int]]:
    """Best AD9361-delay row per lane of a 2-D scan -> (rows, best_delay)."""
    rows, best = [], []
    for lane in scan["lanes"]:
        grid = lane["grid"]
        widths = [longest_run(r)[2] for r in grid]
        k = max(range(len(grid)), key=lambda i: widths[i])
        rows.append(list(grid[k]))
        best.append(k)
    return rows, best


def analyze_eye_idelay(a: AnalysisContext) -> Outcome:
    d = a.load_json("eye_idelay.json")
    thr_rate = float(a.params["threshold_rate_hz"])
    worst = None
    parts = []
    for scan in d["scans"]:
        rate = int(scan["rate_hz"])
        if scan.get("mode") == "2d":
            rows, best = _lane_rows_2d(scan)
            a.metric(f"best_ad9361_delay_{rate}", best)
            for lane in scan["lanes"]:
                heatmap_png(lane["grid"], a.artifact_path(f"eye2d_{rate}_lane{lane['lane']}.png"),
                            f"lane {lane['lane']} {rate / 1e6:g} MSPS", "IDELAY tap",
                            "AD9361 data delay")
        else:
            rows = [lane["pass"] for lane in scan["lanes"]]
        s = eye_summary(rows)
        a.metric(f"window_taps_min_{rate}", s["window_taps_min"])
        a.metric(f"window_ns_min_{rate}", s["window_ns_min"])
        a.metric(f"centre_tap_{rate}", s["centre_tap"])
        a.metric(f"lanes_{rate}", s["lanes"])
        if scan.get("current_taps") is not None:
            a.metric(f"current_taps_{rate}", scan["current_taps"])
        heatmap_png(rows, a.artifact_path(f"eye_idelay_{rate}.png"),
                    f"IDELAY eye {rate / 1e6:g} MSPS", "IDELAY tap (78 ps)", "lane",
                    None)
        if rate <= thr_rate:
            worst = s["window_taps_min"] if worst is None else min(worst, s["window_taps_min"])
        parts.append(f"{rate / 1e6:g} MSPS min window {s['window_taps_min']} taps "
                     f"({s['window_ns_min']} ns), centre {s['centre_tap']}")
    if worst is None:
        raise Inconclusive("no scan at or below the threshold rate")
    a.metric("window_taps_min", worst, min=int(a.params["min_window_taps"]))
    return a.outcome("; ".join(parts))


@bench_test(
    "iface.eye_idelay", tier=0, units="any", maintenance=True,
    params={"rates_hz": [61440000], "dwell_ms": 10, "mode": "idelay", "lanes": [],
            "min_window_taps": 6, "threshold_rate_hz": 61440000},
    description="FPGA IDELAY 0-31 per lane (mode=idelay) or x AD9361 delay (mode=2d); "
                "window/centre per lane and rate, heatmaps.",
    pass_criteria=">= 6-tap window on every lane at 61.44 MSPS",
    artifacts=("eye_idelay.json", "eye_idelay_<rate>.png"), suites=("interface",),
    analyze=analyze_eye_idelay,
)
def iface_eye_idelay(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    mode = str(ctx.params["mode"])
    if mode not in ("idelay", "2d"):
        from ..errors import UsageError

        raise UsageError("mode must be idelay or 2d")
    scans = []
    lanes = [int(x) for x in ctx.params["lanes"]] or None
    for rate in _rates(ctx, name):
        rep = ctx.agent.eyescan(name, mode, rate, int(ctx.params["dwell_ms"]), lanes)
        scans.append({"rate_hz": rate, "mode": mode, **rep})
    ctx.save_json("eye_idelay.json", {"scans": scans})
    return analyze_eye_idelay(ctx)


# ---------------------------------------------------------------------------
# iface.tx_link / iface.fpga_loopback
# ---------------------------------------------------------------------------


def analyze_tx_link(a: AnalysisContext) -> Outcome:
    d = a.load_json("tx_link.json")
    sweep = d.get("sweep") or []
    if sweep:
        clean = [1 if int(p.get("errors", 1)) == 0 else 0 for p in sweep]
        start, end, width = longest_run(clean)
        a.metric("sweep_window_steps", width)
        a.metric("sweep_window", [sweep[start]["delay"], sweep[end]["delay"]] if width else None)
    chosen = d.get("chosen_delay")
    a.metric("chosen_delay", chosen)
    errs = d.get("errors_at_chosen", d.get("errors"))
    if errs is None:
        raise Inconclusive("agent reported no error count at the chosen delay")
    a.metric("clk_polarity", d.get("clk_polarity"))
    a.metric("errors_at_chosen", int(errs), max=0)
    return a.outcome(f"DAC PN via AD9361 loopback: {errs} errors at TX delay {chosen}")


@bench_test(
    "iface.tx_link", tier=0, units="any", maintenance=True,
    params={"sweep": True},
    description="DAC PN through AD9361 digital loopback; TX delay sweep; clock polarity.",
    pass_criteria="0 PN errors at the chosen TX delay",
    artifacts=("tx_link.json",), suites=("interface",), analyze=analyze_tx_link,
)
def iface_tx_link(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.save_json("tx_link.json", ctx.agent.txlink(name, "ad9361-loopback",
                                                   bool(ctx.params["sweep"])))
    return analyze_tx_link(ctx)


def analyze_fpga_loopback(a: AnalysisContext) -> Outcome:
    d = a.load_json("fpga_loopback.json")
    errs = d.get("errors")
    if errs is None:
        raise Inconclusive("agent reported no error count")
    a.metric("samples_checked", d.get("samples_checked"))
    a.metric("errors", int(errs), max=0)
    return a.outcome(f"FPGA DAC->ADC loopback: {errs} PN errors over "
                     f"{d.get('samples_checked', '?')} samples")


@bench_test(
    "iface.fpga_loopback", tier=0, units="any", maintenance=True,
    params={},
    description="FPGA-internal DAC->ADC loopback with PN.",
    pass_criteria="0 errors",
    artifacts=("fpga_loopback.json",), suites=("interface",), analyze=analyze_fpga_loopback,
)
def iface_fpga_loopback(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.save_json("fpga_loopback.json", ctx.agent.txlink(name, "fpga-loopback"))
    return analyze_fpga_loopback(ctx)
