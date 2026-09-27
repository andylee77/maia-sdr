"""sys.* tests: identity, PS audit, telemetry, soak, boot log."""

from __future__ import annotations

from typing import Any

import numpy as np

from ..analysis.bootlog import parse_boot_log
from ..analysis.periodicity import detect_periodicity
from ..config import normalize_dna
from ..errors import FbenchError, Inconclusive
from ..regmaps import check_expected, load_regmaps
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..units import detect_image, firmware_family, info_dna, serial_hint

SD_BOOT_MARKER = "uio_pdrv_genirq.of_id"
FACTORY_NOTE = ("factory firmware: Tier 0 read-only tests over libiio only; no agent unless "
                "SSH works")

#: XADC rails: nominal volts (VCCO_DDR is DDR3L 1.35 V on both boards).
RAIL_NOMINAL = {"vccint": 1.0, "vccaux": 1.8, "vccbram": 1.0, "vccpint": 1.0, "vccpaux": 1.8,
                "vccoddr": 1.35}


def _try(fn: Any, *a: Any, **k: Any) -> tuple[Any, str | None]:
    try:
        return fn(*a, **k), None
    except FbenchError as exc:
        return None, exc.message


# ---------------------------------------------------------------------------
# sys.identity
# ---------------------------------------------------------------------------


def analyze_identity(a: AnalysisContext) -> Outcome:
    d = a.load_json("identity.json")
    iio, info, http_sys = d.get("iio") or {}, d.get("agent_info") or {}, d.get("http_system")
    cfg = d.get("config") or {}
    family = firmware_family(iio, info)
    image = detect_image(iio, info, http_sys)
    serial = iio.get("hw_serial") or info.get("serial")
    hint = d.get("serial_hint") or {}
    a.metric("identity_readable", bool(serial or info), eq=True)
    a.metric("serial", serial)
    a.metric("serial_known", bool(hint.get("known")))
    for w in hint.get("warnings", []):
        a.warn(w)
    a.metric("firmware_family", family)
    a.metric("image", image)
    a.metric("hw_model", iio.get("hw_model") or info.get("model"))
    a.metric("fw_version", iio.get("fw_version") or info.get("fw_version"))
    a.metric("agent_version", d.get("agent_version"))
    bit = info.get("bitstream") or {}
    a.metric("bitstream_product_id", bit.get("product_id"))
    a.metric("p25_build", (http_sys or {}).get("build") if isinstance(http_sys, dict) else None)
    a.metric("boot_medium", info.get("boot_medium"))
    cmdline = info.get("cmdline")
    if cmdline is not None:
        sd_boot = SD_BOOT_MARKER in str(cmdline)
        a.metric("sd_boot", sd_boot)
        if not sd_boot:
            a.warn("cmdline lacks the SD-boot marker uio_pdrv_genirq.of_id: `fbench boot` "
                   "image swaps will not take effect")
    sd = info.get("sd") or {}
    if sd:
        a.metric("sd_free_mb", sd.get("free_mb"))
    a.metric("kernel", info.get("kernel"))
    expected = cfg.get("image", "unknown")
    if expected != "unknown":
        # The image follows the SD card inserted, so a mismatch only warns; the
        # board identity itself is the DNA (checked below).
        a.metric("image_matches_config", image == expected, eq=True, severity="warn")
    dna = info_dna(info) or d.get("dna")
    if dna:
        a.metric("fpga_dna", normalize_dna(dna))
        if cfg.get("fpga_dna"):
            a.metric("dna_matches_config", normalize_dna(dna) == normalize_dna(cfg["fpga_dna"]),
                     eq=True)
    if family == "factory":
        a.warn(FACTORY_NOTE)
    who = f"{d.get('unit')} ({cfg.get('label') or '-'}, {cfg.get('transceiver')})"
    return a.outcome(f"{who}: {family} firmware, image {image}, serial {serial} "
                     f"({'known' if hint.get('known') else 'UNKNOWN'} card), "
                     f"agent {d.get('agent_version') or 'absent'}")


