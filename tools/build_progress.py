#!/usr/bin/env python3
"""Live-tail Vivado build logs and emit clean phase-by-phase progress.

`build_fpga.bat --p25` produces a noisy ~5000-line Vivado log. Most of
it is XDC constraint parsing and AXI interconnect boilerplate that a
human doesn't need to see during a build. This script tails the raw
log (both the outer `bake.log` and the inner
`fishball_p25.runs/impl_1/runme.log` that impl spawns) and prints
one line per milestone — the same progression pattern Claude's
operator-view reporting uses during a bake.

Usage
-----

As a live wrapper (Windows / Git Bash):

    ./build_fpga.bat --p25 2>&1 | tee bake.log | python tools/build_progress.py

Or against an in-progress log after the fact:

    python tools/build_progress.py --tail bake.log

Or against a completed log to replay the milestones:

    python tools/build_progress.py --replay bake.log

Output
------

    [00:00] synth   start
    [00:38] synth   done   (0 err, 1040 warn)
    [00:38] opt     start
    [01:09] opt     done   (31s, 0 err)
    [01:09] place   start
    [02:40] place   done   (1m31s, WNS +0.532 ns)
    [02:47] phys    done   (7s, skipped - WNS already positive)
    [02:47] route   start
    [04:35] route   done   (1m48s, WNS +0.303 ns, WHS +0.012 ns)
    [04:59] bitstream done  (24s)
    [05:06] xsa     written
    [05:06] BUILD SUCCESSFUL

Each progression line is timestamped relative to the first seen
"Step 0" marker in the outer log. Timing slacks are printed only
when present in the parsed line and coloured by sign.
"""
from __future__ import annotations

import argparse
import os
import re
import sys
import time


# ── Milestone patterns ────────────────────────────────────────────
#
# Order matters — the first matching pattern on a line wins. Patterns
# target the same lines Claude's Monitor filter used during the bake.

PATTERNS = [
    # Outer build_fpga.bat markers
    (r"^\[Step 0\]", "begin", "Vivado search"),
    (r"^\[Step 2\]", "verilog", "Verilog staleness check"),
    (r"^\[Step 3\] Packaging Maia SDR IP", "pkg.maia", "package Maia IP"),
    (r"^\[Step 4\] Packaging P25 IP", "pkg.p25", "package P25 IP"),
    (r"^\[Step 5\] Building FPGA bitstream", "impl.start", "FPGA bitstream build"),

    # Synth
    (r"Finished RTL Elaboration", "synth.elab", "RTL elaboration done"),
    (r"Synthesis Optimization Complete", "synth.done", "synthesis complete"),

    # Impl sub-phases (written by runme.log)
    # Match on "PHASE: Time (s):" lines because Vivado prints the
    # `elapsed =` on THAT line, not the preceding "completed" line.
    (r"Command: opt_design", "opt.start", "opt_design start"),
    (r"^opt_design: Time \(s\):", "opt.done", "opt_design complete"),

    (r"Command: place_design", "place.start", "place_design start"),
    (r"Post Placement Timing Summary WNS=([+\-]?\d+(?:\.\d+)?)",
     "place.wns", "placement timing"),
    (r"^place_design: Time \(s\):", "place.done", "place_design complete"),

    (r"Command: phys_opt_design", "phys.start", "phys_opt start"),
    (r"WNS.*?is greater than or equal to 0\.000 ns\. Skipping",
     "phys.skipped", "phys_opt skipped (WNS already positive)"),
    (r"^phys_opt_design: Time \(s\):", "phys.done", "phys_opt complete"),

    (r"Command: route_design", "route.start", "route_design start"),
    (r"Post Routing Timing Summary\s*\|\s*WNS=([+\-]?\d+(?:\.\d+)?)\s*\|"
     r"\s*TNS=([+\-]?\d+(?:\.\d+)?)\s*\|\s*WHS=([+\-]?\d+(?:\.\d+)?)\s*\|"
     r"\s*THS=([+\-]?\d+(?:\.\d+)?)",
     "route.final", "routing final timing"),
    (r"^route_design: Time \(s\):", "route.done", "route_design complete"),

    # Bitstream + XSA
    (r"Command: write_bitstream", "bit.start", "write_bitstream start"),
    (r"Creating bitstream\.\.\.", "bit.write", "bitstream compression"),
    (r"^write_bitstream: Time \(s\):", "bit.done", "bitstream complete"),
    (r"Successfully created Hardware Platform:\s*(\S+\.xsa)",
     "xsa.done", "XSA written"),

    # Final
    (r"BUILD SUCCESSFUL", "build.ok", "BUILD SUCCESSFUL"),
    (r"\[OK\] \*\*\* FPGA BUILD COMPLETE \*\*\*",
     "build.ok", "Vivado impl phase done"),

    # Error surfaces — print immediately, don't suppress
    (r"^ERROR:", "error", "ERROR"),
    (r"^CRITICAL WARNING:", "critwarn", "CRITICAL WARNING"),
    (r"^FAIL", "fail", "FAIL"),
    # Route intermediate timing (useful live; low volume)
    (r"Intermediate Timing Summary\s*\|\s*WNS=([+\-]?\d+(?:\.\d+)?)\s*\|"
     r"\s*TNS=([+\-]?\d+(?:\.\d+)?)\s*\|\s*WHS=([+\-]?\d+(?:\.\d+)?|N/A)\s*\|"
     r"\s*THS=([+\-]?\d+(?:\.\d+)?|N/A)",
     "route.mid", "routing intermediate timing"),
]

