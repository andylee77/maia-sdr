"""hw.* tests (Tier 1, hwval image): drivers around ``fbench-agent hwval …``.

Agent ops used (bench/agent/src/cmd/hwval.rs): ``id``, ``census --gate-ms``,
``ingest``, ``ringv2 setup|stop``, ``legacy setup|stop``, ``mt run``,
``evt enable|drain|disable``, ``contention``; ring data is verified with
``ring check --ring hwval-v2|hwval-legacy``. All ops require the hwval
bitstream (ID 0x68777631 "hwv1"). Analyses accept both the nested reply
shapes of the agent and flat legacy fixtures where cheap.
"""

from __future__ import annotations

from typing import Any

from ..analysis.hwref import rate_inc
from ..analysis.memtest import (
    CYCLE_NS,
    MODE_NAMES,
    PATTERN_NAMES,
    bandwidth_mbs,
    decode_first_error,
    hist_percentile_ns,
    idle_cycles_for_duty,
)
from ..analysis.ring import summarise
from ..errors import FbenchError, Inconclusive
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..units import HWVAL_ID

DOMAINS = ("sync", "mem", "sampling")
#: axi_memtest mode/pattern numbers -> agent option names.
MT_MODE_ARG = {0: "write-only", 1: "read-verify", 2: "write-verify", 3: "read-only",
               4: "byte-lane"}
MT_PATTERN_ARG = {0: "address", 1: "walking1", 2: "walking0", 3: "checkerboard", 4: "prbs",
                  5: "zeros", 6: "ones", 7: "toggle"}
#: Legacy ring write-response latency clock (62.5 MHz sync domain) and the
#: simulated loss cliff (~1040 cycles, 16.6 us of WLAST->B latency).
LEGACY_CYCLE_NS = 16.0
LEGACY_CLIFF_CYCLES = 1040
LEGACY_B_CAP = 5


def _hw(ctx: TestContext, op: str, args: list[Any] | None = None, timeout: float = 120.0
        ) -> dict[str, Any]:
    return ctx.agent.hwval(ctx.roles["dut"], op, [str(a) for a in (args or [])], timeout)


def _require_hwval(ctx: TestContext) -> str:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.require_image(name, "hwval")
    return name


def _int(v: Any, default: int = 0) -> int:
    if v is None:
        return default
    if isinstance(v, bool):
        return int(v)
    if isinstance(v, (int, float)):
        return int(v)
    return int(str(v), 0)


class _Spawned:
    """Background aggressors (agent processes) killed on exit."""

    def __init__(self) -> None:
        self.procs: list[Any] = []

    def add(self, p: Any) -> None:
        self.procs.append(p)

    def stop(self) -> None:
        for p in self.procs:
            try:
                p.kill()
            except Exception:  # noqa: BLE001
                pass
        self.procs.clear()


def _start_aggressors(ctx: TestContext, unit: str, names: list[str], seconds: float,
                      spawned: _Spawned) -> None:
    for agg in names:
        if agg == "sd":
            spawned.add(ctx.agent.spawn(unit, ["sd", "bench", "--mb",
                                               str(int(max(64, seconds * 10))), "--bs", "1024"]))
        elif agg in ("mem", "ps"):
            spawned.add(ctx.agent.spawn(unit, ["mem", "bw", "--size", "64M"]))
        elif agg == "iio":
            cmd = (f"timeout {int(seconds) + 5} iio_readdev -u local: -b 1048576 "
                   f"{ctx.cfg.iio.rx_device} voltage0 voltage1 > /dev/null")
            spawned.add(ctx.services.ssh(unit).spawn(cmd))


# ---------------------------------------------------------------------------
# hw.id
# ---------------------------------------------------------------------------


def analyze_id(a: AnalysisContext) -> Outcome:
    d = a.load_json("hw_id.json")
    ident = _int(d.get("id"))
    a.metric("id", f"0x{ident:08X}")
    a.metric("id_ok", ident == HWVAL_ID, eq=True)
    a.metric("version", d.get("version"))
    a.metric("features", d.get("features"))
    dna = d.get("fpga_dna") or d.get("dna")
    if dna:
        a.metric("fpga_dna", dna)
    snap = d.get("snapshot") or {}
    if isinstance(snap, dict) and "dead_domains" in snap:
        dead = list(snap.get("dead_domains") or [])
    elif isinstance(snap, dict) and all(k in snap for k in DOMAINS):
        dead = [k for k in DOMAINS if not snap.get(k)]
    else:
        ack = _int(d.get("snap_ack", snap.get("ack") if isinstance(snap, dict) else 0))
        dead = [k for i, k in enumerate(DOMAINS) if not ack >> i & 1]
    if d.get("all_clocks_alive") is False and not dead:
        dead = ["unknown"]
    a.metric("dead_domains", dead)
    a.metric("dead_domain_count", len(dead), max=0)
    return a.outcome(f"hwval ID 0x{ident:08X} v{d.get('version')}; clocks "
                     f"{'all alive' if not dead else 'DEAD: ' + ', '.join(map(str, dead))}")


