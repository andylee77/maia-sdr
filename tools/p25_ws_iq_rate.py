#!/usr/bin/env python3
"""Measure actual /ws/iq byte rate per source (post_ddc vs post_lsm).

Opens the WebSocket for a few seconds and compares observed sample
throughput to the hello-announced sample_rate_hz. Any mismatch is the
"eye plot off by 2x" symptom showing up in the raw data.
"""
import argparse
import json
import time
import websocket


def measure(host: str, chain: str, source: str, secs: float) -> dict:
    url = f"ws://{host}/ws/iq?chain={chain}&source={source}"
    ws = websocket.create_connection(url, timeout=5)
    hello = None
    bytes_total = 0
    # 4 bytes per IQ sample (i16 re + i16 im)
    BYTES_PER_SAMPLE = 4
    t0 = None
    try:
        while True:
            msg = ws.recv()
            if isinstance(msg, str):
                if hello is None:
                    hello = json.loads(msg)
                continue
            if t0 is None:
                t0 = time.monotonic()
            bytes_total += len(msg)
            if time.monotonic() - t0 >= secs:
                break
    finally:
        ws.close()
    elapsed = time.monotonic() - t0
    samples = bytes_total / BYTES_PER_SAMPLE
    observed_sps = samples / elapsed
    advertised_sps = (hello or {}).get("sample_rate_hz", 0)
    ratio = observed_sps / advertised_sps if advertised_sps else float("nan")
    return {
        "chain": chain,
        "source": source,
        "hello": hello,
        "elapsed_s": round(elapsed, 3),
        "bytes": bytes_total,
        "samples": int(samples),
        "observed_sps": round(observed_sps, 1),
        "advertised_sps": advertised_sps,
        "observed_over_advertised": round(ratio, 3),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--secs", type=float, default=5.0)
    args = ap.parse_args()

    for chain, source in [
        ("control", "post_ddc"),
        ("control", "post_lsm"),
    ]:
        r = measure(args.host, chain, source, args.secs)
        print(json.dumps(r, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
