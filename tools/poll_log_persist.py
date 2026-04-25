#!/usr/bin/env python3
"""Poll /api/log incrementally and append every new entry to a rolling
JSONL file. Lets us hold log history beyond the 16384-entry ring on
the board.

Usage:
    python tools/poll_log_persist.py --host 192.168.2.1:8080 \
        --outfile _validation/log_history.jsonl

Polls every 30 s by default; uses ?since=<seq> so each request only
returns new entries.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.request
import urllib.error
from pathlib import Path


def fetch(host: str, since: int, limit: int = 16384) -> dict | None:
    url = f"http://{host}/api/log?since={since}&limit={limit}"
    try:
        with urllib.request.urlopen(url, timeout=10) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        print(f"fetch error: {e}", file=sys.stderr)
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--outfile", default="_validation/log_history.jsonl")
    ap.add_argument("--interval", type=float, default=30.0,
                    help="Poll interval in seconds (default 30)")
    ap.add_argument("--max-runtime", type=float, default=0,
                    help="Stop after N seconds (0 = run forever)")
    args = ap.parse_args()

    out = Path(args.outfile)
    out.parent.mkdir(parents=True, exist_ok=True)
    last_seq = 0
    # If the outfile exists, read the last seq from it so we resume.
    if out.exists():
        try:
            with out.open("r", encoding="utf-8") as f:
                for line in f:
                    try:
                        j = json.loads(line)
                        s = j.get("seq", 0)
                        if s > last_seq:
                            last_seq = s
                    except json.JSONDecodeError:
                        continue
            print(f"resuming from seq {last_seq}")
        except IOError as e:
            print(f"warn: couldn't read existing outfile: {e}",
                  file=sys.stderr)

    started = time.time()
    appended_total = 0
    while True:
        data = fetch(args.host, last_seq)
        if data is not None:
            entries = data.get("entries", [])
            if entries:
                with out.open("a", encoding="utf-8") as f:
                    for e in entries:
                        f.write(json.dumps(e) + "\n")
                appended_total += len(entries)
                last_seq = max(e.get("seq", 0) for e in entries)
                print(f"+{len(entries)} entries (seq -> {last_seq}, "
                      f"total appended this run: {appended_total})")
        if args.max_runtime and (time.time() - started) >= args.max_runtime:
            print(f"max-runtime reached, exiting")
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    sys.exit(main())
