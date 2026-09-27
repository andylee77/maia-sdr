"""Adapter for the on-board agent ``fbench-agent`` (design doc 5.3).

Everything the host assumes about the agent's command line and JSON replies
lives in this module, so the shapes can be reconciled with the Rust agent in
one place. :data:`CONTRACT` lists the argv each wrapper sends and the reply
keys the host reads (``fbench agent --contract`` prints it). It was checked
against ``bench/agent/src/cmd/*.rs`` on 2026-09-26.

Replies: ``{"ok": true, "cmd": "...", ...keys..., "warnings"?: [...],
"elapsed_s": N}``; errors ``{"ok": false, "cmd", "code", "error",
"detail"?}``. Error mapping:

- ``code`` in :data:`REFUSED_CODES` -> :class:`AgentRefused` (exit 4)
- ``code`` in :data:`UNSUPPORTED_CODES` or remote rc 127 -> :class:`AgentUnsupported` (exit 3)
- anything else (``usage``, ``error``, ``interrupted``) -> :class:`AgentError` (exit 2)

The agent rejects unknown options, so the host only sends options the agent
reads. Global options used: ``--run-id`` (set by the runner for the duration
of a run, so bulk files land in ``/mnt/sd/bench/runs/<run_id>/``).
Safety flags: ``--tx-ok`` is added only by code paths that already passed
the host TX interlock (TX-enabling ``iio attr set`` values and TX-affecting
register writes are refused by the agent without it).
"""

from __future__ import annotations

import json
import logging
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable

from .config import BenchConfig
from .errors import (
    AgentError,
    AgentRefused,
    AgentUnsupported,
    TransportTimeout,
)
from .transport import SshLike, shell_join

log = logging.getLogger("fbench.agent")

REFUSED_CODES = frozenset({"refused", "forbidden", "not_allowed", "safety", "interlock"})
UNSUPPORTED_CODES = frozenset({"unsupported", "unknown_command", "precondition", "no_device",
                               "wrong_image", "not_found"})

