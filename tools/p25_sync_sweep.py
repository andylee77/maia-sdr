#!/usr/bin/env python3
"""Phase 6F.7+ sync threshold sweep tool.

Walks `SYNC_THRESHOLD` through a configurable list of values, resets the
decoder counters between each one, waits for a measurement window, and
prints the resulting `messages/sec`, `tsdu/sec`, NID OK rate, and
per-block-position CRC OK rates. The output table tells you which
threshold maximises useful throughput on the current signal.

Requires p25-httpd >= phase6f.7 (the build that exposed
`/api/sync_tune?threshold=N` and `/api/decoder_reset`).

Usage:
    python tools/p25_sync_sweep.py [TARGET] [DURATION_SEC] [THRESHOLDS...]

Examples:
    # default: sweep 4,6,8,10,12,14 against 192.168.2.1:8080, 30s each
    python tools/p25_sync_sweep.py

    # 60-second windows on a different host
    python tools/p25_sync_sweep.py fishball.local:8080 60

    # explicit threshold list
    python tools/p25_sync_sweep.py 192.168.2.1:8080 45 4 7 10 13 16

This is a "what's the optimum NOW" probe -- the result depends on PLL
lock state, slicer SNR, and on-air activity. Re-run periodically.
"""

from __future__ import annotations

import json
import sys
import time
import urllib.error
import urllib.request

GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
CYAN = "\033[36m"
DIM = "\033[2m"
BOLD = "\033[1m"
RESET = "\033[0m"


def fetch(target: str, path: str) -> dict:
    url = f"http://{target}{path}"
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.URLError as e:
        print(f"{RED}!! GET {url} failed: {e}{RESET}", file=sys.stderr)
        sys.exit(2)


def set_threshold(target: str, n: int) -> None:
    fetch(target, f"/api/sync_tune?threshold={n}")


def reset(target: str) -> None:
    fetch(target, "/api/decoder_reset")


def measure(target: str, duration_sec: int) -> dict:
    """Reset, wait, and snapshot the decoder counters."""
    reset(target)
    time.sleep(duration_sec)
    op = fetch(target, "/api/tsbk_opcodes")
    dump = fetch(target, "/api/lsm_dibit_dump")

    pos = op.get("by_position", {})
    pipeline = dump.get("pipeline", {})
    sync = dump.get("sync", {})

    return {
        "tsdu_attempts": op.get("tsdu_attempts", 0),
        "tsbk_block_attempts_total": op.get("tsbk_block_attempts_total", 0),
        "blocks_per_tsdu": op.get("blocks_per_tsdu", 0),
        "crc_ok_total": op.get("crc_ok_total", 0),
        "crc_fail_total": op.get("crc_fail_total", 0),
        "crc_ok_pct": op.get("crc_ok_pct", 0),
        "tsbk1_pct": pos.get("tsbk1", {}).get("crc_ok_pct", 0),
        "tsbk2_pct": pos.get("tsbk2", {}).get("crc_ok_pct", 0),
        "tsbk3_pct": pos.get("tsbk3", {}).get("crc_ok_pct", 0),
        "nid_attempts": pipeline.get("nid_attempts", 0),
        "nid_decoded_ok": pipeline.get("nid_decoded_ok", 0),
        "nid_pct": (
            100.0 * pipeline.get("nid_decoded_ok", 0)
            / max(pipeline.get("nid_attempts", 1), 1)
        ),
        "sync_hits": sync.get("hits", 0),
    }


def main() -> int:
    args = sys.argv[1:]
    target = "192.168.2.1:8080"
    duration = 30
    thresholds = [4, 6, 8, 10, 12, 14]

    if args:
        target = args[0]
        args = args[1:]
    if args:
        try:
            duration = int(args[0])
            args = args[1:]
        except ValueError:
            pass
    if args:
        try:
            thresholds = [int(x) for x in args]
        except ValueError:
            print(f"{RED}!! threshold list must be integers{RESET}")
            return 2

    # Bail early if the target isn't running 6F.7+ (the endpoints
    # don't exist on older builds).
    sys_info = fetch(target, "/api/system")
    build = sys_info.get("build", "")
    import re
    m = re.search(r"phase6f\.(\d+)", build or "")
    needs_warn = not (m and int(m.group(1)) >= 7)
    if needs_warn:
        print(f"{YELLOW}!! target build {build!r} may not have "
              f"/api/sync_tune (need >= phase6f.7){RESET}")

    print(f"{BOLD}{CYAN}Phase 6F.7 sync threshold sweep{RESET}")
    print(f"  target:     {target}")
    print(f"  build:      {build}")
    print(f"  thresholds: {thresholds}")
    print(f"  window:     {duration}s per threshold")
    print(f"  total time: ~{duration * len(thresholds)}s")
    print()

    # Header
    print(f"{BOLD}  {'thr':>4s} {'sync/s':>7s} {'tsdu/s':>7s} {'msg/s':>6s} "
          f"{'nid%':>5s} {'TSBK1%':>7s} {'TSBK2%':>7s} {'TSBK3%':>7s} "
          f"{'b/tsdu':>7s}{RESET}")
    print(f"  {'-'*4} {'-'*7} {'-'*7} {'-'*6} {'-'*5} {'-'*7} {'-'*7} "
          f"{'-'*7} {'-'*7}")

    results = []
    for n in thresholds:
        print(f"  {DIM}[{n:>2d}] setting threshold...{RESET}", end="", flush=True)
        set_threshold(target, n)
        print(f" measuring {duration}s...", end="", flush=True)

        m = measure(target, duration)
        sync_per_s = m["sync_hits"] / duration
        tsdu_per_s = m["tsdu_attempts"] / duration
        msg_per_s = m["crc_ok_total"] / duration
        nid_pct = m["nid_pct"]
        bptsdu = m["blocks_per_tsdu"]

        # Best line gets highlighted later. Compute "useful score":
        # CRC-OK blocks/sec is the closest metric to "decoded radio
        # info per unit time".
        score = msg_per_s

        results.append({
            "threshold": n,
            "sync_per_s": sync_per_s,
            "tsdu_per_s": tsdu_per_s,
            "msg_per_s": msg_per_s,
            "nid_pct": nid_pct,
            "tsbk1_pct": m["tsbk1_pct"],
            "tsbk2_pct": m["tsbk2_pct"],
            "tsbk3_pct": m["tsbk3_pct"],
            "blocks_per_tsdu": bptsdu,
            "score": score,
        })

        print(f"\r  {n:>4d} {sync_per_s:>7.2f} {tsdu_per_s:>7.2f} "
              f"{msg_per_s:>6.2f} {nid_pct:>5.1f} {m['tsbk1_pct']:>7.1f} "
              f"{m['tsbk2_pct']:>7.1f} {m['tsbk3_pct']:>7.1f} {bptsdu:>7.2f}")

    # Best threshold by score
    best = max(results, key=lambda r: r["score"])
    print()
    print(f"{BOLD}{GREEN}Best threshold by msg/s: "
          f"{best['threshold']} -> {best['msg_per_s']:.2f} msg/s "
          f"({best['tsdu_per_s']:.2f} tsdu/s, "
          f"NID {best['nid_pct']:.1f}%){RESET}")
    print()

    # Restore default threshold (6) so we don't leave the radio in a
    # weird state after a sweep.
    set_threshold(target, 6)
    print(f"{DIM}Threshold restored to 6 (default){RESET}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
