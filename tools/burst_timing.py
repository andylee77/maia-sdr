#!/usr/bin/env python3
"""The radio core's burst timing gate (doc/changes/082): one burst on the air must start at the same
AD9361 sample, to within one 4096-sample transform, in a spectrum frame's header, in the control
lane's packets and in the capture ring.

Runs the scanner's check (POST /api/v1/diag/burst) and follows it: it says when to press the
transmitter (a key fob, the ESP32-DIV). A first press finds the transmitter's frequency (unless
--no-find), the second is measured. The live site pauses meanwhile. Writes
runs/burst/<time>/result.json and prints where each placed the burst's start.

  python tools/burst_timing.py --host 192.168.120.50 --freq 433.92
  python tools/burst_timing.py --host 192.168.120.50 --freq 315.0 --wait 120

Exit codes: 0 the three agree to within one transform; 1 they do not, or one of them did not see
the burst; 2 the check failed (the unit did not answer, the radio was busy, no burst came).
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import scanner_token

ROOT = Path(__file__).resolve().parent.parent


def get(host: str, path: str):
    with urllib.request.urlopen(f"http://{host}:8080{path}", timeout=10) as r:
        return json.load(r)


def post(host: str, path: str, body: dict | None = None):
    data = json.dumps(body).encode() if body is not None else b""
    req = urllib.request.Request(f"http://{host}:8080{path}", data=data, method="POST",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def describe(t: dict) -> list[str]:
    lines = [f"burst at {t['burst_hz'] / 1e6:.4f} MHz"]
    for name in ["capture", "frame", "lane"]:
        p = t.get(name)
        if p is None:
            lines.append(f"  {name:8} did not see it")
            continue
        extra = ""
        if name == "frame" and t.get("frame_sequence") is not None:
            extra = f" (frame {t['frame_sequence']})"
        if name == "lane":
            extra = f" (the DDC's {t['lane_delay']:.0f}-sample delay taken off)"
        lines.append(f"  {name:8} sample {p['index']:>14,}  {p['db'] - p['floor_db']:5.1f} dB over its floor{extra}")
    for name in ["lane_minus_capture", "frame_minus_capture"]:
        d = t.get(name)
        if d is not None:
            lines.append(f"  {name.replace('_', ' '):22} {d:+8,} samples ({d / 4096:+.2f} transforms)")
    if t.get("frames_missed"):
        lines.append(f"  frames missed while listening: {t['frames_missed']}")
    if t.get("capture_overflow"):
        lines.append("  the capture lost words: its indices past the loss are not exact")
    return lines


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--host", default="192.168.120.50")
    ap.add_argument("--freq", type=float, required=True, help="the transmitter's frequency, MHz")
    ap.add_argument("--offset", type=float, default=300.0, help="the burst above the LO, kHz")
    ap.add_argument("--span", type=float, default=150.0, help="searched either side, kHz")
    ap.add_argument("--wait", type=int, default=60, help="seconds to wait for each press")
    ap.add_argument("--threshold", type=float, default=15.0, help="dB over the floor")
    ap.add_argument("--no-find", action="store_true", help="the lane stays on --freq (no first press)")
    args = ap.parse_args()
    scanner_token.install(args.host)

    req = {"freq_hz": round(args.freq * 1e6), "offset_hz": round(args.offset * 1e3),
           "span_hz": round(args.span * 1e3), "wait_s": args.wait, "threshold_db": args.threshold,
           "find": not args.no_find}
    try:
        started = post(args.host, "/api/v1/diag/burst", req)
    except urllib.error.HTTPError as e:
        print(f"refused: {e.read().decode(errors='replace')}", file=sys.stderr)
        return 2
    except OSError as e:
        print(f"the unit did not answer: {e}", file=sys.stderr)
        return 2
    print(f"check {started['id']} started")
    said = None
    try:
        while True:
            time.sleep(0.5)
            s = get(args.host, "/api/v1/diag/burst")
            say = (s["state"], s.get("message"))
            if say != said:
                said = say
                print(f"[{s['state']}] {s.get('message') or ''}")
            if s["state"] in ("done", "error", "cancelled"):
                break
    except KeyboardInterrupt:
        post(args.host, "/api/v1/diag/burst/cancel")
        print("cancelled", file=sys.stderr)
        return 2

    out = ROOT / "runs" / "burst" / time.strftime("%Y%m%d_%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    (out / "result.json").write_text(json.dumps({"request": req, "state": s}, indent=2) + "\n")
    if s["state"] != "done" or not s.get("timing"):
        print(f"{s['state']}: {s.get('error')}", file=sys.stderr)
        return 2
    t = s["timing"]
    print("\n".join(describe(t)))
    verdict = {True: "PASS: the three agree to within one transform",
               False: "FAIL: they disagree by more than a transform",
               None: "FAIL: one of them did not see the burst"}[t.get("pass")]
    print(verdict)
    print(f"written: {out / 'result.json'}")
    return 0 if t.get("pass") else 1


if __name__ == "__main__":
    sys.exit(main())