COMPILED = [(re.compile(p), key, label) for p, key, label in PATTERNS]


# Keys that should emit exactly once, even if the same log line appears
# many times (Vivado re-prints timing summaries as it iterates). We let
# `route.mid` repeat (it's the progress signal) but cap most others.
SINGLE_SHOT = {
    "begin", "verilog", "pkg.maia", "pkg.p25", "impl.start",
    "synth.elab", "synth.done",
    "opt.start", "opt.done",
    "place.start", "place.wns", "place.done",
    "phys.start", "phys.done", "phys.skipped",
    "route.start", "route.final", "route.done",
    "bit.start", "bit.write", "bit.done", "xsa.done",
    "build.ok",
}

# Last-seen-time suppression for repeating lines — print at most once
# per this many seconds. Error surfaces bypass this.
REPEAT_SUPPRESS_S = 30.0


def ansi(color: str) -> str:
    # Honour NO_COLOR / non-tty.
    if os.environ.get("NO_COLOR") or not sys.stdout.isatty():
        return ""
    return {
        "reset": "\x1b[0m",
        "red":   "\x1b[31m",
        "green": "\x1b[32m",
        "yellow": "\x1b[33m",
        "cyan":  "\x1b[36m",
        "gray":  "\x1b[90m",
    }.get(color, "")


def fmt_elapsed(dt: float) -> str:
    m, s = divmod(int(dt), 60)
    return f"{m:02d}:{s:02d}"


def fmt_slack(val_str: str) -> str:
    """Format a slack value in ns with colour based on sign."""
    try:
        v = float(val_str)
    except ValueError:
        return val_str
    if v < 0:
        return f"{ansi('red')}{v:+.3f} ns{ansi('reset')}"
    if v < 0.100:
        return f"{ansi('yellow')}{v:+.3f} ns{ansi('reset')}"
    return f"{ansi('green')}{v:+.3f} ns{ansi('reset')}"


VIVADO_ELAPSED_RE = re.compile(
    r"elapsed = (\d+):(\d+):(\d+(?:\.\d+)?)")


def parse_vivado_elapsed(line: str) -> str:
    """Return 'MM:SS' from a 'elapsed = HH:MM:SS' line, empty otherwise."""
    m = VIVADO_ELAPSED_RE.search(line)
    if not m:
        return ""
    h, mn, s = int(m.group(1)), int(m.group(2)), float(m.group(3))
    total = h * 3600 + mn * 60 + int(s)
    mm, ss = divmod(total, 60)
    return f"{mm:02d}:{ss:02d}"


