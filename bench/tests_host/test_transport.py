"""Transport argv building and output parsers (runner injected: no network)."""

from __future__ import annotations

from pathlib import Path

import pytest

from fbench.errors import TransportError
from fbench.transport import (
    CmdResult,
    Iio,
    Ssh,
    _parse_iio_attr_value,
    parse_context_attrs,
    parse_ping_rtt,
    ping,
)


class Recorder:
    def __init__(self, result: CmdResult) -> None:
        self.result = result
        self.calls: list[list[str]] = []

    def __call__(self, argv, timeout, **kw) -> CmdResult:
        self.calls.append(list(argv))
        if kw.get("stdout_path"):
            Path(kw["stdout_path"]).write_bytes(b"\x01\x00\x02\x00")
        return self.result


def test_ssh_options_and_connect_failure(tmp_path: Path) -> None:
    rec = Recorder(CmdResult(0, "ok", "", 0.0))
    ssh = Ssh("B", "10.25.0.2", "root", "~/.ssh/id_ed25519", tmp_path / "kh", 5, rec)
    assert ssh.run("true", 5) == (0, "ok", "")
    argv = rec.calls[0]
    for opt in ("BatchMode=yes", "ConnectTimeout=5", "HostKeyAlias=fbench-B",
                "StrictHostKeyChecking=accept-new"):
        assert opt in argv
    assert argv[-2:] == ["root@10.25.0.2", "true"]
    ssh.put(tmp_path / "f", "/mnt/sd/bench/x", 5)
    assert rec.calls[-1][:2] == ["scp", "-O"]
    rec.result = CmdResult(255, "", "Connection timed out", 5.0)
    with pytest.raises(TransportError):
        ssh.run("true", 5)
    assert ssh.reachable() is False


def test_ssh_host_key_alias_follows_sd_card(tmp_path: Path) -> None:
    # Each SD card image has its own dropbear host key: swapping cards on
    # the same unit must select a different known-hosts entry.
    rec = Recorder(CmdResult(0, "", "", 0.0))
    p25 = Ssh("A", "192.168.2.1", "root", "~/.ssh/id_ed25519", tmp_path / "kh", 5, rec,
              card="OVHNVI2FJEBXAB4M")
    factory = Ssh("A", "192.168.2.1", "root", "~/.ssh/id_ed25519", tmp_path / "kh", 5, rec,
                  card="104473023196000bf5ff1a00aae12c3ca8")
    assert p25.host_key_alias == "fbench-A-OVHNVI2FJEBXAB4M"
    assert factory.host_key_alias != p25.host_key_alias
    assert "HostKeyAlias=fbench-A-OVHNVI2FJEBXAB4M" in p25.options()
    unknown = Ssh("A", "192.168.2.1", "root", "~/.ssh/id_ed25519", tmp_path / "kh", 5, rec)
    assert unknown.host_key_alias == "fbench-A"


def test_iio_cli_backend(tmp_path: Path) -> None:
    rec = Recorder(CmdResult(0, "dev 'ad9361-phy', channel 'voltage0' (input), attr "
                                "'hardwaregain', value '71.000000 dB'\n", "", 0.0))
    iio = Iio("ip:10.25.0.2", backend="cli", runner=rec)
    assert iio.attr_get("ad9361-phy", "hardwaregain", "voltage0") == "71.000000 dB"
    assert rec.calls[-1] == ["iio_attr", "-u", "ip:10.25.0.2", "-c", "-i", "ad9361-phy",
                             "voltage0", "hardwaregain"]
    iio.attr_set("ad9361-phy", "hardwaregain", "-89.75", "voltage0", output=True)
    assert rec.calls[-1][-5:] == ["-o", "ad9361-phy", "voltage0", "hardwaregain", "-89.75"]
    iio.attr_set("ad9361-phy", "bist_prbs", 2, debug=True)
    assert rec.calls[-1][3:] == ["-D", "ad9361-phy", "bist_prbs", "2"]
    out = iio.capture("cf-ad9361-lpc", ["voltage0", "voltage1"], 1024, tmp_path / "c.cs16")
    assert rec.calls[-1][:7] == ["iio_readdev", "-u", "ip:10.25.0.2", "-b", "1024", "-s", "1024"]
    assert out.read_bytes() == b"\x01\x00\x02\x00"


def test_parsers() -> None:
    assert _parse_iio_attr_value("8000000\n") == "8000000"
    attrs = parse_context_attrs("IIO context with 3 attributes:\n\thw_model: Analog Devices "
                                "PlutoSDR Rev.C (Z7020-AD9361)\n\thw_serial: 1044\n"
                                "\tfw_version: v0.38\n")
    assert attrs == {"hw_model": "Analog Devices PlutoSDR Rev.C (Z7020-AD9361)",
                     "hw_serial": "1044", "fw_version": "v0.38"}
    assert parse_ping_rtt("Minimum = 0ms, Maximum = 2ms, Average = 1ms") == 1.0
    assert parse_ping_rtt("round-trip min/avg/max = 0.281/0.402/0.612 ms") == 0.402
    assert parse_ping_rtt("64 bytes: time=0.5 ms\n64 bytes: time=1.5 ms") == 1.0
    assert parse_ping_rtt("100% packet loss") is None


def test_ping_windows_argv() -> None:
    rec = Recorder(CmdResult(0, "Average = 3ms", "", 0.0))
    assert ping("192.168.2.1", 2, 1.0, rec, windows=True) == 3.0
    assert rec.calls[0] == ["ping", "-n", "2", "-w", "1000", "192.168.2.1"]
    rec.result = CmdResult(1, "", "", 0.0)
    assert ping("10.25.0.2", 1, 1.0, rec, windows=False) is None
