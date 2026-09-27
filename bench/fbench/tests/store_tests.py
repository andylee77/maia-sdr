"""store.sd_bench: SD card throughput and write latency."""

from __future__ import annotations

from ..errors import Inconclusive
from ..runner import AnalysisContext, Outcome, TestContext, bench_test


def analyze_sd(a: AnalysisContext) -> Outcome:
    d = a.load_json("sd_bench.json")
    if d.get("write_mbs") is None:
        raise Inconclusive("agent reported no write throughput")
    a.metric("write_mbs", d["write_mbs"], min=float(a.params["min_write_mbs"]), severity="warn")
    a.metric("read_mbs", d.get("read_mbs"))
    lat = d.get("write_lat_us") or {}
    for k in ("p50", "p99", "max"):
        a.metric(f"write_lat_{k}_us", lat.get(k))
    a.metric("fsync_ms", d.get("fsync_ms"))
    slow = " (below 8 MB/s: RAM-first capture needed)" if d["write_mbs"] < \
        float(a.params["min_write_mbs"]) else ""
    return a.outcome(f"SD write {d['write_mbs']:.1f} MB/s, read {d.get('read_mbs', 0):.1f} MB/s, "
                     f"write latency p99 {lat.get('p99')} us, max {lat.get('max')} us{slow}",
                     "pass")


@bench_test(
    "store.sd_bench", tier=0, units="any",
    params={"mb": 256, "bs_kb": 1024, "fsync": True, "min_write_mbs": 8.0},
    description="SD sequential write/read MB/s, write-latency p50/p99/max, fsync time "
                "(files under /mnt/sd/bench only).",
    pass_criteria="report; flags < 8 MB/s",
    artifacts=("sd_bench.json",), suites=("memory",), analyze=analyze_sd,
)
def store_sd_bench(ctx: TestContext) -> Outcome:
    name = ctx.roles["dut"]
    ctx.require_agent(name)
    ctx.save_json("sd_bench.json", ctx.agent.sd_bench(name, int(ctx.params["mb"]),
                                                      int(ctx.params["bs_kb"]),
                                                      bool(ctx.params["fsync"])))
    return analyze_sd(ctx)