@bench_test(
    "hw.id", tier=1, units="any",
    params={},
    description="hwval ID/version/features/DNA and a snapshot of all clock domains.",
    pass_criteria="ID = 0x68777631 and all clocks alive (snapshot acked)",
    artifacts=("hw_id.json",), suites=("hwval",), analyze=analyze_id,
)
def hw_id(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    ctx.save_json("hw_id.json", _hw(ctx, "id"))
    return analyze_id(ctx)


# ---------------------------------------------------------------------------
# hw.census
# ---------------------------------------------------------------------------

#: Expected frequency and allowed error (ppm) per census clock. PS-derived
#: clocks share the PS crystal with FCLK0 (the reference), so they are exact
#: to the count resolution; oscillator/AD9361-derived ones carry two crystal
#: tolerances. ``None`` = from the agent's nominal_hz / the sample rate.
CENSUS_EXPECTED: dict[str, tuple[float | None, float]] = {
    "sync": (62.5e6, 5.0), "mem": (125e6, 5.0), "clk3x": (187.5e6, 5.0),
    "fclk1": (200e6, 5.0), "y1": (50e6, 100.0), "sampling": (None, 100.0),
    "lclk": (None, 100.0), "clkout": (None, 0.0),
}


def _clock_hz(v: Any) -> tuple[float, float | None, bool]:
    """(hz, nominal_hz, alive) from an agent clock entry or a bare number."""
    if isinstance(v, dict):
        return float(v.get("hz") or 0.0), v.get("nominal_hz"), bool(v.get("alive", True))
    return float(v), None, float(v) > 0


def analyze_census(a: AnalysisContext) -> Outcome:
    d = a.load_json("census.json")
    samples = d.get("samples") or [d]
    fs = d.get("fs_hz") or next((s.get("ad9361_fs_hz") for s in samples
                                 if s.get("ad9361_fs_hz")), None)
    per: dict[str, list[float]] = {}
    nominal_agent: dict[str, float] = {}
    dead: set[str] = set()
    for s in samples:
        for clk, v in (s.get("clocks") or {}).items():
            hz, nom, alive = _clock_hz(v)
            per.setdefault(clk, []).append(hz)
            if nom:
                nominal_agent[clk] = float(nom)
            if not alive or hz <= 0:
                dead.add(clk)
    if not per:
        raise Inconclusive("census returned no clocks")
    exp_over = dict(a.params.get("expected_hz") or {})
    worst: dict[str, float] = {}
    for clk, vals in per.items():
        nominal, tol = CENSUS_EXPECTED.get(clk, (None, 0.0))
        if clk in exp_over:
            nominal = float(exp_over[clk])
        if nominal is None and clk == "sampling" and fs:
            nominal = float(fs)
        if nominal is None and clk == "lclk" and fs:
            nominal = float(fs) * float(a.params["lclk_mult"])
        if nominal is None:
            nominal = nominal_agent.get(clk)
        mean = sum(vals) / len(vals)
        a.metric(f"{clk}_hz", round(mean, 1))
        if nominal and mean:
            err_ppm = (mean / nominal - 1) * 1e6
            a.metric(f"{clk}_ppm", round(err_ppm, 3), min=-tol if tol else None,
                     max=tol if tol else None)
            worst[clk] = err_ppm
            if len(vals) > 1:
                a.metric(f"{clk}_drift_ppm", round((max(vals) - min(vals)) / nominal * 1e6, 3))
    if "sampling" in worst:
        a.metric("implied_ad9361_ref_ppm", round(worst["sampling"], 3))
    a.metric("dead_clocks", sorted(dead))
    a.metric("dead_clock_count", len(dead), max=0)
    txt = ", ".join(f"{k} {v:+.2f} ppm" for k, v in worst.items())
    return a.outcome(f"clock census over {len(samples)} gate(s): {txt}" +
                     (f"; DEAD: {', '.join(sorted(dead))}" if dead else ""))


@bench_test(
    "hw.census", tier=1, units="any",
    params={"gate_ms": 1000, "repeat": 5, "lclk_mult": 2.0, "expected_hz": {}},
    description="Frequency of every hwval clock vs FCLK0 (repeat gates for drift); implied "
                "AD9361 reference ppm.",
    pass_criteria="within expected ppm windows (PS clocks ±5 ppm, y1/sampling/lclk ±100 ppm)",
    artifacts=("census.json",), suites=("hwval",), analyze=analyze_census,
)
def hw_census(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    gate = int(ctx.params["gate_ms"])
    samples = []
    t0 = ctx.services.monotonic()
    for _ in range(max(1, int(ctx.params["repeat"]))):
        rep = _hw(ctx, "census", ["--gate-ms", gate], timeout=30 + gate / 1000 * 2)
        rep["t"] = ctx.services.monotonic() - t0
        samples.append(rep)
    fs = next((s.get("ad9361_fs_hz") for s in samples if s.get("ad9361_fs_hz")), None)
    ctx.save_json("census.json", {"samples": samples, "fs_hz": fs})
    return analyze_census(ctx)


# ---------------------------------------------------------------------------
# hw.ingest / hw.prbs_ber
# ---------------------------------------------------------------------------


def _bits(v: Any, width: int = 12) -> list[int]:
    if isinstance(v, list):
        return [int(x) for x in v]
    m = _int(v)
    return [b for b in range(width) if m >> b & 1]


def stuck_bits(d: dict[str, Any], width: int = 12) -> dict[str, dict[str, list[int]]]:
    """Stuck bits from the agent window (``*_stuck0/1``) or from OR/AND masks."""
    w = d.get("window") or {}
    if any(k in w for k in ("i_stuck0", "i_stuck1", "q_stuck0", "q_stuck1")):
        return {c: {"stuck0": _bits(w.get(f"{c}_stuck0"), width),
                    "stuck1": _bits(w.get(f"{c}_stuck1"), width)} for c in ("i", "q")}
    out = {}
    for c in ("i", "q"):
        orm, andm = _int(d.get(f"{c}_or_mask"), (1 << width) - 1), _int(d.get(f"{c}_and_mask"))
        out[c] = {"stuck0": [b for b in range(width) if not orm >> b & 1],
                  "stuck1": [b for b in range(width) if andm >> b & 1]}
    return out


def analyze_ingest(a: AnalysisContext) -> Outcome:
    d = a.load_json("ingest.json")
    w = d.get("window") or d
    samples = _int(d.get("samples", w.get("samples")))
    a.metric("samples", samples)
    a.metric("valid_gap_cycles", _int(d.get("valid_gap_cycles")))
    a.metric("valid_gap_runs", _int(d.get("valid_gap_runs")), max=0)
    a.metric("cdc_wrerr", _int(d.get("cdc_wrerr")), max=0)
    for k in ("i_min", "i_max", "q_min", "q_max", "clip_count", "i_rms", "q_rms"):
        if k in w:
            a.metric(k, w.get(k))
    sb = stuck_bits(d)
    a.metric("stuck_bits", sb)
    n_stuck = sum(len(v) for c in sb.values() for v in c.values())
    a.metric("stuck_bit_count", n_stuck, max=0)
    if not samples:
        raise Inconclusive("ingest counted no samples")
    return a.outcome(f"{samples} samples: {_int(d.get('valid_gap_runs'))} valid gaps, "
                     f"{_int(d.get('cdc_wrerr'))} CDC overflows, {n_stuck} stuck bits")


@bench_test(
    "hw.ingest", tier=1, units="any",
    params={"seconds": 10.0, "honor_valid": False},
    description="Ingest monitor: util_wfifo valid gaps (F7), CDC overflows, ADC min/max/rms, "
                "clipping, stuck bits.",
    pass_criteria="0 gaps, 0 overflows, no stuck bits",
    artifacts=("ingest.json",), suites=("hwval",), analyze=analyze_ingest,
    duration_param="seconds",
)
def hw_ingest(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    args: list[Any] = ["--seconds", ctx.params["seconds"], "--clear"]
    if ctx.params["honor_valid"]:
        args.append("--honor-valid")
    ctx.save_json("ingest.json", _hw(ctx, "ingest", args, float(ctx.params["seconds"]) + 60))
    return analyze_ingest(ctx)


def analyze_prbs_ber(a: AnalysisContext) -> Outcome:
    d = a.load_json("prbs_ber.json")
    p = d.get("prbs") or {"checked": d.get("prbs_checked"), "errors": d.get("prbs_errors"),
                          "oos_events": d.get("prbs_oos_events"),
                          "in_sync": d.get("prbs_in_sync", True)}
    checked = _int(p.get("checked"))
    errors = _int(p.get("errors"))
    bits = checked * int(a.params["bits_per_sample"])
    if not checked:
        raise Inconclusive("PRBS checker verified no samples (not in sync?)")
    a.metric("prbs_checked", checked)
    a.metric("bits_checked", bits)
    a.metric("prbs_oos_events", _int(p.get("oos_events")))
    a.metric("in_sync", bool(p.get("in_sync", True)), eq=True)
    a.metric("ber", errors / bits if bits else None)
    upper = 3.0 / bits if errors == 0 else (errors + 3 * errors ** 0.5) / bits
    a.metric("ber_upper_95", upper)
    a.metric("ber_1e12_demonstrated", upper < 1e-12)
    a.metric("prbs_errors", errors, max=0)
    return a.outcome(f"{bits:.3g} bits checked, {errors} errors (BER < {upper:.2g} at 95 %)")


@bench_test(
    "hw.prbs_ber", tier=1, units="any", maintenance=True,
    params={"seconds": 60.0, "prbs_mode": "ad9361", "bits_per_sample": 24},
    description="Per-sample PRBS BER in the ingest monitor with AD9361 BIST PRBS (the agent "
                "enables and restores the BIST: ingest --prbs MODE --bist).",
    pass_criteria="0 errors (BER < 1e-12 needs >= 3e12 bits)",
    artifacts=("prbs_ber.json",), suites=("hwval", "soak"), analyze=analyze_prbs_ber,
    duration_param="seconds",
)
def hw_prbs_ber(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    rep = _hw(ctx, "ingest", ["--seconds", ctx.params["seconds"], "--prbs",
                              ctx.params["prbs_mode"], "--bist", "--clear"],
              float(ctx.params["seconds"]) + 60)
    ctx.save_json("prbs_ber.json", rep)
    return analyze_prbs_ber(ctx)


# ---------------------------------------------------------------------------
# Ring sessions (ring v2 / legacy): setup -> ring check -> stop
# ---------------------------------------------------------------------------


def _ringv2_session(ctx: TestContext, src: str, rate_mbs: float, seconds: float,
                    pattern: str, setup_extra: list[Any] | None = None,
                    stall_ms: list[int] | None = None) -> dict[str, Any]:
    name = ctx.roles["dut"]
    setup = _hw(ctx, "ringv2", ["setup", "--src", src, "--rate-mbs", rate_mbs, "--clear",
                                "--enable", *(setup_extra or [])])
    try:
        check = ctx.agent.ring_check(name, "hwval-v2", pattern, seconds, stall_ms)
    finally:
        stop = _hw(ctx, "ringv2", ["stop"])
    return {"src": src, "rate_mbs": rate_mbs, "setup": setup, "check": check, "stop": stop}


def ringv2_loss(sess: dict[str, Any]) -> int:
    """Checker anomalies + lost units + producer drops for one ring v2 session."""
    chk = sess.get("check") or {}
    s = summarise(chk)
    acct = (sess.get("stop") or {}).get("accounting") or {}
    return (s["total"] + _int(chk.get("lost_units")) + _int(acct.get("drop_full"))
            + _int(acct.get("drop_protect")))


def analyze_ringv2_rate(a: AnalysisContext) -> Outcome:
    d = a.load_json("ringv2_rate.json")
    rows = [{"rate_mbs": float(r["rate_mbs"]), "loss": ringv2_loss(r)} for r in d["runs"]]
    a.metric("runs", rows)
    lossless = [r["rate_mbs"] for r in rows if r["loss"] == 0]
    a.metric("max_lossless_mbs", max(lossless) if lossless else 0.0)
    req = float(a.params["required_mbs"])
    at_req = sum(r["loss"] for r in rows if r["rate_mbs"] <= req)
    a.metric("loss_up_to_required", at_req, max=0)
    return a.outcome(f"ring v2 lossless up to {max(lossless) if lossless else 0:g} MB/s "
                     f"(required {req:g} MB/s)")


@bench_test(
    "hw.ringv2_rate", tier=1, units="any",
    params={"rates_mbs": [8.0, 16.0, 32.0, 64.0, 128.0, 250.0], "pattern": "ramp64",
            "seconds": 10.0, "required_mbs": 32.0},
    description="Ring v2 rate sweep (ringv2 setup --src PATTERN --rate-mbs R, ring check "
                "--ring hwval-v2, ringv2 stop): checker anomalies + DROP_FULL/DROP_PROTECT.",
    pass_criteria="0 loss up to the required rate (report the maximum lossless rate)",
    artifacts=("ringv2_rate.json",), suites=("hwval",), analyze=analyze_ringv2_rate,
    duration_param="seconds",
)
def hw_ringv2_rate(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    runs = [_ringv2_session(ctx, str(ctx.params["pattern"]), float(rate),
                            float(ctx.params["seconds"]), str(ctx.params["pattern"]))
            for rate in ctx.params["rates_mbs"]]
    ctx.save_json("ringv2_rate.json", {"runs": runs})
    return analyze_ringv2_rate(ctx)


def ringv2_assertions(steps: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
    """Protocol assertions from the host-composed ring v2 sessions."""
    out: list[dict[str, Any]] = []

    def add(name: str, ok: bool, detail: str) -> None:
        out.append({"name": name, "ok": bool(ok), "detail": detail})

    base = steps.get("baseline")
    if base:
        acct = (base.get("stop") or {}).get("accounting") or {}
        if all(k in acct for k in ("words_in", "data_words_written", "drop_full",
                                   "drop_protect")):
            lhs = _int(acct["words_in"])
            rhs = (_int(acct["data_words_written"]) + _int(acct["drop_full"])
                   + _int(acct["drop_protect"]))
            add("word_conservation", lhs == rhs,
                f"WORDS_IN {lhs} vs written+DROP_FULL+DROP_PROTECT {rhs}")
        add("drain_to_idle", bool((base.get("stop") or {}).get("drained_idle", acct.get("idle"))),
            "ring idle after stop")
        add("baseline_lossless", ringv2_loss(base) == 0, f"loss {ringv2_loss(base)}")
    hdr = steps.get("header")
    if hdr:
        n = _int((hdr.get("check") or {}).get("ringv2_headers"))
        add("header", n > 0, f"{n} sub-buffer headers seen")
    fl = steps.get("flush")
    if fl:
        n = _int((fl.get("check") or {}).get("ringv2_pads"))
        add("flush_pads", n > 0, f"{n} pad words (flush on timeout)")
    lap = steps.get("lap")
    if lap:
        s = summarise(lap.get("check") or {})
        detected = s["counts"].get("lap", 0) > 0 or _int((lap.get("check") or {})
                                                         .get("lost_units")) > 0
        add("lap_detection", detected, f"counts {s['counts']}")
    prot = steps.get("protect")
    if prot:
        acct = (prot.get("stop") or {}).get("accounting") or {}
        s = summarise(prot.get("check") or {})
        add("protect_mode", _int(acct.get("drop_protect")) > 0 and s["counts"].get("lap", 0) == 0,
            f"DROP_PROTECT {acct.get('drop_protect')}, laps {s['counts'].get('lap', 0)}")
    return out


def analyze_ringv2_protocol(a: AnalysisContext) -> Outcome:
    d = a.load_json("ringv2_protocol.json")
    asserts = ringv2_assertions(d.get("steps") or {}) + list(d.get("assertions") or [])
    if not asserts:
        raise Inconclusive("no protocol assertions could be evaluated")
    failed = [x for x in asserts if not x.get("ok")]
    regs = ((d.get("steps") or {}).get("baseline") or {}).get("setup", {}).get("regs") or {}
    size, mo = regs.get("RINGV2_SIZE_BURSTS"), regs.get("RINGV2_MAX_OUTSTANDING")
    if size is not None and mo is not None:
        a.metric("reader_safety_margin_bursts", _int(size) - 1 - _int(mo))
    a.metric("assertions", [{"name": x["name"], "ok": x["ok"]} for x in asserts])
    a.metric("failed_assertions", [x.get("name") for x in failed])
    a.metric("failed", len(failed), max=0)
    return a.outcome(f"{len(asserts) - len(failed)}/{len(asserts)} ring v2 protocol assertions "
                     "hold" + (": failed " + ", ".join(str(x.get("name")) for x in failed)
                               if failed else ""))


@bench_test(
    "hw.ringv2_protocol", tier=1, units="any",
    params={"seconds": 3.0, "rate_mbs": 32.0, "flush_rate_mbs": 0.01, "lap_stall_ms": 2000},
    description="Ring v2 protocol from agent primitives: word conservation after drain "
                "(WORDS_IN == written + DROP_FULL + DROP_PROTECT), drain to idle, headers, "
                "flush pads at low rate, lap detection (overwrite + stall), protect mode.",
    pass_criteria="all protocol assertions hold; reports the reader margin "
                  "N-1-max_outstanding",
    artifacts=("ringv2_protocol.json",), suites=("hwval",), analyze=analyze_ringv2_protocol,
)
def hw_ringv2_protocol(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    secs, rate = float(ctx.params["seconds"]), float(ctx.params["rate_mbs"])
    stall = [int(ctx.params["lap_stall_ms"])]
    steps = {
        "baseline": _ringv2_session(ctx, "tagged", rate, secs, "tagged"),
        "header": _ringv2_session(ctx, "tagged", rate, secs, "tagged", ["--header"]),
        "flush": _ringv2_session(ctx, "tagged", float(ctx.params["flush_rate_mbs"]), secs,
                                 "tagged"),
        "lap": _ringv2_session(ctx, "tagged", rate, secs + stall[0] / 1000 + 2, "tagged",
                               stall_ms=stall),
        "protect": _ringv2_session(ctx, "tagged", rate, secs + stall[0] / 1000 + 2, "tagged",
                                   ["--protect"], stall_ms=stall),
    }
    ctx.save_json("ringv2_protocol.json", {"steps": steps})
    return analyze_ringv2_protocol(ctx)


def legacy_loss(stop: dict[str, Any]) -> int | None:
    """Words lost = WORDS_IN - WORDS_ACCEPTED (agent ``loss_words`` or the regs)."""
    if stop.get("loss_words") is not None:
        return _int(stop["loss_words"])
    regs = stop.get("regs") or {}
    wi = regs.get("LEGACY_WORDS_IN", stop.get("words_in"))
    wa = regs.get("LEGACY_WORDS_ACCEPTED", stop.get("words_accepted"))
    if wi is None or wa is None:
        return None
    return _int(wi) - _int(wa)


def analyze_legacy(a: AnalysisContext) -> Outcome:
    d = a.load_json("legacy.json")
    stop = d.get("stop") or {}
    regs = stop.get("regs") or {}
    s = summarise(d.get("check") or {})
    a.metric("load", d.get("load"))
    if regs.get("LEGACY_RATE_INC") is not None and d.get("expected_rate_inc") is not None:
        a.metric("rate_inc_matches_reference",
                 _int(regs["LEGACY_RATE_INC"]) == _int(d["expected_rate_inc"]), eq=True)
    loss = legacy_loss(stop)
    if loss is None:
        raise Inconclusive("agent did not report LEGACY_WORDS_IN / LEGACY_WORDS_ACCEPTED")
    a.metric("words_in", _int(regs.get("LEGACY_WORDS_IN")))
    a.metric("words_lost", loss, max=1)  # +-1: the two counters are sampled with skew
    a.metric("packer_overflow_pulses_overstates_loss", _int(regs.get("LEGACY_PACKER_OVF")))
    a.metric("anomalies", s["total"], max=0)
    lat = regs.get("LEGACY_LAT_MAX")
    if stop.get("lat_max_us") is not None:
        a.metric("write_resp_lat_max_ns", float(stop["lat_max_us"]) * 1000)
    elif lat is not None:
        a.metric("write_resp_lat_max_ns", _int(lat) * LEGACY_CYCLE_NS)
    cliff_ns = float(stop.get("lat_cliff_us") or LEGACY_CLIFF_CYCLES * LEGACY_CYCLE_NS / 1000) \
        * 1000
    if "write_resp_lat_max_ns" in a.metrics:
        a.metric("latency_margin_ns", cliff_ns - a.metrics["write_resp_lat_max_ns"])
    mo = regs.get("LEGACY_MAX_OUTSTANDING")
    if mo is not None:
        a.metric("max_outstanding", _int(mo))
        if _int(mo) >= LEGACY_B_CAP:
            a.warn(f"LEGACY_MAX_OUTSTANDING = {mo}: the 5-burst B cap was hit")
    return a.outcome(f"legacy ring at {a.params['rate_msps']:g} MSPS-equivalent with load "
                     f"{d.get('load') or 'none'}: {loss} words lost, {s['total']} anomalies, "
                     f"latency margin {a.metrics.get('latency_margin_ns', '?')} ns to the "
                     f"{cliff_ns / 1000:.1f} us cliff")


@bench_test(
    "hw.legacy_ring", tier=1, units="any",
    params={"rate_msps": 8.0, "seconds": 60.0, "src": "iqramp", "pattern": "iqramp",
            "load": ["sd", "mem"]},
    description="Production-replica ring (IQPacker + DmaStreamRingWrite) at the 8 MSPS "
                "equivalent rate under PS/SD load (legacy setup, ring check --ring "
                "hwval-legacy, legacy stop).",
    pass_criteria="words lost (WORDS_IN - WORDS_ACCEPTED) <= 1, 0 anomalies; packer overflow "
                  "pulses and WLAST->B latency margin reported",
    artifacts=("legacy.json",), suites=("hwval",), analyze=analyze_legacy,
    duration_param="seconds",
)
def hw_legacy_ring(ctx: TestContext) -> Outcome:
    name = _require_hwval(ctx)
    secs = float(ctx.params["seconds"])
    spawned = _Spawned()
    setup = _hw(ctx, "legacy", ["setup", "--src", ctx.params["src"], "--rate-msps",
                                ctx.params["rate_msps"], "--clear", "--enable"])
    try:
        _start_aggressors(ctx, name, list(ctx.params["load"]), secs, spawned)
        check = ctx.agent.ring_check(name, "hwval-legacy", str(ctx.params["pattern"]), secs)
    finally:
        spawned.stop()
        stop = _hw(ctx, "legacy", ["stop"])
    ctx.save_json("legacy.json", {
        "load": list(ctx.params["load"]), "setup": setup, "check": check, "stop": stop,
        "expected_rate_inc": rate_inc(float(ctx.params["rate_msps"]) * 1e6)})
    return analyze_legacy(ctx)


# ---------------------------------------------------------------------------
# hw.memtest / hw.mem_bw
# ---------------------------------------------------------------------------


def _mt_args(mt: int, mode: int, pattern: int, passes: int, burst: int = 16,
             outstanding: int = 4, idle: int = 0, seed: int = 0, seconds: float | None = None
             ) -> list[Any]:
    args: list[Any] = ["run", "--mt", mt, "--mode", MT_MODE_ARG[mode], "--pattern",
                       MT_PATTERN_ARG[pattern], "--passes", passes, "--burst-len", burst,
                       "--outstanding", outstanding, "--idle", idle, "--seed", hex(seed)]
    if seconds is not None:
        args += ["--seconds", seconds]
    return args


def mt_norm(r: dict[str, Any]) -> dict[str, Any]:
    """Flatten an ``mt run`` reply (nested first_err, *_hist) for decoding."""
    fe = r.get("first_err") or {}
    out = dict(r)
    out.setdefault("first_err_addr", fe.get("addr"))
    out.setdefault("first_err_exp", fe.get("expected"))
    out.setdefault("first_err_act", fe.get("actual"))
    out.setdefault("first_err_pass", fe.get("pass"))
    out.setdefault("whist", r.get("wlat_hist"))
    out.setdefault("rhist", r.get("rlat_hist"))
    return out


def analyze_memtest(a: AnalysisContext) -> Outcome:
    d = a.load_json("memtest.json")
    total = blocked = resp = refused = 0
    decoded = []
    for raw in d["runs"]:
        run = mt_norm(raw)
        total += _int(run.get("err_count"))
        blocked += _int(run.get("guard_blocked"))
        resp += _int(run.get("bresp_err")) + _int(run.get("rresp_err"))
        refused += 1 if run.get("refused") else 0
        dec = decode_first_error(run)
        if dec:
            decoded.append({"tester": run.get("tester"),
                            "mode": MODE_NAMES.get(run.get("mode"), run.get("mode")),
                            "pattern": PATTERN_NAMES.get(run.get("pattern"), run.get("pattern")),
                            **dec})
            if dec.get("reference_agrees") is False:
                a.warn(f"mt{run.get('tester')}: agent expected value disagrees with the "
                       "axi_memtest reference (decode bug?)")
    a.save_json("memtest_errors.json", decoded)
    a.metric("runs", len(d["runs"]))
    a.metric("bytes_verified", sum(_int(r.get("bytes_rd")) for r in d["runs"]))
    a.metric("refused_starts", refused, max=0)
    a.metric("guard_blocked", blocked, max=0)
    a.metric("resp_errors", resp, max=0)
    a.metric("errors", total, max=0)
    lanes = sorted({ln for x in decoded for ln in x.get("byte_lanes", [])})
    dq = sorted({ln for x in decoded for ln in x.get("dq_lines", [])})
    a.metric("failing_byte_lanes", lanes)
    a.metric("failing_dq_lines", dq)
    return a.outcome(f"{len(d['runs'])} memtester runs over the hwval-memtest window: {total} "
                     f"errors" + (f" (DQ {dq})" if dq else "") +
                     (f", {blocked} guard-blocked starts" if blocked else ""))


@bench_test(
    "hw.memtest", tier=1, units="any",
    params={"testers": [0, 1], "patterns": [0, 1, 2, 3, 4, 5, 6, 7], "passes": 2,
            "mode": 2, "byte_lane": True, "seed": 305419896},
    description="mt0 (HP0) / mt1 (HP3) write-then-verify over the 64 MiB hwval-memtest window "
                "for every pattern, plus the byte-lane (DM) mode; first errors decoded to DQ "
                "lines with the axi_memtest reference.",
    pass_criteria="0 errors, 0 refused/guard-blocked starts, no BRESP/RRESP errors",
    artifacts=("memtest.json", "memtest_errors.json"), suites=("hwval",),
    analyze=analyze_memtest,
)
def hw_memtest(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    runs = []
    for tester in ctx.params["testers"]:
        for pat in ctx.params["patterns"]:
            rep = _hw(ctx, "mt", _mt_args(int(tester), int(ctx.params["mode"]), int(pat),
                                          int(ctx.params["passes"]),
                                          seed=int(ctx.params["seed"])), 300.0)
            runs.append({"tester": int(tester), "mode": int(ctx.params["mode"]),
                         "pattern": int(pat), "seed": int(ctx.params["seed"]), **rep})
        if ctx.params["byte_lane"]:
            rep = _hw(ctx, "mt", _mt_args(int(tester), 4, 0, 1), 300.0)
            runs.append({"tester": int(tester), "mode": 4, "pattern": 0, **rep})
    ctx.save_json("memtest.json", {"runs": runs})
    return analyze_memtest(ctx)


def analyze_mem_bw(a: AnalysisContext) -> Outcome:
    d = a.load_json("mem_bw_hw.json")
    rows = []
    for raw in d["runs"]:
        r = mt_norm(raw)
        wr = r["mode"] == 0
        bw = r.get("wr_mbs" if wr else "rd_mbs")
        if bw is None:
            bw = bandwidth_mbs(_int(r.get("bytes_wr") if wr else r.get("bytes_rd")),
                               _int(r.get("cycles")))
        lat_ns = r.get("wlat_max_ns" if wr else "rlat_max_ns")
        if lat_ns is None:
            lat_ns = _int(r.get("wlat_max_cycles", r.get("wlat_max")) if wr else
                          r.get("rlat_max_cycles", r.get("rlat_max"))) * CYCLE_NS
        hist = r.get("whist" if wr else "rhist") or []
        rows.append({"tester": r.get("tester"), "mode": "write" if wr else "read",
                     "burst_len": r["burst_len"], "outstanding": r["outstanding"],
                     "mbs": float(bw) if bw is not None else None, "lat_max_ns": float(lat_ns),
                     "lat_p99_ns": hist_percentile_ns(list(hist), 0.99)})
    a.save_json("mem_bw_table.json", rows)
    best = [x for x in rows if x["burst_len"] == 16 and x["outstanding"] >= 4 and x["mbs"]]
    floor = float(a.params["min_mbs"])
    worst_best = min((x["mbs"] for x in best), default=None)
    a.metric("bw_16beat_min_mbs", round(worst_best, 1) if worst_best else None, min=floor)
    wlat = max((x["lat_max_ns"] for x in rows if x["mode"] == "write"), default=None)
    a.metric("write_lat_max_ns", wlat, max=float(a.params["max_write_lat_ns"]))
    a.metric("table_rows", len(rows))
    return a.outcome(f"16-beat >=4-outstanding bandwidth min {worst_best or 0:.0f} MB/s "
                     f"(floor {floor:.0f}), idle write latency max {wlat or 0:.0f} ns")


@bench_test(
    "hw.mem_bw", tier=1, units="any",
    params={"testers": [0], "burst_lens": [1, 2, 4, 8, 16], "outstanding": [1, 2, 4, 8],
            "modes": [0, 3], "passes": 4, "min_mbs": 900.0, "max_write_lat_ns": 16000.0},
    description="Bandwidth and latency vs burst length x outstanding x mode (write-only / "
                "read-only, PRBS) on an idle system; simulated ceiling ~987 MB/s at 125 MHz.",
    pass_criteria="report; 16-beat bandwidth >= 0.9 x 1000 MB/s; write latency max < 16 us idle",
    artifacts=("mem_bw_hw.json", "mem_bw_table.json"), suites=("hwval",), analyze=analyze_mem_bw,
)
def hw_mem_bw(ctx: TestContext) -> Outcome:
    _require_hwval(ctx)
    runs = []
    for tester in ctx.params["testers"]:
        for mode in ctx.params["modes"]:
            for bl in ctx.params["burst_lens"]:
                for osd in ctx.params["outstanding"]:
                    rep = _hw(ctx, "mt", _mt_args(int(tester), int(mode), 4,
                                                  int(ctx.params["passes"]), int(bl), int(osd)),
                              300.0)
                    runs.append({"tester": int(tester), "mode": int(mode), "burst_len": int(bl),
                                 "outstanding": int(osd), **rep})
    ctx.save_json("mem_bw_hw.json", {"runs": runs})
    return analyze_mem_bw(ctx)


# ---------------------------------------------------------------------------
# hw.contention
# ---------------------------------------------------------------------------


def analyze_contention(a: AnalysisContext) -> Outcome:
    d = a.load_json("contention.json")
    cells = []
    for c in d["cells"]:
        if c.get("production_like"):
            loss = ringv2_loss(c["victim"])
            lat = ((c["victim"].get("stop") or {}).get("regs") or {}).get("RINGV2_LAT_MAX")
        else:
            rv = c["cell"].get("ringv2") or {}
            loss = _int(rv.get("drop_full_delta"))
            lat = rv.get("lat_max_cycles")
        cells.append({**c["aggressors"], "loss": loss, "production_like": c["production_like"],
                      "lat_max_ns": _int(lat) * LEGACY_CYCLE_NS if lat is not None else None})
    a.save_json("contention_matrix.json", cells)
    prod_loss = sum(x["loss"] for x in cells if x["production_like"])
    a.metric("cells", len(cells))
    a.metric("lossy_cells", [x for x in cells if x["loss"]])
    a.metric("production_like_loss", prod_loss, max=0)
    a.metric("lat_max_ns", max((x["lat_max_ns"] or 0 for x in cells), default=0))
    return a.outcome(f"{len(cells)} cells; loss in production-like cells: {prod_loss}; "
                     f"{sum(1 for x in cells if x['loss'])} lossy cells overall")


@bench_test(
    "hw.contention", tier=1, units="any",
    params={"seconds": 5.0, "pl_duty_pct": [0, 25, 50, 75, 100], "ps": [False, True],
            "sd": [False, True], "iio": [False, True], "burst_len": 16, "rate_mbs": 32.0},
    description="Ring v2 loss/latency under a PS (memcpy) x SD x IIO DMA x PL memtester duty "
                "matrix. PL duty via `hwval contention --idle` (idle = burst x (1-d)/d); "
                "duty 0 = production-like cell (ring v2 alone under the host aggressors).",
    pass_criteria="0 ring loss in production-like cells",
    artifacts=("contention.json", "contention_matrix.json"), suites=("hwval",),
    analyze=analyze_contention, duration_param="seconds",
)
def hw_contention(ctx: TestContext) -> Outcome:
    name = _require_hwval(ctx)
    secs, bl = float(ctx.params["seconds"]), int(ctx.params["burst_len"])
    duties = [float(x) for x in ctx.params["pl_duty_pct"]]
    idles = [idle_cycles_for_duty(bl, d) for d in duties if d > 0]
    cells = []
    for ps in ctx.params["ps"]:
        for sd in ctx.params["sd"]:
            for iio in ctx.params["iio"]:
                aggs = [k for k, on in (("ps", ps), ("sd", sd), ("iio", iio)) if on]
                spawned = _Spawned()
                try:
                    _start_aggressors(ctx, name, aggs, secs * (len(idles) + 2), spawned)
                    if 0.0 in duties:
                        victim = _ringv2_session(ctx, "tagged", float(ctx.params["rate_mbs"]),
                                                 secs, "tagged")
                        cells.append({"aggressors": {"pl_duty_pct": 0.0, "ps": ps, "sd": sd,
                                                     "iio": iio},
                                      "production_like": True, "victim": victim})
                    if idles:
                        rep = _hw(ctx, "contention", ["--seconds", secs, "--idle",
                                                      ",".join(str(i) for i in idles),
                                                      "--burst-len", bl],
                                  secs * len(idles) * 2 + 60)
                        for cell in rep.get("cells", []):
                            load = cell.get("offered_load")
                            cells.append({"aggressors": {
                                "pl_duty_pct": round(float(load) * 100, 1) if load is not None
                                else None, "idle_cycles": cell.get("idle_cycles"), "ps": ps,
                                "sd": sd, "iio": iio},
                                "production_like": False, "cell": cell})
                finally:
                    spawned.stop()
    ctx.save_json("contention.json", {"cells": cells})
    return analyze_contention(ctx)


# ---------------------------------------------------------------------------
# hw.ctrl_out
# ---------------------------------------------------------------------------


def lock_drops(events: list[dict[str, Any]], mask: int) -> list[float]:
    """Times where any bit of ``mask`` goes 1 -> 0 in the CTRL_OUT event stream."""
    drops = []
    prev = None
    for e in sorted(events, key=lambda x: float(x.get("t_s", x.get("t", 0)))):
        if e.get("heartbeat"):
            continue
        v = _int(e.get("value"))
        if prev is not None and (prev & ~v) & mask:
            drops.append(float(e.get("t_s", e.get("t", 0))))
        prev = v
    return drops


def analyze_ctrl_out(a: AnalysisContext) -> Outcome:
    d = a.load_json("ctrl_out.json")
    mask = int(a.params["lock_mask"])
    phases = d.get("phases") or {}
    if not phases:
        raise Inconclusive("no CTRL_OUT events drained")
    counts, unexplained, overflows = {}, [], 0
    for name, rep in phases.items():
        events = rep.get("events") or []
        overflows += _int(rep.get("overflows"))
        drops = lock_drops(events, mask) if mask else []
        counts[name] = {"events": len(events), "lock_drops": len(drops)}
        if name == "soak":
            unexplained += drops
    a.metric("phases", counts)
    a.metric("evt_overflows", overflows, max=0)
    a.metric("unexplained_lock_drops", len(unexplained), max=0)
    a.metric("unexplained_times_s", unexplained[:20])
    if not mask:
        a.warn("lock_mask = 0: lock bits not selected, drops not evaluated")
    total = sum(c["events"] for c in counts.values())
    return a.outcome(f"{total} CTRL_OUT events; {len(unexplained)} lock drops during the soak "
                     f"phase, {counts.get('exercise', {}).get('lock_drops', 0)} during rate/LO "
                     "changes (explained)")


@bench_test(
    "hw.ctrl_out", tier=1, units="any",
    params={"seconds": 60.0, "exercise": ["rate", "lo"], "lock_mask": 3, "evt_mask": 255,
            "alt_rate_hz": 30720000.0, "lo_step_hz": 1000000.0},
    description="AD9361 CTRL_OUT event recorder: a quiet soak phase (drops there are "
                "unexplained) then rate/LO changes (drops there are expected). lock_mask "
                "selects the lock bits of the CTRL_OUT word.",
    pass_criteria="no lock drops during the soak phase, no recorder overflow",
    artifacts=("ctrl_out.json",), suites=("hwval",), analyze=analyze_ctrl_out,
    duration_param="seconds",
)
def hw_ctrl_out(ctx: TestContext) -> Outcome:
    name = _require_hwval(ctx)
    phy = ctx.cfg.iio.phy_device
    _hw(ctx, "evt", ["enable", "--mask", int(ctx.params["evt_mask"])])
    phases: dict[str, Any] = {}
    try:
        _hw(ctx, "evt", ["drain"])  # discard history
        ctx.sleep(float(ctx.params["seconds"]))
        phases["soak"] = _hw(ctx, "evt", ["drain"])
        if ctx.params["exercise"]:
            orig_fs = ctx.iio_get(name, phy, "sampling_frequency", "voltage0", False)
            orig_lo = ctx.iio_get(name, phy, "frequency", "altvoltage0", True)
            try:
                if "rate" in ctx.params["exercise"]:
                    ctx.iio_set(name, phy, "sampling_frequency",
                                f"{int(ctx.params['alt_rate_hz'])}", "voltage0", False)
                if "lo" in ctx.params["exercise"]:
                    lo = float(str(orig_lo).split()[0]) + float(ctx.params["lo_step_hz"])
                    ctx.iio_set(name, phy, "frequency", f"{int(lo)}", "altvoltage0", True)
                ctx.sleep(1.0)
            finally:
                ctx.iio_set(name, phy, "sampling_frequency", str(orig_fs).split()[0],
                            "voltage0", False)
                ctx.iio_set(name, phy, "frequency", str(orig_lo).split()[0], "altvoltage0", True)
            ctx.sleep(1.0)
            phases["exercise"] = _hw(ctx, "evt", ["drain"])
    finally:
        try:
            _hw(ctx, "evt", ["disable"])
        except FbenchError as exc:
            ctx.warn(f"evt disable failed: {exc.message}")
    ctx.save_json("ctrl_out.json", {"phases": phases})
    return analyze_ctrl_out(ctx)
