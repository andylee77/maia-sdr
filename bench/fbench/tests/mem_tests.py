"""mem.* tests: PS memtest, carve-out canaries, PS bandwidth."""

from __future__ import annotations

from ..errors import FbenchError, Inconclusive
from ..runner import AnalysisContext, Outcome, TestContext, bench_test


def analyze_memtest(a: AnalysisContext) -> Outcome:
    d = a.load_json("mem_test.json")
    if "errors" not in d:
        raise Inconclusive("agent reported no error count")
    a.metric("bytes_tested", d.get("bytes_tested"))
    a.metric("seconds", d.get("seconds"))
    a.metric("patterns", d.get("patterns"))
    a.metric("errors", int(d["errors"]), max=0)
    return a.outcome(f"PS memtest {a.params['anon_mb']} MiB x {a.params['passes']} pass(es), "
                     f"{d.get('patterns') and len(d['patterns']) or '?'} patterns: "
                     f"{d['errors']} errors")


@bench_test(
    "mem.ps_memtest", tier=0, units="any",
    params={"anon_mb": 256, "patterns": "all", "passes": 1, "cpu": -1},
    description="PS memory test on anonymous memory (walking 1/0, address, inverse, March C-, "
                "PRBS, checkerboard).",
    pass_criteria="0 errors",
    artifacts=("mem_test.json",), suites=("memory",), analyze=analyze_memtest,
)
def mem_ps_memtest(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    cpu = int(ctx.params["cpu"])
    rep = ctx.agent.mem_test(name, int(ctx.params["anon_mb"]), str(ctx.params["patterns"]),
                             int(ctx.params["passes"]), cpu if cpu >= 0 else None)
    ctx.save_json("mem_test.json", rep)
    return analyze_memtest(ctx)


def analyze_canary(a: AnalysisContext) -> Outcome:
    d = a.load_json("canary.json")
    total = 0
    parts = []
    for region in d["verify"]:
        n = int(region.get("corrupt_words", 0))
        total += n
        parts.append(f"{region.get('region')}: {n}")
    if not d["verify"]:
        raise Inconclusive("no regions verified")
    a.metric("regions", [r.get("region") for r in d["verify"]])
    a.metric("load", d.get("load"))
    a.metric("corrupt_words_total", total, max=0)
    return a.outcome("carve-out canaries after load — corrupt words " + ", ".join(parts))


def idle_carveouts(reserved: list[dict]) -> list[str]:
    """no-map reserved-memory nodes that are not DMA rx buffers (safe to overwrite)."""
    out = []
    for r in reserved or []:
        compat = str(r.get("compatible") or "")
        if r.get("no_map") and "rxbuffer" not in compat and not r.get("overlaps_system_ram"):
            out.append(str(r.get("node")))
    return out


@bench_test(
    "mem.canary", tier=0, units="any",
    params={"regions": ["auto"], "load": ["sd", "mem"], "load_mb": 256, "soak_s": 60.0},
    description="Fill idle carve-outs with canaries, apply load (SD writes, memcpy), verify. "
                "regions=auto: every no-map reserved-memory node that is not a DMA rxbuffer "
                "(from `audit`), so active rings are never overwritten.",
    pass_criteria="canary intact",
    artifacts=("canary.json",), suites=("memory",), analyze=analyze_canary,
    duration_param="soak_s",
)
def mem_canary(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    regions = [str(r) for r in ctx.params["regions"]]
    if regions == ["auto"]:
        regions = idle_carveouts(ctx.agent.audit(name).get("reserved_memory") or [])
        if not regions:
            raise Inconclusive("no idle carve-out (no-map, not an rxbuffer ring) on this image; "
                               "pass -p regions=<reserved-memory node>")
    fill = [ctx.agent.mem_canary(name, "fill", r) for r in regions]
    load_reports = {}
    try:
        if "sd" in ctx.params["load"]:
            load_reports["sd"] = ctx.agent.sd_bench(name, int(ctx.params["load_mb"]), 1024, True)
        if "mem" in ctx.params["load"]:
            load_reports["mem"] = ctx.agent.mem_bw(name, f"{int(ctx.params['load_mb'])}M")
    except FbenchError as exc:
        ctx.warn(f"load generation failed: {exc.message}")
    ctx.sleep(float(ctx.params["soak_s"]))
    verify = [ctx.agent.mem_canary(name, "verify", r) for r in regions]
    ctx.save_json("canary.json", {"fill": fill, "verify": verify,
                                  "load": sorted(load_reports), "load_reports": load_reports})
    return analyze_canary(ctx)


def analyze_bw(a: AnalysisContext) -> Outcome:
    d = a.load_json("mem_bw.json")
    results = d.get("results") or {k: v for k, v in d.items() if isinstance(v, (int, float))}
    if not results:
        raise Inconclusive("agent reported no bandwidth figures")
    for k, v in results.items():
        a.metric(f"{k}_mbs", v)
    return a.outcome("bandwidth (MB/s): " + ", ".join(f"{k} {v:.0f}" for k, v in results.items()),
                     "pass")


@bench_test(
    "mem.ps_bw", tier=0, units="any",
    params={"size": "64M"},
    description="memcpy/read/write bandwidth: cached anon, uncached /dev/mem, reserved region.",
    pass_criteria="report only",
    artifacts=("mem_bw.json",), suites=("memory",), analyze=analyze_bw,
)
def mem_ps_bw(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.save_json("mem_bw.json", ctx.agent.mem_bw(name, str(ctx.params["size"])))
    return analyze_bw(ctx)
