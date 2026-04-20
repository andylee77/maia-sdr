#!/usr/bin/env python3
"""p25_retune_probe.py

Trigger N traffic retunes to a known voice frequency and sample
constellation / dibit / traffic-chain state at fixed offsets
post-retune.

Used to diagnose the 1-3 s grant-to-lock gap + 33% never-lock rate.

Key fix vs first attempt: `/api/traffic?retune_hz=X` takes an NCO
offset RELATIVE to rx_lo, not an absolute frequency. We read rx_lo
via /api/stats and compute the offset each retune. We also record
deltas (not absolute counters) so 'activity-since-retune' is visible.
"""
import json
import os
import sys
import time
import urllib.request
from collections import Counter
from datetime import datetime

BOARD = "http://192.168.2.1:8080"
OFFSETS_MS = [-50, 0, 50, 150, 300, 500, 800, 1200, 1800, 2500, 4000]
RETUNES = 5
GAP_S = 6
TARGET_HZ = 858_462_500  # TG 301 voice channel on Clay County


def get(path, timeout=6):
    with urllib.request.urlopen(f"{BOARD}{path}", timeout=timeout) as r:
        return json.loads(r.read())


def put(path, timeout=5):
    req = urllib.request.Request(f"{BOARD}{path}", method="PUT")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def dibit_window_histogram(hex_str):
    """Compute live dibit histogram from the latest-N dibits_hex window."""
    c = Counter()
    for ch in hex_str:
        v = int(ch, 16)
        c[(v >> 2) & 3] += 1  # upper dibit
        c[v & 3] += 1          # lower dibit
    total = sum(c.values())
    if total == 0:
        return {}
    return {str(i): round(100 * c[i] / total, 2) for i in range(4)}


def snapshot():
    """One quick snapshot of all relevant telemetry."""
    out = {"t_wall": datetime.now().isoformat(timespec="milliseconds")}
    try:
        td = get("/api/traffic_lsm_dibit_dump")
        out["pipeline"] = dict(td.get("pipeline", {}))
        out["sync_hits"] = td.get("sync", {}).get("hits", 0)
        out["total_dibits"] = td.get("total_dibits", 0)
        # Live window histogram from the latest ~2048 dibits
        out["window_hist"] = dibit_window_histogram(
            td.get("dibits_hex", ""))
        # Cumulative raw_duid counts
        rd = td.get("raw_duid", {})
        out["raw_duid_counts"] = rd.get("counts", [])
    except Exception as e:
        out["dibit_err"] = str(e)
    try:
        out["traffic"] = get("/api/traffic")
    except Exception as e:
        out["traffic_err"] = str(e)
    try:
        c = get("/api/constellation?chain=traffic")
        out["constellation"] = {
            "count": c.get("count"),
            "pll_final": c.get("pll_final"),
            "timing_final": c.get("timing_final"),
            "i": c.get("i", [])[-128:],
            "q": c.get("q", [])[-128:],
        }
    except Exception as e:
        out["constellation_err"] = str(e)
    return out


def delta(a, b):
    """Subtract pipeline counters b - a, for deltas since baseline."""
    out = {}
    if not a or not b:
        return out
    for k in set(a) | set(b):
        if isinstance(a.get(k), (int, float)) and isinstance(b.get(k), (int, float)):
            out[k] = b[k] - a[k]
    return out


