#!/usr/bin/env python3
"""Capture raw IMBE frames from Fishball and analyze them.

Usage:
    python tools/p25_imbe_test.py [TARGET]

Fetches the IMBE ring buffer from /api/imbe_dump, filters to
unencrypted frames, and dumps them for offline analysis. Also
saves as a binary file compatible with mbelib test tools.
"""

import json
import sys
import urllib.request


def fetch(target, path):
    url = f"http://{target}{path}"
    with urllib.request.urlopen(url, timeout=5) as r:
        return json.loads(r.read().decode("utf-8"))


def main():
    target = sys.argv[1] if len(sys.argv) >= 2 else "192.168.2.1:8080"
    print(f"Fetching IMBE ring from {target}...")

    data = fetch(target, "/api/imbe_dump")
    frames = data.get("frames", [])
    print(f"Got {len(frames)} frames in ring buffer")

    if not frames:
        print("No frames captured. Wait for a call and try again.")
        return

    # Separate encrypted vs clear
    clear = [f for f in frames if not f["encrypted"]]
    enc = [f for f in frames if f["encrypted"]]
    print(f"  Clear: {len(clear)}, Encrypted: {len(enc)}")

    # Show TG breakdown
    tgs = {}
    for f in frames:
        tg = f["talkgroup"]
        tgs[tg] = tgs.get(tg, 0) + 1
    for tg, count in sorted(tgs.items()):
        enc_flag = " [ENC]" if any(
            f["encrypted"] for f in frames if f["talkgroup"] == tg
        ) else ""
        print(f"  TG {tg}: {count} frames{enc_flag}")

    # Dump clear frames
    if clear:
        print(f"\nFirst 9 clear frames (1 LDU):")
        for i, f in enumerate(clear[:9]):
            h = f["hex"]
            print(f"  [{i}] TG={f['talkgroup']} {h}")

        # Save clear frames as binary
        out_path = "imbe_capture_clear.bin"
        with open(out_path, "wb") as fp:
            for f in clear:
                fp.write(bytes.fromhex(f["hex"]))
        print(f"\nSaved {len(clear)} clear frames to {out_path}")
        print(f"  ({len(clear) * 18} bytes, {len(clear) * 20}ms of audio)")

        # Also save as hex-per-line for easy parsing
        hex_path = "imbe_capture_clear.hex"
        with open(hex_path, "w") as fp:
            for f in clear:
                fp.write(f["hex"] + "\n")
        print(f"  Hex dump: {hex_path}")

    # Show frame entropy (encrypted frames should be high-entropy)
    if clear:
        bits_set = sum(
            bin(int(f["hex"], 16)).count("1") for f in clear
        )
        total_bits = len(clear) * 144
        print(f"\nClear frame bit stats: {bits_set}/{total_bits} ones "
              f"({100*bits_set/total_bits:.1f}%)")

    if enc:
        bits_set = sum(
            bin(int(f["hex"], 16)).count("1") for f in enc
        )
        total_bits = len(enc) * 144
        print(f"Encrypted frame bit stats: {bits_set}/{total_bits} ones "
              f"({100*bits_set/total_bits:.1f}%)")


if __name__ == "__main__":
    main()
