"""Register maps built from the real p25.svd plus the ADI/PS tables."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from fbench.errors import SafetyRefusal
from fbench.regmaps import (
    P25_SVD,
    agent_builtin_maps,
    merge_doc,
    SCHEMA,
    build_adi,
    build_all,
    build_p25,
    build_ps,
    check_expected,
    find_reg,
    load_regmaps,
)


def _regs(core: dict) -> dict[int, dict]:
    return {int(r["offset"], 16): r for b in core["blocks"] for r in b["regs"]}


def test_p25_from_real_svd() -> None:
    m = build_p25(P25_SVD)
    assert m["schema"] == SCHEMA and m["core"] == "p25" and m["base"] == "0x7C460000"
    assert m["vacant"] == [["0x120", "0x180"], ["0x1E0", "0x200"]]
    regs = _regs(m)
    assert len(regs) == 54
    side = {off for off, r in regs.items() if r["read_side_effect"]}
    assert side == {0x0C, 0xA4, 0xC4, 0x60, 0x80, 0xE0, 0x184, 0x1A0, 0x1C0}
    assert not any(0x120 <= off < 0x180 or 0x1E0 <= off < 0x200 for off in regs)
    assert regs[0x0]["name"] == "product_id" and regs[0x0]["expected"] == "0x70323566"
    assert regs[0x8]["access"] == "rw" and regs[0xC]["access"] == "ro"
    for r in regs.values():
        assert set(r) == {"name", "offset", "access", "width", "reset", "snapshot", "desc",
                          "fields", "read_side_effect", "expected"}
    ver = regs[0x4]["fields"]
    assert [(f["name"], f["lsb"], f["width"]) for f in ver][:2] == [("bugfix", 0, 8),
                                                                    ("minor", 8, 8)]
    names = [b["name"] for b in m["blocks"]]
    assert names[:3] == ["control", "ddc", "traffic_ddc"] and "spectrometer" in names


def test_adi_cores() -> None:
    doc = build_adi()
    cores = {c["core"]: c for c in doc["cores"]}
    assert set(cores) == {"adi_adc", "adi_dac", "rx_dmac", "tx_dmac"}
    adc = _regs(cores["adi_adc"])
    assert adc[0x54]["name"] == "CLK_FREQ" and adc[0x88]["access"] == "w1c"
    assert adc[0x400]["name"] == "CHAN0_CNTRL" and adc[0x444]["name"] == "CHAN1_STATUS"
    assert [adc[0x800 + 4 * i]["name"] for i in range(7)] == [f"IDELAY_LANE{i}" for i in range(7)]
    dac = _regs(cores["adi_dac"])
    assert dac[0x4000]["name"] == "DAC_VERSION" and dac[0x4088]["access"] == "w1c"
    assert dac[0x4410]["name"] == "DAC_CHAN0_PAT_DATA" and dac[0x4458]["name"] == \
        "DAC_CHAN1_CNTRL_7"
    assert cores["rx_dmac"]["base"] == "0x7C400000" and cores["tx_dmac"]["base"] == "0x7C420000"


def test_ps_expected_values() -> None:
    cores = {c["core"]: c for c in build_ps()["cores"]}
    slcr, ddrc = _regs(cores["slcr"]), _regs(cores["ddrc"])
    assert slcr[0x104]["expected"] == {"mask": "0x0007F000", "value": "0x00020000"}
    assert slcr[0x124]["expected"] == "0x0C200003"
    assert ddrc[0x30]["expected"] == "0x00040B30"
    assert ddrc[0x2C]["expected"] == {"mask": "0x0000FFFF", "value": "0x00000008"}
    assert [slcr[o]["name"] for o in range(0xB40, 0xB78, 4)][0] == "DDRIOB_ADDR0"
    assert all(slcr[o]["expected"] is None for o in range(0xB40, 0xB78, 4))
    assert slcr[0xB74]["name"] == "DDRIOB_DCI_STATUS"
    assert {o for o in range(0x208, 0x228, 4)} <= set(ddrc)
    assert all(r["access"] == "ro" for c in cores.values() for r in _regs(c).values())
    assert _regs(cores["l2c"])[0xF60]["name"] == "REG15_PREFETCH_CTRL"


def test_check_expected() -> None:
    assert check_expected(None, 5) is None
    assert check_expected("0x0C200003", 0x0C200003) is True
    assert check_expected({"mask": "0x0007F000", "value": "0x00020000"}, 0x00020008) is True
    assert check_expected({"mask": "0x0007F000", "value": "0x00020000"}, 0x00024008) is False


def test_build_all_and_load(tmp_path: Path) -> None:
    written = build_all(tmp_path)
    assert sorted(p.name for p in written) == ["adi_regs.json", "p25_regs.json", "ps_regs.json"]
    for p in written:
        assert json.loads(p.read_text())["schema"] == SCHEMA
    maps = load_regmaps(tmp_path)
    assert {"p25", "adi_adc", "adi_dac", "rx_dmac", "tx_dmac", "slcr", "ddrc", "l2c"} <= set(maps)
    r = find_reg(maps, "p25", "PRODUCT_ID")
    assert r.address == 0x7C460000 and r.decode(0x70323566) == {"product_id": 0x70323566}
    assert find_reg(maps, "p25", "0xE0").read_side_effect
    with pytest.raises(SafetyRefusal):
        find_reg(maps, "p25", "0x130")  # vacant bank: not in the allow-list


def test_repo_share_is_up_to_date() -> None:
    from fbench import BENCH_DIR

    share = BENCH_DIR / "share"
    builtins = agent_builtin_maps()
    for name, doc in (("p25_regs.json", build_p25()),
                      ("adi_regs.json", merge_doc(build_adi(), builtins)),
                      ("ps_regs.json", merge_doc(build_ps(), builtins))):
        assert json.loads((share / name).read_text()) == doc, f"run `fbench regmaps build` ({name})"


def test_merge_keeps_agent_builtins_and_adds_aliases(tmp_path: Path) -> None:
    agent_dir = tmp_path / "maps"
    agent_dir.mkdir()
    builtin = {"schema": SCHEMA, "core": "ddrc", "base": "0xF8006000", "size": 4096,
               "readonly": True, "blocks": [{"name": "all", "offset": "0x0", "regs": [
                   {"name": "DDRC_CTRL", "offset": "0x0", "access": "rw", "width": 32},
                   {"name": "DRAM_EMR_MR_REG", "offset": 48, "access": "rw", "width": 32}]}]}
    (agent_dir / "ddrc.json").write_text(json.dumps(builtin))
    merged = merge_doc(build_ps(), agent_builtin_maps(agent_dir))
    ddrc = next(c for c in merged["cores"] if c["core"] == "ddrc")
    regs = _regs(ddrc)
    assert regs[0x0]["name"] == "DDRC_CTRL"  # agent-only register kept
    r = regs[0x30]
    assert r["name"] == "DRAM_EMR_MR_REG" and r["aliases"] == ["DRAM_EMR_MR"]
    assert r["expected"] == "0x00040B30" and r["access"] == "ro"  # more restrictive wins
    assert ddrc["readonly"] is True
    assert 0x208 in regs  # host-only registers appended
    (tmp_path / "share").mkdir()
    (tmp_path / "share" / "ps_regs.json").write_text(json.dumps(merged))
    maps = load_regmaps(tmp_path / "share")
    assert find_reg(maps, "ddrc", "DRAM_EMR_MR").name == "DRAM_EMR_MR_REG"
