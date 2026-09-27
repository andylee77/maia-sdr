"""Config loading and validation."""

from __future__ import annotations

from pathlib import Path

import pytest

from conftest import BENCH, config_dict, write_config
from fbench.config import TRANSCEIVER_LIMITS, load_config, normalize_dna, parse_config
from fbench.errors import ConfigError


def test_live_config_loads() -> None:
    # Only structural checks: pads and cabling in bench.toml follow the bench.
    cfg = load_config(BENCH / "config" / "bench.toml")
    assert set(cfg.units) == {"A", "B"}
    assert all(lk.pad_db >= cfg.safety.min_pad_db for lk in cfg.rf.links)


def test_default_config_loads() -> None:
    cfg = load_config(BENCH / "config" / "bench.example.toml")
    assert set(cfg.units) == {"A", "B"}
    a, b = cfg.unit("A"), cfg.unit("B")
    assert a.label == "original" and a.transceiver == "AD9361"
    assert b.label == "newer" and b.transceiver == "AD9363"
    assert "OVHNVI2FJEBXAB4M" in a.known_serials
    assert "104473023196000bf5ff1a00aae12c3ca8" in a.known_serials
    assert b.known_serials == ["2IROGKMXCZTSTL3O"]
    assert b.via == "A" and a.forwarder
    assert cfg.rf.cabled_confirmed is False  # example ships unconfirmed
    assert cfg.rf.test_freq_hz == 858_100_000
    assert cfg.rf.p25_cc_freq_hz == 860_962_500
    assert [(lk.tx, lk.rx, lk.pad_db) for lk in cfg.rf.links] == [
        ("A.TX1A", "B.RX1A", 30.0), ("B.TX1A", "A.RX1A", 30.0)]
    assert a.tx_max_dbm == 20.0 and a.rx_abs_max_dbm == -10.0 and a.rx_linear_max_dbm == -30.0
    assert a.has_jp5 and "MT41K256M16TW-107" in a.dram_part
    assert b.uboot_env["ipaddr"] == "192.168.12.1"


def test_example_config_loads_and_matches_units() -> None:
    ex = load_config(BENCH / "config" / "bench.example.toml")
    cfg = load_config(BENCH / "config" / "bench.toml")
    assert set(ex.units) == set(cfg.units)
    for n in cfg.units:
        assert ex.unit(n).transceiver == cfg.unit(n).transceiver
        assert ex.unit(n).known_serials == cfg.unit(n).known_serials


def test_transceiver_limits() -> None:
    assert TRANSCEIVER_LIMITS["AD9363"]["rf_bw_max_hz"] == 20e6
    assert TRANSCEIVER_LIMITS["AD9363"]["lo_min_hz"] == 325e6
    assert TRANSCEIVER_LIMITS["AD9361"]["lo_max_hz"] == 6e9


def test_units_for_serial_is_only_a_hint(tmp_path: Path) -> None:
    cfg = load_config(write_config(tmp_path))
    assert [u.name for u in cfg.units_for_serial("OVHNVI2FJEBXAB4M")] == ["A"]
    assert cfg.units_for_serial("nope") == []


def test_dna_lookup(tmp_path: Path) -> None:
    def mutate(d: dict) -> None:
        d["units"]["B"]["fpga_dna"] = "0x1A2B"

    cfg = load_config(write_config(tmp_path, mutate=mutate))
    assert cfg.unit_by_dna("1a2b").name == "B"
    assert cfg.unit_by_dna("0x1A2B").name == "B"
    assert cfg.unit_by_dna("0x9") is None
    assert normalize_dna(0x1A2B) == "0x1a2b"


@pytest.mark.parametrize("mutate,msg", [
    (lambda d: d["units"]["A"].update(transceiver="AD9364"), "transceiver"),
    (lambda d: d["units"]["A"].update(bogus=1), "unknown key"),
    (lambda d: d["units"]["B"].update(via="C"), "via"),
    (lambda d: d["rf"]["links"].append({"tx": "A.RX1A", "rx": "B.RX1A", "pad_db": 30.0}),
     "TX port"),
    (lambda d: d["rf"]["links"].append({"tx": "Z.TX1A", "rx": "B.RX1A", "pad_db": 30.0}),
     "unknown unit"),
    (lambda d: d.update(units={}), "defines no"),
    (lambda d: d["bench"].update(default_unit="Q"), "default_unit"),
    (lambda d: d["units"]["A"].update(image="weird"), "image"),
])
def test_bad_configs_are_rejected(tmp_path: Path, mutate, msg: str) -> None:
    d = config_dict(tmp_path)
    mutate(d)
    with pytest.raises(ConfigError, match=msg):
        parse_config(d)


def test_missing_file_is_config_error(tmp_path: Path) -> None:
    with pytest.raises(ConfigError):
        load_config(tmp_path / "missing.toml")


def test_per_unit_safety_defaults_to_global(tmp_path: Path) -> None:
    d = config_dict(tmp_path)
    d["safety"]["tx_max_dbm"] = 7.0
    d["units"]["A"].pop("tx_max_dbm", None)
    d["units"]["B"]["tx_max_dbm"] = 15.0
    cfg = parse_config(d)
    assert cfg.unit("A").tx_max_dbm == 7.0
    assert cfg.unit("B").tx_max_dbm == 15.0
