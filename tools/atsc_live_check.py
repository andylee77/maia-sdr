#!/usr/bin/env python3
"""Live ATSC TV on a unit in ATSC mode (doc/changes/085): the stations named live, or a channel watched.

  names  each RF channel in turn played live (POST /api/v1/atsc/live) until its PSIP names the
         station or --dwell seconds pass: its TSID and virtual channels, the MER, the codewords
         corrected and beyond Reed-Solomon. With --lineup (an HDHomeRun lineup.json saved
         beside the captures) each TSID's names in the lineup go beside it.
  watch  one RF channel for --minutes, sampled every --interval: live TV's statistics; with --ssh
         the scanner's CPU, its live TV thread's and both A9 cores' over 5 s, and its memory; with
         --vlc, VLC plays the stream (one programme with --program) for the whole run, its log
         kept in the run directory.

Each sample is a JSON line (channels.jsonl or samples.jsonl); summary.json holds the totals (also
printed). Live TV is stopped at the end.

  python tools/atsc_live_check.py --host 192.168.120.50 names --rf 9,13,14,19,20,21 --dwell 20
  python tools/atsc_live_check.py --host 192.168.120.50 watch --rf 19 --program 3 --minutes 30 --ssh --vlc

Exit codes: 0 the run completed, 2 the unit stopped answering, 3 live TV did not play.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import scanner_token

SSH = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
       "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10"]
VLC = "C:/Program Files/VideoLAN/VLC/vlc.exe"


def get(host: str, path: str, timeout: float = 10.0):
    with urllib.request.urlopen(f"{scanner_token.base(host)}{path}", timeout=timeout) as r:
        return json.load(r)


def post(host: str, path: str, body: dict | None = None, timeout: float = 60.0):
    data = json.dumps(body).encode() if body is not None else b""
    req = urllib.request.Request(f"{scanner_token.base(host)}{path}", data=data, method="POST",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def rf_of(freq_hz: int) -> int:
    """The US RF channel whose centre is `freq_hz`."""
    mhz = freq_hz / 1e6
    if mhz >= 470:
        return round((mhz - 473) / 6) + 14
    if mhz >= 174:
        return round((mhz - 177) / 6) + 7
    return {57: 2, 63: 3, 69: 4, 79: 5, 85: 6}.get(round(mhz), 0)


def lineup(path: str | None) -> dict[int, list[str]]:
    """TSID -> the lineup's '<number> <name>' entries."""
    if not path:
        return {}
    out: dict[int, list[str]] = {}
    for e in json.loads(Path(path).read_text(encoding="utf-8")):
        if e.get("TransportStreamID") is not None:
            out.setdefault(e["TransportStreamID"], []).append(f"{e['GuideNumber']} {e['GuideName']} (RF {rf_of(e.get('Frequency', 0))})")
    return out


def names(stats: dict) -> list[str]:
    return [f"{c['major']}.{c['minor']} {c['short_name']}" for c in (stats.get("psip") or {}).get("channels") or []]


def board(host: str) -> dict | None:
    """The scanner's and its live TV thread's CPU and both cores' busy share over 5 s, and memory."""
    cmd = ("p=$(pidof scanner); t=$(grep -l atsc-live /proc/$p/task/*/comm 2>/dev/null | head -1 | xargs -r dirname); "
           "a() { awk '{print $14+$15}' $1/stat 2>/dev/null || echo 0; }; "
           "c() { awk '/^cpu[0-9]/ {print $1, $2+$3+$4+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat; }; "
           "p0=$(a /proc/$p); t0=$(a ${t:-/nonexistent}); c0=$(c); sleep 5; "
           "p1=$(a /proc/$p); t1=$(a ${t:-/nonexistent}); c1=$(c); "
           "echo proc $((p1-p0)); echo thread $((t1-t0)); echo \"$c0\" | sed 's/^/a /'; echo \"$c1\" | sed 's/^/b /'; "
           "grep VmRSS /proc/$p/status")
    try:
        out = subprocess.run(SSH + [f"root@{host}", cmd], capture_output=True, text=True, timeout=40).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    b: dict = {"cores": {}}
    first: dict = {}
    for line in out.splitlines():
        p = line.split()
        if not p:
            continue
        if p[0] in ("proc", "thread") and len(p) == 2:
            # Clock ticks (100 a second) over 5 s: per cent of one core.
            b[f"{p[0]}_pct"] = round(int(p[1]) / 5.0, 1)
        elif p[0] == "a" and len(p) == 4:
            first[p[1]] = (int(p[2]), int(p[3]))
        elif p[0] == "b" and len(p) == 4 and p[1] in first:
            busy = int(p[2]) - first[p[1]][0]
            total = int(p[3]) - first[p[1]][1]
            b["cores"][p[1]] = round(100.0 * busy / total, 1) if total else None
        elif p[0] == "VmRSS:":
            b["rss_kb"] = int(p[1])
    return b


