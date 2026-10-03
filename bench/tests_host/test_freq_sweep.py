"""rf.freq_sweep: the plan, the per-point line measurement, failures and the run comparison."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import numpy as np
import pytest

from conftest import FakeServices, write_config
from fbench.config import load_config
from fbench.runner import build_params, load_tests, run_test
from fbench.tests.sweep_tests import SPEC_EDGES_HZ, measure_point, plan_freqs

SMALL = {"freqs_mhz": [100.0, 900.0, 4000.0], "nsamples": 16384, "lo_offset_hz": 400000.0,
         "offset_hz": 200000.0, "tx_atten_db": 40.0}


def _sweep(cfg: Any, services: FakeServices, tx: str, rx: str, **params: Any) -> Any:
    spec = load_tests()["rf.freq_sweep"]
    return run_test(spec, cfg, services, {"tx": tx, "rx": rx},
                    build_params(spec, {**SMALL, **params}))


def test_default_plan_covers_the_range_and_the_ad9363_edges() -> None:
    plan = plan_freqs(70e6, 5998.5e6, 8, 100e6, SPEC_EDGES_HZ)
    assert plan[0] == 70e6 and plan[-1] == 5998.5e6
    assert 325e6 in plan and 3800e6 in plan
    assert len(plan) == 84
    assert max(np.diff(plan)) <= 100e6 + 1
    assert sum(1 for f in plan if f < 325e6) == 18 and sum(1 for f in plan if f > 3800e6) == 23


def _capture(fs: float, n: int, lines: dict[float, float], noise_dbfs: float = -95.0,
             seed: int = 1) -> np.ndarray:
    rng = np.random.default_rng(seed)
    t = np.arange(n) / fs
    sigma = 10 ** (noise_dbfs / 20) / np.sqrt(2)
    iq = rng.normal(0, sigma, n) + 1j * rng.normal(0, sigma, n)
    for f, dbfs in lines.items():
        iq = iq + 10 ** (dbfs / 20) * np.exp(2j * np.pi * f * t)
    return iq


def test_measure_point_reads_each_line_relative_to_the_tone() -> None:
    fs, n = 8e6, 65536
    iq = _capture(fs, n, {1.5e6: -20.0, -1.5e6: -65.0, 1.0e6: -55.0, 0.5e6: -60.0,
                          2.7e6: -80.0})
    m = measure_point(iq, fs, 1e6, 0.5e6, 20e3, True, 20.0)
    assert m["sign"] == 1 and m["tx_sideband"] == 1
    assert m["level_dbfs"] == pytest.approx(-20.0, abs=0.2)
    assert m["rx_image_dbc"] == pytest.approx(-45.0, abs=1.0)
    assert m["tx_lo_dbc"] == pytest.approx(-35.0, abs=1.0)
    assert m["tx_image_dbc"] == pytest.approx(-40.0, abs=1.0)
    assert not (m["rx_image_at_floor"] or m["tx_lo_at_floor"] or m["tx_image_at_floor"])
    assert m["spur_hz"] == pytest.approx(2.7e6, abs=500)
    assert m["spur_dbc"] == pytest.approx(-60, abs=1)


def test_measure_point_follows_an_inverted_rx_and_a_lower_tx_sideband() -> None:
    fs, n = 8e6, 65536
    # Tone below the TX LO (lo_offset - offset) and the RX spectrum inverted on top.
    iq = np.conj(_capture(fs, n, {0.5e6: -20.0, 1.0e6: -50.0, 1.5e6: -58.0}))
    m = measure_point(iq, fs, 1e6, 0.5e6, 20e3, True, 20.0)
    assert m["sign"] == -1 and m["tx_sideband"] == -1
    assert m["tone_hz"] == pytest.approx(-0.5e6, abs=10)
    assert m["tx_lo_dbc"] == pytest.approx(-30.0, abs=1.0)
    assert m["tx_image_dbc"] == pytest.approx(-38.0, abs=1.0)
    assert m["rx_image_at_floor"]


def test_sweep_measures_every_method_on_both_sides(cfg: Any) -> None:
    services = FakeServices(cfg)
    res = _sweep(cfg, services, "A", "B")
    assert res.verdict == "pass", (res.summary, res.result["errors"])
    m = res.result["metrics"]
    assert m["methods"] == ["pattern", "cyclic"] and m["points_measured"] == 6
    assert m["tx_lock_read"] is True and m["rx_lock_read"] is False  # B has no agent
    # The AD9363 RX unit's out-of-spec points are reported separately.
    assert "pattern_rx_image_dbc_worst_out_of_spec" in m
    cmds = [a for u, a in services.agent.calls if u == "A"]
    assert ["maint", "enter"] in cmds and ["maint", "exit"] in cmds
    cal = [a for a in cmds if a[:3] == ["iio", "attr", "set"] and "calib_mode" in a]
    assert len(cal) == 3  # one TX quadrature calibration per cyclic point, none for pattern
    rows = json.loads((res.run_dir / "artifacts" / "sweep_points.json").read_text())
    assert {r["f_mhz"] for r in rows} == {100.0, 900.0, 4000.0}
    # LOs restored, the TX left at maximum attenuation.
    sim = services.sim
    assert sim.num("B", "ad9361-phy", "frequency", "altvoltage0", True) == 858100000
    assert sim.num("A", "ad9361-phy", "hardwaregain", "voltage0", True) == -89.75


def test_sweep_fails_where_a_synthesizer_does_not_lock(cfg: Any) -> None:
    services = FakeServices(cfg)
    services.sim.unlocked[("A", "rx")] = [(3.5e9, 6e9)]
    res = _sweep(cfg, services, "B", "A")
    assert res.verdict == "fail"
    m = res.result["metrics"]
    assert m["methods"] == ["dds", "cyclic"]  # factory image, no agent on B
    assert m["rx_unlocked"] == 2 and m["rx_unlocked_mhz"] == [4000.0]
    assert "rx unlocked at 4000 MHz" in res.summary


def test_a_rejected_frequency_is_recorded_and_the_sweep_goes_on(cfg: Any) -> None:
    from fbench.errors import AgentError

    services = FakeServices(cfg)
    services.sim.agent_units = {"A", "B"}
    real = services.agent._default

    def reject_low(unit: str, args: list[str]) -> dict:
        if "frequency" in args and unit == "B" and float(args[args.index("--value") + 1]) < 325e6:
            raise AgentError("ad9361-phy: Invalid argument")
        return real(unit, services.agent._key(args), args)

    services.agent.overrides["iio attr set"] = reject_low
    res = _sweep(cfg, services, "A", "B", stimuli=["pattern"])
    m = res.result["metrics"]
    assert res.verdict == "fail" and m["point_errors"] == 1 and m["point_errors_mhz"] == [100.0]
    assert m["points_measured"] == 2


def _with_self_loop(tmp: Path, pad_db: float = 30.0) -> Any:
    def mutate(d: dict) -> None:
        d["rf"]["links"].append({"tx": "A.TX1A", "rx": "A.RX1A", "pad_db": pad_db,
                                 "note": "A self loop"})
    return load_config(write_config(tmp, mutate=mutate))


def test_compare_isolates_the_rx_difference_from_runs_sharing_a_tx(tmp_path: Path) -> None:
    cfg = _with_self_loop(tmp_path)
    services = FakeServices(cfg)
    services.sim.agent_units = {"A", "B"}
    loop = _sweep(cfg, services, "A", "A", stimuli=["pattern"])
    rev = _sweep(cfg, services, "B", "A", stimuli=["pattern"])
    res = _sweep(cfg, services, "A", "B", stimuli=["pattern"],
                 compare=[str(loop.run_dir), str(rev.run_dir)])
    assert loop.verdict == rev.verdict == res.verdict == "pass", res.summary
    doc = json.loads((res.run_dir / "artifacts" / "compare.json").read_text())
    kinds = {p["kind"]: p for p in doc["pairs"]}
    assert set(kinds) == {"rx", "tx", "reverse"}
    rx = kinds["rx"]
    assert rx["label"] == "RX B - RX A (TX A), pattern"
    assert rx["median_db"] == pytest.approx(0.0, abs=0.5)  # the same model behind both RXs
    assert rx["spec_mhz"] == [325.0, 3800.0] and rx["median_out_of_spec_db"] is not None
    assert kinds["tx"]["label"] == "TX B - TX A (RX A), pattern"
    assert "RX B - RX A (TX A), pattern: median" in res.summary
    assert (res.run_dir / "artifacts" / "compare_diff.png").exists()


def test_compare_warns_when_the_runs_used_different_pads(tmp_path: Path) -> None:
    cfg = _with_self_loop(tmp_path, pad_db=40.0)
    services = FakeServices(cfg)
    loop = _sweep(cfg, services, "A", "A", stimuli=["pattern"])
    res = _sweep(cfg, services, "A", "B", stimuli=["pattern"], compare=[str(loop.run_dir)])
    assert any("different pads" in w for w in res.result["warnings"])
    assert any(p["kind"] == "rx" for p in res.result["metrics"]["compare_pairs"])


def test_analyze_takes_a_compare_list_after_the_runs(cfg: Any, cli: Any) -> None:
    services_runs = FakeServices(cfg)
    first = _sweep(cfg, services_runs, "B", "A", stimuli=["cyclic"])
    second = _sweep(cfg, services_runs, "A", "B", stimuli=["cyclic"])
    code, doc = cli("analyze", str(second.run_dir), "-p", f"compare={first.run_dir.as_posix()}")
    assert code == 0, doc
    res = json.loads((second.run_dir / "result.json").read_text())
    assert res["params"]["compare"] == [first.run_dir.as_posix()]
    assert [p["kind"] for p in res["metrics"]["compare_pairs"]] == ["reverse"]
    code, doc = cli("analyze", str(second.run_dir), "-p", "no_such=1")
    assert code == 2