@bench_test(
    "sys.identity", tier=0, units="any",
    params={"http_probe": True},
    description="Serial (SD-card hint), firmware family, image, bitstream ID, boot medium, "
                "SD layout, agent version, PL DNA when exposed.",
    pass_criteria="identity readable; image (and DNA when configured) match the config; an "
                  "unknown serial is only a warning",
    artifacts=("identity.json",), suites=("smoke",), analyze=analyze_identity,
)
def sys_identity(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    unit = ctx.cfg.unit(name)
    attrs, iio_err = _try(ctx.services.iio(name).context_attrs, 10.0)
    caps = ctx.caps(name)
    info = caps.get("info")
    ver = None
    if caps["agent"]:
        ver, _ = _try(ctx.agent.version, name)
    http_sys = None
    if ctx.params["http_probe"] and caps["image"] in ("p25", "unknown"):
        http_sys, _ = _try(ctx.http(name).get_json, "/api/system", None, 3.0)
    dna = info_dna(info)
    if dna is None and caps["agent"] and caps["image"] == "hwval":
        hid, _ = _try(ctx.agent.hwval, name, "id")
        dna = (hid or {}).get("fpga_dna") or (hid or {}).get("dna")
    serial = (attrs or {}).get("hw_serial") or (info or {}).get("serial")
    ctx.save_json("identity.json", {
        "unit": name,
        "config": {"label": unit.label, "transceiver": unit.transceiver, "image": unit.image,
                   "known_serials": unit.known_serials, "fpga_dna": unit.fpga_dna},
        "iio": attrs, "iio_error": iio_err, "agent_info": info,
        "agent_version": (ver or {}).get("version") if ver else None,
        "agent_error": caps.get("agent_error"), "http_system": http_sys, "dna": dna,
        "serial_hint": serial_hint(ctx.cfg, unit, serial),
    })
    if not attrs and not info:
        from ..errors import PreconditionError

        raise PreconditionError(f"unit {name} unreachable: IIO ({iio_err}) and agent "
                                f"({caps.get('agent_error')}) both failed")
    return analyze_identity(ctx)


# ---------------------------------------------------------------------------
# sys.audit
# ---------------------------------------------------------------------------


def ddr_timing(regs: dict[str, int], ps_clk_hz: float) -> dict[str, Any]:
    """DDR clock and CAS latency from live SLCR/DDRC values."""
    out: dict[str, Any] = {}
    pll = regs.get("DDR_PLL_CTRL")
    clk = regs.get("DDR_CLK_CTRL")
    mr = regs.get("DRAM_EMR_MR", regs.get("DRAM_EMR_MR_REG"))
    if pll is not None:
        out["fdiv"] = (pll >> 12) & 0x7F
    if pll is not None and clk is not None:
        div3x = (clk >> 20) & 0x3F
        if div3x:
            out["ddr_clk_hz"] = ps_clk_hz * out["fdiv"] / div3x
    if mr is not None:
        mr0 = mr & 0xFFFF
        a6a4 = (mr0 >> 4) & 0x7
        a2 = (mr0 >> 2) & 0x1
        out["cl"] = (12 if a2 else 4) + a6a4
    if "cl" in out and out.get("ddr_clk_hz"):
        out["taa_ns"] = out["cl"] / out["ddr_clk_hz"] * 1e9
    return out


def analyze_audit(a: AnalysisContext) -> Outcome:
    audit = a.load_json("audit.json")
    ucfg = a.load_json("unit_config.json")
    regs_doc = a.load_json("ps_regs_live.json")
    regs = {k: int(v) for k, v in (regs_doc.get("values") or {}).items()}
    expected = regs_doc.get("expected") or {}
    bad: list[str] = []
    for chk in audit.get("checks", []) or []:
        if chk.get("ok") is False:
            text = (f"agent check {chk.get('name')}: value {chk.get('value')} expected "
                    f"{chk.get('expected')} {chk.get('detail') or ''}").strip()
            if str(chk.get("severity", "fail")).lower() == "fail":
                bad.append(text)
            else:
                a.warn(text)
    host_bad = []
    for name, exp in expected.items():
        if name in regs:
            ok = check_expected(exp, regs[name])
            if ok is False:
                host_bad.append(f"{name}=0x{regs[name]:08X} expected {exp}")
    bad += host_bad
    a.metric("agent_checks", len(audit.get("checks", []) or []))
    a.metric("host_expected_checks", sum(1 for n in expected if n in regs))
    a.metric("mismatches", len(bad), max=0)
    t = ddr_timing(regs, float(ucfg.get("ps_clk_hz", 33_333_333.0)))
    derived = audit.get("derived") or {}
    if "ddr_clk_hz" not in t and derived.get("ddr_mhz"):
        t["ddr_clk_hz"] = float(derived["ddr_mhz"]) * 1e6
    if "cl" not in t and derived.get("cas_latency"):
        t["cl"] = int(derived["cas_latency"])
    if "taa_ns" not in t and derived.get("taa_ns"):
        t["taa_ns"] = float(derived["taa_ns"])
    if derived.get("taa_ns") and t.get("taa_ns") and \
            abs(float(derived["taa_ns"]) - t["taa_ns"]) > 0.05:
        a.warn(f"host tAA {t['taa_ns']:.3f} ns differs from the agent's {derived['taa_ns']} ns")
    if "fdiv" in t:
        a.metric("ddr_pll_fdiv", t["fdiv"])
    if "ddr_clk_hz" in t:
        a.metric("ddr_clk_mhz", round(t["ddr_clk_hz"] / 1e6, 3))
    if "cl" in t:
        a.metric("ddr_cl", t["cl"])
    if "taa_ns" in t:
        taa_min = float(ucfg.get("dram_taa_min_ns", 13.125))
        ok = a.metric("ddr_taa_ns", round(t["taa_ns"], 3), min=taa_min - 0.005)
        if not ok:
            bad.append(f"tAA {t['taa_ns']:.2f} ns < {taa_min} ns (CL{t['cl']} at "
                       f"{t['ddr_clk_hz'] / 1e6:.0f} MHz): overclock FSBL — invalid for "
                       "measurement runs")
    if t.get("fdiv") == 0x24:
        a.warn("DDR_PLL FDIV 36 = 600 MHz overclock FSBL: CL7 gives tAA 11.7 ns, below the "
               "MT41K256M16TW-107 minimum 13.125 ns — FAIL for measurement runs")
    ddr_ctrl = regs.get("DDRIOB_DDR_CTRL")
    if ddr_ctrl is not None:
        sel = (ddr_ctrl >> 1) & 0xF
        a.metric("ddriob_vref_sel", sel)
        a.metric("ddriob_iostd", {1: "LPDDR2 0.6 V", 2: "SSTL135 0.675 V (DDR3L)",
                                  4: "SSTL15 0.75 V (DDR3)", 8: "SSTL18 0.9 V"}.get(sel,
                                                                                  "unknown"))
    if "REBOOT_STATUS" in regs:
        a.metric("reboot_status", f"0x{regs['REBOOT_STATUS']:08X}")
    tel = a.load_json("telemetry.json") if a.has_artifact("telemetry.json") else None
    vddr = (audit.get("xadc") or {}).get("vccoddr")
    if vddr is None and tel:
        vals = [s.get("xadc", {}).get("vccoddr") for s in tel.get("samples", []) or []]
        vals = [v for v in vals if v is not None]
        vddr = float(np.mean(vals)) if vals else None
    if vddr is not None:
        if not a.metric("vccoddr_v", round(vddr, 4), min=1.35 * 0.95, max=1.35 * 1.05):
            bad.append(f"VCCO_DDR {vddr:.3f} V outside 1.35 V ±5 % (DDR3L)")
    else:
        a.warn("VCCO_DDR not measured (no telemetry)")
    for key in ("kmod", "services", "reserved_memory"):
        if audit.get(key) is not None:
            a.metric(key, audit.get(key))
    a.save_json("audit_findings.json", {"mismatches": bad})
    if bad:
        return a.outcome(f"{len(bad)} audit mismatch(es): " + "; ".join(bad[:4]))
    return a.outcome(f"PS configuration matches expectations "
                     f"(DDR {a.metrics.get('ddr_clk_mhz', '?')} MHz CL{a.metrics.get('ddr_cl', '?')}"
                     f", tAA {a.metrics.get('ddr_taa_ns', '?')} ns)")


@bench_test(
    "sys.audit", tier=0, units="any",
    params={"telemetry_check": True},
    description="PLLs, DDR mode/timing (tAA from live FDIV/CL), DDRC priorities, DDR IOB "
                "standard, PL310 prefetch, reserved memory vs /proc/iomem, kmod provenance, "
                "leftover services, reboot status, VCCO_DDR.",
    pass_criteria="live values match the expected table; tAA >= 13.125 ns; VCCO_DDR 1.35 V ±5 %",
    artifacts=("audit.json", "ps_regs_live.json", "telemetry.json"), suites=("smoke",),
    analyze=analyze_audit,
)
def sys_audit(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    unit = ctx.cfg.unit(name)
    ctx.require_agent(name)
    audit = ctx.agent.audit(name)
    ctx.save_json("audit.json", audit)
    maps = load_regmaps(ctx.cfg.paths.share_dir)
    values: dict[str, int] = {}
    for k, v in (audit.get("regs") or {}).items():
        try:
            values[k] = int(str(v), 0) if not isinstance(v, int) else v
        except ValueError:
            continue
    for core in ("slcr", "ddrc", "l2c"):
        if core in maps and not all(r in values for r in maps[core]):
            dump, err = _try(ctx.agent.reg_dump, name, core)
            if dump:
                values.update({k: v for k, v in dump.items() if k not in values})
            elif err:
                ctx.warn(f"reg dump {core}: {err}")
    # Registers may be reported under the agent's or the host's name: make every
    # alias resolve to the same value.
    for core in ("slcr", "ddrc", "l2c"):
        for r in maps.get(core, {}).values():
            names = (r.name, *r.aliases, f"{core.upper()}_{r.name}")
            got = next((values[n] for n in names if n in values), None)
            if got is not None:
                for n in names:
                    values.setdefault(n, got)
    expected = {r.name: r.expected for core in ("slcr", "ddrc", "l2c") if core in maps
                for r in maps[core].values() if r.expected is not None}
    ctx.save_json("ps_regs_live.json", {"values": values, "expected": expected})
    ctx.save_json("unit_config.json", {"ps_clk_hz": unit.ps_clk_hz,
                                       "dram_taa_min_ns": unit.dram_taa_min_ns,
                                       "dram_part": unit.dram_part})
    if ctx.params["telemetry_check"]:
        tel, err = _try(ctx.agent.telemetry, name, 1, 500)
        if tel:
            ctx.save_json("telemetry.json", tel)
        else:
            ctx.warn(f"telemetry for VCCO_DDR failed: {err}")
    return analyze_audit(ctx)


# ---------------------------------------------------------------------------
# sys.telemetry
# ---------------------------------------------------------------------------


def _samples(tel: dict[str, Any]) -> list[dict[str, Any]]:
    return [s for s in tel.get("samples", []) or [] if isinstance(s, dict)]


def _plot_telemetry(a: AnalysisContext, samples: list[dict[str, Any]]) -> None:
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:  # pragma: no cover
        return
    t = [s.get("t", i) for i, s in enumerate(samples)]
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(8, 5), sharex=True)
    ax1.plot(t, [s.get("xadc", {}).get("temp_c") for s in samples], label="XADC")
    ax1.plot(t, [s.get("ad9361_temp_c") for s in samples], label="AD9361")
    ax1.set_ylabel("°C")
    ax1.legend()
    for rail, nom in RAIL_NOMINAL.items():
        ax2.plot(t, [(s.get("xadc", {}).get(rail) or np.nan) / nom * 100 - 100 for s in samples],
                 label=rail)
    ax2.set_ylabel("rail deviation %")
    ax2.set_xlabel("s")
    ax2.legend(fontsize=7, ncol=3)
    fig.tight_layout()
    fig.savefig(a.artifact_path("telemetry.png"), dpi=100)
    plt.close(fig)


def analyze_telemetry(a: AnalysisContext) -> Outcome:
    tel = a.load_json("telemetry.json")
    samples = _samples(tel)
    if not samples:
        raise Inconclusive("telemetry returned no samples")
    tol = float(a.params["rail_tol_pct"])
    worst = 0.0
    for rail, nom in RAIL_NOMINAL.items():
        vals = [s.get("xadc", {}).get(rail) for s in samples]
        vals = [float(v) for v in vals if v is not None]
        if not vals:
            continue
        dev = max(abs(min(vals) / nom - 1), abs(max(vals) / nom - 1)) * 100
        worst = max(worst, dev)
        a.metric(f"{rail}_v_mean", round(float(np.mean(vals)), 4))
        a.metric(f"{rail}_dev_pct", round(dev, 3), max=tol)
    temps = [s.get("xadc", {}).get("temp_c") for s in samples]
    temps = [t for t in temps if t is not None]
    if temps:
        a.metric("xadc_temp_max_c", round(max(temps), 2), max=float(a.params["temp_max_c"]))
    ad = [s.get("ad9361_temp_c") for s in samples if s.get("ad9361_temp_c") is not None]
    if ad:
        a.metric("ad9361_temp_max_c", round(max(ad), 2), max=float(a.params["temp_max_c"]))
    clk = [s.get("clk_freq_hz") for s in samples if s.get("clk_freq_hz") is not None]
    if clk:
        a.metric("clk_freq_hz_mean", float(np.mean(clk)))
        a.metric("clk_freq_hz_span", float(np.ptp(clk)))
    mem = [s.get("mem_available_kb") for s in samples if s.get("mem_available_kb") is not None]
    if mem:
        a.metric("mem_available_kb_min", min(mem))
    irq_tot: dict[str, float] = {}
    for s in samples:
        for k, v in (s.get("irq_deltas") or {}).items():
            irq_tot[k] = irq_tot.get(k, 0.0) + float(v)
    if irq_tot:
        span = max(1e-9, (samples[-1].get("t", len(samples)) - samples[0].get("t", 0)) or
                   len(samples))
        a.metric("irq_rates_per_s", {k: round(v / span, 2) for k, v in irq_tot.items()})
    a.metric("samples", len(samples))
    _plot_telemetry(a, samples)
    return a.outcome(f"{len(samples)} samples; worst rail deviation {worst:.2f} %, XADC max "
                     f"{max(temps) if temps else '?'} °C, AD9361 max {max(ad) if ad else '?'} °C")


@bench_test(
    "sys.telemetry", tier=0, units="any",
    params={"seconds": 30, "interval_ms": 1000, "rail_tol_pct": 5.0, "temp_max_c": 85.0},
    description="XADC temperature/rails (VCCO_DDR nominal 1.35 V), AD9361 temperature, "
                "CLK_FREQ, IRQ/softirq rates, MemAvailable.",
    pass_criteria="rails within ±5 %, temperatures < 85 °C",
    artifacts=("telemetry.json", "telemetry.png"), suites=("smoke",),
    analyze=analyze_telemetry, duration_param="seconds",
)
def sys_telemetry(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    tel = ctx.agent.telemetry(name, float(ctx.params["seconds"]), int(ctx.params["interval_ms"]))
    ctx.save_json("telemetry.json", tel)
    return analyze_telemetry(ctx)


# ---------------------------------------------------------------------------
# sys.soak
# ---------------------------------------------------------------------------


def soak_events(samples: list[dict[str, Any]], explicit: list[dict[str, Any]],
                dip_frac: float = 0.5, min_rate: float = 10.0) -> list[dict[str, Any]]:
    """Agent events plus host-derived IRQ-rate dips (rate < dip_frac x median)."""
    events = [dict(e, kind=e.get("kind") or e.get("event") or "event") for e in explicit
              if "t" in e]
    # The agent reports only non-zero IRQ deltas: a missing IRQ in a sample is 0.
    names = {str(k) for s in samples for k in (s.get("irq_deltas") or {})}
    series: dict[str, list[tuple[float, float]]] = {n: [] for n in names}
    for i, s in enumerate(samples):
        t = float(s.get("t", i))
        deltas = {str(k): float(v) for k, v in (s.get("irq_deltas") or {}).items()}
        for n in names:
            series[n].append((t, deltas.get(n, 0.0)))
    for irq, pts in series.items():
        vals = np.array([v for _, v in pts])
        med = float(np.median(vals)) if len(vals) else 0.0
        if med < min_rate:
            continue
        for t, v in pts:
            if v < dip_frac * med:
                events.append({"t": t, "kind": f"irq_dip:{irq}", "value": v, "median": med})
    return sorted(events, key=lambda e: e["t"])


def analyze_soak(a: AnalysisContext) -> Outcome:
    import json as _json

    samples: list[dict[str, Any]] = []
    explicit: list[dict[str, Any]] = []
    path = a.artifacts_dir / "telemetry.jsonl"
    if path.exists():
        a.artifact_path("telemetry.jsonl")
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                obj = _json.loads(line)
            except _json.JSONDecodeError:
                continue
            (explicit if "event" in obj else samples).append(obj)
    reply = a.load_json("soak_reply.json") if a.has_artifact("soak_reply.json") else {}
    if not samples:  # with --jsonl the reply only keeps the last samples
        samples = _samples(reply)
    explicit += list(reply.get("events", []) or [])
    if not samples and not explicit:
        raise Inconclusive("soak produced no telemetry")
    events = soak_events(samples, explicit)
    a.save_json("events.json", events)
    kinds: dict[str, list[float]] = {}
    for e in events:
        kinds.setdefault(e["kind"], []).append(float(e["t"]))
    periodic = []
    per_kind = {}
    for kind, times in kinds.items():
        res = detect_periodicity(times, min_events=int(a.params["min_events"]))
        per_kind[kind] = {"count": len(times), **res.to_dict()}
        if res.periodic:
            periodic.append({"kind": kind, "period_s": res.period_s, "count": len(times)})
    all_times = [float(e["t"]) for e in events]
    overall = detect_periodicity(all_times, min_events=int(a.params["min_events"]))
    a.save_json("periodicity.json", {"per_kind": per_kind, "overall": overall.to_dict()})
    a.metric("samples", len(samples))
    a.metric("events", len(events))
    a.metric("event_kinds", {k: len(v) for k, v in kinds.items()})
    a.metric("periodic_kinds", periodic)
    a.metric("periodic_event_kinds", len(periodic), max=0)
    temps = [s.get("xadc", {}).get("temp_c") for s in samples if s.get("xadc")]
    temps = [t for t in temps if t is not None]
    if temps:
        a.metric("xadc_temp_max_c", max(temps))
    if periodic:
        desc = ", ".join(f"{p['kind']} every {p['period_s']:.2f} s ({p['count']}x)"
                         for p in periodic)
        return a.outcome(f"periodic events found: {desc}")
    return a.outcome(f"{len(events)} events over {len(samples)} samples; no periodic pattern")


@bench_test(
    "sys.soak", tier=0, units="any",
    params={"seconds": 3600, "interval_ms": 1000, "min_events": 5},
    description="Telemetry + error events over hours; periodicity search (e.g. ~10 s dropout).",
    pass_criteria="no unexplained periodic events",
    artifacts=("telemetry.jsonl", "events.json", "periodicity.json"), suites=("soak",),
    analyze=analyze_soak, duration_param="seconds",
)
def sys_soak(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    remote = f"{ctx.remote_run_dir(name)}/telemetry.jsonl"
    reply = ctx.agent.telemetry(name, float(ctx.params["seconds"]),
                                int(ctx.params["interval_ms"]), jsonl=remote)
    ctx.save_json("soak_reply.json", reply)
    ctx.pull(name, str(reply.get("jsonl", remote)), "telemetry.jsonl")
    return analyze_soak(ctx)


# ---------------------------------------------------------------------------
# sys.boot_log (UART console)
# ---------------------------------------------------------------------------


def analyze_boot_log(a: AnalysisContext) -> Outcome:
    path = a.artifacts_dir / "console.log"
    if not path.exists():
        raise Inconclusive("console.log missing")
    a.artifact_path("console.log")
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    info = parse_boot_log(lines)
    a.save_json("boot_log.json", info)
    if info["lines"] == 0:
        raise Inconclusive("no console output: power-cycle the unit during the capture window "
                           "(and check the DEBUG USB port / --port)")
    for key in ("uboot_version", "kernel_version", "firmware_family", "reached_login"):
        a.metric(key, info[key])
    for key, n in info["counts"].items():
        a.metric(f"count_{key}", n)
    a.metric("lines", info["lines"])
    a.metric("kernel_panics", info["counts"]["kernel_panic"], max=0)
    a.metric("reached_login", info["reached_login"], eq=True)
    for key in ("unable_to", "failed", "watchdog_reset", "usb_gadget"):
        for ex in info["examples"][key][:5]:
            a.warnings.append(f"{key} (line {ex['line']}): {ex['text']}")
    state = "reached login" if info["reached_login"] else "did NOT reach login"
    return a.outcome(f"boot {state}; U-Boot {info['uboot_version']}, Linux "
                     f"{info['kernel_version']}, {info['firmware_family']} firmware, "
                     f"{info['counts']['kernel_panic']} panic(s), {info['problems']} problem lines")


@bench_test(
    "sys.boot_log", tier=0, units="any",
    params={"port": "", "baud": 115200, "seconds": 120.0, "until": "", "send": ""},
    description="Record the full boot on the FT2232 DEBUG UART (power-cycle the unit during "
                "the window); extract U-Boot/kernel versions, panics, 'Unable to'/'failed' "
                "lines, RNDIS/g_ether/udc, iiod and p25-httpd start, watchdog reset marker.",
    pass_criteria="boot reaches a login prompt with no kernel panic",
    artifacts=("console.log", "boot_log.json"), requires=("pyserial", "console"),
    analyze=analyze_boot_log, duration_param="seconds",
)
def sys_boot_log(ctx: TestContext) -> Outcome:
    from ..console import capture, resolve_port

    name = ctx.roles["dut"]
    unit = ctx.cfg.unit(name)
    port = resolve_port(ctx.params["port"] or None, unit.console_port or None,
                        ctx.services.list_serial_ports)
    ctx.log.info("console capture on %s for %s s", port, ctx.params["seconds"])
    rep = capture(port, int(ctx.params["baud"] or unit.console_baud),
                  float(ctx.params["seconds"]), ctx.artifact_path("console.log"),
                  until=ctx.params["until"] or None, send=ctx.params["send"] or None,
                  serial_factory=ctx.services.serial_open, clock=ctx.services.monotonic)
    ctx.metric("port", rep["port"])
    return analyze_boot_log(ctx)
