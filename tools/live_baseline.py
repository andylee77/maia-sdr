#!/usr/bin/env python3
"""Live baseline of a unit's receiver on its active site.

Polls the unit's HTTP API for a fixed window and reports what the 076 phase checks compare:
control-channel decode quality (P25 TSBK CRC %, DMR valid %), calls, follow rate, vocoder
errors, recordings, history rows and CPU. Read-only: it never retunes or restarts the unit.

Usage:
  python tools/live_baseline.py --minutes 30 [--host 192.168.120.50] [--label clay]

Writes runs/076/baseline/<label>_<utc>/{summary.json,calls.json,samples.jsonl} and prints the
summary as JSON. Exit codes: 0 done, 2 the unit could not be read.
"""

import argparse
import datetime as dt
import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SSH_OPTS = ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]


def get(base, path, timeout=10):
    with urllib.request.urlopen(base + path, timeout=timeout) as r:
        return json.load(r)


def proc_cpu(host):
    """Jiffies of the whole system and of p25-httpd, read over SSH."""
    cmd = "head -1 /proc/stat; cat /proc/$(pidof p25-httpd | cut -d' ' -f1)/stat"
    out = subprocess.run(["ssh", *SSH_OPTS, f"root@{host}", cmd], capture_output=True,
                         text=True, timeout=20, check=True).stdout.splitlines()
    total = [int(v) for v in out[0].split()[1:]]
    fields = out[1].rsplit(")", 1)[1].split()
    return {"total": sum(total), "idle": total[3] + total[4],
            "proc": int(fields[11]) + int(fields[12])}


def cpu_pct(a, b, ncpu=2):
    dt_total = b["total"] - a["total"]
    if dt_total <= 0:
        return None
    return {
        "system_busy_pct": round(100.0 * (1 - (b["idle"] - a["idle"]) / dt_total), 1),
        # % of one core, like top.
        "daemon_pct_of_core": round(100.0 * ncpu * (b["proc"] - a["proc"]) / dt_total, 1),
    }


def control_counts(base, protocol, modulation):
    """Cumulative control-channel counters: (good, bad) messages."""
    if protocol == "dmr":
        d = get(base, "/api/dmr")
        good = sum(c["valid"] for c in d["classes"].values())
        bad = sum(c["invalid"] for c in d["classes"].values())
        return {"good": good, "bad": bad, "cach_ok": d["cach"]["ok"], "cach_bad": d["cach"]["bad"]}
    d = get(base, "/api/decoder_compare")
    dec = d["ps_lsm"] if "LSM" in (modulation or "").upper() else d["ps_c4fm"]
    return {"good": dec["tsbk_crc_ok"], "bad": dec["tsbk_crc_failures"]}


def history_calls(base, site):
    d = get(base, "/api/activity/sites")
    return next((s["calls"] for s in d.get("sites", []) if s["site"] == site), 0)