def main():
    outdir = sys.argv[1] if len(sys.argv) > 1 else (
        "doc/diagnostics/2026-04-19/retune_probe")
    os.makedirs(outdir, exist_ok=True)

    # Lock modulation to LSM — eliminates c4fm phantom-grant noise
    try:
        put("/api/modulation?set=lsm")
        print("locked modulation -> LSM")
    except Exception as e:
        print(f"modulation lock failed: {e}")

    # Read rx_lo to compute NCO offset
    stats = get("/api/stats")
    rx_lo = stats["rx_lo_hz"]
    nco_offset = TARGET_HZ - rx_lo
    print(f"rx_lo_hz  = {rx_lo}")
    print(f"target_hz = {TARGET_HZ}")
    print(f"NCO offset (traffic) = {nco_offset:+d} Hz")

    # Disable follower during the probe so natural grants don't compete
    try:
        get("/api/traffic?follower=off")
        print("follower disabled")
    except Exception as e:
        print(f"disable follower err: {e}")

    # Park traffic DDC on a known-empty offset first so each retune
    # is a real tune (not a no-op same-freq call). Use control_offset-
    # a-few-MHz as the park.
    park_offset = 0  # relative to rx_lo (i.e. DC) — no P25 signal expected
    try:
        rv = get(f"/api/traffic?retune_hz={park_offset}&reset_stats=1")
        print(f"parked traffic DDC at offset=0, applied={rv.get('applied')}")
    except Exception as e:
        print(f"park err: {e}")

    results = []
    try:
        for i in range(RETUNES):
            print(f"\n=== retune {i+1}/{RETUNES} offset={nco_offset:+d} Hz"
                  f" (target {TARGET_HZ/1e6:.4f} MHz) ===")

            # Park between retunes so each retune is a real transition
            if i > 0:
                try:
                    get(f"/api/traffic?retune_hz={park_offset}&reset_stats=1")
                    time.sleep(0.3)
                except Exception:
                    pass

            # Baseline snapshot
            pre = snapshot()
            pre_pipe = pre.get("pipeline", {})
            pre_sync = pre.get("sync_hits", 0)

            # Trigger the retune
            t_req = time.monotonic()
            try:
                rv = get(f"/api/traffic?retune_hz={nco_offset}")
                print(f"  retune applied={rv.get('applied')}  "
                      f"errors={rv.get('errors')}")
            except Exception as e:
                print(f"  retune err: {e}")
                continue
            t_ack = time.monotonic()
            rtt_ms = 1000 * (t_ack - t_req)
            print(f"  retune round-trip {rtt_ms:.1f} ms")

            samples = [{"t_rel_ms": -100, "tag": "pre_retune", **pre}]
            for off_ms in OFFSETS_MS:
                target = t_ack + off_ms / 1000.0
                while time.monotonic() < target:
                    time.sleep(0.001)
                actual_ms = 1000 * (time.monotonic() - t_ack)
                s = snapshot()
                s["t_rel_ms"] = round(actual_ms, 1)
                s["tag"] = f"post+{off_ms}ms"
                # Compute deltas since baseline
                s["pipeline_delta"] = delta(pre_pipe, s.get("pipeline", {}))
                s["sync_hits_delta"] = s.get("sync_hits", 0) - pre_sync
                samples.append(s)

            rec = {
                "retune_index": i,
                "target_hz": TARGET_HZ,
                "rx_lo_hz": rx_lo,
                "nco_offset_hz": nco_offset,
                "http_rtt_ms": rtt_ms,
                "samples": samples,
            }
            results.append(rec)

            # Print compact summary
            print("  compact view (delta since pre_retune):")
            print(f"  {'tag':>14s} {'t_ms':>6s} {'cur_hz':>11s} "
                  f"{'sync_d':>7s} {'nid_att_d':>9s} {'nid_ok_d':>9s} "
                  f"{'w_h0%':>6s} {'w_h1%':>6s} {'w_h2%':>6s} {'w_h3%':>6s} "
                  f"{'pll_f':>6s}")
            for s in samples:
                pd = s.get("pipeline_delta", {})
                wh = s.get("window_hist", {})
                con = s.get("constellation", {}) or {}
                traf = s.get("traffic", {}) or {}
                pllf = con.get("pll_final")
                pllf_s = f"{pllf:+.2f}" if isinstance(pllf, (int, float)) else "  x  "
                print(f"  {s['tag']:>14s} {s['t_rel_ms']:>6.0f} "
                      f"{traf.get('current_frequency_hz', 0)/1e6:>8.4f}Mhz "
                      f"{s.get('sync_hits_delta', 0):>7d} "
                      f"{pd.get('nid_attempts', 0):>9d} "
                      f"{pd.get('nid_decoded_ok', 0):>9d} "
                      f"{wh.get('0', 0):>5.1f}% "
                      f"{wh.get('1', 0):>5.1f}% "
                      f"{wh.get('2', 0):>5.1f}% "
                      f"{wh.get('3', 0):>5.1f}% "
                      f"{pllf_s:>6s}")

            time.sleep(GAP_S)
    finally:
        try:
            get("/api/traffic?follower=on")
            print("\nfollower re-enabled")
        except Exception as e:
            print(f"re-enable follower err: {e}")

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    path = os.path.join(outdir, f"retune_probe_{ts}.json")
    with open(path, "w") as f:
        json.dump({
            "board": BOARD,
            "target_hz": TARGET_HZ,
            "rx_lo_hz": rx_lo,
            "nco_offset_hz": nco_offset,
            "offsets_ms": OFFSETS_MS,
            "retunes": results,
        }, f, indent=2)
    print(f"\nwrote {path}")


if __name__ == "__main__":
    main()
