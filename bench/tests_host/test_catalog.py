"""Every test in design doc section 9 is registered and runs green on the simulated bench."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

import numpy as np
import pytest

from conftest import BENCH, FakeServices
from fbench.analysis import sigmf
from fbench.runner import build_params, load_tests, resolve_roles, run_test, validate_result

DOC = BENCH.parent / "doc" / "HW_VALIDATION_SUITE.md"
EXTRA_TESTS = {"sys.boot_log", "rf.refclk_eth"}  # added after the doc table (F19, UART)


def doc_catalog() -> dict[str, dict[str, Any]]:
    rows = {}
    for line in DOC.read_text(encoding="utf-8").splitlines():
        m = re.match(r"\|\s*`([a-z]+\.[a-z0-9_]+)`\s*\|\s*(\d)\s*\|\s*([^|]+?)\s*\|"
                     r"\s*([^|]*?)\s*\|", line)
        if m:
            rows[m.group(1)] = {"tier": int(m.group(2)), "units": m.group(3).strip(),
                                "maintenance": m.group(4).strip() == "M"}
    return rows


def test_every_doc_test_is_registered_with_matching_metadata() -> None:
    reg = load_tests()
    doc = doc_catalog()
    assert len(doc) >= 32
    for tid, row in doc.items():
        assert tid in reg, f"{tid} missing from the registry"
        spec = reg[tid]
        assert spec.tier == row["tier"], tid
        assert spec.units == row["units"], tid
        assert spec.maintenance == row["maintenance"], tid
        assert spec.pass_criteria and spec.description
    assert set(reg) - set(doc) <= EXTRA_TESTS | set(doc)


def test_every_tier1_test_drives_the_hwval_agent_op() -> None:
    reg = load_tests()
    tier1 = [s for s in reg.values() if s.tier == 1]
    assert len(tier1) == 11
    assert all("hwval" in s.suites for s in tier1)


def _clip(tmp: Path) -> Path:
    fs = 1_000_000.0
    t = np.arange(int(fs * 0.2)) / fs
    iq = 3000 * np.exp(2j * np.pi * 50e3 * t)
    base = tmp / "site_clip"
    sigmf.write(base, iq, fs, 860_962_500.0, "ci16_le")
    return base.with_suffix(".sigmf-meta")


#: per-test scenario: roles, sim tweaks, parameter overrides, expected verdict.
SCENARIOS: dict[str, dict[str, Any]] = {
    "sys.telemetry": {"params": {"seconds": 5}},
    "sys.soak": {"params": {"seconds": 120}},
    "sys.boot_log": {"params": {"seconds": 2}},
    "iface.prbs_soak": {"params": {"seconds": 5}},
    "mem.canary": {"params": {"soak_s": 1}},
    "net.link": {"agents": {"A", "B"}},
    "rf.cw_ppm": {"tx": "A", "rx": "B", "params": {"nsamples": 65536, "interval_s": 120}},
    "rf.level_sweep": {"tx": "A", "rx": "B", "params": {"nsamples": 32768}},
    "rf.spur_scan": {"unit": "B", "params": {"nsamples": 65536}},
    "rf.isolation": {"tx": "A", "rx": "B", "params": {"nsamples": 65536},
                     "verdict": "inconclusive"},
    "rf.p25_replay": {"tx": "B", "rx": "A", "clip": True, "params": {"seconds": 10}},
    "rf.refclk_eth": {"tx": "A", "rx": "B",
                      "params": {"capture_s": 0.05, "bounce_s": 0.1, "bounce_pad_s": 0.1,
                                 "rx_rate_hz": 1000000.0, "nfft": 8192}},
    "hw.ctrl_out": {"params": {"lock_mask": 2}},
}


def run_scenario(tid: str, cfg: Any, tmp_path: Path, scen: dict[str, Any] | None = None
                 ) -> tuple[Any, FakeServices]:
    scen = SCENARIOS.get(tid, {}) if scen is None else scen
    services = FakeServices(cfg)
    spec = load_tests()[tid]
    if spec.tier == 1:
        services.sim.images["A"] = "hwval"
    if "agents" in scen:
        services.sim.agent_units = set(scen["agents"])
    overrides = dict(scen.get("params", {}))
    if scen.get("clip"):
        overrides["clip"] = str(_clip(tmp_path))
    params = build_params(spec, overrides)
    roles = resolve_roles(spec, cfg, scen.get("unit"), scen.get("tx"), scen.get("rx"))[0]
    return run_test(spec, cfg, services, roles, params), services


@pytest.mark.parametrize("tid", sorted(load_tests()))
def test_each_test_runs_green_on_simulated_bench(tid: str, cfg: Any, tmp_path: Path) -> None:
    res, services = run_scenario(tid, cfg, tmp_path)
    expected = SCENARIOS.get(tid, {}).get("verdict", "pass")
    assert res.verdict == expected, (res.summary, res.result["errors"], res.result["warnings"])
    doc = json.loads((res.run_dir / "result.json").read_text(encoding="utf-8"))
    assert validate_result(doc) == []
    for rel in doc["artifacts"]:
        assert (res.run_dir / rel).exists(), rel
    assert (res.run_dir / "FINDINGS.md").exists()
    assert (res.run_dir / "params.json").exists()
    assert (res.run_dir / "units.json").exists()
    spec = load_tests()[tid]
    if spec.tx:
        assert doc["tx_used"] is True
        tx_unit = res.result["params"] and next(iter(doc["units"]))
        assert tx_unit
    if spec.maintenance and "A" in [u for u in doc["units"]] and \
            services.sim.images["A"] in ("p25", "hwval"):
        assert doc["maintenance_mode"] is True


@pytest.mark.parametrize("tid", sorted(t for t, s in load_tests().items() if s.analyze))
def test_each_analysis_reruns_offline(tid: str, cfg: Any, tmp_path: Path) -> None:
    from fbench.runner import reanalyze

    res, _ = run_scenario(tid, cfg, tmp_path)
    again = reanalyze(res.run_dir)
    assert again.verdict == res.verdict
    for k, v in res.result["metrics"].items():
        if k in again.result["metrics"] and isinstance(v, float):
            assert again.result["metrics"][k] == pytest.approx(v, rel=1e-6, abs=1e-9), k