def pct(num, den):
    return round(100.0 * num / den, 2) if den else None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--host", default="192.168.120.50")
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--minutes", type=float, default=30.0)
    ap.add_argument("--interval", type=float, default=15.0)
    ap.add_argument("--label", default=None)
    ap.add_argument("--out", type=Path, default=REPO / "runs" / "076" / "baseline")
    args = ap.parse_args()
    base = f"http://{args.host}:{args.port}"

    try:
        state = get(base, "/api/ui/state")
        site = state["site"]["name"]
        protocol = state["site"]["protocol"]
        modulation = state["site"]["modulation"]
        start_ms = state["now_unix_ms"]
        ctl0 = control_counts(base, protocol, modulation)
        hist0 = history_calls(base, site)
        cpu0 = proc_cpu(args.host)
    except Exception as e:
        print(json.dumps({"error": f"cannot read the unit: {e}"}))
        return 2

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_dir = args.out / f"{args.label or site}_{stamp}"
    run_dir.mkdir(parents=True, exist_ok=True)
    calls = {}
    deadline = time.monotonic() + args.minutes * 60.0
    with open(run_dir / "samples.jsonl", "w") as samples:
        while True:
            try:
                ui = get(base, f"/api/ui/calls?limit=250&site={site}")
                cores = get(base, "/api/ps_cores")
                for c in ui["items"]:
                    if c["started_unix_ms"] >= start_ms:
                        calls[c["call_id"]] = c
                samples.write(json.dumps({"t": ui["now_unix_ms"],
                                          "busy_pct": [c["busy_pct"] for c in cores["cpus"]],
                                          "calls_seen": len(calls)}) + "\n")
                samples.flush()
            except Exception as e:
                samples.write(json.dumps({"t": int(time.time() * 1000), "error": str(e)}) + "\n")
            if time.monotonic() >= deadline:
                break
            time.sleep(args.interval)

    try:
        end_state = get(base, "/api/ui/state")
        ctl1 = control_counts(base, protocol, modulation)
        hist1 = history_calls(base, site)
        cpu1 = proc_cpu(args.host)
    except Exception as e:
        print(json.dumps({"error": f"cannot read the unit at the end: {e}"}))
        return 2
    end_ms = end_state["now_unix_ms"]
    # Calls still open at the end are left out.
    closed = [c for c in calls.values() if c.get("ended_unix_ms") is not None]
    clear = [c for c in closed if not c["encrypted"]]
    followed = [c for c in clear if c["not_followed"] is None]
    voiced = [c for c in followed if c["voice_ms"] > 0]
    frames = sum(c["imbe"] for c in followed)
    reasons = {}
    for c in clear:
        key = c["not_followed"] or "followed"
        reasons[key] = reasons.get(key, 0) + 1
    closes = {}
    for c in followed:
        closes[c["close_reason"]] = closes.get(c["close_reason"], 0) + 1

    good = ctl1["good"] - ctl0["good"]
    bad = ctl1["bad"] - ctl0["bad"]
    summary = {
        "site": site,
        "protocol": protocol,
        "modulation": modulation,
        "build": state["build"],
        "start_unix_ms": start_ms,
        "end_unix_ms": end_ms,
        "minutes": round((end_ms - start_ms) / 60000.0, 1),
        "control": {
            "good": good,
            "bad": bad,
            "ok_pct": pct(good, good + bad),
            "per_s": round(good / max((end_ms - start_ms) / 1000.0, 1.0), 2),
        },
        "calls": {
            "total": len(closed),
            "encrypted": len(closed) - len(clear),
            "clear": len(clear),
            "followed": len(followed),
            "follow_pct": pct(len(followed), len(clear)),
            "with_voice": len(voiced),
            "voice_pct_of_followed": pct(len(voiced), len(followed)),
            "voice_s": round(sum(c["voice_ms"] for c in followed) / 1000.0, 1),
            "outcome": reasons,
            "close_reason": closes,
        },
        "vocoder": {
            "frames": frames,
            "errors": sum(c["vocoder_errors"] for c in followed),
            "silent": sum(c["vocoder_silent"] for c in followed),
            "error_pct": pct(sum(c["vocoder_errors"] for c in followed), frames),
        },
        "recordings": sum(1 for c in followed if c.get("recording")),
        "history_rows_added": hist1 - hist0,
        "cpu": cpu_pct(cpu0, cpu1),
    }
    if protocol == "dmr":
        cach = (ctl1["cach_ok"] - ctl0["cach_ok"], ctl1["cach_bad"] - ctl0["cach_bad"])
        summary["control"]["cach_ok_pct"] = pct(cach[0], sum(cach))
    (run_dir / "calls.json").write_text(json.dumps(sorted(calls.values(),
                                                          key=lambda c: c["call_id"]), indent=1))
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=1))
    print(json.dumps({"run_dir": str(run_dir), **summary}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
