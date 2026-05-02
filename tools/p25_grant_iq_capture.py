#!/usr/bin/env python3
"""
p25_grant_iq_capture.py — wait for the next P25 voice grant and snap a
fixed-duration wideband IQ capture aligned to it. Logs the LCN
frequency + talkgroup so the offline software_decode can target it.

Usage:
  python tools/p25_grant_iq_capture.py [--seconds 5] [--clear-only]
                                       [--host 192.168.2.1:8080]

Workflow:
  1. Polls /api/grants every 200 ms.
  2. On first non-empty entry that ISN'T encrypted, POSTs to
     /api/wideband_iq_capture?seconds=N immediately.
  3. Logs the grant + capture file path.
  4. Prints a ready-to-paste cargo-test command line so you can run
     software_decode against the capture without retyping the
     center / target frequency.

Use --clear-only to skip and just dump current grant state for debug.
"""

import argparse
import sys
import time
import urllib.request
import json


def http_get(url: str, timeout: float = 5.0):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.load(r)


def http_post(url: str, timeout: float = 5.0):
    req = urllib.request.Request(url, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="192.168.2.1:8080",
                    help="board host:port")
    ap.add_argument("--seconds", type=float, default=5.0,
                    help="capture duration (seconds)")
    ap.add_argument("--clear-only", action="store_true",
                    help="dump current grants and exit")
    ap.add_argument("--allow-encrypted", action="store_true",
                    help="trigger on encrypted grants too (no audio possible)")
    ap.add_argument("--target-tg", type=int, default=None,
                    help="only trigger for this talkgroup id")
    args = ap.parse_args()

    base = f"http://{args.host}"

    # Snapshot the system state for the cargo-test recipe.
    sys_info = http_get(f"{base}/api/system")
    stats = http_get(f"{base}/api/stats")
    rx_lo = stats.get("rx_lo_hz")
    print(f"build = {sys_info.get('build')}, rx_lo = {rx_lo} Hz")

    if args.clear_only:
        grants = http_get(f"{base}/api/grants")
        print(json.dumps(grants, indent=2))
        return 0

    print(f"watching /api/grants every 200 ms; --seconds={args.seconds}",
          file=sys.stderr)
    seen_call_ids = set()
    poll_n = 0

    while True:
        try:
            grants = http_get(f"{base}/api/grants")
        except Exception as e:
            print(f"  poll {poll_n}: {e}", file=sys.stderr)
            time.sleep(0.5)
            continue

        if grants:
            # Find the first interesting grant.
            for g in grants:
                # Grants come as dicts; identify by call_id (stable across polls)
                # or fall back to (tg, freq, started_unix_ms).
                cid = g.get("call_id") or (
                    g.get("talkgroup"), g.get("frequency_hz"),
                    g.get("started_unix_ms"),
                )
                if cid in seen_call_ids:
                    continue
                seen_call_ids.add(cid)

                if g.get("encrypted") and not args.allow_encrypted:
                    print(f"  poll {poll_n}: skip encrypted TG "
                          f"{g.get('talkgroup')}", file=sys.stderr)
                    continue
                if args.target_tg is not None and \
                        g.get("talkgroup") != args.target_tg:
                    continue

                print(f"GRANT {g}")
                # Trigger capture as fast as possible.
                cap = http_post(
                    f"{base}/api/wideband_iq_capture?seconds={args.seconds}")
                print(f"CAPTURE {cap}")

                target_hz = g.get("frequency_hz") or g.get("freq_hz")
                if target_hz is None:
                    # /api/grants returns frequency_mhz, not _hz.
                    fmhz = g.get("frequency_mhz")
                    if fmhz is not None:
                        target_hz = int(round(fmhz * 1_000_000))
                tg = g.get("talkgroup")
                # Print ready-to-run recipe for the host.
                path = cap.get("path")
                print()
                print("=== ready-to-run software_decode recipe ===")
                print(f"# scp the capture down:")
                print(f"scp -O root@{args.host.split(':')[0]}:{path} "
                      f"/c/Users/Andy/AppData/Local/Temp/wb_iq_call.cs16")
                print(f"# then offline decode it (TG {tg}):")
                print(f"SOFTDEC_INPUT=/c/Users/Andy/AppData/Local/Temp/wb_iq_call.cs16 \\")
                print(f"  SOFTDEC_INPUT_RATE=8000000 \\")
                print(f"  SOFTDEC_CENTER_HZ={rx_lo} \\")
                print(f"  SOFTDEC_TARGET_HZ={target_hz} \\")
                print(f"  SOFTDEC_OUT_WAV=/c/Users/Andy/AppData/Local/Temp/sw_call.wav \\")
                print(f"  cargo test --release software_decode -- "
                      f"--ignored --nocapture")
                return 0
        else:
            poll_n += 1
            if poll_n % 25 == 0:
                print(f"  poll {poll_n}: no grants yet "
                      f"({0.2 * poll_n:.1f}s elapsed)", file=sys.stderr)
        time.sleep(0.2)


if __name__ == "__main__":
    sys.exit(main())
