"""net.link: ping RTT and TCP throughput host<->A and A<->B."""

from __future__ import annotations

from typing import Any

from ..errors import FbenchError, Inconclusive
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..transport import parse_ping_rtt


def analyze_net(a: AnalysisContext) -> Outcome:
    d = a.load_json("net.json")
    keys = ("host_a_rtt_ms", "a_b_rtt_ms", "a_b_mbs", "host_a_mbs")
    for k in keys:
        a.metric(k, d.get(k))
    for e in d.get("errors", []):
        a.warn(e)
    measured = [k for k in keys if d.get(k) is not None]
    if not measured:
        raise Inconclusive("nothing could be measured")
    return a.outcome(", ".join(f"{k}={d[k]:.2f}" for k in measured), "pass")


@bench_test(
    "net.link", tier=0, units="A,B",
    params={"ping_count": 10, "mb": 64, "host_mb": 32, "port": 5201},
    description="Ping RTT and TCP throughput host<->A (USB RNDIS) and A<->B (GbE), agent to "
                "agent.",
    pass_criteria="report",
    artifacts=("net.json",), suites=("transport",), analyze=analyze_net,
)
def net_link(ctx: TestContext) -> Outcome:
    a_name, b_name = ctx.roles["a"], ctx.roles["b"]
    a_unit, b_unit = ctx.cfg.unit(a_name), ctx.cfg.unit(b_name)
    out: dict[str, Any] = {"errors": []}
    n = int(ctx.params["ping_count"])
    out["host_a_rtt_ms"] = ctx.services.ping(a_unit.host, n, 1.0)
    b_ip = b_unit.eth_ip or b_unit.host
    try:
        rc, text, _ = ctx.services.ssh(a_name).run(f"ping -c {n} -q {b_ip}", n * 2 + 10)
        out["a_b_rtt_ms"] = parse_ping_rtt(text) if rc == 0 else None
    except FbenchError as exc:
        out["errors"].append(f"A->B ping: {exc.message}")
    port, mb = int(ctx.params["port"]), int(ctx.params["mb"])
    try:
        ctx.require_agent(a_name)
        ctx.require_agent(b_name)
        server = ctx.agent.net_serve(b_name, port, mb)
        try:
            ctx.sleep(1.0)
            out["a_b"] = ctx.agent.net_send(a_name, b_ip, port, mb)
            out["a_b_mbs"] = out["a_b"].get("mbs")
            out["b_serve"] = server.wait(60.0)
        finally:
            server.kill()
    except FbenchError as exc:
        out["errors"].append(f"A<->B TCP: {exc.message}")
    try:
        ctx.require_agent(a_name)
        hmb = int(ctx.params["host_mb"])
        server = ctx.agent.net_serve(a_name, port + 1, hmb)
        try:
            ctx.sleep(1.0)
            out["host_a"] = ctx.services.tcp_send(a_unit.host, port + 1, hmb, 120.0)
            out["host_a_mbs"] = out["host_a"].get("mbs")
            out["a_serve"] = server.wait(60.0)
        finally:
            server.kill()
    except (FbenchError, OSError) as exc:
        out["errors"].append(f"host->A TCP: {exc}")
    ctx.save_json("net.json", out)
    return analyze_net(ctx)