#: Agent contract assumed by the host: subcommand argv -> reply keys read.
CONTRACT: dict[str, dict[str, Any]] = {
    "global": {"argv": ["[--run-id ID]"], "reply": ["ok", "cmd", "warnings?", "elapsed_s"]},
    "version": {"argv": ["version"], "reply": ["version", "git", "build{}", "patterns[]"]},
    "info": {"argv": ["info"],
             "reply": ["model", "serial", "hw_model", "fw_version", "image "
                       "(p25|hwval|maia|factory|unknown)", "bitstream{product_id,name,fpga_dna?}",
                       "fpga_dna", "cmdline", "boot_medium", "uptime_s", "kernel",
                       "sd{mounted,total_mb,free_mb,...}", "maintenance", "build",
                       "sample_rate_hz", "rx_lo_hz"]},
    "audit": {"argv": ["audit"],
              "reply": ["pass", "checks[{name,addr,value,expected,ok,severity,detail}]",
                        "derived{ddr_mhz,cas_latency,taa_ns,...}", "regs{NAME: hex} (PL310 "
                        "names prefixed L2C_)", "xadc{temp_c,vccoddr,...}", "kmod",
                        "reserved_memory[]", "services"]},
    "reg": {"argv": ["reg", "read|write|dump", "--core", "C", "[--reg R]", "[--value V]",
                     "[--force-side-effects]", "[--tx-ok]"],
            "reply": ["read: value (int), hex, fields", "write: written",
                      "dump: regs{NAME: value}"]},
    "telemetry": {"argv": ["telemetry", "--seconds", "N", "--interval-ms", "M",
                           "[--jsonl FILE]"],
                  "reply": ["samples[{t, xadc{temp_c,vccint,vccaux,vccbram,vccpint,vccpaux,"
                            "vccoddr}|null, ad9361_temp_c, clk_freq_hz, loadavg, irq_deltas{} "
                            "(non-zero only), softirq_deltas{}, mem_available_kb}]",
                            "samples_truncated (last 10 kept with --jsonl)",
                            "events[{t,kind,detail}]", "jsonl (path); file = one sample per "
                            "line"]},
    "iio attr": {"argv": ["iio", "attr", "get|set", "--dev", "D", "[--chan C [--out]]",
                          "--attr", "A", "[--value V]", "[--tx-ok]"], "reply": ["value"]},
    "iio debug": {"argv": ["iio", "debug", "get|set", "--dev", "D", "--attr", "A",
                           "[--value V]", "[--tx-ok]"], "reply": ["value"]},
    "ad9361 spi": {"argv": ["ad9361", "spi", "read|write", "--addr", "A", "[--value V]"],
                   "reply": ["value"]},
    "eyescan": {"argv": ["eyescan", "--mode", "idelay|ad9361|2d", "--rate", "HZ",
                         "--dwell-ms", "D", "[--lanes 0,1,..]"],
                "reply": ["idelay: lanes[{lane, pass[32], window}], current_taps",
                          "ad9361: grid[clk][data] (16x16), chosen{clk,data}",
                          "2d: lanes[{lane, grid[data][tap]}]"]},
    "prbs soak": {"argv": ["prbs", "soak", "--seconds", "N", "--poll-ms", "P", "[--rate HZ]"],
                  "reply": ["seconds", "polls", "error_intervals", "oos_events",
                            "first_error_s"]},
    "txlink": {"argv": ["txlink", "--mode", "ad9361-loopback|fpga-loopback", "[--sweep]"],
               "reply": ["sweep[{delay,clk,data,clksel,errors}]", "chosen_delay ('0xNN')",
                         "errors_at_chosen", "errors", "samples_checked"]},
    "ring capture": {"argv": ["ring", "capture", "--ring", "R", "--bytes", "B", "--mapping",
                              "cached|uncached", "--out", "FILE"],
                     "reply": ["path (.sigmf-data)", "meta_path", "bytes", "sample_rate_hz"]},
    "ring check": {"argv": ["ring", "check", "--ring", "p25-wideband|hwval-legacy|hwval-v2",
                            "--pattern", "pn0fn|ramp64|tagged|prbs31|iqramp|tone",
                            "--seconds", "N", "[--stall-ms a,b,..]", "[--bist prbs|tone]",
                            "[--enable]", "[--release-reset]"],
                   "reply": ["bytes_checked", "subbuffers", "subbuf_period_ms", "num_buffers",
                             "lap_threshold_ms", "lost_bytes", "anomalies_total",
                             "counts{word_gap,lap,torn,stale_line,splice,bit_error,repeat}",
                             "anomalies[{class,t,...}]",
                             "stalls[{stall_ms,anomalies_after{},lost_units_after}]", "pass"]},
    "mem test": {"argv": ["mem", "test", "--anon-mb", "N", "--patterns", "P", "--passes", "K",
                          "[--cpu C]"],
                 "reply": ["errors", "bytes_tested", "seconds", "patterns[{name,errors}]"]},
    "mem canary": {"argv": ["mem", "canary", "fill|verify", "--region", "NAME"],
                   "reply": ["region", "bytes", "corrupt_words (verify)", "intact"]},
    "mem bw": {"argv": ["mem", "bw", "--size", "S"],
               "reply": ["results{cached_memcpy,...: MB/s}"]},
    "sd bench": {"argv": ["sd", "bench", "--mb", "N", "--bs", "K", "[--fsync]"],
                 "reply": ["write_mbs", "read_mbs", "write_lat_us{p50,p99,max}", "fsync_ms",
                           "slow"]},
    "net serve": {"argv": ["net", "serve", "--port", "P", "--mb", "N"],
                  "reply": ["bytes", "seconds", "mbs"]},
    "net send": {"argv": ["net", "send", "--host", "H", "--port", "P", "--mb", "N"],
                 "reply": ["bytes", "seconds", "mbs"]},
    "hwval id": {"argv": ["hwval", "id"],
                 "reply": ["id", "id_ok", "version", "features", "fpga_dna",
                           "snapshot{mask,ack,ok,dead_domains}", "all_clocks_alive"]},
    "hwval census": {"argv": ["hwval", "census", "--gate-ms", "G"],
                     "reply": ["clocks{name:{hz,alive,nominal_hz?,ppm_vs_fclk0?}}",
                               "ad9361_fs_hz", "resolution_ppm"]},
    "hwval ingest": {"argv": ["hwval", "ingest", "--seconds", "N", "[--prbs MODE --bist]",
                              "[--honor-valid]"],
                     "reply": ["samples", "valid_gap_cycles", "valid_gap_runs", "cdc_wrerr",
                               "window{i_min,..,clip_count,i_stuck0,i_stuck1,q_stuck0,q_stuck1}",
                               "prbs{checked,errors,oos_events,in_sync,ber}"]},
    "hwval ringv2": {"argv": ["hwval", "ringv2", "setup|stop|status", "--src S",
                              "--rate-mbs R", "[--protect] [--header] [--clear] [--enable]"],
                     "reply": ["regs{RINGV2_*}", "accounting{idle,words_in,data_words_written,"
                               "drop_full,drop_protect,ok}", "drained_idle (stop)"]},
    "hwval legacy": {"argv": ["hwval", "legacy", "setup|stop|status", "--src S",
                              "--rate-msps R", "[--clear] [--enable]"],
                     "reply": ["regs{LEGACY_WORDS_IN,LEGACY_WORDS_ACCEPTED,LEGACY_PACKER_OVF,"
                               "LEGACY_LAT_MAX,LEGACY_MAX_OUTSTANDING,...}", "loss_words",
                               "lat_max_us", "lat_cliff_us"]},
    "hwval mt": {"argv": ["hwval", "mt", "run", "--mt", "0|1", "--mode", "write-verify|...",
                          "--pattern", "prbs|...", "--burst-len", "B", "--outstanding", "O",
                          "--passes", "P", "--seed", "S", "--idle", "N", "[--seconds S]"],
                 "reply": ["err_count", "first_err{addr,expected,actual}", "bytes_wr",
                           "bytes_rd", "cycles", "wr_mbs", "rd_mbs", "wlat_max_ns",
                           "rlat_max_ns", "wlat_hist[16]", "rlat_hist[16]", "bresp_err",
                           "rresp_err", "guard_blocked", "refused", "pass"]},
    "hwval evt": {"argv": ["hwval", "evt", "enable|drain|disable", "[--mask M]"],
                  "reply": ["drain: events[{t_s,cycles,heartbeat,value}]", "overflows"]},
    "hwval contention": {"argv": ["hwval", "contention", "--seconds", "S", "--idle", "a,b,..",
                                  "--burst-len", "B"],
                         "reply": ["cells[{idle_cycles,offered_load,mt0_mbs,mt1_mbs,"
                                   "ringv2{drop_full_delta,lat_max_cycles,...}}]"]},
    "replay stream": {"argv": ["replay", "stream", "--playlist", "P", "[--ring-mb M]",
                               "[--prefill-mb M]", "[--status F]", "[--report F]",
                               "[--on-underrun wait|zero]", "[--out F]"],
                      "reply": ["stdout = int16 I/Q for iio_writedev; reply on stderr and in "
                                "--report", "state (done|stopped|downstream_closed|error)",
                                "complete", "samples_out", "prefill_s", "t_first_out_unix",
                                "ring_bytes", "ring_min_fill_s", "read_mbs", "read_max_ms",
                                "read_stalls[]", "underruns", "underrun_ms",
                                "underrun_events[]", "zero_samples", "skipped_samples"]},
    "replay check": {"argv": ["replay", "check", "--playlist", "P"],
                     "reply": ["bytes", "samples", "seconds", "mem_available_kb"]},
    "replay verify": {"argv": ["replay", "verify", "--file", "F", "[--sha256 H]"],
                      "reply": ["sha256", "match", "bytes", "read_mbs"]},
    "tx off": {"argv": ["tx", "off"], "reply": ["tx_atten_db", "actions[]"]},
    "maint": {"argv": ["maint", "enter|exit|status"],
              "reply": ["maintenance (bool)", "services"]},
    "boot": {"argv": ["boot", "status | select <name> | install <name> --from DIR "
                      "--sha256-boot H --sha256-dtb H"],
             "reply": ["status: active, images{name:{present,sha256_ok}}",
                       "select: image, changed, reboot_required", "install: staged, sha256"]},
}


