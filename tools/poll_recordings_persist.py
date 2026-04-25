#!/usr/bin/env python3
"""Poll /api/recordings + download every new WAV to local disk so the
audio survives the on-board ring cap (40) for cross-check against the
log_history.jsonl.

Saves to <outdir>/recs/<id>_<unix_ms>_tg<tg>_src<src>.wav and writes
<outdir>/recordings_index.jsonl with one JSON line per pulled
recording (metadata snapshot at pull time).
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.request
import urllib.error
from pathlib import Path


def fetch_json(host: str, path: str, timeout: float = 10.0):
    url = f"http://{host}{path}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        print(f"fetch error {path}: {e}", file=sys.stderr)
        return None


def fetch_bytes(host: str, path: str, timeout: float = 30.0):
    url = f"http://{host}{path}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.read()
    except (urllib.error.URLError, TimeoutError) as e:
        print(f"fetch_bytes error {path}: {e}", file=sys.stderr)
        return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--outdir", default="_validation/audio")
    ap.add_argument("--interval", type=float, default=15.0)
    ap.add_argument("--max-runtime", type=float, default=0,
                    help="Stop after N seconds (0 = run forever)")
    args = ap.parse_args()

    out = Path(args.outdir)
    (out / "recs").mkdir(parents=True, exist_ok=True)
    index_path = out / "recordings_index.jsonl"

    # Track which recording IDs we've already pulled (id is unique per process)
    pulled = set()
    if index_path.exists():
        with index_path.open("r", encoding="utf-8") as f:
            for line in f:
                try:
                    j = json.loads(line)
                    if "filename" in j:
                        pulled.add(j["id"])
                except json.JSONDecodeError:
                    continue
        print(f"resuming: {len(pulled)} recordings already pulled")

    started = time.time()
    while True:
        data = fetch_json(args.host, "/api/recordings")
        if data and "items" in data:
            for r in data["items"]:
                rid = r.get("id")
                if rid is None or rid in pulled:
                    continue
                # Filename
                tg = r.get("talkgroup", 0)
                src = r.get("source")
                src_s = str(src) if src else "none"
                start_ms = r.get("started_unix_ms", 0)
                fname = f"{rid:05d}_{start_ms}_tg{tg}_src{src_s}.wav"
                local_path = out / "recs" / fname
                if local_path.exists():
                    pulled.add(rid)
                    continue
                # Download
                wav_bytes = fetch_bytes(args.host, f"/api/recordings/{rid}.wav")
                if wav_bytes is None:
                    continue
                with local_path.open("wb") as f:
                    f.write(wav_bytes)
                # Append to index
                entry = dict(r)
                entry["filename"] = fname
                entry["pulled_unix_ms"] = int(time.time() * 1000)
                with index_path.open("a", encoding="utf-8") as f:
                    f.write(json.dumps(entry) + "\n")
                pulled.add(rid)
                print(f"pulled rec#{rid} -> {fname} ({len(wav_bytes)} bytes)")
        if args.max_runtime and (time.time() - started) >= args.max_runtime:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    sys.exit(main())
