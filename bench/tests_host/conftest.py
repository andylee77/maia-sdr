"""Offline test harness: a simulated two-board bench behind fake transports.

Nothing here touches the network. ``BenchSim`` holds IIO attributes, DAC
registers and a virtual clock; ``FakeAgent`` (a real ``AgentClient`` whose
``run`` is replaced) answers every agent subcommand from
``fixtures/agent/*.json`` plus the simulation; ``FakeIio`` synthesises RX
captures (a CW whose frequency/level follow the TX unit's settings);
``FakeSsh``/``FakeHttp`` cover the shell and p25-httpd paths.
"""

from __future__ import annotations

import copy
import json
import re
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable

import numpy as np
import pytest

BENCH = Path(__file__).resolve().parents[1]
if str(BENCH) not in sys.path:
    sys.path.insert(0, str(BENCH))

from fbench.agent import AgentClient, AgentProcess  # noqa: E402
from fbench.config import BenchConfig, parse_config  # noqa: E402
from fbench.errors import (  # noqa: E402
    AgentError,
    AgentUnsupported,
    TransportError,
)
from fbench.transport import CmdResult  # noqa: E402

FIXTURES = Path(__file__).parent / "fixtures" / "agent"
PHY, RXDEV, TXDEV = "ad9361-phy", "cf-ad9361-lpc", "cf-ad9361-dds-core-lpc"


def fixture(name: str) -> dict[str, Any]:
    return json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


def config_dict(tmp: Path, cabled: bool = True) -> dict[str, Any]:
    import tomllib

    # The example config is the stable reference; bench.toml is the operator's
    # live bench (pads, cabling) and must not change test outcomes.
    with open(BENCH / "config" / "bench.example.toml", "rb") as fh:
        data = tomllib.load(fh)
    data["paths"] = {"diagnostics_dir": str(tmp / "diag"), "state_dir": str(tmp / "state"),
                     "share_dir": str(BENCH / "share")}
    data["rf"]["cabled_confirmed"] = cabled
    return data


def write_config(tmp: Path, cabled: bool = True, mutate: Callable[[dict], None] | None = None
                 ) -> Path:
    data = config_dict(tmp, cabled)
    if mutate:
        mutate(data)
    path = tmp / "bench.toml"
    path.write_text(_to_toml(data), encoding="utf-8")
    return path


def _toml_value(v: Any) -> str:
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, str):
        return json.dumps(v)
    if isinstance(v, list):
        return "[" + ", ".join(_toml_value(x) for x in v) + "]"
    raise TypeError(v)


def _to_toml(data: dict[str, Any], prefix: str = "") -> str:
    """Tiny TOML writer for the bench config shape (tables, arrays of tables)."""
    lines: list[str] = []
    scalars = {k: v for k, v in data.items()
               if not isinstance(v, dict) and not (isinstance(v, list) and v and
                                                  isinstance(v[0], dict))}
    for k, v in scalars.items():
        lines.append(f"{k} = {_toml_value(v)}")
    for k, v in data.items():
        name = f"{prefix}{k}"
        if isinstance(v, dict):
            lines.append(f"\n[{name}]")
            lines.append(_to_toml(v, name + "."))
        elif isinstance(v, list) and v and isinstance(v[0], dict):
            for item in v:
                lines.append(f"\n[[{name}]]")
                lines.append(_to_toml(item, name + "."))
    return "\n".join(x for x in lines if x.strip() != "")


# ---------------------------------------------------------------------------
# Simulation
# ---------------------------------------------------------------------------


