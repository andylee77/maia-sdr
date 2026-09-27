"""Agent adapter: argv building, JSON extraction, error mapping, deploy."""

from __future__ import annotations

from pathlib import Path

import pytest

from conftest import FakeSsh
from fbench.agent import CONTRACT, AgentClient, as_int, check_reply, extract_json
from fbench.errors import AgentError, AgentRefused, AgentUnsupported, ExitCode


class ScriptedSsh:
    def __init__(self, reply: tuple[int, str, str]) -> None:
        self.reply = reply
        self.cmds: list[str] = []
        self.puts: list[tuple[str, str]] = []

    def run(self, cmd: str, timeout: float) -> tuple[int, str, str]:
        self.cmds.append(cmd)
        if cmd.endswith("version"):
            return 0, '{"ok": true, "version": "0.1.0"}', ""
        return self.reply

    def put(self, local: Path, remote: str, timeout: float) -> None:
        self.puts.append((str(local), remote))


def client(cfg, reply=(0, '{"ok": true, "value": "0x10"}', "")) -> tuple[AgentClient, ScriptedSsh]:
    ssh = ScriptedSsh(reply)
    return AgentClient(cfg, lambda unit: ssh), ssh


def test_command_line_is_quoted_and_has_no_extra_flags(cfg) -> None:
    ag, ssh = client(cfg)
    ag.reg_read("A", "p25", "product_id")
    assert ssh.cmds[-1] == ("/mnt/sd/bench/bin/fbench-agent reg read --core p25 --reg product_id")
    ag.reg_read("A", "p25", "iq_dma_status", force_side_effects=True)
    assert ssh.cmds[-1].endswith("--force-side-effects")
    ag.iio_attr_set("A", "ad9361-phy", "hardwaregain", -89.75, "voltage0", True)
    assert "--chan voltage0 --out --attr hardwaregain --value -89.75" in ssh.cmds[-1]
    ag.boot("A", "select", "hwval")
    assert ssh.cmds[-1].endswith("boot select hwval")


def test_extra_args_from_config(cfg) -> None:
    cfg.agent.extra_args = ["--json"]
    ag, ssh = client(cfg)
    ag.run("A", ["version"])
    assert ssh.cmds[-1].endswith("version --json")


def test_extract_json_variants() -> None:
    assert extract_json('{"ok": true}') == {"ok": True}
    assert extract_json('log line\n{"ok": true, "x": 1}\n') == {"ok": True, "x": 1}
    assert extract_json('{\n  "ok": true\n}') == {"ok": True}
    assert extract_json("garbage") is None
    assert extract_json("") is None
    assert as_int("0x1F") == 31 and as_int(7) == 7 and as_int("12") == 12


@pytest.mark.parametrize("code,exc,exit_code", [
    ("safety", AgentRefused, ExitCode.SAFETY),
    ("refused", AgentRefused, ExitCode.SAFETY),
    ("unknown_command", AgentUnsupported, ExitCode.PRECONDITION),
    ("wrong_image", AgentUnsupported, ExitCode.PRECONDITION),
    ("no_device", AgentUnsupported, ExitCode.PRECONDITION),
    ("usage", AgentError, ExitCode.ERROR),
    ("error", AgentError, ExitCode.ERROR),
])
def test_error_mapping(code: str, exc: type, exit_code: int) -> None:
    with pytest.raises(exc) as ei:
        check_reply("A", ["x"], {"ok": False, "error": "nope", "code": code}, 3)
    assert ei.value.exit_code == exit_code


def test_rc127_means_not_deployed(cfg) -> None:
    ag, _ = client(cfg, (127, "", "not found"))
    with pytest.raises(AgentUnsupported):
        ag.run("A", ["info"])


def test_no_json_is_agent_error(cfg) -> None:
    ag, _ = client(cfg, (0, "Segmentation fault", ""))
    with pytest.raises(AgentError):
        ag.run("A", ["info"])


def test_deploy_copies_binary_and_share(cfg, tmp_path: Path) -> None:
    ag, ssh = client(cfg, (0, "", ""))
    binary = tmp_path / "fbench-agent"
    binary.write_bytes(b"\x7fELF")
    share = [tmp_path / "p25_regs.json"]
    share[0].write_text("{}")
    rep = ag.deploy("A", binary, share)
    assert ssh.puts[0][1] == "/mnt/sd/bench/bin/fbench-agent.new"
    assert ssh.puts[1][1] == "/mnt/sd/bench/share/p25_regs.json"
    assert any("mv -f" in c for c in ssh.cmds)
    assert rep["version"]["version"] == "0.1.0"


def test_contract_lists_every_design_doc_subcommand() -> None:
    for sub in ("version", "info", "audit", "reg", "telemetry", "iio attr", "iio debug",
                "ad9361 spi", "eyescan", "prbs soak", "txlink", "ring capture", "ring check",
                "mem test", "mem canary", "mem bw", "sd bench", "net serve", "net send", "hwval",
                "tx off", "maint", "boot"):
        assert any(k == sub or k.startswith(sub + " ") for k in CONTRACT), sub


def test_fake_ssh_reports_missing_agent(services) -> None:
    ssh: FakeSsh = services.ssh("B")
    rc, _, _ = ssh.run("/mnt/sd/bench/bin/fbench-agent version", 5)
    assert rc == 127


def test_run_id_and_tx_ok_flags(cfg) -> None:
    ag, ssh = client(cfg)
    ag.run_id = "20260926_153000_rf.cw_ppm"
    ag.run("A", ["ring", "check", "--ring", "p25-wideband"])
    assert ssh.cmds[-1].endswith("--run-id 20260926_153000_rf.cw_ppm")
    ag.run_id = None
    ag.iio_attr_set("A", "ad9361-phy", "hardwaregain", -40, "voltage0", True, tx_ok=True)
    assert ssh.cmds[-1].endswith("--value -40 --tx-ok")
    ag.reg_write("A", "adi_dac", "DAC_CHAN0_CNTRL_7", 1, tx_ok=True)
    assert ssh.cmds[-1].endswith("--value 0x1 --tx-ok")
    ag.ring_check("A", "p25-wideband", "pn0fn", 10, [100, 500], bist="prbs", enable=True,
                  release_reset=True)
    assert "--stall-ms 100,500 --bist prbs --enable --release-reset" in ssh.cmds[-1]


def test_boot_install_requires_staging_dir(cfg) -> None:
    ag, _ = client(cfg)
    with pytest.raises(AgentError):
        ag.boot("A", "install", "hwval")


def test_runner_binds_run_id(cfg, services) -> None:
    from fbench.runner import Outcome, TestSpec, run_test

    seen = []
    spec = TestSpec(id="t.rid", tier=0, units="any", maintenance=False, tx=False, params={},
                    description="", pass_criteria="",
                    func=lambda ctx: seen.append(ctx.agent.run_id) or Outcome("x"))
    res = run_test(spec, cfg, services, {"dut": "A"}, {})
    assert seen == [res.run_id] and services.agent.run_id is None
