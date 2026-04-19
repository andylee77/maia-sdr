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


def source_column(entry: dict) -> str:
    """Match SDRTrunk's `<src-col>` — `CC` for control chain,
    `T1 <freq MHz>` for traffic chain (we don't track concurrent traffic
    channels yet, so always T1), `PS` for ps_c4fm dormant chain."""
    f = entry.get("fields", {})
    chain = f.get("chain", "")
    cat = entry.get("category", "")
    if chain == "control" or cat == "grant":
        return "CC"
    if chain == "traffic" or cat in ("imbe", "recorder"):
        return "T1"
    if chain == "ps_c4fm":
        return "PS"
    return cat.upper()[:3] if cat else "-"


def nac_decimal_hex(entry: dict) -> str:
    """SDRTrunk renders NAC as `NAC:<decimal>/x<hex>`. Our fields store
    hex only, so reconstruct decimal from the hex string."""
    f = entry.get("fields", {})
    nac_field = f.get("nac")
    if isinstance(nac_field, str) and nac_field.startswith("0x"):
        try:
            n = int(nac_field, 16)
            return f"NAC:{n}/x{n:03X}"
        except ValueError:
            return ""
    if isinstance(nac_field, int):
        return f"NAC:{nac_field}/x{nac_field:03X}"
    return ""


def render_sdrtrunk_line(entry: dict) -> str:
    """SDRTrunk-timeline-style line. Matches the shape of
    doc/diagnostics/2026-04-19/sdrtrunk_call_timeline/timeline_unified.log:

        HH:MM:SS  <src>  PASSED    NAC:<dec>/x<hex> <message>

    For non-NAC-bearing categories (recorder, vocoder, system) the
    NAC column is blank so the columns still line up in editors."""
    ts_ms = entry.get("timestamp_ms", 0)
    dts = dt.datetime.fromtimestamp(ts_ms / 1000).strftime("%H:%M:%S")
    src = source_column(entry)
    status = status_from(entry)
    nac = nac_decimal_hex(entry)
    msg = entry.get("message", "")
    nac_cell = f"{nac} " if nac else ""
    return f"{dts}  {src:<14}  {status:<9}  {nac_cell}{msg}"


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