class BenchSim:
    """Shared state of the simulated boards."""

    def __init__(self, cfg: BenchConfig) -> None:
        self.cfg = cfg
        self.t = 0.0
        self.attrs: dict[tuple, str] = {}
        self.regs: dict[tuple, int] = {}
        self.ppm = {"A": 0.0, "B": 1.5}
        self.noise_dbfs = -60.0
        self.cyclic: dict[str, float] = {}
        self.images = {"A": "p25", "B": "factory"}
        self.imbe_rate = 0.0  # IMBE frames/s the fake p25-httpd extracts while TX is live
        self.agent_units = {"A"}
        self.reachable = {"A", "B"}
        self.phase_step_in: str | None = None  # refclk phase with an injected phase step
        self.decode_rate = 30.0
        self.cable_removed = False
        self.leak_db = -75.0  # coupling with the cable removed (relative to cabled)
        # rf.p25_corpus: the TX board's relay and what the DUT "decodes" from it.
        self.relay: dict[str, Any] | None = None
        self.relay_underruns = 0
        self.corpus_fn: Callable[[str], list[tuple]] | None = None  # item -> frames
        self.corpus_drop: set[str] = set()  # transmission ids the DUT misses
        self.corpus_park: tuple[float, int] | None = None  # mode C: (freq, TG) after primer
        self.corpus_audio_fn: Callable[[str], list[tuple[float, bytes]]] | None = None
        self.corpus_hex_flip = 0  # bits flipped in every /api/imbe_dump frame
        self.corpus_imbe_zero = False  # calls followed but nothing decoded (09:11 on the board)
        self.traffic = {"follower_enabled": True, "lock_freq": False}
        self.traffic_calls: list[dict[str, str]] = []
        for u in cfg.units:
            self.set_attr(u, PHY, "sampling_frequency", "2500000", "voltage0", False)
            self.set_attr(u, PHY, "sampling_frequency", "2500000", "voltage0", True)
            self.set_attr(u, PHY, "frequency", "858100000", "altvoltage0", True)
            self.set_attr(u, PHY, "frequency", "858100000", "altvoltage1", True)
            self.set_attr(u, PHY, "hardwaregain", "-89.75", "voltage0", True)
            self.set_attr(u, PHY, "hardwaregain", "20", "voltage0", False)
            self.set_attr(u, PHY, "gain_control_mode", "slow_attack", "voltage0", False)
            self.set_attr(u, PHY, "rssi", "100.00 dB", "voltage0", False)
            for ch in ("altvoltage0", "altvoltage1", "altvoltage2", "altvoltage3"):
                self.set_attr(u, TXDEV, "scale", "0", ch, True)
                self.set_attr(u, TXDEV, "frequency", "0", ch, True)
            self.regs[(u, "adi_dac", "DAC_CHAN0_CNTRL_7")] = 2
            self.regs[(u, "adi_dac", "DAC_CHAN1_CNTRL_7")] = 2
            self.regs[(u, "adi_adc", "CLK_FREQ")] = int(round(2 * 2.5e6 / (100e6 / 65536)))
            self.regs[(u, "adi_adc", "CLK_RATIO")] = 1

    # -- attributes ----------------------------------------------------------
    def key(self, unit: str, dev: str, attr: str, chan: str | None, output: bool) -> tuple:
        return (unit, dev, chan, bool(output) if chan else None, attr)

    def set_attr(self, unit: str, dev: str, attr: str, value: Any, chan: str | None = None,
                 output: bool = False) -> None:
        if dev == PHY and attr == "sampling_frequency":
            for out in (False, True):
                self.attrs[self.key(unit, dev, attr, chan, out)] = str(value)
            return
        self.attrs[self.key(unit, dev, attr, chan, output)] = str(value)

    def get_attr(self, unit: str, dev: str, attr: str, chan: str | None = None,
                 output: bool = False) -> str:
        k = self.key(unit, dev, attr, chan, output)
        if k not in self.attrs:
            raise TransportError(f"no attr {k}")
        if attr == "rssi":
            return f"{100.0 + self._rx_level_db(unit) / 2:.2f} dB"
        return self.attrs[k]

    def num(self, unit: str, dev: str, attr: str, chan: str, output: bool) -> float:
        return float(self.get_attr(unit, dev, attr, chan, output).split()[0])

    # -- TX model ------------------------------------------------------------
    def tx_source(self, unit: str) -> tuple[float, float] | None:
        """(RF frequency, dBFS at 0 dB atten) of the unit's active tone, if any."""
        lo = self.num(unit, PHY, "frequency", "altvoltage1", True)
        if unit in self.cyclic:
            return lo + self.cyclic[unit], 28.0
        if float(self.attrs.get(self.key(unit, TXDEV, "scale", "altvoltage0", True), "0")) > 0:
            f = float(self.attrs[self.key(unit, TXDEV, "frequency", "altvoltage0", True)])
            return lo + f, 28.0
        if self.regs.get((unit, "adi_dac", "DAC_CHAN0_CNTRL_7")) == 1:
            return lo, 28.0
        return None

    def _link(self, tx: str, rx: str) -> float | None:
        lk = self.cfg.link_for(tx, rx)
        return lk.pad_db if lk else None

    def _rx_level_db(self, rx: str) -> float:
        for tx in self.cfg.units:
            if tx == rx:
                continue
            src = self.tx_source(tx)
            pad = self._link(tx, rx)
            if src and pad is not None:
                atten = -self.num(tx, PHY, "hardwaregain", "voltage0", True)
                leak = self.leak_db if self.cable_removed else 0.0
                return src[1] - atten + (30.0 - pad) + leak
        return -200.0

    def capture(self, rx: str, n: int, phase_step: bool = False) -> bytes:
        fs = self.num(rx, PHY, "sampling_frequency", "voltage0", False)
        rx_lo = self.num(rx, PHY, "frequency", "altvoltage0", True)
        rng = np.random.default_rng(int(self.t * 1000) % 2 ** 32)
        sigma = 10 ** (self.noise_dbfs / 20) / np.sqrt(2)
        iq = (rng.normal(0, sigma, n) + 1j * rng.normal(0, sigma, n))
        t = (np.arange(n) / fs) + self.t
        for tx in self.cfg.units:
            if tx == rx:
                continue
            src = self.tx_source(tx)
            if src and self._link(tx, rx) is not None:
                f_rf, lvl = src
                bb = f_rf * (1 + self.ppm[tx] * 1e-6) - rx_lo * (1 + self.ppm[rx] * 1e-6)
                amp = 10 ** (self._rx_level_db(rx) / 20)
                ph = 2 * np.pi * bb * t
                if phase_step:
                    ph = ph + np.where(np.arange(n) > n // 2, np.radians(40), 0.0)
                iq = iq + amp * np.exp(1j * ph)
        iq = np.clip(iq.real, -1, 1) + 1j * np.clip(iq.imag, -1, 1)
        out = np.empty(2 * n, dtype="<i2")
        out[0::2] = np.round(iq.real * 2047)
        out[1::2] = np.round(iq.imag * 2047)
        self.t += n / fs
        return out.tobytes()


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class FakeIio:
    def __init__(self, sim: BenchSim, unit: str) -> None:
        self.sim, self.unit = sim, unit
        self.calls: list[tuple] = []

    def _check(self) -> None:
        if self.unit not in self.sim.reachable:
            raise TransportError(f"iio {self.unit} unreachable")

    def context_attrs(self, timeout: float = 10.0) -> dict[str, str]:
        self._check()
        serial = {"A": "104473023196000bf5ff1a00aae12c3ca8"}.get(self.unit, "unknownserial")
        fw = "v0.38" if self.sim.images[self.unit] == "factory" else "tezuka-v0.3"
        if self.sim.images[self.unit] == "p25":
            serial = "OVHNVI2FJEBXAB4M"
        return {"hw_model": "Analog Devices PlutoSDR Rev.C (Z7020-AD9361)", "hw_serial": serial,
                "fw_version": fw}

    def attr_get(self, dev: str, attr: str, chan: str | None = None, output: bool = False,
                 debug: bool = False, timeout: float = 10.0) -> str:
        self._check()
        self.calls.append(("get", dev, chan, output, attr))
        if debug and attr == "direct_reg_access":
            off = int(self.sim.attrs.get((self.unit, dev, "dra"), "0"), 0)
            name = {0x54: "CLK_FREQ", 0x58: "CLK_RATIO"}.get(off, "?")
            return hex(self.sim.regs.get((self.unit, "adi_adc", name), 0))
        return self.sim.get_attr(self.unit, dev, attr, chan, output)

    def attr_set(self, dev: str, attr: str, value: Any, chan: str | None = None,
                 output: bool = False, debug: bool = False, timeout: float = 10.0) -> None:
        self._check()
        self.calls.append(("set", dev, chan, output, attr, str(value)))
        if debug and attr == "direct_reg_access":
            self.sim.attrs[(self.unit, dev, "dra")] = str(value)
            return
        if dev == TXDEV and self.sim.images[self.unit] == "p25":
            raise TransportError("DDS compiled out on the P25 image")
        self.sim.set_attr(self.unit, dev, attr, value, chan, output)

    def capture(self, dev: str, channels: list[str], nsamples: int, out_path: Path,
                timeout: float = 30.0) -> Path:
        self._check()
        Path(out_path).parent.mkdir(parents=True, exist_ok=True)
        Path(out_path).write_bytes(self.sim.capture(self.unit, nsamples))
        return Path(out_path)

    def start_cyclic_tx(self, dev: str, channels: list[str], data_path: Path, nsamples: int,
                        timeout: float = 60.0) -> Any:
        a = np.fromfile(data_path, dtype="<i2").astype(float)
        iq = a[0::2] + 1j * a[1::2]
        fs = self.sim.num(self.unit, PHY, "sampling_frequency", "voltage0", True)
        spec = np.abs(np.fft.fft(iq[:65536]))
        k = int(np.argmax(spec))
        f = np.fft.fftfreq(min(len(iq), 65536), 1 / fs)[k]
        self.sim.cyclic[self.unit] = float(f)
        sim, unit = self.sim, self.unit

        class _H:
            def stop(self) -> None:
                sim.cyclic.pop(unit, None)
        return _H()


class FakePopen:
    def __init__(self, out: str = "", rc: int = 0) -> None:
        self.out, self.returncode, self.killed = out, rc, False

    def communicate(self, timeout: float | None = None) -> tuple[bytes, bytes]:
        return self.out.encode(), b""

    def poll(self) -> int:
        return self.returncode

    def kill(self) -> None:
        self.killed = True

    def terminate(self) -> None:
        self.killed = True

    def wait(self, timeout: float | None = None) -> int:
        return self.returncode


class FakeSsh:
    def __init__(self, sim: BenchSim, unit: str) -> None:
        self.sim, self.unit, self.host = sim, unit, f"sim-{unit}"
        self.commands: list[str] = []
        self.puts: list[tuple[str, str]] = []
        self.pending: dict[str, int] = {}
        self.files: dict[str, bytes] = {}
        self.sizes: dict[str, int] = {}

    def _check(self) -> None:
        if self.unit not in self.sim.reachable:
            raise TransportError(f"unit {self.unit} unreachable over SSH")

    def put_stream(self, chunks: Any, remote: str, timeout: float) -> int:
        self._check()
        data = bytearray()
        n = 0
        for blk in chunks:
            n += len(blk)
            if len(data) < (1 << 20):
                data += blk
        self.sizes[remote] = n
        if n <= (1 << 20):
            self.files[remote] = bytes(data)
        self.puts.append(("<stream>", remote))
        return n

    def _ls(self, path: str) -> str:
        path = path.rstrip("/")
        rows = []
        if path in self.sizes:
            rows.append((path.rsplit("/", 1)[-1], self.sizes[path]))
        else:
            for p, n in self.sizes.items():
                if p.startswith(path + "/") and "/" not in p[len(path) + 1:]:
                    rows.append((p[len(path) + 1:], n))
        return "".join(f"-rw-r--r--    1 0        0        {n} Sep 27 05:40 {name}\n"
                       for name, n in sorted(rows))

    def _relay(self, cmd: str) -> tuple[int, str, str] | None:
        """``fbench-agent replay stream | iio_writedev`` on the TX board (rf.p25_corpus)."""
        sim = self.sim
        if "setsid sh -c" in cmd and "replay stream" in cmd:
            pl = json.loads(self.files["/tmp/fbench_relay/playlist.json"])
            bps = {"cs16": 4, "cs12": 3, "cs8": 2}[pl["format"]]
            samples = sum(int(it["zeros"]) if "zeros" in it else
                          self.sizes.get(it["path"], 0) // bps for it in pl["items"])
            dur = samples / float(pl["rate_hz"])
            start = sim.t + 0.5
            sim.relay = {"unit": self.unit, "id": pl["id"], "t_start": start,
                         "t_air0": start + 0.3, "seconds": dur, "samples": samples,
                         "rate": float(pl["rate_hz"]), "state": "prefill"}
            sim.cyclic[self.unit] = 0.0
            return 0, "", ""
        r = sim.relay
        if r is None or r["unit"] != self.unit:
            return None
        if "/tmp/fbench_relay/status.json" in cmd or "/tmp/fbench_relay/report.json" in cmd:
            if r["state"] not in ("stopped", "done"):
                el = sim.t - r["t_start"]
                r["state"] = "prefill" if el < 0 else "streaming" if el < r["seconds"] else "done"
                if r["state"] == "done":
                    sim.cyclic.pop(self.unit, None)
            el = max(0.0, min(sim.t - r["t_start"], r["seconds"]))
            st = {"state": r["state"], "now_unix": 1_790_000_005.0 + sim.t,
                  "t_first_out_unix": 1_790_000_005.0 + r["t_start"]
                  if sim.t >= r["t_start"] else None,
                  "prefill_s": 0.5, "samples_out": int(el * r["rate"]),
                  "underruns": sim.relay_underruns, "underrun_ms": 120.0 * sim.relay_underruns,
                  "ring_min_fill_s": 9.5, "read_max_ms": 850.0, "read_mbs": 21.0,
                  "zero_samples": 0,
                  "complete": r["state"] == "done"}
            return 0, json.dumps(st) + "\n", ""
        if "pkill -x iio_writedev" in cmd:
            if r["state"] != "done":
                r["state"] = "stopped"
            sim.cyclic.pop(self.unit, None)
            return 0, "", ""
        return None

    def run(self, cmd: str, timeout: float) -> tuple[int, str, str]:
        self._check()
        self.commands.append(cmd)
        if "fbench-agent" in cmd and self.unit not in self.sim.agent_units:
            return 127, "", "fbench-agent: not found"
        rel = self._relay(cmd)
        if rel is not None:
            return rel
        m = re.search(r"ls -ln (\S+)", cmd)
        if m and "awk" not in cmd:
            return 0, self._ls(m.group(1)), ""
        m = re.match(r"mv -f (\S+) (\S+)$", cmd)
        if m:
            for d in (self.sizes, self.files):
                if m.group(1) in d:
                    d[m.group(2)] = d.pop(m.group(1))
            return 0, "", ""
        if cmd.startswith("df -k"):
            return 0, "/dev/mmcblk0p1  62000000  5000000  57000000   8% /mnt/sd\n", ""
        if "MemAvailable" in cmd:
            return 0, "MemAvailable:     880000 kB\n", ""
        if cmd.startswith("T0=$(date"):
            n = int(re.search(r"-s (\d+)", cmd).group(1))
            fs = self.sim.num(self.unit, PHY, "sampling_frequency", "voltage0", False)
            t0 = 1000.0 + self.sim.t
            dt = n / fs + 0.030
            self.sim.t += dt
            return 0, f"0 {t0:.9f} {t0 + dt:.9f}\n", ""
        if cmd.startswith("ping -c"):
            return 0, "round-trip min/avg/max = 0.281/0.402/0.612 ms\n", ""
        if "authorized_keys" in cmd:
            return 0, "linked\n", ""
        m = re.search(r"iio_readdev -u local: -b \d+ -s (\d+) \S+ voltage0 voltage1 > (\S+)", cmd)
        if m and "nohup" in cmd:
            path = m.group(2).rstrip(";'")
            phase = re.search(r"_([a-z]+)\.cs16", path).group(1)
            self.files[path] = self.sim.capture(self.unit, int(m.group(1)),
                                                self.sim.phase_step_in == phase)
            self.files[path + ".done"] = b"0\n"
            return 0, "", ""
        m = re.match(r"cat (\S+) 2>/dev/null", cmd)
        if m:
            data = self.files.get(m.group(1))
            return (0, data.decode(), "") if data is not None else (1, "", "")
        if "setsid sh -c" in cmd and "iio_writedev" in cmd:  # rf.p25_replay board stream
            self.sim.cyclic[self.unit] = 0.0
            return 0, "", ""
        if cmd.startswith("pgrep -x iio_writedev"):
            return 0, "streaming\n" if self.unit in self.sim.cyclic else "", ""
        if "pkill -x iio_writedev" in cmd:
            self.sim.cyclic.pop(self.unit, None)
            return 0, "", ""
        return 0, "", ""

    def spawn(self, cmd: str) -> FakePopen:
        self._check()
        self.commands.append(cmd)
        return FakePopen("")

    def put(self, local: Path, remote: str, timeout: float) -> None:
        self._check()
        self.puts.append((str(local), remote))

    def get(self, remote: str, local: Path, timeout: float) -> None:
        self._check()
        Path(local).parent.mkdir(parents=True, exist_ok=True)
        if remote in self.files:
            Path(local).write_bytes(self.files[remote])
        elif remote.endswith("telemetry.jsonl"):
            Path(local).write_text(telemetry_jsonl(), encoding="utf-8")
        else:
            Path(local).write_bytes(b"")


def telemetry_samples(n: int = 10, dip_every: float | None = None) -> list[dict[str, Any]]:
    out = []
    for i in range(n):
        irq = 1000.0
        if dip_every and i > 0 and i % int(dip_every) == 0:
            irq = 100.0
        out.append({"t": float(i), "xadc": {"temp_c": 48.0 + i * 0.01, "vccint": 1.0,
                                            "vccaux": 1.79, "vccbram": 1.0, "vccpint": 1.0,
                                            "vccpaux": 1.8, "vccoddr": 1.352},
                    "ad9361_temp_c": 41.0, "clk_freq_hz": 5000000.0,
                    "loadavg": [0.2, 0.2, 0.1], "irq_deltas": {"45": irq},
                    "mem_available_kb": 420000})
    return out


def telemetry_jsonl(n: int = 120, dip_every: float | None = None) -> str:
    return "\n".join(json.dumps(s) for s in telemetry_samples(n, dip_every)) + "\n"


class FakeAgent(AgentClient):
    """AgentClient answering from fixtures + simulation; records every call."""

    def __init__(self, cfg: BenchConfig, sim: BenchSim, ssh_for: Callable[[str], FakeSsh]) -> None:
        super().__init__(cfg, ssh_for)
        self.sim = sim
        self.calls: list[tuple[str, list[str]]] = []
        self.overrides: dict[str, Any] = {}
        self.ringv2_setup: list[str] = []  # last `hwval ringv2 setup` args

    def _key(self, args: list[str]) -> str:
        two = {"iio", "ad9361", "prbs", "ring", "mem", "sd", "net", "hwval", "tx", "maint",
               "boot", "reg"}
        if args[0] in two and len(args) > 1:
            if args[0] == "mem" and args[1] == "canary":
                return f"mem canary {args[2]}"
            if args[0] == "iio":
                return f"iio {args[1]} {args[2]}"
            if args[0] == "hwval" and args[1] in ("ringv2", "legacy", "mt", "evt") and \
                    len(args) > 2 and not args[2].startswith("--"):
                return f"hwval {args[1]} {args[2]}"
            return f"{args[0]} {args[1]}"
        return args[0]

    def run(self, unit: str, args: list[str], timeout: float | None = None) -> dict:
        args = [str(a) for a in args]
        self.calls.append((unit, args))
        if unit not in self.sim.reachable:
            raise TransportError(f"unit {unit} unreachable over SSH")
        if unit not in self.sim.agent_units:
            raise AgentUnsupported(f"fbench-agent not found on {unit}")
        key = self._key(args)
        ov = self.overrides.get(key)
        if ov is not None:
            reply = ov(unit, args) if callable(ov) else copy.deepcopy(ov)
            if isinstance(reply, Exception):
                raise reply
            if not reply.get("ok", True):
                from fbench.agent import check_reply

                return check_reply(unit, args, reply, 1)
            return reply
        return self._default(unit, key, args)

    def spawn(self, unit: str, args: list[str]) -> AgentProcess:
        reply = self.run(unit, args)
        return AgentProcess(unit, [str(a) for a in args], FakePopen(json.dumps(reply)))

    @staticmethod
    def _opt(args: list[str], name: str, default: str | None = None) -> str | None:
        return args[args.index(name) + 1] if name in args else default

    def _default(self, unit: str, key: str, args: list[str]) -> dict:
        sim = self.sim
        if key == "version":
            return fixture("version")
        if key == "info":
            return fixture("info_hwval" if sim.images[unit] == "hwval" else "info_p25")
        if key == "audit":
            return fixture("audit")
        if key == "reg read":
            core, reg = self._opt(args, "--core"), self._opt(args, "--reg")
            return {"ok": True, "value": hex(sim.regs.get((unit, core, reg), 0))}
        if key == "reg write":
            core, reg = self._opt(args, "--core"), self._opt(args, "--reg")
            sim.regs[(unit, core, reg)] = int(self._opt(args, "--value"), 0)
            return {"ok": True}
        if key == "reg dump":
            regs = fixture("audit")["regs"]
            return {"ok": True, "regs": regs}
        if key == "telemetry":
            secs = float(self._opt(args, "--seconds", "1"))
            if "--jsonl" in args:
                return {"ok": True, "jsonl": self._opt(args, "--jsonl"),
                        "samples_written": int(secs)}
            return {"ok": True, "samples": telemetry_samples(max(2, int(secs))), "events": []}
        if key in ("iio attr get", "iio debug get"):
            dev = self._opt(args, "--dev")
            return {"ok": True, "value": sim.get_attr(unit, dev, self._opt(args, "--attr"),
                                                      self._opt(args, "--chan"),
                                                      "--out" in args)}
        if key in ("iio attr set", "iio debug set"):
            dev = self._opt(args, "--dev")
            sim.set_attr(unit, dev, self._opt(args, "--attr"), self._opt(args, "--value"),
                         self._opt(args, "--chan"), "--out" in args)
            return {"ok": True}
        if key == "eyescan":
            return fixture("eyescan_ad9361" if self._opt(args, "--mode") == "ad9361"
                           else "eyescan_idelay")
        if key == "prbs soak":
            return fixture("prbs_soak")
        if key == "txlink":
            return fixture("txlink_fpga" if "fpga-loopback" in args else "txlink_loopback")
        if key == "ring check":
            return self._ring_check(args)
        if key == "mem test":
            return fixture("mem_test")
        if key.startswith("mem canary"):
            return fixture("mem_canary_fill" if key.endswith("fill") else "mem_canary_verify")
        if key == "mem bw":
            return fixture("mem_bw")
        if key == "sd bench":
            return fixture("sd_bench")
        if key == "net send":
            return fixture("net_send")
        if key == "net serve":
            return fixture("net_serve")
        if key == "tx off":
            sim.cyclic.pop(unit, None)
            sim.set_attr(unit, PHY, "hardwaregain", "-89.75", "voltage0", True)
            sim.regs[(unit, "adi_dac", "DAC_CHAN0_CNTRL_7")] = 3
            return fixture("tx_off")
        if key == "maint enter":
            return {"ok": True, "maintenance": True}
        if key == "maint exit":
            return {"ok": True, "maintenance": False}
        if key == "maint status":
            return fixture("maint_status")
        if key == "boot status":
            return fixture("boot_status")
        if key in ("boot select", "boot install"):
            if key == "boot select":
                sim.images[unit] = args[2]
            return {"ok": True, "changed": True, "reboot_required": True}
        if key.startswith("hwval"):
            if sim.images[unit] != "hwval":
                return check_wrong_image(unit, args)
            return self._hwval(key, args)
        raise AgentError(f"fake agent: unhandled {args}")

    def _ring_check(self, args: list[str]) -> dict:
        """Wideband ring: laps above (N-1) x T_buf; ring v2: per the last setup."""
        rep = fixture("ring_check")
        rep["ring"] = self._opt(args, "--ring")
        stalls = [int(x) for x in (self._opt(args, "--stall-ms") or "").split(",") if x]
        threshold = (rep["num_buffers"] - 1) * rep["subbuf_period_ms"]
        setup = self.ringv2_setup
        protect = "--protect" in setup
        if stalls:
            rep["stalls"] = []
            for st in stalls:
                lap = st >= threshold and not protect
                rep["stalls"].append({"stall_ms": st, "anomalies_after": {"lap": int(lap)},
                                      "lost_units_after": 4096 if lap else 0})
                if lap:
                    rep["counts"]["lap"] += 1
                    rep["lost_bytes"] += 16384
        if rep["ring"] == "hwval-v2":
            rep["ringv2_headers"] = 25 if "--header" in setup else 0
            rate = float(self._opt(setup, "--rate-mbs", "32") or 32)
            rep["ringv2_pads"] = 7 if rate < 1 else 0
        return rep

    def _hwval(self, key: str, args: list[str]) -> dict:
        if key == "hwval ringv2 setup":
            self.ringv2_setup = list(args)
            return fixture("hwval_ringv2_setup")
        if key == "hwval ringv2 stop":
            rep = fixture("hwval_ringv2_stop")
            if "--protect" in self.ringv2_setup:
                acct = rep["accounting"]
                acct["drop_protect"] = 400
                acct["data_words_written"] = acct["words_in"] - 400
            self.ringv2_setup = []
            return rep
        if key in ("hwval legacy setup", "hwval legacy stop"):
            return fixture("hwval_legacy_" + key.split()[-1])
        if key == "hwval mt run":
            return fixture("hwval_mt")
        if key == "hwval evt drain":
            return fixture("hwval_evt_drain")
        if key in ("hwval evt enable", "hwval evt disable"):
            return {"ok": True, "ctrl": "0x1", "level": 0, "overflows": 0, "current": "0x03"}
        op = key.split()[1]
        return fixture(f"hwval_{op}")


def check_wrong_image(unit: str, args: list[str]) -> dict:
    from fbench.agent import check_reply

    return check_reply(unit, args, {"ok": False, "error": "hwval core not present",
                                    "code": "wrong_image"}, 3)


class FakeHttp:
    def __init__(self, sim: BenchSim, unit: str) -> None:
        self.sim, self.unit = sim, unit
        self.calls: list[str] = []
        self.base_t = 0.0

    # -- rf.p25_corpus: frames the fake DUT decodes from the relay's stream --------
    def _aired(self) -> list[tuple]:
        r, fn = self.sim.relay, self.sim.corpus_fn
        if not r or fn is None:
            return []
        s_now = min(self.sim.t, r["t_start"] + r["seconds"]) - r["t_air0"]
        return [f for f in fn(r["id"]) if f[0] + 0.4 <= s_now and f[3] not in self.sim.corpus_drop
                and not f[4]]

    def _corpus_calls(self) -> list[dict[str, Any]]:
        r = self.sim.relay
        by: dict[str, list[tuple]] = {}
        for f in self._aired():
            by.setdefault(f[3], []).append(f)
        out = []
        for k, (tid, fr) in enumerate(sorted(by.items(), key=lambda x: x[1][0][0])):
            t0 = r["t_air0"] + fr[0][0] - 0.4
            dur = fr[-1][0] - fr[0][0] + 0.02
            z = self.sim.corpus_imbe_zero
            out.append({"call_id": 1000 + k, "tg": fr[0][1], "source": fr[0][5],
                        "freq_hz": 858437500, "started_unix_ms": int((1_790_000_000 + t0) * 1000),
                        "open_ms": int((dur + 2.5) * 1000), "voice_ms": 0 if z else int(dur * 1000),
                        "imbe": 0 if z else len(fr), "ldu": 0 if z else len(fr) // 9,
                        "close_reason": "call_end", "not_followed": None, "encrypted": False})
        return out

    def get_json(self, path: str, params: dict | None = None, timeout: float | None = None
                 ) -> Any:
        if self.unit not in self.sim.reachable or self.sim.images[self.unit] != "p25":
            raise TransportError(f"HTTP to {self.unit} refused")
        self.calls.append(path)
        if path == "/api/system":
            return {"build": "2026-09-20-p25", "nac": "8A1", "wacn": "BEE00"}
        if path == "/api/imbe_dump":
            fr = self._aired()[-128:]
            if self.sim.corpus_imbe_zero:
                fr = []
            flip = (1 << self.sim.corpus_hex_flip) - 1
            return {"count": len(fr), "frames": [{"talkgroup": f[1], "encrypted": False,
                                                  "hex": format(int(f[2], 16) ^ flip, "036x")}
                                                 for f in fr]}
        if path == "/api/ui/calls":
            return {"items": list(reversed(self._corpus_calls()))}
        if path == "/api/ui/state":
            return {"now_unix_ms": int((1_790_000_000 + self.sim.t) * 1000), "clock_valid": True}
        if path == "/api/monitor":
            return {"talkgroups": []}
        if path == "/api/traffic" and params:
            self.sim.traffic_calls.append({k: str(v) for k, v in params.items()})
            for k, key in (("follower", "follower_enabled"), ("lock", "lock_freq")):
                if k in params:
                    self.sim.traffic[key] = str(params[k]) in ("on", "1", "true")
        if path == "/api/decoder_reset":
            self.base_t = self.sim.t
            return {"ok": True}
        el = self.sim.t - self.base_t
        live = any(self.sim.tx_source(u) for u in self.sim.cfg.units if u != self.unit)
        r = self.sim.decode_rate if live else 0.0
        if path == "/api/decoder_compare":
            return {"ps_lsm": {"tsbk_crc_ok": int(r * el), "tsbk_block_attempts": int(r * el * 1.1),
                               "nid_decoded_ok": int(10 * el * (r > 0)),
                               "nid_attempts": int(11 * el * (r > 0))}}
        if path == "/api/stats":
            return {"dibit_count": int(4800 * el), "rx_rssi_db": 100.0}
        if path == "/api/traffic":
            f = self.sim.imbe_rate * el if live else 0.0
            f += len(self._aired())
            park = self.sim.corpus_park
            r = self.sim.relay
            parked = park is not None and r is not None and self.sim.t >= r["t_air0"] + 1.0
            return {"imbe": {"ldu1_count": int(f / 18), "ldu2_count": int(f / 18),
                             "hdu_count": 0, "imbe_frames_extracted": int(f)},
                    **self.sim.traffic,
                    "current_frequency_hz": park[0] if parked else 858100000,
                    "current_talkgroup": park[1] if parked else None}
        return {}

    def post_json(self, path: str, params: dict | None = None, body: Any = None,
                  timeout: float | None = None) -> Any:
        return self.get_json(path, params, timeout)


@dataclass
class FakePort:
    device: str
    vid: int | None
    description: str = ""
    serial_number: str | None = None
    location: str | None = None
    interface: str | None = None


BOOT_LOG = [
    "U-Boot 2016.07 (Apr 30 2026 - 10:00:00 +0000)",
    "Booting Linux on physical CPU 0x0",
    "Linux version 6.1.0-tezuka (builder@host) #1 SMP",
    "usb 1-1: new high-speed USB device; g_ether gadget: using random self ethernet address",
    "Starting iiod: OK",
    "Starting p25-httpd: OK",
    "Welcome to Tezuka",
    "fishball-p25 login:",
]


class FakeSerial:
    def __init__(self, lines: list[str]) -> None:
        self.lines = [ln.encode() + b"\r\n" for ln in lines]
        self.written: list[bytes] = []

    def readline(self) -> bytes:
        return self.lines.pop(0) if self.lines else b""

    def write(self, data: bytes) -> None:
        self.written.append(data)

    def close(self) -> None:
        pass


class FakeServices:
    def __init__(self, cfg: BenchConfig, sim: BenchSim | None = None) -> None:
        self.cfg = cfg
        self.sim = sim or BenchSim(cfg)
        self._ssh = {u: FakeSsh(self.sim, u) for u in cfg.units}
        self._iio = {u: FakeIio(self.sim, u) for u in cfg.units}
        self._http = {u: FakeHttp(self.sim, u) for u in cfg.units}
        self.agent = FakeAgent(cfg, self.sim, self.ssh)
        self.local_cmds: list[list[str]] = []
        self.paramiko_calls: list[tuple[str, list[str]]] = []
        self.serial_lines = list(BOOT_LOG)
        self.t0 = datetime(2026, 9, 26, 15, 30, 0, tzinfo=timezone(timedelta(hours=-4)))

    def ssh(self, unit: str) -> FakeSsh:
        self.cfg.unit(unit)
        return self._ssh[unit]

    def http(self, unit: str, timeout: float = 5.0) -> FakeHttp:
        return self._http[unit]

    def iio(self, unit: str) -> FakeIio:
        return self._iio[unit]

    def ws_audio(self, unit: str) -> "FakeWsAudio":
        return FakeWsAudio(self.sim)

    def ping(self, host: str, count: int = 1, timeout_s: float = 1.0) -> float | None:
        return 0.8

    def run_local(self, argv: list[str], timeout: float) -> CmdResult:
        self.local_cmds.append(list(argv))
        if argv[:2] == ["route", "print"] or argv[:2] == ["ip", "route"]:
            text = ("          10.25.0.0    255.255.255.0      192.168.2.1     192.168.2.10     26\n"
                    "10.25.0.0/24 via 192.168.2.1 dev usb0\n")
            return CmdResult(0, text, "", 0.01)
        return CmdResult(0, "", "", 0.01)

    def paramiko_exec(self, unit: str, password: str, commands: list[str],
                      timeout: float = 30.0) -> list[tuple[int, str, str]]:
        self.paramiko_calls.append((unit, commands))
        return [(0, "", "")] * (len(commands) - 1) + [(0, "linked\n", "")]

    def tcp_send(self, host: str, port: int, mb: int, timeout: float = 60.0) -> dict[str, Any]:
        return {"bytes": mb << 20, "seconds": 1.2, "mbs": (mb << 20) / 1.2 / 1e6}

    def list_serial_ports(self) -> list[FakePort]:
        return [FakePort("COM7", 0x0403, "USB Serial Port (FT2232H B)"),
                FakePort("COM1", None, "Communications Port")]

    def serial_open(self, port: str, baud: int, timeout: float = 0.2) -> FakeSerial:
        return FakeSerial(self.serial_lines)

    def sleep(self, seconds: float) -> None:
        self.sim.t += max(0.0, seconds)

    def monotonic(self) -> float:
        # Advance a little on every read so polling loops terminate.
        self.sim.t += 0.001
        return self.sim.t

    def now(self) -> datetime:
        return self.t0 + timedelta(seconds=self.sim.t)


# ---------------------------------------------------------------------------
# pytest fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def cfg_path(tmp_path: Path) -> Path:
    return write_config(tmp_path, cabled=True)


@pytest.fixture
def cfg(cfg_path: Path) -> BenchConfig:
    from fbench.config import load_config

    return load_config(cfg_path)


@pytest.fixture
def services(cfg: BenchConfig) -> FakeServices:
    return FakeServices(cfg)


@pytest.fixture
def cli(cfg_path: Path, services: FakeServices, capsys: pytest.CaptureFixture
        ) -> Callable[..., tuple[int, dict]]:
    """Run the CLI with --json against the fakes; returns (exit code, JSON)."""
    from fbench.cli import main

    def run(*argv: str, config: Path | None = None) -> tuple[int, dict]:
        args = list(argv)
        if "--" in args:
            i = args.index("--")
            args = args[:i] + ["--json", "--config", str(config or cfg_path)] + args[i:]
        else:
            args += ["--json", "--config", str(config or cfg_path)]
        code = main(args, services_factory=lambda c: services)
        out = capsys.readouterr().out
        return code, json.loads(out)

    return run


def parse_cfg(data: dict[str, Any]) -> BenchConfig:
    return parse_config(data)


# ---------------------------------------------------------------------------
# rf.p25_corpus: a tiny synthetic SDRTrunk corpus and the fake DUT's view of it
# ---------------------------------------------------------------------------


class FakeWsAudio:
    """``/ws/audio``: PCM of what the fake DUT decodes (``sim.corpus_audio_fn``)."""

    def __init__(self, sim: BenchSim) -> None:
        import time as _time

        self.sim = sim
        self.wall0, self.t0 = _time.time(), sim.t

    def stop(self) -> dict[str, Any]:
        r, fn = self.sim.relay, self.sim.corpus_audio_fn
        chunks = []
        if r is not None and fn is not None:
            for s, pcm in fn(r["id"]):
                t = r["t_air0"] + s + 0.3
                if t <= self.sim.t:
                    chunks.append((self.wall0 + (t - self.t0), pcm))
        return {"chunks": chunks, "texts": [], "error": None}


def _wav(path: Path, iq: np.ndarray, rate: int) -> None:
    import wave as _wave

    inter = np.empty(2 * iq.size, dtype="<i2")
    inter[0::2], inter[1::2] = np.round(iq.real), np.round(iq.imag)
    with _wave.open(str(path), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)
        wf.setframerate(rate)
        wf.writeframes(inter.tobytes())


def _log(path: Path, bit0: float, msgs: list[tuple[str, int]], latency: float = 0.05) -> None:
    """decoded_messages log: (body, bits) messages on a 9600 bit/s clock from ``bit0``."""
    import time as _time

    lines = ["DECODED Message Logger", ""]
    cum = 0
    for body, bits in msgs:
        cum += bits
        t = bit0 + cum / 9600.0 + latency
        lines.append(f"{_time.strftime('%Y%m%d %H%M%S', _time.localtime(t))},PASSED,{body}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def make_corpus(tmp: Path, tone: bool = True) -> dict[str, Any]:
    """CC + one traffic recording, a clear TG 300 call (focus) and an encrypted TG 402
    call inside it, a 400 kSPS capture covering both, logs and a manifest."""
    import time as _time

    from fbench.analysis import p25_corpus as pc

    rec, logs, caps = tmp / "recordings", tmp / "event_logs", tmp / "captures"
    for d in (rec, logs, caps):
        d.mkdir(parents=True, exist_ok=True)
    T0 = _time.mktime(_time.strptime("20260503084247", "%Y%m%d%H%M%S"))
    rng = np.random.default_rng(7)

    def noise(n: int, rms: float) -> np.ndarray:
        return rms / np.sqrt(2) * (rng.normal(size=n) + 1j * rng.normal(size=n))

    _wav(rec / "20260503_084247_860962500_Clay-County_Clay_LCN-11_3_baseband.wav",
         noise(24 * 50000, 600), 50000)
    cc = [("<-> SYNC LOSS - BITS PROCESSED [172]", 172)]
    for k in range(int(24 * 9600 / 360)):
        body = "NAC:2209/x8A1 TSBK1 RFSS_STATUS_BCST SYSTEM:2208/x8A0"
        if k == int(5.0 * 9600 / 360):
            body = "NAC:2209/x8A1 TSBK1 GRP_VCH_GRANT FM:1014 TO:300 CHAN:0-1193 PRI4 CIRCUIT"
        cc.append((body, 360))
    _log(logs / "20260503_084247.412_860962500_Hz_LCN-11_decoded_messages.log", T0 + 0.5, cc)
    _wav(rec / "20260503_084255_858437500_Clay-County_Clay_T-LCN-11_79_baseband.wav",
         noise(4 * 50000, 900), 50000)
    (rec / "20260503_084255_858437500_9600BPS_APCO25PHASE1_Clay-County_Clay_T-LCN-11_79.bits"
     ).write_bytes(b"\x00" * 4800)
    bit0 = T0 + 8.3
    tr = [("<-> SYNC LOSS - BITS PROCESSED [100]", 100)]
    tr += [("NAC:2209/x8A1 LDU1 VOICE GROUP VOICE CHANNEL USER FM:1014 TO:300", 1728)] * 18
    _log(logs / "20260503_084255.300_858437500_Hz_T-LCN-11_decoded_messages.log", bit0, tr)
    start = bit0 + pc.LOG_TO_WAV_S

    def frames(t0: float, n: int, seed: int) -> list[dict[str, Any]]:
        r = np.random.default_rng(seed)
        return [{"time": int(round((t0 + 0.02 * i) * 1000)),
                 "hex": r.bytes(18).hex().upper()} for i in range(n)]
    call = "20260503_084258_858437500_1_300_1014"
    (rec / f"{call}.mbe").write_text(json.dumps({
        "protocol": "APCO25-PHASE1", "call_type": "GROUP", "from": "1014", "to": "300",
        "encrypted": False, "frames": frames(start + 1.0 + pc.MBE_LAG_S, 90, 1)}))
    (rec / "20260503_084259_858437500_2_402_3412739_encrypted.mbe").write_text(json.dumps({
        "protocol": "APCO25-PHASE1", "from": "3412739", "to": "402", "encrypted": True,
        "frames": frames(start + 2.5 + pc.MBE_LAG_S, 30, 2)}))
    (rec / "20260503_084259_Clay-County_Clay_T-LCN-11__TO_300_FROM_1014.mp3").write_bytes(b"")
    cap_start = int(T0 + 5)
    _wav(caps / f"{cap_start}_858437500_400000_baseband.wav", noise(10 * 400000, 300), 400000)
    man = pc.build_manifest(caps, rec, logs, focus=(call,), plan_kw={
        "B": {"pre_s": 3.0, "post_s": 1.0}, "C": {"primer_s": 3.0, "gap_s": 1.0,
                                                  "batch_s": 60.0},
        "primer": {"min_len_s": 10.0}})
    if tone:
        man["focus_reference"] = {call: {"recording": "x", "mp3": [
            {"mp3": "ref.mp3", "seconds": 1.0, "tones_hz": [807.0, 1506.0], "tonal_frames": 44,
             "frames": 48}]}}
    path = tmp / "manifest.json"
    from fbench.util import dump_json

    dump_json(man, path)
    return {"manifest": path, "man": man, "call": call, "T0": T0}


def install_corpus(services: "FakeServices", manifest: dict[str, Any], mode: str,
                   a_unit: str = "whole", tone_gap_frames: tuple[int, ...] = ()) -> None:
    """Teach the fake DUT to 'decode' the relay's stream: every clear truth frame, at
    its stream time, plus the focus call's two-tone alert on /ws/audio."""
    from fbench import corpus as cp
    from fbench.analysis import p25_corpus as pc

    txs = pc.tx_index(manifest)
    cache: dict[str, list[tuple]] = {}

    def frames(item_id: str) -> list[tuple]:
        if item_id not in cache:
            item = cp.build_item(manifest, mode, item_id, a_unit=a_unit)
            rows = []
            for tid, fr in cp.truth_stream_frames(item, manifest).items():
                t = txs[tid]
                rows += [(s, t["tg"], h.lower(), tid, t["encrypted"], t["src"]) for s, h in fr]
            cache[item_id] = sorted(rows)
        return cache[item_id]

    def audio(item_id: str) -> list[tuple[float, bytes]]:
        out = []
        focus = set(manifest["focus"])
        fr = [f for f in frames(item_id) if txs[f[3]]["call"] in focus]
        for k, f in enumerate(fr):
            tone_hz = 1506.0 if (k // 15) % 2 == 0 else 807.0
            n = np.arange(160) + 160 * k
            amp = 0.0 if k in tone_gap_frames else 8000.0
            pcm = (amp * np.sin(2 * np.pi * tone_hz * n / 8000.0)).astype("<i2").tobytes()
            out.append((f[0], pcm))
        return out
    services.sim.corpus_fn = frames
    services.sim.corpus_audio_fn = audio
    services.sim.images["B"] = "p25"
    services.sim.agent_units = {"A", "B"}
    if mode == "C":
        plan = manifest["plans"]["C"]
        services.sim.corpus_park = (plan["channel_hz"], plan["primer"]["grant"]["tg"])