def live_sample(host: str) -> dict:
    s = get(host, "/api/v1/atsc/live")
    s["t"] = time.time()
    return s


def play(host: str, rf: int, timeout: float) -> dict | None:
    """Live TV on `rf` (started unless it already runs there, for another player); its statistics
    once it plays, or None."""
    now = live_sample(host)
    if not (now.get("rf") == rf and now.get("state") in ("starting", "searching", "playing")):
        post(host, "/api/v1/atsc/live", {"rf": rf})
    end = time.time() + timeout
    while time.time() < end:
        s = live_sample(host)
        if s.get("state") == "playing" and s.get("rf") == rf:
            return s
        if s.get("state") == "error":
            return None
        time.sleep(1)
    return None


def run_names(args, run: Path) -> dict:
    known = lineup(args.lineup)
    rows = []
    with open(run / "channels.jsonl", "w", encoding="utf-8") as f:
        for rf in args.rf:
            t0 = time.time()
            row: dict = {"rf": rf}
            try:
                s = play(args.host, rf, args.dwell)
            except urllib.error.HTTPError as e:
                s = None
                row["error"] = e.read().decode(errors="replace")
            row["played"] = s is not None
            if s is not None:
                row["fields_after_s"] = round(time.time() - t0, 1)
                end = t0 + args.dwell
                while time.time() < end:
                    s = live_sample(args.host)
                    if (s.get("psip") or {}).get("tsid") is not None and names(s):
                        break
                    time.sleep(1)
            if s is None:
                try:
                    s = live_sample(args.host)
                except OSError:
                    s = {}
            row.update({
                "seconds": round(time.time() - t0, 1),
                "state": s.get("state"),
                "error": row.get("error") or s.get("error"),
                "tsid": (s.get("psip") or {}).get("tsid"),
                "names": names(s),
                "mer_db": s.get("mer_db", [])[-1] if s.get("mer_db") else None,
                "codewords": s.get("codewords"), "corrected": s.get("corrected"), "failed": s.get("failed"),
                "solves": s.get("solves"), "restarts": s.get("restarts"),
                "missed": s.get("missed"), "dropped": s.get("dropped"),
                "offset_hz": s.get("offset_hz"), "carrier_hz": s.get("carrier_hz"), "timing_ppm": s.get("timing_ppm"),
            })
            if row["tsid"] is not None and known:
                row["lineup"] = known.get(row["tsid"], [])
            rows.append(row)
            f.write(json.dumps(row) + "\n")
            f.flush()
            print(json.dumps(row), flush=True)
    return {"channels": rows, "named": [r["rf"] for r in rows if r["names"]]}


