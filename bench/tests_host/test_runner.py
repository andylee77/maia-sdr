"""Runner: run dirs, result.json schema, suites, maintenance and TX-off guarantees."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from conftest import FakeServices
from fbench.errors import AgentError, UsageError
from fbench.runner import (
    REGISTRY,
    Outcome,
    TestSpec,
    build_params,
    expand,
    load_tests,
    reanalyze,
    resolve_roles,
    run_target,
    run_test,
    suites,
    validate_result,
)
from fbench.state import SessionState


def _spec(func, **kw: Any) -> TestSpec:
    base = dict(id="t.dummy", tier=0, units="any", maintenance=False, tx=False, params={},
                description="d", pass_criteria="p", func=func)
    base.update(kw)
    return TestSpec(**base)


def test_run_dir_layout_and_schema(cfg, services) -> None:
    def fn(ctx):
        ctx.save_json("x.json", {"a": 1})
        ctx.metric("m", 3, max=5)
        return Outcome("fine")

    res = run_test(_spec(fn), cfg, services, {"dut": "A"}, {})
    assert res.verdict == "pass" and res.exit_code == 0
    rd = res.run_dir
    assert rd.parent.name == "bench"
    assert rd.parent.parent.name == "2026-09-26"
    assert rd.name == "run_20260926_153000_t.dummy"
    for f in ("result.json", "params.json", "units.json", "log.txt", "FINDINGS.md"):
        assert (rd / f).exists(), f
    doc = json.loads((rd / "result.json").read_text())
    assert validate_result(doc) == []
    assert list(doc) == ["schema", "test", "run_id", "started", "ended", "verdict", "summary",
                         "units", "params", "metrics", "thresholds", "maintenance_mode",
                         "tx_used", "artifacts", "warnings", "errors"]
    assert doc["units"]["A"]["transceiver"] == "AD9361"
    assert doc["thresholds"] == {"m": {"max": 5}}
    assert doc["artifacts"] == ["artifacts/x.json"]
    assert doc["started"].endswith("-04:00")
    findings = (rd / "FINDINGS.md").read_text(encoding="utf-8")
    assert "# t.dummy — PASS" in findings and "\n\n## Metrics\n\n" in findings


def test_same_second_runs_get_unique_dirs(cfg, services) -> None:
    fn = lambda ctx: Outcome("x")  # noqa: E731
    a = run_test(_spec(fn), cfg, services, {"dut": "A"}, {})
    services.sim.t = 0.0
    b = run_test(_spec(fn), cfg, services, {"dut": "A"}, {})
    assert a.run_dir != b.run_dir and b.run_id.endswith("_2")


def test_threshold_failure_gives_fail(cfg, services) -> None:
    def fn(ctx):
        ctx.metric("errors", 2, max=0)
        ctx.metric("slow", 1.0, min=8.0, severity="warn")
        return Outcome("bad")

    res = run_test(_spec(fn), cfg, services, {"dut": "A"}, {})
    assert res.verdict == "fail" and res.exit_code == 1
    assert any("slow" in w for w in res.result["warnings"])


@pytest.mark.parametrize("exc,verdict,code", [
    (UsageError("x"), "error", 2),
    (AgentError("x"), "error", 2),
    (RuntimeError("boom"), "error", 2),
])
def test_exceptions_map_to_verdicts(cfg, services, exc, verdict: str, code: int) -> None:
    def fn(ctx):
        raise exc

    res = run_test(_spec(fn), cfg, services, {"dut": "A"}, {})
    assert res.verdict == verdict and res.exit_code == code
    assert validate_result(res.result) == []


def test_precondition_when_agent_missing(cfg, services) -> None:
    services.sim.agent_units = set()
    res = run_test(REGISTRY["sys.telemetry"] if REGISTRY else load_tests()["sys.telemetry"],
                   cfg, services, {"dut": "A"}, build_params(load_tests()["sys.telemetry"], {}))
    assert res.verdict == "precondition" and res.exit_code == 3


def test_maintenance_exit_runs_even_when_test_raises(cfg, services) -> None:
    def fn(ctx):
        raise RuntimeError("mid-test crash")

    res = run_test(_spec(fn, maintenance=True), cfg, services, {"dut": "A"}, {})
    calls = [a for u, a in services.agent.calls if a[0] == "maint"]
    assert calls == [["maint", "enter"], ["maint", "exit"]]
    assert res.verdict == "error"
    assert res.result["maintenance_mode"] is True
    st = SessionState(cfg.paths.state_dir).load()
    assert st["units"]["A"]["maintenance"] is False


def test_failed_maint_exit_is_reported_as_error(cfg, services) -> None:
    services.agent.overrides["maint exit"] = AgentError("stuck")
    res = run_test(_spec(lambda ctx: Outcome("ok"), maintenance=True), cfg, services,
                   {"dut": "A"}, {})
    assert res.verdict == "error"
    assert any("maint exit failed" in e for e in res.result["errors"])


def _tx_spec(func) -> TestSpec:
    return _spec(func, id="t.tx", units="tx,rx", tx=True, params={"tx_atten_db": 40.0})


def test_tx_off_after_tx_test_even_on_exception(cfg, services) -> None:
    def fn(ctx):
        ctx.mark_tx_active("A")
        raise RuntimeError("crash while transmitting")

    res = run_test(_tx_spec(fn), cfg, services, {"tx": "A", "rx": "B"}, {"tx_atten_db": 40.0})
    assert ["tx", "off"] in [a for u, a in services.agent.calls if u == "A"]
    assert res.verdict == "error" and res.result["tx_used"] is True
    assert SessionState(cfg.paths.state_dir).load()["units"]["A"]["tx_active"] is False


def test_tx_off_happens_before_maint_exit(cfg, services) -> None:
    spec = _tx_spec(lambda ctx: Outcome("ok"))
    spec.maintenance = True
    run_test(spec, cfg, services, {"tx": "A", "rx": "B"}, {"tx_atten_db": 40.0})
    seq = [" ".join(a[:2]) for u, a in services.agent.calls if a[0] in ("tx", "maint")]
    assert seq.index("tx off") < seq.index("maint exit")


def test_tx_off_falls_back_to_host_iio(cfg, services) -> None:
    """Unit B has no agent: tx off must go through libiio (hardwaregain -89.75)."""
    run_test(_spec(lambda ctx: Outcome("ok"), id="t.tx", units="tx,rx", tx=True,
                   params={"tx_atten_db": 40.0}), cfg, services, {"tx": "B", "rx": "A"},
             {"tx_atten_db": 40.0})
    sets = [c for c in services.iio("B").calls if c[0] == "set" and c[4] == "hardwaregain"]
    assert sets and sets[0][5] == "-89.75"


def test_tx_off_failure_turns_verdict_into_error(cfg, services) -> None:
    services.agent.overrides["tx off"] = AgentError("dead")
    services.sim.reachable.discard("A")  # host fallback cannot reach it either
    res = run_test(_tx_spec(lambda ctx: Outcome("ok")), cfg, services, {"tx": "A", "rx": "B"},
                   {"tx_atten_db": 40.0})
    assert res.verdict in ("error", "precondition")
    assert any("TX OFF FAILED" in e for e in res.result["errors"])


def test_safety_refusal_before_hardware(tmp_path: Path) -> None:
    from conftest import write_config
    from fbench.config import load_config

    cfg = load_config(write_config(tmp_path, cabled=False))
    services = FakeServices(cfg)
    touched = []
    res = run_test(_tx_spec(lambda ctx: touched.append(1) or Outcome("x")), cfg, services,
                   {"tx": "A", "rx": "B"}, {"tx_atten_db": 40.0})
    assert res.verdict == "refused" and res.exit_code == 4
    assert touched == [] and services.agent.calls == []
    assert validate_result(res.result) == []


def test_params_coercion_and_unknown_keys() -> None:
    spec = load_tests()["iface.eye_idelay"]
    p = build_params(spec, {"rates_hz": "61440000,30720000", "dwell_ms": "20"})
    assert p["rates_hz"] == [61440000, 30720000] and p["dwell_ms"] == 20
    with pytest.raises(UsageError):
        build_params(spec, {"bogus": 1})
    with pytest.raises(UsageError):
        build_params(spec, {"dwell_ms": "fast"})
    p = build_params(load_tests()["sys.telemetry"], {}, duration=12)
    assert p["seconds"] == 12
    with pytest.raises(UsageError):
        build_params(load_tests()["iface.clk_freq"], {}, duration=5)


def test_role_resolution(cfg) -> None:
    reg = load_tests()
    assert resolve_roles(reg["sys.audit"], cfg) == [{"dut": "A"}]
    assert resolve_roles(reg["sys.audit"], cfg, "A,B") == [{"dut": "A"}, {"dut": "B"}]
    assert resolve_roles(reg["rf.cw_ppm"], cfg) == [{"tx": "A", "rx": "B"}]
    assert resolve_roles(reg["rf.cw_ppm"], cfg, tx="B", rx="A") == [{"tx": "B", "rx": "A"}]
    assert resolve_roles(reg["net.link"], cfg) == [{"a": "A", "b": "B"}]


def test_suites_expand() -> None:
    s = suites()
    assert [t for t, _ in s["smoke"]] == ["sys.identity", "sys.audit", "sys.telemetry",
                                          "iface.clk_freq"]
    assert dict(s["smoke"])["sys.telemetry"] == {"seconds": 10}
    assert all(t.startswith("iface.") for t, _ in s["interface"])
    assert {t for t, _ in s["hwval"]} == {t for t, sp in load_tests().items() if sp.tier == 1}
    assert "rf.isolation" not in [t for t, _ in s["rf"]]
    suite, entries = expand("smoke")
    assert suite == "smoke" and len(entries) == 4
    assert expand("sys.audit")[0] is None
    with pytest.raises(UsageError):
        expand("nope")


def test_suite_run_writes_summary_and_worst_exit(cfg, services) -> None:
    suite, results = run_target("smoke", cfg, services, unit="A")
    assert suite == "smoke" and len(results) == 4
    assert all(r.verdict == "pass" for r in results), [(r.test, r.summary) for r in results]
    sums = list((cfg.paths.diagnostics_dir).glob("*/bench/suite_*_smoke.json"))
    assert len(sums) == 1
    doc = json.loads(sums[0].read_text())
    assert doc["exit_code"] == 0 and len(doc["runs"]) == 4
    tel = [r for r in results if r.test == "sys.telemetry"][0]
    assert tel.result["params"]["seconds"] == 10


def test_reanalyze_preserves_notes(cfg, services) -> None:
    res = run_test(load_tests()["iface.clk_freq"], cfg, services, {"dut": "A"},
                   build_params(load_tests()["iface.clk_freq"], {}))
    f = res.run_dir / "FINDINGS.md"
    text = f.read_text(encoding="utf-8").split("## Notes")[0] + "## Notes\n\nAndy: looked fine on the scope.\n"
    f.write_text(text, encoding="utf-8")
    again = reanalyze(res.run_dir)
    assert again.verdict == "pass"
    assert "Andy: looked fine on the scope." in f.read_text(encoding="utf-8")


def test_validate_result_catches_problems() -> None:
    assert validate_result([]) == ["result is not an object"]
    probs = validate_result({"schema": "x", "verdict": "maybe"})
    assert any("missing keys" in p for p in probs)
    assert any("verdict" in p for p in probs)
