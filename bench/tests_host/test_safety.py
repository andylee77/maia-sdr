"""RF interlock math and refusals (design doc section 3)."""

from __future__ import annotations

from pathlib import Path

import pytest

from conftest import config_dict, write_config
from fbench.config import load_config, parse_config
from fbench.errors import ExitCode, SafetyRefusal
from fbench.safety import all_budgets, link_budget, min_atten, require_tx_allowed


def test_budget_at_limit_is_allowed_with_linear_warning(cfg) -> None:
    b = link_budget(cfg, "A", "B", 0.0)
    assert b.p_rx_dbm == pytest.approx(-10.0)  # 20 - 0 - 30
    assert b.level_ok is True  # -10 > -10 is false -> allowed
    assert b.linear_ok is False
    assert b.allowed is True
    assert any("linear" in w for w in b.warnings)


def test_budget_comfortable(cfg) -> None:
    b = link_budget(cfg, "A", "B", 40.0)
    assert b.p_rx_dbm == pytest.approx(-50.0)
    assert b.allowed and b.linear_ok and not b.warnings


def test_refuses_above_abs_max(tmp_path: Path) -> None:
    d = config_dict(tmp_path)
    d["rf"]["links"][0]["pad_db"] = 20.0
    cfg = parse_config(d)
    b = link_budget(cfg, "A", "B", 5.0)
    assert b.p_rx_dbm == pytest.approx(-5.0)
    assert not b.allowed and not b.level_ok
    assert "raise tx_atten to >= 10" in b.reasons[0]
    with pytest.raises(SafetyRefusal) as ei:
        require_tx_allowed(cfg, "A", "B", 5.0)
    assert ei.value.exit_code == ExitCode.SAFETY


def test_refuses_when_not_cabled(tmp_path: Path) -> None:
    cfg = load_config(write_config(tmp_path, cabled=False))
    b = link_budget(cfg, "A", "B", 60.0)
    assert b.level_ok and not b.allowed
    assert "cabled_confirmed" in b.reasons[0]


def test_refuses_without_link(tmp_path: Path) -> None:
    d = config_dict(tmp_path)
    d["rf"]["links"] = d["rf"]["links"][:1]
    cfg = parse_config(d)
    b = link_budget(cfg, "B", "A", 60.0)
    assert not b.allowed and b.p_rx_dbm is None
    assert "no rf.links entry" in b.reasons[0]


@pytest.mark.parametrize("atten", [-1.0, 90.0])
def test_refuses_out_of_range_attenuation(cfg, atten: float) -> None:
    assert not link_budget(cfg, "A", "B", atten).allowed


def test_pad_below_minimum_warns(tmp_path: Path) -> None:
    d = config_dict(tmp_path)
    d["rf"]["links"][0]["pad_db"] = 20.0
    b = link_budget(parse_config(d), "A", "B", 60.0)
    assert b.allowed and any("pad" in w for w in b.warnings)


def test_per_unit_tx_max(tmp_path: Path) -> None:
    d = config_dict(tmp_path)
    d["units"]["A"]["tx_max_dbm"] = 25.0
    b = link_budget(parse_config(d), "A", "B", 0.0)
    assert b.p_rx_dbm == pytest.approx(-5.0) and not b.allowed


def test_all_budgets_and_min_atten(cfg) -> None:
    assert len(all_budgets(cfg, 40.0)) == 2
    assert min_atten([70.0, 40.0, 55.0]) == 40.0
    assert min_atten(12.5) == 12.5