def extract_json(stdout: str) -> dict | None:
    """Return the agent's JSON object from stdout (whole text or last line)."""
    text = stdout.strip()
    if not text:
        return None
    try:
        obj = json.loads(text)
        return obj if isinstance(obj, dict) else None
    except json.JSONDecodeError:
        pass
    for line in reversed(text.splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict):
                return obj
    return None


def as_int(value: Any) -> int:
    """Agent register values may be ints or ``"0x…"`` strings."""
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    return int(str(value), 0)


@dataclass
class AgentProcess:
    """A backgrounded agent invocation (e.g. ``net serve``)."""

    unit: str
    args: list[str]
    proc: subprocess.Popen

    def wait(self, timeout: float) -> dict:
        try:
            out, err = self.proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as exc:
            self.proc.kill()
            raise TransportTimeout(f"agent {' '.join(self.args)} on {self.unit} timed out") from exc
        text = out.decode("utf-8", "replace") if isinstance(out, bytes) else str(out)
        reply = extract_json(text)
        if reply is None:
            raise AgentError(f"agent on {self.unit} returned no JSON", stderr=str(err)[:300])
        return check_reply(self.unit, self.args, reply, self.proc.returncode or 0)

    def kill(self) -> None:
        if self.proc.poll() is None:
            self.proc.kill()


