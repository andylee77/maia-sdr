#!/usr/bin/env python3
"""
p25_dibit_diff.py — slide-align two P25 dibit streams (one from our LSM
demod, one from SDRTrunk's `.bits` reference) and locate positions where
they diverge. Used to figure out *where* in a call our demod loses
synchronisation vs SDRTrunk's, given that:

  * Our framer is bit-exact correct (proven by feeding SDRTrunk's .bits
    through it: identical LDU1/LDU2 counts).
  * Our DDC is exonerated (per-call WAV gives the same gap as wideband).
  * The 6.5 % LDU1 deficit must therefore come from per-symbol
    differences in the demod path (PLL/Gardner/AGC dynamics, filter
    transient, resampling chain).

Both .bits files use SDRTrunk's packing: 4 dibits per byte, MSB-first.

Usage:
  python tools/p25_dibit_diff.py OURS.bits REF.bits [--window 200] \\
      [--match-threshold 0.85] [--max-search 16384]

The aligner scans `OURS` against the first ~max-search dibits of REF,
finds the offset that maximises agreement in the first 1024-dibit window
(this assumes that within that window we're in coarse sync at least
once), then reports per-window agreement rate from that offset onward.
"""

import argparse
import sys
from pathlib import Path


def unpack_dibits(packed: bytes) -> list[int]:
    """Unpack 4-dibits-per-byte (MSB-first) into a list of dibits 0..3."""
    out = []
    for b in packed:
        out.append((b >> 6) & 0x3)
        out.append((b >> 4) & 0x3)
        out.append((b >> 2) & 0x3)
        out.append(b & 0x3)
    return out


def find_best_offset(ours: list[int], ref: list[int],
                     probe_len: int, max_search: int) -> tuple[int, float]:
    """Slide `ours` against `ref` (offsets in `ref`) and find the offset
    that maximises agreement over the first `probe_len` dibits."""
    probe = ours[:probe_len]
    if len(probe) < probe_len:
        probe_len = len(probe)
    best_off = 0
    best_match = -1
    upper = min(max_search, len(ref) - probe_len)
    for off in range(upper):
        match = sum(1 for i in range(probe_len)
                    if ref[off + i] == probe[i])
        if match > best_match:
            best_match = match
            best_off = off
    return best_off, best_match / probe_len


def report(ours: list[int], ref: list[int], offset: int, window: int,
           threshold: float):
    """At `offset` into ref, walk both streams in parallel and emit
    per-window agreement. Flag windows below threshold."""
    overlap = min(len(ours), len(ref) - offset)
    print(f"# overlap: {overlap} dibits "
          f"({overlap / 4800:.2f} s of voice)")
    print(f"# {'window':>10} {'sym_idx':>10} {'time_s':>8} "
          f"{'match':>7} {'agree%':>7}  marker")
    n_windows = overlap // window
    bad = 0
    bad_runs = []
    in_bad = False
    bad_start = 0
    for w in range(n_windows):
        s = w * window
        agree = sum(1 for i in range(window)
                    if ours[s + i] == ref[offset + s + i])
        rate = agree / window
        marker = ""
        if rate < threshold:
            marker = "<<< below threshold"
            bad += 1
            if not in_bad:
                in_bad = True
                bad_start = s
        else:
            if in_bad:
                in_bad = False
                bad_runs.append((bad_start, s))
        print(f"  {w:10d} {s:10d} {s/4800:8.2f} "
              f"{agree:5d}/{window:<3d} {rate*100:6.1f}%  {marker}")
    if in_bad:
        bad_runs.append((bad_start, n_windows * window))
    print()
    print(f"# windows: {n_windows}, below threshold: {bad}")
    if bad_runs:
        print("# divergence runs (sym_start, sym_end, time_start, time_end):")
        for s0, s1 in bad_runs:
            print(f"   {s0:7d} - {s1:7d}   {s0/4800:7.2f}s - {s1/4800:7.2f}s")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ours", type=Path, help=".bits dump from our demod (SOFTDEC_DIBITS_OUT)")
    ap.add_argument("ref", type=Path, help="SDRTrunk reference .bits")
    ap.add_argument("--window", type=int, default=200,
                    help="dibits per agreement window (default 200 = ~42 ms)")
    ap.add_argument("--match-threshold", type=float, default=0.85,
                    help="windows below this agreement marked as divergence")
    ap.add_argument("--probe-len", type=int, default=1024,
                    help="dibits used to find alignment offset (default 1024)")
    ap.add_argument("--max-search", type=int, default=16384,
                    help="max offsets in ref to scan during alignment (default 16384)")
    args = ap.parse_args()

    if not args.ours.is_file():
        print(f"ours not found: {args.ours}", file=sys.stderr); return 1
    if not args.ref.is_file():
        print(f"ref not found: {args.ref}", file=sys.stderr); return 1

    ours = unpack_dibits(args.ours.read_bytes())
    ref = unpack_dibits(args.ref.read_bytes())
    print(f"# ours: {len(ours)} dibits ({len(ours)/4800:.2f} s)")
    print(f"# ref:  {len(ref)} dibits ({len(ref)/4800:.2f} s)")

    # SDRTrunk's .bits files are typically *shorter* than the input
    # because they only cover periods where SDRTrunk had sync. So we
    # slide REF inside OURS (find where in our long stream the ref
    # starts), not the other way around.
    if len(ours) > len(ref):
        # Slide ref inside ours
        upper = min(args.max_search, len(ours) - args.probe_len)
        probe = ref[:args.probe_len]
        best_off = 0
        best_match = -1
        for off in range(upper):
            m = sum(1 for i in range(args.probe_len)
                    if ours[off + i] == probe[i])
            if m > best_match:
                best_match = m
                best_off = off
        agree_rate = best_match / args.probe_len
        print(f"# alignment: ref starts at our dibit "
              f"{best_off} ({best_off/4800:.3f}s) "
              f"with probe agreement {agree_rate*100:.1f}%")
        if agree_rate < 0.6:
            print("# WARNING: low alignment agreement — alignment may be wrong")
        # Walk both from best_off in ours, 0 in ref
        ours_aligned = ours[best_off:]
        report(ours_aligned, ref, 0, args.window, args.match_threshold)
    else:
        best_off, agree_rate = find_best_offset(
            ours, ref, args.probe_len, args.max_search)
        print(f"# alignment: ours starts at ref dibit "
              f"{best_off} ({best_off/4800:.3f}s) "
              f"with probe agreement {agree_rate*100:.1f}%")
        report(ours, ref, best_off, args.window, args.match_threshold)
    return 0


if __name__ == "__main__":
    sys.exit(main())
