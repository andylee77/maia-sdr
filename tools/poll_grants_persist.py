#!/usr/bin/env python3
"""Poll /api/grant_decode_stats and append every newly-completed grant
to a rolling JSONL. The on-board ring caps at 20 — this captures grants
indefinitely so per-call accountability can be done over hours of data.

Dedup by (started_unix_ms, tg, source) — grants don't change after
SpeakerEnd so first-seen wins.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.request
import urllib.error
from pathlib import Path


def fetch(host: str, timeout: float = 8.0):
    url = f"http://{host}/api/grant_decode_stats"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        print(f"fetch error: {e}", file=sys.stderr)
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--outfile", default="_validation/grants_history.jsonl")
    ap.add_argument("--interval", type=float, default=10.0)
    ap.add_argument("--max-runtime", type=float, default=0)
    args = ap.parse_args()

    out = Path(args.outfile)
    out.parent.mkdir(parents=True, exist_ok=True)
    seen = set()
    if out.exists():
        with out.open("r", encoding="utf-8") as f:
            for line in f:
                try:
                    j = json.loads(line)
                    seen.add((j['started_unix_ms'], j['tg'],
                              j.get('source')))
                except (json.JSONDecodeError, KeyError):
                    continue
        print(f"resuming: {len(seen)} grants already saved")

    started = time.time()
    while True:
        data = fetch(args.host)
        if data and 'items' in data:
            new_count = 0
            with out.open("a", encoding="utf-8") as f:
                for g in data['items']:
                    key = (g['started_unix_ms'], g['tg'], g.get('source'))
                    if key in seen:
                        continue
                    seen.add(key)
                    f.write(json.dumps(g) + "\n")
                    new_count += 1
            if new_count > 0:
                print(f"+{new_count} grants (total {len(seen)})")
        if args.max_runtime and (time.time() - started) >= args.max_runtime:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    sys.exit(main())
