#!/usr/bin/env python3
"""p25_retune_monitor.py

Continuous traffic-chain monitor. Watches `/api/traffic` +
`/api/traffic_lsm_dibit_dump` + `/api/constellation?chain=traffic`
at 5 Hz, detects retune events (TG or freq change), and reports
per-retune lock stats:

  - tune latency: time from retune to first sync_hit delta > 0
  - first-LDU latency: time to first clean LDU (bch_err <= 2)
  - total clean LDU / TDU / phantom TDU_LC counts for the dwell
  - final verdict: LOCKED (≥5 clean LDU) / MARGINAL (1-4) / NEVER

Runs until Ctrl-C. Writes a CSV row per retune to the outdir.

Usage:
    python tools/p25_retune_monitor.py            # default outdir
    python tools/p25_retune_monitor.py --duration 300
"""
import argparse
import csv
import json
import os
import sys
import time
import urllib.request
from collections import Counter
from datetime import datetime

BOARD = "http://192.168.2.1:8080"
POLL_HZ = 5.0


def get(path, timeout=3):
    try:
        with urllib.request.urlopen(f"{BOARD}{path}", timeout=timeout) as r:
            return json.loads(r.read())
    except Exception as e:
        return {"_err": str(e)}


def dibit_window_hist(hex_str):
    c = Counter()
    for ch in hex_str:
        v = int(ch, 16)
        c[(v >> 2) & 3] += 1
        c[v & 3] += 1
    total = sum(c.values())
    if total == 0:
        return (0.0, 0.0, 0.0, 0.0)
    return tuple(round(100 * c[i] / total, 1) for i in range(4))


def snapshot():
    """One fast roundtrip snapshot of all telemetry we want."""
    t = get("/api/traffic")
    d = get("/api/traffic_lsm_dibit_dump")
    c = get("/api/constellation?chain=traffic")
    pipe = d.get("pipeline", {})
    counts = d.get("raw_duid", {}).get("counts", [0] * 16)
    sync = d.get("sync", {})
    window = dibit_window_hist(d.get("dibits_hex", ""))
    return {
        "wall": time.time(),
        "current_tg": t.get("current_talkgroup") or 0,
        "current_freq": t.get("current_frequency_hz") or 0,
        "follower": t.get("follower_enabled", False),
        "nid_attempts": pipe.get("nid_attempts", 0),
        "nid_decoded_ok": pipe.get("nid_decoded_ok", 0),
        "sync_hits": sync.get("hits", 0),
        "total_dibits": d.get("total_dibits", 0),
        # Per-DUID counts (cumulative)
        "n_hdu": counts[0] if len(counts) > 0 else 0,
        "n_ldu1": counts[5] if len(counts) > 5 else 0,
        "n_ldu2": counts[10] if len(counts) > 10 else 0,
        "n_tdu": counts[3] if len(counts) > 3 else 0,
        "n_tdu_lc": counts[15] if len(counts) > 15 else 0,
        # Live dibit slicer distribution (latest ~2048 dibits)
        "h0": window[0], "h1": window[1], "h2": window[2], "h3": window[3],
        "pll_final": c.get("pll_final"),
        "timing_final": c.get("timing_final"),
    }


