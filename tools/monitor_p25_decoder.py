#!/usr/bin/env python3
"""monitor_p25_decoder.py -- capture p25-httpd diagnostic data over time.

Polls the p25-httpd dashboard's `/api/dibit_dump` and `/api/stats` endpoints
on a schedule and writes a JSONL snapshot file. Also prints a one-line live
summary per poll so you can watch the symbol timing loop converge in real
time without having to ssh + tail + grep yourself into knots.

Why this exists
---------------
The Gardner symbol timing loop in `p25_hdl/symbol_timing.py` converges
slowly (KP=185, KI=1 are very small for a Q16 fixed-point loop). On hardware
the best Hamming distance to the P25 frame sync drifts from ~30 down to ~7
over the course of MINUTES TO HOURS rather than the usual ~10 ms a properly
tuned PI loop should take. We need to characterise that convergence to
decide whether the right fix is just retuning the gains, swapping the TED
algorithm, or adding a hardware DC blocker. This script makes it cheap to
collect the data: leave it running for an hour, then look at the JSONL.

Output format
-------------
JSONL: one JSON object per line. Each object is:

    {
        "t":               123.456,         // seconds since script start
        "wall_iso":        "2026-04-09T...", // ISO 8601 wall-clock timestamp
        "dibit_dump":      { ... },          // raw JSON from /api/dibit_dump
        "stats":           { ... },          // raw JSON from /api/stats
        "deltas": {                          // computed against previous poll
            "dibits_delta":      12345,
            "dibits_per_sec":    4794.5,
            "sync_hits_delta":   23,
            "sync_hits_per_sec": 8.95
        }
    }

You can later load it with pandas:

    import pandas as pd, json
    df = pd.read_json("decoder.jsonl", lines=True)
    df["best_distance"] = df["dibit_dump"].apply(lambda d: d["sync"]["best_distance"])
    df.plot(x="t", y="best_distance")

Usage
-----
    python tools/monitor_p25_decoder.py \\
        --target 192.168.2.1:8080 \\
        --interval 30 \\
        --duration 3600 \\
        --out runs/decoder_$(date +%Y%m%dT%H%M%S).jsonl

Stop early with Ctrl+C; the file is flushed after every poll so partial
captures are always safe.
"""
from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone


def fetch_json(url: str, timeout: float = 5.0) -> dict | None:
    """GET a JSON endpoint. Returns dict on success, None on any failure."""
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, OSError) as e:
        print(f"  WARN: fetch {url} failed: {e}", file=sys.stderr)
        return None
    except json.JSONDecodeError as e:
        print(f"  WARN: fetch {url} returned non-JSON: {e}", file=sys.stderr)
        return None


def safe_get(d: dict | None, *path, default=None):
    """Walk a nested dict path, returning `default` if any link is missing."""
    cur = d
    for k in path:
        if not isinstance(cur, dict) or k not in cur:
            return default
        cur = cur[k]
    return cur


def format_summary(t: float, dump: dict | None, stats: dict | None,
                   deltas: dict, lifetime_best: int | None) -> str:
    """One-line live summary of the most useful fields. Compact on purpose."""
    # u32::MAX is the sentinel the decoder uses for "no sync seen since the
    # last 16k-dibit reset window", which is what you see right after a fresh
    # service restart or for a window with zero hits. Display as "--" so the
    # eye doesn't latch onto a meaningless 4-billion number.
    #
    # IMPORTANT: the decoder's `best_distance` is the lowest Hamming distance
    # observed in the LAST ~3.4 SECOND WINDOW (16384-dibit reset cadence,
    # see control_channel.rs:217-246). It is NOT a lifetime minimum. So
    # window-to-window jumping (32 → 22 → 33 → --) does NOT mean the loop
    # is converging or diverging -- it means each window is an independent
    # draw. Use the `best_lt` value below for a true lifetime minimum.
    best_raw = safe_get(dump, "sync", "best_distance", default=None)
    is_sentinel = best_raw is None or best_raw >= (1 << 32) - 1
    best = "--" if is_sentinel else str(best_raw)
    best_lt_str = "--" if lifetime_best is None else str(lifetime_best)
    hits = safe_get(dump, "sync", "hits", default=0)
    inner = safe_get(dump, "histogram", "inner_pct", default=0.0)
    outer = safe_get(dump, "histogram", "outer_pct", default=0.0)
    duid7 = safe_get(dump, "raw_duid", "pct_7_tsdu", default=0.0)
    duid_n = safe_get(dump, "raw_duid", "total", default=0)
    rssi = safe_get(stats, "rx_rssi_db", default=None)
    gain = safe_get(stats, "rx_gain_db", default=None)
    overflow = safe_get(stats, "overflow", default=False)
    dibits_per_sec = deltas.get("dibits_per_sec", 0.0)
    hits_per_sec = deltas.get("sync_hits_per_sec", 0.0)

    rssi_s = f"{rssi:5.1f}" if isinstance(rssi, (int, float)) else "  -- "
    gain_s = f"{gain:4.1f}" if isinstance(gain, (int, float)) else "  --"
    return (
        f"[t={t:6.0f}s] "
        f"best_w={best!s:>3} "
        f"best_lt={best_lt_str:>3} "
        f"hits={hits:>5} ({hits_per_sec:4.1f}/s) "
        f"in/out={inner:4.1f}/{outer:4.1f}% "
        f"duid7={duid7:4.1f}% (n={duid_n:>4}) "
        f"rssi={rssi_s}dB gain={gain_s}dB "
        f"sym/s={dibits_per_sec:6.0f} "
        f"ovf={'Y' if overflow else 'n'}"
    )