class Progress:
    def __init__(self) -> None:
        self.seen: set[str] = set()
        self.last_print_at: dict[str, float] = {}
        self.start_time: float | None = None
        self.phase_start_s: dict[str, float] = {}

    def emit(self, key: str, text: str) -> None:
        now = time.monotonic()
        if self.start_time is None:
            self.start_time = now
        elapsed = now - self.start_time

        # Single-shot gating
        if key in SINGLE_SHOT and key in self.seen:
            return
        # Repeat suppression for non-single-shot keys
        if key not in SINGLE_SHOT:
            last = self.last_print_at.get(key, 0)
            if now - last < REPEAT_SUPPRESS_S:
                return

        self.seen.add(key)
        self.last_print_at[key] = now
        stamp = ansi("gray") + f"[{fmt_elapsed(elapsed)}]" + ansi("reset")
        print(f"{stamp} {text}", flush=True)

    def phase_elapsed(self, start_key: str) -> str:
        if start_key not in self.phase_start_s:
            return ""
        dt = time.monotonic() - self.phase_start_s[start_key]
        return f" ({fmt_elapsed(dt)})"

    def handle(self, line: str) -> None:
        line = line.rstrip()
        for regex, key, label in COMPILED:
            m = regex.search(line)
            if not m:
                continue
            # Custom formatting per key
            if key == "synth.done":
                self.emit(key, f"{ansi('green')}synth{ansi('reset')}      "
                          f"done   {label}")
            elif key == "opt.start":
                self.phase_start_s["opt"] = time.monotonic()
                self.emit(key, f"opt        start")
            elif key == "opt.done":
                # Prefer Vivado's own elapsed stamp when present
                # (accurate under replay; wall-clock is accurate live).
                vivado_elapsed = parse_vivado_elapsed(line)
                elapsed = (f" ({vivado_elapsed})" if vivado_elapsed
                           else self.phase_elapsed("opt"))
                self.emit(key, f"{ansi('green')}opt{ansi('reset')}        "
                          f"done  {elapsed}")
            elif key == "place.start":
                self.phase_start_s["place"] = time.monotonic()
                self.emit(key, f"place      start  (longest single phase)")
            elif key == "place.wns":
                wns = fmt_slack(m.group(1))
                self.emit(key, f"place      timing WNS {wns}")
            elif key == "place.done":
                vivado_elapsed = parse_vivado_elapsed(line)
                elapsed = (f" ({vivado_elapsed})" if vivado_elapsed
                           else self.phase_elapsed("place"))
                self.emit(key, f"{ansi('green')}place{ansi('reset')}      "
                          f"done  {elapsed}")
            elif key == "phys.start":
                self.phase_start_s["phys"] = time.monotonic()
                self.emit(key, f"phys_opt   start")
            elif key == "phys.skipped":
                self.emit(key, f"{ansi('green')}phys_opt{ansi('reset')}   "
                          f"done   (skipped, WNS already positive)")
            elif key == "phys.done":
                vivado_elapsed = parse_vivado_elapsed(line)
                elapsed = (f" ({vivado_elapsed})" if vivado_elapsed
                           else self.phase_elapsed("phys"))
                self.emit(key, f"{ansi('green')}phys_opt{ansi('reset')}   "
                          f"done  {elapsed}")
            elif key == "route.start":
                self.phase_start_s["route"] = time.monotonic()
                self.emit(key, f"route      start  (multithreaded)")
            elif key == "route.mid":
                wns = fmt_slack(m.group(1))
                whs = m.group(3)
                whs_f = whs if whs == "N/A" else fmt_slack(whs)
                self.emit(key, f"route      timing WNS {wns}  WHS {whs_f}")
            elif key == "route.final":
                wns = fmt_slack(m.group(1))
                whs = fmt_slack(m.group(3))
                self.emit(key, f"{ansi('green')}route{ansi('reset')}      "
                          f"final  WNS {wns}  WHS {whs}")
            elif key == "route.done":
                vivado_elapsed = parse_vivado_elapsed(line)
                elapsed = (f" ({vivado_elapsed})" if vivado_elapsed
                           else self.phase_elapsed("route"))
                self.emit(key, f"{ansi('green')}route{ansi('reset')}      "
                          f"done  {elapsed}")
            elif key == "bit.start":
                self.phase_start_s["bit"] = time.monotonic()
                self.emit(key, f"bitstream  writing")
            elif key == "bit.done":
                vivado_elapsed = parse_vivado_elapsed(line)
                elapsed = (f" ({vivado_elapsed})" if vivado_elapsed
                           else self.phase_elapsed("bit"))
                self.emit(key, f"{ansi('green')}bitstream{ansi('reset')}  "
                          f"done  {elapsed}")
            elif key == "xsa.done":
                path = m.group(1)
                self.emit(key, f"{ansi('green')}XSA{ansi('reset')}        "
                          f"written  {path}")
            elif key == "build.ok":
                self.emit(key, f"{ansi('green')}BUILD{ansi('reset')}      "
                          f"SUCCESSFUL")
            elif key == "error":
                print(f"{ansi('red')}!! {line}{ansi('reset')}", flush=True)
            elif key == "critwarn":
                # Show first 3 (so genuine surprises get noticed), then
                # suppress and print a single summary at build end.
                cnt = int(self.last_print_at.get("critwarn_count", 0))
                if cnt < 3:
                    print(f"{ansi('yellow')}?? {line}{ansi('reset')}",
                          flush=True)
                elif cnt == 3:
                    print(f"{ansi('gray')}   ... (further CRITICAL "
                          f"WARNING lines suppressed -- see full log)"
                          f"{ansi('reset')}", flush=True)
                self.last_print_at["critwarn_count"] = cnt + 1
            elif key == "fail":
                print(f"{ansi('red')}!! {line}{ansi('reset')}", flush=True)
            else:
                self.emit(key, label)
            return


def tail_stdin(progress: Progress) -> None:
    for line in sys.stdin:
        progress.handle(line)


def tail_file(progress: Progress, path: str, follow: bool = True) -> None:
    """Tail a file. If `follow`, keep reading as new lines append."""
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        # Read existing content first
        for line in f:
            progress.handle(line)
        if not follow:
            return
        # Switch to follow mode — poll for new lines every 250 ms
        while True:
            where = f.tell()
            line = f.readline()
            if line:
                progress.handle(line)
            else:
                f.seek(where)
                time.sleep(0.25)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--tail", metavar="LOGFILE",
                   help="Live-follow a log file; keeps running until Ctrl-C.")
    g.add_argument("--replay", metavar="LOGFILE",
                   help="Parse an existing log once and exit.")
    args = ap.parse_args()

    progress = Progress()
    try:
        if args.tail:
            tail_file(progress, args.tail, follow=True)
        elif args.replay:
            tail_file(progress, args.replay, follow=False)
        else:
            # Read from stdin (pipeline wrapper usage).
            tail_stdin(progress)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
