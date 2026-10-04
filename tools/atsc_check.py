#!/usr/bin/env python3
"""The scanner's TV channel finder against an HDHomeRun on the same air.

Puts the unit in ATSC mode (unless it is already), runs a TV scan (naming the stations unless
--no-names), reads the HDHomeRun's lineup with its tuning (each program's RF frequency,
modulation and signal) and compares the two RF channel by RF channel: the kind of signal, and
the virtual channels (number and short name) the unit read from each station's PSIP against
the HDHomeRun's programs. The unit goes back to the mode it was in unless --keep-mode. Writes
runs/atsc/<time>/ (scan.json, hdhomerun.json, compare.json) and prints the comparison.

  python tools/atsc_check.py --host 192.168.120.50 --hdhomerun 10.0.0.117
  python tools/atsc_check.py --host 192.168.120.50 --gain 30 --frames 16

Exit codes: 0 every channel the HDHomeRun receives now (it reports a signal there) is found with
the kind its modulation says (8vsb: 8-VSB, atsc3: no 8-VSB pilot), and every virtual channel the
unit named is one the HDHomeRun lists on that RF channel; 1 a channel is missed; 3 a name or
number differs (a station the unit could not decode is not a difference); 2 the unit or the
HDHomeRun did not answer, or the scan failed.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# The US plan: (first channel, last channel, lower edge of the first, Hz).
BANDS = [(2, 4, 54_000_000), (5, 6, 76_000_000), (7, 13, 174_000_000), (14, 36, 470_000_000)]
EXPECT = {"8vsb": "8vsb", "atsc3": "no_pilot"}


def rf_of(center_hz: int) -> int | None:
    for first, last, low in BANDS:
        n = first + round((center_hz - 3_000_000 - low) / 6_000_000)
        if first <= n <= last and low + (n - first) * 6_000_000 + 3_000_000 == center_hz:
            return n
    return None


def call(url: str, method: str = "GET", body: object | None = None, timeout: float = 120.0):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers={"Content-Type": "application/json"} if data else {})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def hdhomerun(host: str) -> dict[int, dict]:
    """The HDHomeRun's lineup by RF channel: modulation, signal and the programs."""
    out: dict[int, dict] = {}
    for p in call(f"http://{host}/lineup.json?show=all&tuning", timeout=10):
        n = rf_of(p.get("Frequency", 0))
        if n is None:
            continue
        ch = out.setdefault(n, {"modulation": p.get("Modulation"), "strength": None, "quality": None, "programs": []})
        ch["programs"].append(f"{p['GuideNumber']} {p['GuideName']}")
        for k, key in (("strength", "SignalStrength"), ("quality", "SignalQuality")):
            if key in p:
                ch[k] = p[key]
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--host", required=True, help="the unit")
    ap.add_argument("--hdhomerun", default="10.0.0.117")
    ap.add_argument("--gain", type=int, help="manual gain, dB (default: the AGC)")
    ap.add_argument("--frames", type=int, default=8, help="spectrometer frames a window")
    ap.add_argument("--keep-mode", action="store_true", help="leave the unit in ATSC mode")
    ap.add_argument("--no-names", action="store_true", help="find the channels only, no station names")
    args = ap.parse_args()
    unit = f"http://{args.host}:8080"
    run = ROOT / "runs" / "atsc" / time.strftime("%Y%m%d_%H%M%S")
    run.mkdir(parents=True, exist_ok=True)
    try:
        hd = hdhomerun(args.hdhomerun)
        before = call(f"{unit}/api/v1/status")["mode"]
        if before != "atsc":
            call(f"{unit}/api/v1/mode", "PUT", {"mode": "atsc"})
        try:
            call(f"{unit}/api/v1/atsc/scan", "POST", {"frames": args.frames, "gain_db": args.gain, "identify": not args.no_names})
            t0 = time.time()
            scan = call(f"{unit}/api/v1/atsc/scan")
            while scan["state"] == "sweeping" and time.time() - t0 < 600:
                time.sleep(1)
                scan = call(f"{unit}/api/v1/atsc/scan")
            scan["seconds"] = round(time.time() - t0, 1)
        finally:
            if before != "atsc" and not args.keep_mode:
                call(f"{unit}/api/v1/mode", "PUT", {"mode": before})
    except OSError as e:
        print(f"no answer: {e}", file=sys.stderr)
        return 2
    (run / "scan.json").write_text(json.dumps(scan, indent=1))
    (run / "hdhomerun.json").write_text(json.dumps(hd, indent=1))
    if scan["state"] != "done":
        print(f"scan {scan['state']}: {scan.get('error')}", file=sys.stderr)
        return 2

    found = {c["number"]: c for c in scan["found"]}
    rows, misses, wrong = [], [], []
    for n in sorted(set(found) | set(hd)):
        u, h = found.get(n), hd.get(n)
        want = EXPECT.get(h["modulation"]) if h else None
        received = bool(h and h["strength"] is not None)
        ok = (u is not None and want == u["kind"]) if h else (u is None or u["kind"] == "vacant")
        if h and received and not ok:
            misses.append(n)
        # The unit's virtual channels as the HDHomeRun writes its programs.
        station = (u or {}).get("station") or {}
        named = [f"{c['major']}.{c['minor']} {c['short_name']}" for c in station.get("channels", [])]
        listed = set(h["programs"]) if h else set()
        unlisted = [v for v in named if v not in listed]
        if unlisted:
            wrong.append(n)
        rows.append({"rf": n, "unit": u, "hdhomerun": h, "expected": want or "vacant", "ok": ok, "hdhomerun_receives": received,
                     "named": named, "named_unlisted": unlisted})
    (run / "compare.json").write_text(json.dumps(rows, indent=1))

    fmt = "{:>3} {:>9} | {:<9} {:>5} {:>6} {:>8} {:>7} {:>6} | {:<6} {:>4} {:>4} | {:<4} | {:>5} {:>5} {:<5} | {}"
    print(fmt.format("RF", "MHz", "unit", "C/N", "pilot", "offset", "dBm", "ppm", "HDHR", "str", "qual", "", "MER", "names", "", "programs"))
    for r in rows:
        u, h = r["unit"] or {}, r["hdhomerun"] or {}
        num = lambda v, d=1: "" if v is None else f"{v:.{d}f}"
        center = u.get("center_hz") or (rf_of_center(r["rf"]))
        print(fmt.format(
            r["rf"], f"{center / 1e6:.0f}", u.get("kind", "-"), num(u.get("level_db")), num(u.get("pilot_db")),
            "" if u.get("pilot_offset_hz") is None else f"{u['pilot_offset_hz'] / 1e3:+.1f}k", num(u.get("power_dbm")),
            "" if not u.get("clips_ppm") else f"{u['clips_ppm']:.0f}", h.get("modulation", "-") or "-", h.get("strength") or "", h.get("quality") or "",
            "ok" if r["ok"] else "MISS" if r["rf"] in misses else "diff", *names_cells(r), ", ".join(h.get("programs", [])[:3])))
    kinds = {k: sum(1 for c in scan["found"] if c["kind"] == k) for k in ("8vsb", "no_pilot", "vacant")}
    print(f"\n{len(scan['found'])} channels in {scan['seconds']} s: {kinds}; HDHomeRun lists {len(hd)} RF channels, "
          f"{sum(1 for h in hd.values() if h['strength'] is not None)} with a signal now; missed: {misses or 'none'}; "
          f"named: {[r['rf'] for r in rows if r['named']] or 'none'}; names the HDHomeRun does not list: {wrong or 'none'}. Run: {run}")
    for r in rows:
        if r["named"]:
            extra = f"  (not listed: {', '.join(r['named_unlisted'])})" if r["named_unlisted"] else ""
            print(f"  RF {r['rf']}: {', '.join(r['named'])}{extra}")
    return 1 if misses else 3 if wrong else 0


def names_cells(r: dict) -> tuple[str, str, str]:
    """MER, the virtual channels named that the HDHomeRun lists there of its programs, a mark."""
    station = (r["unit"] or {}).get("station") or {}
    mer = "" if station.get("mer_db") is None else f"{station['mer_db']:.1f}"
    programs = (r["hdhomerun"] or {}).get("programs", [])
    if not r["named"]:
        mark = "error" if station.get("error") else "-" if station else ""
        return mer, f"0/{len(programs)}" if station else "", mark
    alike = len(r["named"]) - len(r["named_unlisted"])
    return mer, f"{alike}/{len(programs)}", "ok" if not r["named_unlisted"] else "DIFF"


def rf_of_center(n: int) -> int:
    for first, last, low in BANDS:
        if first <= n <= last:
            return low + (n - first) * 6_000_000 + 3_000_000
    return 0


if __name__ == "__main__":
    sys.exit(main())
