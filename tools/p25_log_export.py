#!/usr/bin/env python3
"""Export the Fishball event-log ring to files.

Pulls /api/log?limit=16384 and writes both:
  - raw JSON as-returned (for programmatic analysis)
  - SDRTrunk-style decoded_messages.log-ish text rendering (human read)

Usage:
    python tools/p25_log_export.py --host 192.168.2.1:8080 \\
        --outdir doc/diagnostics/2026-04-19/debug_logs \\
        [--category duid|grant|imbe|recorder|traffic|vocoder]

With --category, only entries of that category are written. Without, a
unified log across all categories is written, interleaved in timestamp
order (matching SDRTrunk's `decoded_messages.log` multi-source style).

The SDRTrunk-style format is:
  <YYYY-MM-DD HH:MM:SS.mmm> ,<STATUS>,<CATEGORY>,<message>

Where STATUS is PASSED / CORRECTED / FAILED for DUID entries (based on
the BCH(63,16,t=11) correction count), INFO for all other categories.
This mirrors SDRTrunk's MessageEventLogger.java "PASSED/FAILED" flag
but adds an intermediate CORRECTED state for our FEC telemetry.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import sys
import urllib.error
import urllib.request


def fetch(host: str, category: str | None, limit: int) -> dict:
    q = f"limit={limit}"
    if category:
        q += f"&category={category}"
    url = f"http://{host}/api/log?{q}"
    with urllib.request.urlopen(url, timeout=15) as r:
        return json.loads(r.read().decode("utf-8"))


def status_from(entry: dict) -> str:
    if entry.get("category") != "duid":
        return "INFO"
    bch = entry.get("fields", {}).get("bch_errors", 0)
    if bch == 0:
        return "PASSED"
    if bch <= 11:
        return "CORRECTED"
    return "FAILED"


def render_sdrtrunk_line(entry: dict) -> str:
    ts_ms = entry.get("timestamp_ms", 0)
    dts = dt.datetime.fromtimestamp(ts_ms / 1000).strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]
    cat = entry.get("category", "?").upper()
    msg = entry.get("message", "")
    status = status_from(entry)
    return f"{dts} ,{status:<10},{cat:<9}, {msg}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--outdir", default=".")
    ap.add_argument(
        "--category",
        choices=["duid", "grant", "imbe", "recorder", "traffic", "vocoder"],
        help="If set, only write entries of this category",
    )
    ap.add_argument("--limit", type=int, default=16384)
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)

    try:
        data = fetch(args.host, args.category, args.limit)
    except (urllib.error.URLError, TimeoutError) as e:
        print(f"fetch failed: {e}", file=sys.stderr)
        return 1

    entries = data.get("entries", [])
    print(f"fetched {len(entries)} entries  last_seq: {data.get('last_seq')}")
    if not entries:
        print("no entries — board idle or log ring empty")
        return 0

    tag = args.category or "all"
    json_path = os.path.join(args.outdir, f"log_{tag}.json")
    text_path = os.path.join(args.outdir, f"log_{tag}_sdrtrunk_style.log")

    with open(json_path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
    print(f"wrote raw JSON: {json_path}")

    with open(text_path, "w", encoding="utf-8") as f:
        for e in entries:
            f.write(render_sdrtrunk_line(e))
            f.write("\n")
    print(f"wrote SDRTrunk-style: {text_path}")

    # Category tallies for quick sanity
    by_cat: dict[str, int] = {}
    for e in entries:
        c = e.get("category", "?")
        by_cat[c] = by_cat.get(c, 0) + 1
    print("by category:")
    for k, v in sorted(by_cat.items(), key=lambda x: -x[1]):
        print(f"  {k:10s} {v}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