def check_reply(unit: str, args: list[str], reply: dict, rc: int) -> dict:
    if reply.get("ok") is True and rc == 0:
        return reply
    if reply.get("ok") is True:  # ok but non-zero rc: trust the JSON, note it
        log.warning("agent %s on %s: ok=true but rc=%d", " ".join(args), unit, rc)
        return reply
    msg = str(reply.get("error", "agent reported failure"))
    code = str(reply.get("code", "")).lower()
    detail = {"unit": unit, "args": args, "reply": reply}
    if code in REFUSED_CODES:
        raise AgentRefused(f"agent on {unit} refused: {msg}", **detail)
    if code in UNSUPPORTED_CODES:
        raise AgentUnsupported(f"agent on {unit}: {msg}", **detail)
    raise AgentError(f"agent on {unit} failed: {msg}", **detail)


def _flag(on: bool, name: str) -> list[str]:
    return [name] if on else []


class AgentClient:
    """Run ``fbench-agent`` on a unit over SSH and parse its single JSON reply."""

    def __init__(self, cfg: BenchConfig, ssh_for: Callable[[str], SshLike]) -> None:
        self.cfg = cfg
        self._ssh_for = ssh_for
        #: Set by the runner while a test runs (agent ``--run-id``).
        self.run_id: str | None = None

    # -- core ----------------------------------------------------------------
    @property
    def remote_binary(self) -> str:
        return self.cfg.agent.remote_binary

    def command(self, args: Iterable[str]) -> str:
        argv = [self.remote_binary, *[str(a) for a in args]]
        if self.run_id:
            argv += ["--run-id", self.run_id]
        argv += list(self.cfg.agent.extra_args)
        return shell_join(argv)

    def run(self, unit: str, args: list[str], timeout: float | None = None) -> dict:
        """Run one agent subcommand; returns the reply dict (``ok`` is true)."""
        args = [str(a) for a in args]
        t = timeout if timeout is not None else self.cfg.agent.default_timeout_s
        rc, out, err = self._ssh_for(unit).run(self.command(args), t)
        if rc == 127:
            raise AgentUnsupported(
                f"fbench-agent not found on {unit} ({self.remote_binary}); run `fbench setup agent`",
                unit=unit,
            )
        reply = extract_json(out)
        if reply is None:
            raise AgentError(
                f"agent on {unit} returned no JSON (rc={rc})",
                unit=unit, args=args, stderr=err.strip()[:300], stdout=out.strip()[:300],
            )
        return check_reply(unit, args, reply, rc)

    def spawn(self, unit: str, args: list[str]) -> AgentProcess:
        args = [str(a) for a in args]
        return AgentProcess(unit, args, self._ssh_for(unit).spawn(self.command(args)))

    # -- identity / audit ----------------------------------------------------
    def version(self, unit: str, timeout: float = 15.0) -> dict:
        return self.run(unit, ["version"], timeout)

    def info(self, unit: str, timeout: float = 20.0) -> dict:
        return self.run(unit, ["info"], timeout)

    def audit(self, unit: str, timeout: float = 60.0) -> dict:
        return self.run(unit, ["audit"], timeout)

    # -- registers -----------------------------------------------------------
    def reg_read(self, unit: str, core: str, reg: str, force_side_effects: bool = False) -> int:
        """Read one register. Read-to-clear registers need ``force_side_effects``."""
        args = ["reg", "read", "--core", core, "--reg", reg]
        args += _flag(force_side_effects, "--force-side-effects")
        return as_int(self.run(unit, args)["value"])

    def reg_write(self, unit: str, core: str, reg: str, value: int, tx_ok: bool = False) -> dict:
        """Write one register; ``tx_ok`` only after the host TX interlock passed."""
        return self.run(unit, ["reg", "write", "--core", core, "--reg", reg, "--value", hex(value),
                               *_flag(tx_ok, "--tx-ok")])

    def reg_dump(self, unit: str, core: str) -> dict[str, int]:
        reply = self.run(unit, ["reg", "dump", "--core", core], 60.0)
        out = {}
        for k, v in dict(reply.get("regs", {})).items():
            try:
                out[k] = as_int(v)
            except (TypeError, ValueError):
                continue
        return out

    # -- telemetry -----------------------------------------------------------
    def telemetry(self, unit: str, seconds: float, interval_ms: int,
                  jsonl: str | None = None) -> dict:
        args = ["telemetry", "--seconds", f"{seconds:g}", "--interval-ms", str(interval_ms)]
        if jsonl:
            args += ["--jsonl", jsonl]
        return self.run(unit, args, seconds + 60.0)

    # -- IIO / AD9361 --------------------------------------------------------
    def iio_attr_get(self, unit: str, dev: str, attr: str, chan: str | None = None,
                     output: bool = False) -> str:
        args = ["iio", "attr", "get", "--dev", dev]
        if chan is not None:
            args += ["--chan", chan] + (["--out"] if output else [])
        reply = self.run(unit, args + ["--attr", attr])
        return str(reply.get("value", ""))

    def iio_attr_set(self, unit: str, dev: str, attr: str, value: Any, chan: str | None = None,
                     output: bool = False, tx_ok: bool = False) -> dict:
        args = ["iio", "attr", "set", "--dev", dev]
        if chan is not None:
            args += ["--chan", chan] + (["--out"] if output else [])
        return self.run(unit, args + ["--attr", attr, "--value", str(value),
                                      *_flag(tx_ok, "--tx-ok")])

    def iio_debug_get(self, unit: str, dev: str, attr: str) -> str:
        reply = self.run(unit, ["iio", "debug", "get", "--dev", dev, "--attr", attr])
        return str(reply.get("value", ""))

    def iio_debug_set(self, unit: str, dev: str, attr: str, value: Any, tx_ok: bool = False
                      ) -> dict:
        return self.run(unit, ["iio", "debug", "set", "--dev", dev, "--attr", attr,
                               "--value", str(value), *_flag(tx_ok, "--tx-ok")])

    def spi_read(self, unit: str, addr: int) -> int:
        reply = self.run(unit, ["ad9361", "spi", "read", "--addr", hex(addr)])
        return as_int(reply["value"])

    def spi_write(self, unit: str, addr: int, value: int) -> dict:
        return self.run(unit, ["ad9361", "spi", "write", "--addr", hex(addr),
                               "--value", hex(value)])

    # -- interface tests -----------------------------------------------------
    def eyescan(self, unit: str, mode: str, rate_hz: int, dwell_ms: int,
                lanes: list[int] | None = None) -> dict:
        args = ["eyescan", "--mode", mode, "--rate", str(int(rate_hz)), "--dwell-ms", str(dwell_ms)]
        if lanes:
            args += ["--lanes", ",".join(str(x) for x in lanes)]
        # 32 taps x 16 delays x dwell, plus retune overhead.
        budget = 60.0 + (32 * 16 if mode == "2d" else 256) * dwell_ms / 1000.0 * 2
        return self.run(unit, args, budget)

    def prbs_soak(self, unit: str, seconds: float, poll_ms: int,
                  rate_hz: int | None = None) -> dict:
        args = ["prbs", "soak", "--seconds", f"{seconds:g}", "--poll-ms", str(poll_ms)]
        if rate_hz:
            args += ["--rate", str(int(rate_hz))]
        return self.run(unit, args, seconds + 60.0)

    def txlink(self, unit: str, mode: str, sweep: bool = False) -> dict:
        args = ["txlink", "--mode", mode] + _flag(sweep, "--sweep")
        return self.run(unit, args, 180.0)

    # -- ring ------------------------------------------------------------------
    def ring_capture(self, unit: str, ring: str, nbytes: int, mapping: str, out: str) -> dict:
        return self.run(unit, ["ring", "capture", "--ring", ring, "--bytes", str(nbytes),
                               "--mapping", mapping, "--out", out], 120.0)

    def ring_check(self, unit: str, ring: str, pattern: str, seconds: float,
                   stall_ms: list[int] | int | None = None, bist: str | None = None,
                   enable: bool = False, release_reset: bool = False) -> dict:
        args = ["ring", "check", "--ring", ring, "--pattern", pattern, "--seconds", f"{seconds:g}"]
        if stall_ms is not None:
            stalls = stall_ms if isinstance(stall_ms, list) else [stall_ms]
            args += ["--stall-ms", ",".join(str(int(s)) for s in stalls)]
        if bist:
            args += ["--bist", bist]
        args += _flag(enable, "--enable") + _flag(release_reset, "--release-reset")
        return self.run(unit, args, seconds + 60.0)

    # -- memory / storage / network ----------------------------------------------
    def mem_test(self, unit: str, anon_mb: int, patterns: str, passes: int,
                 cpu: int | None = None) -> dict:
        args = ["mem", "test", "--anon-mb", str(anon_mb), "--passes", str(passes)]
        # The agent runs every pattern when --patterns is omitted; it has no
        # literal "all" pattern name.
        if patterns and patterns != "all":
            args += ["--patterns", patterns]
        if cpu is not None:
            args += ["--cpu", str(cpu)]
        return self.run(unit, args, 60.0 + anon_mb * passes * 2.0)

    def mem_canary(self, unit: str, action: str, region: str) -> dict:
        return self.run(unit, ["mem", "canary", action, "--region", region], 120.0)

    def mem_bw(self, unit: str, size: str) -> dict:
        return self.run(unit, ["mem", "bw", "--size", size], 180.0)

    def sd_bench(self, unit: str, mb: int, bs_kb: int, fsync: bool = True) -> dict:
        args = ["sd", "bench", "--mb", str(mb), "--bs", str(bs_kb)] + _flag(fsync, "--fsync")
        return self.run(unit, args, 60.0 + mb * 0.5)

    def net_serve(self, unit: str, port: int, mb: int) -> AgentProcess:
        return self.spawn(unit, ["net", "serve", "--port", str(port), "--mb", str(mb)])

    def net_send(self, unit: str, host: str, port: int, mb: int) -> dict:
        return self.run(unit, ["net", "send", "--host", host, "--port", str(port),
                               "--mb", str(mb)], 60.0 + mb * 0.5)

    # -- Tier 1 ------------------------------------------------------------------
    def hwval(self, unit: str, op: str, args: list[str] | None = None,
              timeout: float = 120.0) -> dict:
        return self.run(unit, ["hwval", op, *(args or [])], timeout)

    # -- safety / maintenance / boot -------------------------------------------
    def tx_off(self, unit: str) -> dict:
        return self.run(unit, ["tx", "off"], 30.0)

    def maint(self, unit: str, action: str) -> dict:
        if action not in ("enter", "exit", "status"):
            raise ValueError(action)
        return self.run(unit, ["maint", action], 60.0)

    def boot(self, unit: str, action: str, image: str | None = None, src: str | None = None,
             sha256_boot: str | None = None, sha256_dtb: str | None = None) -> dict:
        """``boot status`` / ``boot select <name>`` / ``boot install <name> --from DIR``.

        ``install`` without ``src`` is refused here: the agent treats it as
        ``select`` (it would overwrite the SD boot files).
        """
        if action == "install" and not src:
            raise AgentError("boot install needs a staging directory (--from)")
        args = ["boot", action] + ([image] if image else [])
        if src:
            args += ["--from", src]
        if sha256_boot:
            args += ["--sha256-boot", sha256_boot]
        if sha256_dtb:
            args += ["--sha256-dtb", sha256_dtb]
        return self.run(unit, args, 180.0)

    # -- deployment --------------------------------------------------------------
    def deploy(self, unit: str, local_binary: Path, share_files: list[Path],
               timeout: float = 120.0) -> dict:
        """Copy the agent binary and register maps to ``/mnt/sd/bench``."""
        root = self.cfg.agent.remote_root
        ssh = self._ssh_for(unit)
        dirs = " ".join(f"{root}/{d}" for d in ("bin", "share", "keys", "runs", "stimulus",
                                                   "images/p25", "images/hwval"))
        rc, _, err = ssh.run(f"mkdir -p {dirs}", timeout)
        if rc != 0:
            raise AgentError(f"cannot create {root} on {unit}: {err.strip()}")
        tmp = f"{self.remote_binary}.new"
        ssh.put(Path(local_binary), tmp, timeout)
        rc, _, err = ssh.run(f"chmod 755 {tmp} && mv -f {tmp} {self.remote_binary}", timeout)
        if rc != 0:
            raise AgentError(f"cannot install agent on {unit}: {err.strip()}")
        for f in share_files:
            ssh.put(Path(f), f"{root}/share/{Path(f).name}", timeout)
        ver = self.version(unit)
        return {"unit": unit, "binary": self.remote_binary,
                "share": [Path(f).name for f in share_files], "version": ver}