def delta(a, b, keys):
    return {k: b.get(k, 0) - a.get(k, 0) for k in keys}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--outdir",
                    default="doc/diagnostics/2026-04-19/retune_monitor")
    ap.add_argument("--duration", type=float, default=0,
                    help="seconds to run (0 = until Ctrl-C)")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    csv_path = os.path.join(args.outdir, f"retunes_{ts}.csv")

    fieldnames = [
        "retune_idx", "wall_start", "tg", "freq_mhz",
        "t_first_sync_s", "t_first_ldu_s",
        "dwell_s", "clean_ldu", "clean_ldu2", "tdu", "tdu_lc_phantom",
        "verdict",
        "w_hist_avg_bias",  # max - min of dibit window pcts, averaged across dwell
    ]

    with open(csv_path, "w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=fieldnames)
        writer.writeheader()

        print(f"writing to {csv_path}")
        print(f"{'idx':>3s} {'start':>8s} {'tg':>4s} {'freq_MHz':>9s} "
              f"{'t_sync_s':>9s} {'t_ldu_s':>8s} {'dwell_s':>7s} "
              f"{'ldu':>4s} {'ldu2':>4s} {'tdu':>3s} {'tdu_lc':>6s}  verdict")

        prev = snapshot()
        t_start = time.monotonic()
        retune_idx = 0
        # State for current retune window
        in_retune = False
        retune_start_t = None
        retune_start_wall = None
        retune_tg = None
        retune_freq = None
        first_sync_t = None
        first_ldu_t = None
        baseline = None
        bias_samples = []

        try:
            while True:
                if args.duration and (time.monotonic() - t_start) >= args.duration:
                    break
                time.sleep(1.0 / POLL_HZ)
                cur = snapshot()

                # Detect retune: frequency changed
                if cur["current_freq"] != prev["current_freq"] and cur["current_freq"] > 0:
                    # Close prior retune if any
                    if in_retune:
                        _close_retune(writer, retune_idx, retune_start_wall,
                                      retune_tg, retune_freq, baseline, cur,
                                      first_sync_t, first_ldu_t, retune_start_t,
                                      bias_samples)
                        retune_idx += 1

                    # Open new retune
                    in_retune = True
                    retune_start_t = time.monotonic()
                    retune_start_wall = cur["wall"]
                    retune_tg = cur["current_tg"]
                    retune_freq = cur["current_freq"]
                    baseline = cur
                    first_sync_t = None
                    first_ldu_t = None
                    bias_samples = []

                if in_retune:
                    # Track first sync hit + first clean LDU
                    d = delta(baseline, cur,
                              ["sync_hits", "n_ldu1", "n_ldu2",
                               "n_tdu", "n_tdu_lc", "n_hdu"])
                    rel_t = time.monotonic() - retune_start_t
                    if first_sync_t is None and d["sync_hits"] > 0:
                        first_sync_t = rel_t
                    if (first_ldu_t is None
                            and (d["n_ldu1"] > 0 or d["n_ldu2"] > 0)):
                        first_ldu_t = rel_t

                    # Sample dibit-window bias (max - min of %s)
                    hs = [cur["h0"], cur["h1"], cur["h2"], cur["h3"]]
                    bias_samples.append(max(hs) - min(hs))

                    # Close retune if traffic goes Idle (freq -> 0)
                    # or if TG changes
                    if (cur["current_freq"] == 0
                            or cur["current_tg"] != retune_tg):
                        _close_retune(writer, retune_idx, retune_start_wall,
                                      retune_tg, retune_freq, baseline, cur,
                                      first_sync_t, first_ldu_t, retune_start_t,
                                      bias_samples)
                        retune_idx += 1
                        in_retune = False

                prev = cur
        except KeyboardInterrupt:
            print("\n^C caught")
        finally:
            # Close final retune if still open
            if in_retune:
                _close_retune(writer, retune_idx, retune_start_wall,
                              retune_tg, retune_freq, baseline, prev,
                              first_sync_t, first_ldu_t, retune_start_t,
                              bias_samples)
        print(f"\ndone. {retune_idx+1 if in_retune else retune_idx} retune(s)"
              f" captured -> {csv_path}")


def _close_retune(writer, idx, start_wall, tg, freq, baseline, final,
                  first_sync_t, first_ldu_t, retune_start_t, bias_samples):
    dwell_s = time.monotonic() - retune_start_t
    d = delta(baseline, final,
              ["n_ldu1", "n_ldu2", "n_tdu", "n_tdu_lc"])
    clean_ldu = d["n_ldu1"]
    clean_ldu2 = d["n_ldu2"]
    tdu = d["n_tdu"]
    tdu_lc = d["n_tdu_lc"]

    if clean_ldu + clean_ldu2 >= 10:
        verdict = "LOCKED"
    elif clean_ldu + clean_ldu2 >= 1:
        verdict = "MARGINAL"
    else:
        verdict = "NEVER"

    avg_bias = (sum(bias_samples) / len(bias_samples)
                if bias_samples else 0.0)

    row = {
        "retune_idx": idx,
        "wall_start": datetime.fromtimestamp(start_wall).isoformat(timespec='milliseconds'),
        "tg": tg,
        "freq_mhz": round(freq / 1e6, 4),
        "t_first_sync_s": round(first_sync_t, 3) if first_sync_t is not None else "",
        "t_first_ldu_s": round(first_ldu_t, 3) if first_ldu_t is not None else "",
        "dwell_s": round(dwell_s, 2),
        "clean_ldu": clean_ldu,
        "clean_ldu2": clean_ldu2,
        "tdu": tdu,
        "tdu_lc_phantom": tdu_lc,
        "verdict": verdict,
        "w_hist_avg_bias": round(avg_bias, 2),
    }
    writer.writerow(row)
    # Live line
    tsyn = f"{first_sync_t:.2f}" if first_sync_t is not None else "—"
    tldu = f"{first_ldu_t:.2f}" if first_ldu_t is not None else "—"
    print(
        f"{idx:>3d} {datetime.fromtimestamp(start_wall).strftime('%H:%M:%S'):>8s} "
        f"{tg:>4d} {row['freq_mhz']:>9.4f} "
        f"{tsyn:>9s} {tldu:>8s} {dwell_s:>7.2f} "
        f"{clean_ldu:>4d} {clean_ldu2:>4d} {tdu:>3d} {tdu_lc:>6d}  {verdict}")


if __name__ == "__main__":
    main()
