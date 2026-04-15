#!/usr/bin/env python3
"""p25_nid_analyze.py -- NID batch-capture sweep tool.

Pulls a batch NID capture from /api/nid_capture, re-runs the BCH(63,16,23)
decoder against every entry with a sweep of `t` (error-correction tolerance)
values, and reports the resulting DUID histogram per t. Used to diagnose the
"tdu_lc >> ldu1+ldu2" inversion on the traffic-side decoder without
reflashing the board.

Typical session:

    # 1. Arm the traffic-side capture ring for 256 entries
    python tools/p25_nid_analyze.py arm --side traffic --limit 256

    # 2. Wait ~10 s for real traffic to populate the ring
    sleep 10

    # 3. Drain + analyze
    python tools/p25_nid_analyze.py sweep --side traffic

    # 4. (Optional) push the chosen threshold back to the live decoder
    python tools/p25_nid_analyze.py set-t --side traffic --value 3

The BCH decoder used for replay is the Python reference in
`tools/p25_nid_fec.py` -- the same ML codebook search the Rust side uses,
so replay results are bit-identical to what the board would produce under
the same override.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.parse
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

# Import the reference BCH from the sibling tools file so we don't have to
# re-embed the 16-row generator matrix here. Assumes this script runs from
# the repo root or from inside tools/.
sys.path.insert(0, str(Path(__file__).parent))
import p25_nid_fec  # noqa: E402

DUID_LABELS = {
    0x0: "HDU",
    0x3: "TDU",
    0x5: "LDU1",
    0x7: "TSDU",
    0xA: "LDU2",
    0xC: "PDU",
    0xF: "TDU_LC",
}

ANSI = {
    "bold":  "\033[1m",
    "dim":   "\033[2m",
    "green": "\033[32m",
    "red":   "\033[31m",
    "yellow": "\033[33m",
    "cyan":  "\033[36m",
    "reset": "\033[0m",
}


def fetch(target: str, path: str) -> dict:
    url = f"http://{target}{path}"
    try:
        with urllib.request.urlopen(url, timeout=15) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.URLError as e:
        print(f"{ANSI['red']}!! GET {url} failed: {e}{ANSI['reset']}")
        sys.exit(2)


import numpy as np  # noqa: E402

# Lazily build the reference codebook once per process.
_CB_CACHE = None


def _get_codebook():
    global _CB_CACHE
    if _CB_CACHE is None:
        cb, cd = p25_nid_fec._build_codebook()  # noqa: SLF001
        _CB_CACHE = (cb, cd)
    return _CB_CACHE


def ml_decode(nid_bits: int, max_t: int) -> tuple[int, int, int] | None:
    """Maximum-likelihood decode of a 64-bit NID. Returns
    (nac, duid, n_errors) on success, or None if `n_errors > max_t`.

    Uses the reference codebook from p25_nid_fec so the result is bit-exact
    with the Rust lsm::nid_fec::decode_nid at the codebook level, with a
    custom runtime `t` threshold instead of the hardcoded 11.
    """
    cb, cd = _get_codebook()
    recv = np.uint64(nid_bits & ((1 << 63) - 1 | (1 << 63)))
    diff = cb ^ recv
    distances = p25_nid_fec._popcount64(diff)  # noqa: SLF001
    best_idx = int(np.argmin(distances))
    best_dist = int(distances[best_idx])
    if best_dist > max_t:
        return None
    data = int(cd[best_idx])
    nac = (data >> 4) & 0xFFF
    duid = data & 0xF
    return nac, duid, best_dist


def hex_to_nid_bits(hex_str: str) -> int:
    # The API returns nid_bits as a zero-padded 16-hex-digit string
    return int(hex_str, 16)


def sweep(entries: list[dict], t_values: list[int]) -> None:
    """Sweep BCH `t` thresholds over a batch of captured NIDs and print
    a DUID histogram table per t."""
    total = len(entries)
    if total == 0:
        print("(no entries to analyze)")
        return

    print(f"\n{ANSI['bold']}== BCH-t sweep over {total} NIDs =={ANSI['reset']}\n")

    # Header row
    print(f"{'t':>3}  {'rejected':>9}  {'HDU':>6}  {'TDU':>6}  "
          f"{'LDU1':>6}  {'LDU2':>6}  {'TSDU':>6}  {'TDU_LC':>7}  "
          f"{'other':>6}")
    print(f"{'-'*3}  {'-'*9}  {'-'*6}  {'-'*6}  {'-'*6}  {'-'*6}  "
          f"{'-'*6}  {'-'*7}  {'-'*6}")

    rows = []
    for t in t_values:
        hist = Counter()
        rejected = 0
        for e in entries:
            nid_bits = hex_to_nid_bits(e["nid_bits_hex"])
            res = ml_decode(nid_bits, t)
            if res is None:
                rejected += 1
            else:
                _, duid, _ = res
                hist[duid] += 1

        hdu    = hist.get(0x0, 0)
        tdu    = hist.get(0x3, 0)
        ldu1   = hist.get(0x5, 0)
        ldu2   = hist.get(0xA, 0)
        tsdu   = hist.get(0x7, 0)
        tdu_lc = hist.get(0xF, 0)
        other  = sum(v for k, v in hist.items()
                     if k not in (0x0, 0x3, 0x5, 0x7, 0xA, 0xF))
        rows.append((t, rejected, hdu, tdu, ldu1, ldu2, tsdu, tdu_lc, other))

        pct = lambda n: f"{100*n/total:5.1f}%"  # noqa: E731
        print(f"{t:>3}  {rejected:>4} {pct(rejected):>5}  "
              f"{hdu:>6}  {tdu:>6}  {ldu1:>6}  {ldu2:>6}  "
              f"{tsdu:>6}  {tdu_lc:>7}  {other:>6}")

    # Recommendation: find the t value that minimizes tdu_lc share without
    # crossing the "reject > 50%" threshold. That's the classic "balance
    # point" for BCH tuning on a marginal P25 site.
    print()
    best_t = None
    best_tdu_lc_frac = 1.0
    for (t, rejected, _hdu, _tdu, ldu1, ldu2, _tsdu, tdu_lc, _o) in rows:
        if rejected / total > 0.5:
            continue  # too aggressive
        tdu_lc_frac = tdu_lc / max(1, ldu1 + ldu2 + tdu_lc)
        if tdu_lc_frac < best_tdu_lc_frac:
            best_tdu_lc_frac = tdu_lc_frac
            best_t = t

    if best_t is not None:
        print(f"{ANSI['green']}Recommended: t={best_t}  "
              f"(tdu_lc is {best_tdu_lc_frac*100:.1f}% of "
              f"ldu1+ldu2+tdu_lc){ANSI['reset']}")
        print(f"{ANSI['dim']}Push to the live decoder:\n"
              f"  curl -X PUT \"http://{{TARGET}}/api/bch_t?"
              f"side=traffic&value={best_t}\"{ANSI['reset']}")
    else:
        print(f"{ANSI['yellow']}No t value gives <50% rejection. Either "
              f"the capture is mostly noise or the framer is "
              f"misaligned upstream of BCH.{ANSI['reset']}")

    # Raw DUID histogram (pre-BCH, on-air). This tells you what the
    # framer is actually seeing before correction -- if it's uniform-ish
    # the signal is bad; if it's skewed toward 0xF the slicer is biased.
    print(f"\n{ANSI['bold']}== Raw on-air DUID histogram "
          f"(pre-BCH) =={ANSI['reset']}")
    raw_hist = Counter(int(e["raw_duid"], 16) for e in entries)
    for v in sorted(raw_hist.keys()):
        label = DUID_LABELS.get(v, "?")
        n = raw_hist[v]
        bar = "#" * int(40 * n / total)
        print(f"  0x{v:X} {label:>6}: {n:>4} ({100*n/total:5.1f}%) {bar}")

    # Sync distance histogram (how clean the sync hits were)
    print(f"\n{ANSI['bold']}== Sync distance histogram =={ANSI['reset']}")
    sync_hist = Counter(int(e["sync_distance"]) for e in entries)
    for d in sorted(sync_hist.keys()):
        n = sync_hist[d]
        bar = "#" * int(40 * n / total)
        print(f"  dist={d:>2}: {n:>4} ({100*n/total:5.1f}%) {bar}")


def cmd_arm(args):
    q = urllib.parse.urlencode({
        "side": args.side,
        "arm":   "1",
        "limit": str(args.limit),
    })
    d = fetch(args.target, f"/api/nid_capture?{q}")
    print(json.dumps(d, indent=2))


def cmd_sweep(args):
    q = urllib.parse.urlencode({
        "side":  args.side,
        "clear": "1",
    })
    d = fetch(args.target, f"/api/nid_capture?{q}")
    entries = d.get("entries", [])
    if not entries:
        print(f"{ANSI['yellow']}!! no entries in capture ring. "
              f"Did you arm first + wait for traffic?{ANSI['reset']}")
        sys.exit(1)
    print(f"fetched {len(entries)} NID captures from side={args.side}")
    t_values = [int(x) for x in args.t_values.split(",")]
    sweep(entries, t_values)


def cmd_set_t(args):
    q = urllib.parse.urlencode({
        "side":  args.side,
        "value": str(args.value),
    })
    url = f"http://{args.target}/api/bch_t?{q}"
    req = urllib.request.Request(url, method="PUT")
    with urllib.request.urlopen(req, timeout=10) as r:
        print(json.dumps(json.loads(r.read().decode("utf-8")), indent=2))


def cmd_status(args):
    d = fetch(args.target, "/api/bch_t")
    print(json.dumps(d, indent=2))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", default="192.168.2.1:8080",
                    help="board host:port (default 192.168.2.1:8080)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    p_arm = sub.add_parser("arm", help="arm a capture ring")
    p_arm.add_argument("--side", choices=["control", "traffic"], default="traffic")
    p_arm.add_argument("--limit", type=int, default=256)
    p_arm.set_defaults(func=cmd_arm)

    p_sweep = sub.add_parser("sweep", help="drain + BCH-t sweep analysis")
    p_sweep.add_argument("--side", choices=["control", "traffic"], default="traffic")
    p_sweep.add_argument("--t-values", default="11,7,5,4,3,2,1",
                         help="comma-separated BCH-t values to sweep")
    p_sweep.set_defaults(func=cmd_sweep)

    p_set = sub.add_parser("set-t", help="push a BCH-t override to the live decoder")
    p_set.add_argument("--side", choices=["control", "traffic", "both"], default="traffic")
    p_set.add_argument("--value", required=True,
                       help="0..=11 or 'reset' to clear")
    p_set.set_defaults(func=cmd_set_t)

    p_status = sub.add_parser("status", help="show current BCH-t override state")
    p_status.set_defaults(func=cmd_status)

    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