def compute_deltas(dump: dict | None, stats: dict | None,
                   prev: dict | None, dt: float) -> dict:
    """Per-poll rates relative to the previous snapshot."""
    if prev is None or dt <= 0:
        return {"dibits_delta": 0, "dibits_per_sec": 0.0,
                "sync_hits_delta": 0, "sync_hits_per_sec": 0.0}

    dibits_now = safe_get(dump, "total_dibits", default=0) or 0
    dibits_prev = safe_get(prev.get("dibit_dump"), "total_dibits", default=0) or 0
    hits_now = safe_get(dump, "sync", "hits", default=0) or 0
    hits_prev = safe_get(prev.get("dibit_dump"), "sync", "hits", default=0) or 0

    # total_dibits resets on service restart -- guard against negative deltas
    dibits_delta = max(0, dibits_now - dibits_prev)
    hits_delta = max(0, hits_now - hits_prev)
    return {
        "dibits_delta": dibits_delta,
        "dibits_per_sec": dibits_delta / dt,
        "sync_hits_delta": hits_delta,
        "sync_hits_per_sec": hits_delta / dt,
    }


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Poll p25-httpd diagnostics and write JSONL snapshots.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    p.add_argument(
        "--target", default="192.168.2.1:8080",
        help="host:port of the p25-httpd dashboard (default: %(default)s)",
    )
    p.add_argument(
        "--interval", type=float, default=30.0,
        help="seconds between polls (default: %(default)s)",
    )
    p.add_argument(
        "--duration", type=float, default=3600.0,
        help="total run time in seconds, 0 for unlimited (default: %(default)s)",
    )
    p.add_argument(
        "--out", default=None,
        help="output JSONL file (default: runs/decoder_<timestamp>.jsonl)",
    )
    p.add_argument(
        "--quiet", action="store_true",
        help="suppress live summary lines (still writes JSONL)",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()

    out_path = args.out
    if out_path is None:
        ts = datetime.now().strftime("%Y%m%dT%H%M%S")
        out_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                               "..", "runs")
        os.makedirs(out_dir, exist_ok=True)
        out_path = os.path.normpath(os.path.join(out_dir, f"decoder_{ts}.jsonl"))
    else:
        out_dir = os.path.dirname(out_path)
        if out_dir:
            os.makedirs(out_dir, exist_ok=True)

    base = f"http://{args.target}"
    dump_url = f"{base}/api/dibit_dump"
    stats_url = f"{base}/api/stats"

    print(f"# target:   {base}")
    print(f"# interval: {args.interval}s")
    print(f"# duration: {args.duration}s ({'unlimited' if args.duration == 0 else f'{int(args.duration / 60)} min'})")
    print(f"# output:   {out_path}")
    print(f"# Ctrl+C to stop early; partial captures are always safe.")
    print()

    # Graceful shutdown on Ctrl+C: drop into the loop's normal exit path
    stop = {"requested": False}

    def handle_sigint(signum, frame):
        stop["requested"] = True
        print("\n# Ctrl+C received -- finishing current poll then exiting.",
              file=sys.stderr)

    signal.signal(signal.SIGINT, handle_sigint)

    start = time.monotonic()
    prev_snapshot: dict | None = None
    prev_t = start
    # Lifetime minimum across the whole run -- the decoder's own
    # `best_distance` resets every ~3.4s window so we have to track this
    # client-side. If this number ever drops below the SYNC_THRESHOLD it's
    # the first hard evidence that the symbol timing loop actually works.
    # If it stays bouncing in the 7..14 range for the entire run, the loop
    # is structurally broken (random fluke matches, no real tracking).
    lifetime_best: int | None = None

    with open(out_path, "a", encoding="utf-8") as fh:
        poll_idx = 0
        while True:
            t = time.monotonic() - start
            wall = datetime.now(timezone.utc).isoformat()

            dump = fetch_json(dump_url)
            stats = fetch_json(stats_url)

            dt = time.monotonic() - prev_t
            deltas = compute_deltas(dump, stats, prev_snapshot, dt)

            # Update lifetime minimum from the per-window best, ignoring
            # the u32::MAX sentinel that means "no sync seen this window".
            best_raw = safe_get(dump, "sync", "best_distance", default=None)
            if best_raw is not None and best_raw < (1 << 32) - 1:
                if lifetime_best is None or best_raw < lifetime_best:
                    lifetime_best = best_raw

            snapshot = {
                "t": round(t, 3),
                "wall_iso": wall,
                "poll_idx": poll_idx,
                "dibit_dump": dump,
                "stats": stats,
                "deltas": deltas,
                "lifetime_best_distance": lifetime_best,
            }
            fh.write(json.dumps(snapshot, separators=(",", ":")) + "\n")
            fh.flush()

            if not args.quiet:
                print(format_summary(t, dump, stats, deltas, lifetime_best))

            prev_snapshot = snapshot
            prev_t = time.monotonic()
            poll_idx += 1

            if stop["requested"]:
                break
            if args.duration > 0 and t >= args.duration:
                break

            # Sleep in small chunks so Ctrl+C is responsive
            sleep_until = time.monotonic() + args.interval
            while time.monotonic() < sleep_until and not stop["requested"]:
                time.sleep(min(0.5, sleep_until - time.monotonic()))
            if stop["requested"]:
                break

    print(f"\n# wrote {poll_idx} snapshots to {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