def run_watch(args, run: Path) -> dict:
    rf = args.rf[0]
    first = play(args.host, rf, 30)
    if first is None:
        return {"error": "live TV did not play", "last": live_sample(args.host)}
    vlc = None
    if args.vlc:
        # The player sends no header: a closed scanner takes its token in the address.
        query = [f"program={args.program}"] if args.program else []
        query += [f"token={scanner_token.token()}"] if scanner_token.token() else []
        url = f"{scanner_token.base(args.host)}/api/v1/atsc/stream/{rf}" + ("?" + "&".join(query) if query else "")
        vlc = subprocess.Popen([VLC, "-I", "dummy", "--dummy-quiet", "--vout", "dummy", "--aout", "dummy",
                                "--run-time", str(int(args.minutes * 60)), "--file-logging", "--logfile", str(run / "vlc.log"),
                                "--log-verbose", "1", url, "vlc://quit"])
    samples = [first]
    lost = 0
    end = time.time() + args.minutes * 60
    with open(run / "samples.jsonl", "w", encoding="utf-8") as f:
        f.write(json.dumps(first) + "\n")
        while time.time() < end:
            time.sleep(min(args.interval, max(0.0, end - time.time())))
            try:
                s = live_sample(args.host)
            except OSError as e:
                lost += 1
                f.write(json.dumps({"t": time.time(), "error": str(e)}) + "\n")
                if lost >= 3:
                    break
                continue
            lost = 0
            if args.ssh:
                s["board"] = board(args.host)
            samples.append(s)
            f.write(json.dumps(s) + "\n")
            f.flush()
    last = samples[-1]

    def gained(k: str):
        return (last.get(k) or 0) - (first.get(k) or 0)

    mers = [m for s in samples[1:] for m in (s.get("mer_db") or [])[-min(len(s.get("mer_db") or []), 10):]]
    summary: dict = {
        "rf": rf, "program": args.program,
        "minutes": round((last["t"] - first["t"]) / 60, 1), "samples": len(samples), "unreachable": lost >= 3,
        "tsid": (last.get("psip") or {}).get("tsid"), "names": names(last),
        "codewords": gained("codewords"), "corrected": gained("corrected"), "failed": gained("failed"),
        "packets": gained("packets"), "missed": gained("missed"), "dropped": gained("dropped"),
        "restarts": gained("restarts"), "snapshots": gained("snapshots"), "solves": gained("solves"),
        "viewers": [s.get("viewers") for s in samples[-3:]],
        "states": sorted({s.get("state") for s in samples}),
    }
    if summary["codewords"]:
        summary["failed_share"] = summary["failed"] / summary["codewords"]
        summary["corrected_share"] = summary["corrected"] / summary["codewords"]
    if mers:
        summary["mer_db"] = {"mean": round(sum(mers) / len(mers), 2), "min": round(min(mers), 2), "max": round(max(mers), 2)}
    solve_ms = [s["solve_ms"] for s in samples if s.get("solve_ms") is not None]
    if solve_ms:
        summary["solve_ms"] = {"mean": round(sum(solve_ms) / len(solve_ms), 1), "max": round(max(solve_ms), 1)}
    boards = [s["board"] for s in samples if s.get("board")]
    if boards:
        def stats(vals):
            vals = [v for v in vals if v is not None]
            return {"mean": round(sum(vals) / len(vals), 1), "max": max(vals)} if vals else None
        summary["scanner_cpu_pct"] = stats([b.get("proc_pct") for b in boards])
        summary["live_thread_cpu_pct"] = stats([b.get("thread_pct") for b in boards])
        summary["cores_busy_pct"] = {c: stats([b["cores"].get(c) for b in boards]) for c in boards[0]["cores"]}
        summary["rss_kb"] = [boards[0].get("rss_kb"), boards[-1].get("rss_kb")]
    if vlc is not None:
        try:
            summary["vlc_exit"] = vlc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            vlc.kill()
            summary["vlc_exit"] = "killed"
        log = (run / "vlc.log").read_text(encoding="utf-8", errors="replace") if (run / "vlc.log").exists() else ""
        lines = log.splitlines()
        summary["vlc_log"] = {
            "lines": len(lines),
            "errors": sum(" error:" in l for l in lines),
            "warnings": sum(" warning:" in l for l in lines),
            "discontinuities": sum("discontinuity" in l.lower() for l in lines),
        }
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", required=True)
    ap.add_argument("--out", default="runs/085/tv")
    sub = ap.add_subparsers(dest="mode", required=True)
    n = sub.add_parser("names")
    n.add_argument("--rf", type=lambda v: [int(x) for x in v.split(",")], required=True)
    n.add_argument("--dwell", type=float, default=20.0)
    n.add_argument("--lineup")
    w = sub.add_parser("watch")
    w.add_argument("--rf", type=lambda v: [int(x) for x in v.split(",")], required=True)
    w.add_argument("--program", type=int)
    w.add_argument("--minutes", type=float, default=30.0)
    w.add_argument("--interval", type=float, default=30.0)
    w.add_argument("--ssh", action="store_true")
    w.add_argument("--vlc", action="store_true")
    args = ap.parse_args()
    scanner_token.install(args.host)

    run = Path(args.out) / f"{args.host}_{args.mode}_{time.strftime('%Y%m%d_%H%M%S')}"
    run.mkdir(parents=True, exist_ok=True)
    try:
        summary = run_names(args, run) if args.mode == "names" else run_watch(args, run)
    finally:
        try:
            post(args.host, "/api/v1/atsc/live/stop")
        except OSError:
            pass
    summary["host"] = args.host
    (run / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary, indent=2))
    print(f"run dir: {run}")
    if summary.get("unreachable"):
        return 2
    return 3 if summary.get("error") else 0


if __name__ == "__main__":
    sys.exit(main())
