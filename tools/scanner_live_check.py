#!/usr/bin/env python3
"""A live run of the scanner on a unit: samples it every minute and summarises the window.

Each sample (one JSON line in samples.jsonl): the control channel's decode rate and CRC share,
grants, the crystal correction, recordings, and with --ssh the process's memory, threads and
CPU. The summary (summary.json, also printed): the Activity summary of the window (calls,
followed, voice), recordings made, memory at the start and end, panics and errors in the log.

  python tools/scanner_live_check.py --host 10.25.0.2 --minutes 60 --ssh
  python tools/scanner_live_check.py --host 10.25.0.2 --site cec_gcs --minutes 60 --ssh

Exit codes: 0 the run completed, 2 the unit stopped answering.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

SSH = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
       "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10"]


def get(host: str, path: str, timeout: float = 10.0):
    with urllib.request.urlopen(f"http://{host}:8080{path}", timeout=timeout) as r:
        return json.load(r)


def post(host: str, path: str, timeout: float = 60.0):
    req = urllib.request.Request(f"http://{host}:8080{path}", data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def board(host: str) -> dict | None:
    cmd = ("p=$(pidof scanner); grep -E 'VmRSS|Threads' /proc/$p/status; "
           "top -bn1 | awk -v p=$p '$1==p {print \"cpu \" $8}'; "
           "grep -ciE 'panic' /tmp/scanner.log /var/log/scanner.log 2>/dev/null | awk -F: '{s+=$2} END {print \"panics \" s+0}'; "
           "grep -cE ' ERROR ' /tmp/scanner.log /var/log/scanner.log 2>/dev/null | awk -F: '{s+=$2} END {print \"errors \" s+0}'")
    try:
        out = subprocess.run(SSH + [f"root@{host}", cmd], capture_output=True, text=True, timeout=30).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    b: dict = {}
    for line in out.splitlines():
        parts = line.replace(":", " ").split()
        if len(parts) < 2:
            continue
        key, val = parts[0], parts[1].rstrip("%")
        try:
            b[{"VmRSS": "rss_kb", "Threads": "threads"}.get(key, key)] = float(val)
        except ValueError:
            pass
    return b


def sample(host: str, ssh: bool) -> dict:
    s = get(host, "/api/v1/status")
    c = s.get("control") or {}
    t = s.get("tuning") or {}
    recs = get(host, "/api/v1/recordings?limit=1")
    out = {
        "t": time.time(),
        "uptime_s": s.get("uptime_s"),
        "site": ((s.get("live") or {}).get("site") or {}).get("id"),
        "msgs_per_s": c.get("msgs_per_s"),
        "ok_pct": c.get("ok_pct"),
        "grants": c.get("grants"),
        "grants_dropped": c.get("grants_dropped"),
        "decoder_cpu_pct": c.get("cpu_pct"),
        "dibit_resyncs": (c.get("input") or {}).get("dibit_resyncs"),
        "iq_dropped": (c.get("input") or {}).get("iq_dropped"),
        "crystal_ppm": t.get("crystal_ppm"),
        "lo_shift_hz": t.get("lo_shift_hz"),
        "recordings": recs.get("total", recs.get("count")),
    }
    if ssh:
        out["board"] = board(host)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", required=True)
    ap.add_argument("--site", help="make this site live first")
    ap.add_argument("--minutes", type=float, default=60.0)
    ap.add_argument("--interval", type=float, default=60.0)
    ap.add_argument("--ssh", action="store_true", help="also read memory, CPU and the log over SSH")
    ap.add_argument("--out", default="runs/076/live")
    args = ap.parse_args()

    if args.site:
        post(args.host, f"/api/v1/sites/{args.site}/activate")
        time.sleep(20)
    first = sample(args.host, args.ssh)
    site = first["site"] or "none"
    run = Path(args.out) / f"{args.host}_{site}_{time.strftime('%Y%m%d_%H%M%S')}"
    run.mkdir(parents=True, exist_ok=True)
    start_ms = int(time.time() * 1000)
    samples = [first]
    lost = 0
    with open(run / "samples.jsonl", "w", encoding="utf-8") as f:
        f.write(json.dumps(first) + "\n")
        end = time.time() + args.minutes * 60
        while time.time() < end:
            time.sleep(min(args.interval, max(0.0, end - time.time())))
            try:
                s = sample(args.host, args.ssh)
            except OSError as e:
                lost += 1
                f.write(json.dumps({"t": time.time(), "error": str(e)}) + "\n")
                if lost >= 3:
                    break
                continue
            lost = 0
            samples.append(s)
            f.write(json.dumps(s) + "\n")
            f.flush()
    end_ms = int(time.time() * 1000)
    summary: dict = {"host": args.host, "site": site, "minutes": round((end_ms - start_ms) / 60000, 1),
                     "samples": len(samples), "unreachable": lost >= 3}
    try:
        a = get(args.host, f"/api/v1/activity/summary?site={site}&from={start_ms}&to={end_ms}")
        summary["activity"] = a.get("summary", a)
    except OSError as e:
        summary["activity_error"] = str(e)
    last = samples[-1]
    summary["recordings_made"] = (last.get("recordings") or 0) - (first.get("recordings") or 0)
    summary["msgs_per_s_min"] = min((s["msgs_per_s"] for s in samples if s.get("msgs_per_s") is not None), default=None)
    summary["ok_pct_min"] = min((s["ok_pct"] for s in samples if s.get("ok_pct") is not None), default=None)
    summary["grants_dropped"] = last.get("grants_dropped")
    summary["dibit_resyncs"] = last.get("dibit_resyncs")
    summary["crystal_ppm"] = [first.get("crystal_ppm"), last.get("crystal_ppm")]
    summary["uptime_s"] = [first.get("uptime_s"), last.get("uptime_s")]
    if args.ssh:
        boards = [s["board"] for s in samples if s.get("board")]
        if boards:
            summary["rss_kb"] = [boards[0].get("rss_kb"), max(b.get("rss_kb", 0) for b in boards), boards[-1].get("rss_kb")]
            summary["threads"] = [boards[0].get("threads"), boards[-1].get("threads")]
            cpus = [b["cpu"] for b in boards if "cpu" in b]
            summary["cpu_pct_mean"] = round(sum(cpus) / len(cpus), 1) if cpus else None
            summary["panics"] = boards[-1].get("panics")
            summary["errors"] = boards[-1].get("errors")
    (run / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    print(f"run dir: {run}")
    return 2 if summary["unreachable"] else 0


if __name__ == "__main__":
    sys.exit(main())
