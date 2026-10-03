"""Every CLI verb: --json output shape and exit codes (fakes only, no network)."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from conftest import BENCH, write_config
from fbench.cli import main


def _common(doc: dict, verb: str, code: int) -> None:
    assert doc["verb"] == verb and doc["exit_code"] == code and doc["ok"] is (code == 0)


def test_units_offline(cli) -> None:
    code, doc = cli("units")
    _common(doc, "units", 0)
    assert doc["probed"] is False
    assert [u["name"] for u in doc["units"]] == ["A", "B"]
    assert doc["units"][1]["transceiver"] == "AD9363"


def test_units_probe(cli, services) -> None:
    code, doc = cli("units", "--probe")
    _common(doc, "units", 0)
    a = doc["units"][0]["probe"]
    assert a["reachable"] and a["image"] == "p25" and a["firmware_family"] == "tezuka"
    assert a["serial"]["known"] is True
    b = doc["units"][1]["probe"]
    assert b["firmware_family"] == "factory"
    assert any("not in units.B.known_serials" in w for w in b["warnings"])  # warns, never fails


def test_units_probe_unreachable_is_precondition(cli, services) -> None:
    services.sim.reachable.discard("B")
    services.ping = lambda *a, **k: None  # type: ignore[method-assign]
    code, doc = cli("units", "--probe", "--unit", "B")
    _common(doc, "units", 3)
    assert doc["units"][0]["probe"]["reachable"] is False


def test_units_probe_dna_warning(cli, services, cfg_path: Path, tmp_path: Path) -> None:
    services.sim.images["A"] = "hwval"
    path = write_config(tmp_path, mutate=lambda d: d["units"]["A"].update(fpga_dna="0x99"))
    code, doc = cli("units", "--probe", "--unit", "A", config=path)
    probe = doc["units"][0]["probe"]
    assert probe["dna"]["authoritative"] and any("different board" in w for w in probe["warnings"])


def test_list_and_filters(cli) -> None:
    code, doc = cli("list")
    _common(doc, "list", 0)
    assert doc["count"] == 38 and set(doc["suites"]) == {"smoke", "interface", "transport",
                                                         "memory", "rf", "hwval", "soak"}
    code, doc = cli("list", "--tier", "1")
    assert doc["count"] == 11 and all(t["tier"] == 1 for t in doc["tests"])
    code, doc = cli("list", "--suite", "smoke")
    assert [t["id"] for t in doc["tests"]] == ["sys.identity", "sys.audit", "sys.telemetry",
                                               "iface.clk_freq"]
    code, doc = cli("list", "--suite", "nope")
    assert code == 2 and doc["ok"] is False


def test_describe(cli) -> None:
    code, doc = cli("describe", "iface.eye_idelay")
    _common(doc, "describe", 0)
    t = doc["test"]
    assert t["maintenance"] is True and t["params"]["rates_hz"] == [61440000]
    assert ">= 6-tap" in t["pass_criteria"] and t["reanalyzable"]
    code, doc = cli("describe", "smoke")
    assert doc["suite"] == "smoke" and len(doc["tests"]) == 4
    code, doc = cli("describe", "nope")
    assert code == 2


def test_safety_refuses_when_not_cabled(cli, tmp_path: Path) -> None:
    path = write_config(tmp_path, cabled=False)
    code, doc = cli("safety", "--tx", "A", "--rx", "B", "--tx-atten", "0", config=path)
    _common(doc, "safety", 4)
    b = doc["budgets"][0]
    assert b["p_rx_dbm"] == -10.0 and b["level_ok"] is True and b["allowed"] is False


def test_safety_at_limit_allowed_when_cabled(cli) -> None:
    code, doc = cli("safety", "--tx", "A", "--rx", "B", "--tx-atten", "0")
    _common(doc, "safety", 0)
    assert doc["budgets"][0]["warnings"]
    code, doc = cli("safety")
    assert len(doc["budgets"]) == 2 and code == 0
    code, doc = cli("safety", "--tx", "A")
    assert code == 2


def test_run_single_pass(cli) -> None:
    code, doc = cli("run", "iface.clk_freq", "--unit", "A")
    _common(doc, "run", 0)
    assert doc["verdict"] == "pass" and doc["suite"] is None
    run = doc["runs"][0]
    assert set(run) == {"test", "run_id", "run_dir", "verdict", "exit_code", "summary"}
    assert json.loads((Path(run["run_dir"]) / "result.json").read_text())["verdict"] == "pass"


def test_run_fail_exit_1(cli, services) -> None:
    services.agent.overrides["prbs soak"] = {"ok": True, "seconds": 5, "polls": 50,
                                             "error_intervals": 3}
    code, doc = cli("run", "iface.prbs_soak", "-p", "seconds=5")
    _common(doc, "run", 1)
    assert doc["verdict"] == "fail"


def test_run_precondition_exit_3(cli, services) -> None:
    code, doc = cli("run", "hw.id")  # unit A runs the p25 image
    _common(doc, "run", 3)
    assert doc["verdict"] == "precondition"


def test_run_safety_exit_4(cli, tmp_path: Path) -> None:
    path = write_config(tmp_path, cabled=False)
    code, doc = cli("run", "rf.cw_ppm", "--tx", "A", "--rx", "B", config=path)
    _common(doc, "run", 4)
    assert doc["verdict"] == "refused"


def test_run_inconclusive_exit_5(cli) -> None:
    code, doc = cli("run", "rf.isolation", "--tx", "A", "--rx", "B", "-p", "nsamples=32768")
    _common(doc, "run", 5)


def test_run_dry_run(cli, tmp_path: Path) -> None:
    code, doc = cli("run", "rf", "--dry-run")
    _common(doc, "run", 0)
    assert doc["suite"] == "rf" and all("safety" in p for p in doc["plan"] if p["tx"])
    path = write_config(tmp_path, cabled=False)
    code, doc = cli("run", "rf.level_sweep", "--dry-run", config=path)
    assert code == 4
    assert doc["plan"][0]["safety"]["tx_atten_db"] == 40.0  # smallest atten of the sweep


def test_run_bad_param_is_usage_error(cli) -> None:
    code, doc = cli("run", "iface.clk_freq", "-p", "bogus=1")
    assert code == 2 and "unknown parameter" in doc["error"]
    code, doc = cli("run", "iface.clk_freq", "-p", "novalue")
    assert code == 2


def test_run_suite_worst_exit(cli, services) -> None:
    services.agent.overrides["audit"] = lambda u, a: {**json.loads(
        (Path(__file__).parent / "fixtures" / "agent" / "audit.json").read_text()),
        "checks": [{"name": "x", "ok": False, "value": 1, "expected": 2}]}
    code, doc = cli("run", "smoke")
    assert doc["suite"] == "smoke" and len(doc["runs"]) == 4
    assert code == 1 and [r["verdict"] for r in doc["runs"]].count("fail") == 1


def test_status_shows_runs_and_alerts(cli, cfg) -> None:
    cli("run", "iface.clk_freq")
    from fbench.state import SessionState

    SessionState(cfg.paths.state_dir).set_tx("B", True, "x")
    code, doc = cli("status")
    _common(doc, "status", 0)
    assert doc["runs"][0]["test"] == "iface.clk_freq"
    assert any("TX flagged active" in a for a in doc["alerts"])
    code, doc = cli("status", "--probe")
    assert doc["live"]["A"]["maintenance"] is False and "error" in doc["live"]["B"]


def test_analyze_and_compare(cli, services) -> None:
    _, d1 = cli("run", "iface.clk_freq")
    services.sim.regs[("A", "adi_adc", "CLK_FREQ")] += 3  # 4.6 kHz off -> fail
    _, d2 = cli("run", "iface.clk_freq")
    r1, r2 = d1["runs"][0]["run_dir"], d2["runs"][0]["run_dir"]
    code, doc = cli("analyze", r1)
    _common(doc, "analyze", 0)
    code, doc = cli("analyze", r2)
    assert code == 1
    code, doc = cli("compare", r1, r2)
    _common(doc, "compare", 1)
    assert doc["regressions"] and doc["metrics"]["error_hz_abs"][1]["delta"] > 4000
    code, doc = cli("compare", r1, r1)
    assert code == 0 and doc["regressions"] == []
    code, doc = cli("compare", r1)
    assert code == 2


def test_boot_status_and_swap(cli, services) -> None:
    code, doc = cli("boot", "A", "status")
    _common(doc, "boot", 0)
    assert doc["status"]["active"] == "p25"

    # simulate the reboot: the unit goes away once, then comes back
    ssh = services.ssh("A")
    orig = ssh.run
    state = {"n": 0}

    def run(cmd: str, timeout: float):
        if cmd == "true":
            state["n"] += 1
            if state["n"] == 1:
                from fbench.errors import TransportError

                raise TransportError("rebooting")
        return orig(cmd, timeout)

    ssh.run = run  # type: ignore[method-assign]
    code, doc = cli("boot", "A", "hwval")
    _common(doc, "boot", 0)
    assert doc["running_image"] == "hwval" and doc["went_down"] is True
    code, doc = cli("boot", "A", "p25", "--no-reboot")
    assert code == 0 and doc["reboot"].startswith("skipped")


def test_boot_install(cli, services, tmp_path: Path) -> None:
    src = tmp_path / "img"
    src.mkdir()
    (src / "BOOT.bin").write_bytes(b"boot")
    (src / "devicetree.dtb").write_bytes(b"dtb")
    code, doc = cli("boot", "A", "install", "--image", "hwval", "--from", str(src))
    _common(doc, "boot", 0)
    assert len(doc["sha256"]) == 2
    assert "/mnt/sd/bench/images/.incoming_hwval/BOOT.bin" in [r for _, r in
                                                               services.ssh("A").puts]
    call = [a for u, a in services.agent.calls if a[:2] == ["boot", "install"]][-1]
    assert call[2] == "hwval" and "--from" in call and "--sha256-boot" in call
    assert "--sha256-dtb" in call  # never install without --from (agent would `select`)
    code, doc = cli("boot", "A", "install", "--image", "hwval", "--from", str(tmp_path))
    assert code == 3


def test_agent_passthrough_and_contract(cli, services) -> None:
    code, doc = cli("agent", "A", "--", "maint", "status")
    _common(doc, "agent", 0)
    assert doc["reply"]["maintenance"] is False
    assert services.agent.calls[-1] == ("A", ["maint", "status"])
    code, doc = cli("agent", "B", "--", "version")
    assert code == 3  # no agent on B
    code, doc = cli("agent", "--contract")
    assert "ring check" in doc["contract"]
    code, doc = cli("agent", "A")
    assert code == 2


def test_reg_read_write_and_refusals(cli, services) -> None:
    services.sim.regs[("A", "p25", "product_id")] = 0x70323566
    code, doc = cli("reg", "A", "p25", "read", "product_id")
    _common(doc, "reg", 0)
    assert doc["value"] == "0x70323566" and doc["matches_expected"] is True
    assert doc["address"] == "0x7c460000"
    code, doc = cli("reg", "A", "p25", "read", "iq_dma_status")
    assert code == 4 and "read-to-clear" in doc["error"]
    code, doc = cli("reg", "A", "p25", "read", "iq_dma_status", "--allow-side-effect")
    assert code == 0
    assert services.agent.calls[-1][1][-1] == "--force-side-effects"
    code, doc = cli("reg", "A", "p25", "read", "0x1E8")
    assert code == 4  # vacant bank
    code, doc = cli("reg", "A", "p25", "write", "product_id", "0x1")
    assert code == 4  # read-only
    code, doc = cli("reg", "A", "adi_adc", "write", "SCRATCH", "0x1234")
    assert code == 0 and services.sim.regs[("A", "adi_adc", "SCRATCH")] == 0x1234
    code, doc = cli("reg", "A", "nocore", "read", "x")
    assert code == 3


def test_tx_off_agent_and_host_fallback(cli, services) -> None:
    code, doc = cli("tx", "A", "off")
    _common(doc, "tx", 0)
    assert doc["via"] == "agent"
    code, doc = cli("tx", "B", "off")
    _common(doc, "tx", 0)
    assert doc["via"] == "host-iio" and doc["host"]["ok"] is True


def test_regmaps_build_and_show(cli, tmp_path: Path) -> None:
    code, doc = cli("regmaps", "build", "--out", str(tmp_path))
    _common(doc, "regmaps", 0)
    assert doc["cores"]["p25"] == 70
    code, doc = cli("regmaps", "show", "--core", "p25", "--out", str(tmp_path))
    assert any(r["read_side_effect"] for r in doc["regs"])


def test_setup_session_keys_net_agent(cli, services, tmp_path: Path, monkeypatch) -> None:
    code, doc = cli("setup", "session")
    _common(doc, "setup", 0)
    assert doc["session"]["units"]["A"]["ip_forward"] is True
    assert doc["session"]["units"]["B"]["agent"] is False
    code, doc = cli("setup", "net", "--skip-route")
    assert code == 0 and doc["net"]["units"]["B"]["uboot_env"].startswith("not applied")
    code, doc = cli("setup", "net", "--apply-env")
    assert doc["net"]["route"] == "present" and doc["net"]["units"]["B"]["uboot_env"] == "applied"
    assert any("fw_setenv ipaddr 192.168.12.1" in c for c in services.ssh("B").commands)
    key = tmp_path / "id_test"
    key.write_text("PRIVATE")
    (tmp_path / "id_test.pub").write_text("ssh-ed25519 AAAA test@host")
    path = write_config(tmp_path, mutate=lambda d: d["ssh"].update(identity=str(key)))
    monkeypatch.setenv("FB_PW", "analog")
    code, doc = cli("setup", "keys", "--unit", "B", "--password-env", "FB_PW", config=path)
    _common(doc, "setup", 0)
    unit, cmds = services.paramiko_calls[-1]
    assert unit == "B" and "ssh-ed25519 AAAA test@host" in cmds[0]
    code, doc = cli("setup", "agent", "--binary", str(tmp_path / "missing"))
    assert code == 3
    binary = tmp_path / "fbench-agent"
    binary.write_bytes(b"\x7fELF")
    code, doc = cli("setup", "agent", "--unit", "A", "--binary", str(binary))
    assert code == 0 and doc["agent"]["units"]["A"]["ok"]


def test_setup_session_unreachable_hint(cli, services) -> None:
    services.sim.reachable.discard("B")
    code, doc = cli("setup", "session")
    assert code == 3
    assert "password-prompt" in doc["session"]["units"]["B"]["hint"]


def test_console_capture_and_list(cli) -> None:
    code, doc = cli("console", "--list")
    _common(doc, "console", 0)
    assert [p["device"] for p in doc["ports"]] == ["COM7"]
    code, doc = cli("console", "A", "--seconds", "1", "--until", "login:")
    _common(doc, "console", 0)
    assert doc["port"] == "COM7" and doc["matched"].endswith("login:")
    log = Path(doc["path"]).read_text(encoding="utf-8").splitlines()
    assert "\tU-Boot 2016.07" in log[0]
    code, doc = cli("console", "A", "--seconds", "0.5", "--until", "never-matches")
    assert code == 5


def test_passthrough_only_for_agent(cli) -> None:
    code, doc = cli("list", "--", "x")
    assert code == 2


def test_human_output_and_no_verb(capsys) -> None:
    assert main(["--config", str(BENCH / "config" / "bench.toml"), "list"]) == 0
    out = capsys.readouterr().out
    assert "iface.eye_idelay" in out and "suite smoke" in out
    assert main([]) == 2


def test_launcher_runs_from_anywhere(tmp_path: Path) -> None:
    example = BENCH / "config" / "bench.example.toml"  # bench.toml follows the live cabling
    res = subprocess.run([sys.executable, str(BENCH / "fbench.py"), "--config", str(example),
                          "safety", "--json"],
                         cwd=tmp_path, capture_output=True, text=True, timeout=60,
                         stdin=subprocess.DEVNULL)
    doc = json.loads(res.stdout)
    assert res.returncode == 4 and doc["verb"] == "safety"  # example config: not cabled
