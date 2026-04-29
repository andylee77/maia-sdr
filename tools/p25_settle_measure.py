#!/usr/bin/env python3
"""p25_settle_measure.py

Stage 0 channelizer-redesign measurement: how fast does the traffic
chain produce real audio after a retune, with the PLL+AGC seeding
already shipped?

Pulls /api/log incrementally, anchors on each `traffic` retune /
nco_skip event, and times to the next `voice` TRF_HDU / TRF_LDU1
event on the traffic chain. Records the seed diagnostic fields
(pll_drift, agc_drift, agc_cache_hit, pre/post values) so we can
tell which retunes had a usable cache.

Heartbeat-vs-parse mode (default ON): also polls
/api/traffic_lsm_dibit_dump and records the time from each retune
anchor to (a) first sync.hits delta > 0 and (b) first nid_attempts
delta > 0. Together with first-real-frame latency this tells us
which stage of the chain holds the settle budget:

  retune -> sync_first_ms  : DDC FIR + AGC + PLL settle
  sync_first_ms -> nid_first_ms : Costas convergence + slicer
  nid_first_ms -> first_frame_ms : framer hunting + BCH FEC

Outputs a CSV row per retune plus a summary at exit.

Decision rule for whether channelizer Stage 1+ is needed:
  - If p90(first_hdu_ms) < 500 ms with a populated cache, single-chain
    seeding is sufficient and channelizer work can be deferred.
  - If p90 >> 500 ms, IQ ring lookahead (Stage 1) is required.

Setup flags:
  --lock-freq    park the chain on its current freq for the run
  --agc-off      disable per-symbol AGC for the run

Both restore the prior state on exit (Ctrl-C or --duration timeout).

Usage:
    python tools/p25_settle_measure.py            # run until Ctrl-C
    python tools/p25_settle_measure.py --duration 300
    python tools/p25_settle_measure.py --lock-freq --duration 120
    python tools/p25_settle_measure.py --agc-off  --duration 120
"""
import argparse
import csv
import json
import os
import statistics
import sys
import time
import urllib.request
from datetime import datetime

DEFAULT_BOARD = "http://192.168.2.1:8080"
POLL_INTERVAL_S = 0.25  # 250 ms gives heartbeat ~125 ms resolution
# Wait at most this long for a real frame to appear after a retune
# before declaring the call dead (no audio reached us).
MAX_PENDING_AGE_S = 12.0


def fetch_log(board, last_seq=None, limit=4096, timeout=5):
    qs = f"?limit={limit}"
    if last_seq is not None:
        qs += f"&since={last_seq}"
    with urllib.request.urlopen(f"{board}/api/log{qs}", timeout=timeout) as r:
        return json.loads(r.read())


def fetch_dibit_dump(board, timeout=3):
    """Snapshot of the traffic LSM chain's PL-side counters.
    Returns (sync_hits, nid_attempts, ldu1_count) or None on failure."""
    try:
        with urllib.request.urlopen(
                f"{board}/api/traffic_lsm_dibit_dump", timeout=timeout) as r:
            d = json.loads(r.read())
        sync_hits = (d.get("sync") or {}).get("hits", 0) or 0
        nid_attempts = (d.get("pipeline") or {}).get("nid_attempts", 0) or 0
        counts = (d.get("raw_duid") or {}).get("counts") or [0] * 16
        ldu1 = counts[5] if len(counts) > 5 else 0
        return (sync_hits, nid_attempts, ldu1)
    except Exception:
        return None


def apply_setup(board, lock_freq=False, agc_off=False, verbose=True):
    """Apply test setup. Returns a dict of prior values for teardown."""
    prior = {}
    if lock_freq:
        try:
            with urllib.request.urlopen(
                    f"{board}/api/traffic?lock=on", timeout=5) as r:
                _ = r.read()
            if verbose:
                print("setup: lock_freq = ON")
            prior["lock_freq"] = True
        except Exception as e:
            print(f"setup: lock_freq ON failed: {e}", file=sys.stderr)
    if agc_off:
        try:
            with urllib.request.urlopen(
                    f"{board}/api/traffic_lsm_control?agc=0", timeout=5) as r:
                _ = r.read()
            if verbose:
                print("setup: traffic AGC = OFF")
            prior["agc_off"] = True
        except Exception as e:
            print(f"setup: AGC OFF failed: {e}", file=sys.stderr)
    return prior


def restore_setup(board, prior, verbose=True):
    """Restore the prior board state regardless of how the run ended."""
    if prior.get("lock_freq"):
        try:
            with urllib.request.urlopen(
                    f"{board}/api/traffic?lock=off", timeout=5) as r:
                _ = r.read()
            if verbose:
                print("teardown: lock_freq = OFF")
        except Exception as e:
            print(f"teardown: lock_freq OFF failed: {e}", file=sys.stderr)
    if prior.get("agc_off"):
        try:
            with urllib.request.urlopen(
                    f"{board}/api/traffic_lsm_control?agc=1", timeout=5) as r:
                _ = r.read()
            if verbose:
                print("teardown: traffic AGC = ON")
        except Exception as e:
            print(f"teardown: AGC ON failed: {e}", file=sys.stderr)


def is_retune_anchor(entry):
    """Return True if entry is a traffic retune or nco_skip with seed diag."""
    if entry.get("category") != "traffic":
        return False
    f = entry.get("fields") or {}
    return "pll_seed_written" in f


def is_real_frame(entry):
    """First real demodulated frame on the traffic chain.
    HDU is strongest; LDU1/LDU2 acceptable in case HDU FEC fails.
    Match by prefix because event_types carry suffixes (e.g.
    TRF_HDU_INFO, TRF_LDU1_LC, TRF_LDU2_ESS). We deliberately exclude
    TRF_TDULC* (false-positive prone during settle).
    """
    if entry.get("category") != "voice":
        return False
    et = (entry.get("fields") or {}).get("event_type") or ""
    return (
        et.startswith("TRF_HDU")
        or et.startswith("TRF_LDU1")
        or et.startswith("TRF_LDU2")
    )


def fmt_ms(v):
    return f"{v:>6.0f}" if v is not None else "    --"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--board", default=DEFAULT_BOARD)
    ap.add_argument(
        "--outdir",
        default="doc/diagnostics/2026-04-29/settle_measure")
    ap.add_argument("--duration", type=float, default=0,
                    help="seconds to run (0 = until Ctrl-C)")
    ap.add_argument("--lock-freq", action="store_true",
                    help="park the chain on its current freq for the run")
    ap.add_argument("--agc-off", action="store_true",
                    help="disable per-symbol AGC for the run")
    ap.add_argument("--no-heartbeat", action="store_true",
                    help="skip heartbeat polling (PL counter sampling)")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    csv_path = os.path.join(args.outdir, f"settle_{ts}.csv")

    fieldnames = [
        "wall", "tg", "freq_mhz", "anchor",
        "agc_cache_hit", "pll_seed", "pll_pre", "pll_post", "pll_drift",
        "agc_seed", "agc_pre", "agc_post", "agc_drift",
        "sync_first_ms", "nid_first_ms",
        "first_frame_ms", "first_frame_kind", "result",
    ]

    fh = open(csv_path, "w", newline="")
    writer = csv.DictWriter(fh, fieldnames=fieldnames)
    writer.writeheader()

    print(f"writing to {csv_path}")
    print(f"{'time':>12s} {'tg':>4s} {'freq_MHz':>9s} {'anchor':>9s} "
          f"{'cache':>5s} {'sync_ms':>8s} {'nid_ms':>8s} "
          f"{'first_ms':>8s} {'kind':>13s}  result")

    pending = []          # list of pending retune anchors awaiting a frame
    completed_latencies = []  # only the ones with a valid frame
    cache_hits = 0
    cache_misses = 0
    timed_out = 0
    pll_drift_nonzero = 0
    agc_drift_nonzero = 0

    # Anchor at the current tail of the log so we only watch live
    # retunes. Without this, the first fetch pulls thousands of
    # historical entries whose matching frames are also in the past
    # and may have already rolled out of the ring.
    try:
        bootstrap = fetch_log(args.board, last_seq=None, limit=1)
        last_seq = bootstrap.get("last_seq") or (
            bootstrap["entries"][-1]["seq"] if bootstrap.get("entries") else 0
        )
        print(f"bootstrapped at seq={last_seq}")
    except Exception as e:
        print(f"bootstrap failed: {e}", file=sys.stderr)
        last_seq = 0

    # Apply test setup (lock-freq, AGC-off). The teardown in the
    # finally block restores the prior state regardless of how the
    # run ends — Ctrl-C, --duration timeout, or unhandled exception.
    prior_setup = apply_setup(
        args.board, lock_freq=args.lock_freq, agc_off=args.agc_off)

    # Heartbeat baselines are stored INSIDE each pending anchor
    # (`hb_baseline` field), so back-to-back anchors don't overwrite
    # each other's tracking. The chain is single-tuner, so when two
    # anchors arrive close together they share the same counter
    # stream — but each anchor's baseline is its own snap, and the
    # delta from that baseline measures progress relative to that
    # anchor's start.
    t_start = time.monotonic()

    try:
        while True:
            if args.duration and (time.monotonic() - t_start) >= args.duration:
                break
            try:
                resp = fetch_log(args.board, last_seq=last_seq)
            except Exception as e:
                print(f"  fetch error: {e}", file=sys.stderr)
                time.sleep(POLL_INTERVAL_S)
                continue

            entries = resp.get("entries") or []
            if entries:
                last_seq = entries[-1]["seq"]

            for e in entries:
                ts_ms = e.get("timestamp_ms", 0)
                if is_retune_anchor(e):
                    f = e.get("fields") or {}
                    is_skip = "nco_skip" in (e.get("message") or "")
                    anchor = {
                        "ts_ms": ts_ms,
                        "tg": f.get("tg") or 0,
                        "freq_hz": f.get("frequency") or 0,
                        "anchor": "nco_skip" if is_skip else "retune",
                        "agc_cache_hit": bool(f.get("agc_cache_hit", False)),
                        "pll_seed": f.get("pll_seed_written"),
                        "pll_pre": f.get("traffic_pll_pre_reset"),
                        "pll_post": f.get("traffic_pll_post_reset"),
                        "pll_drift": f.get("seed_drift"),
                        "agc_seed": f.get("agc_seed_written"),
                        "agc_pre": f.get("traffic_agc_pre_reset"),
                        "agc_post": f.get("traffic_agc_post_reset"),
                        "agc_drift": f.get("agc_drift"),
                        "sync_first_ms": None,
                        "nid_first_ms": None,
                    }
                    # Snap PL counters as this anchor's baseline so
                    # subsequent deltas measure post-retune progress.
                    # Stored on the anchor itself so back-to-back
                    # anchors don't clobber each other's tracking.
                    if not args.no_heartbeat:
                        snap = fetch_dibit_dump(args.board)
                        anchor["hb_baseline"] = snap
                    pending.append(anchor)
                    # Drift counters apply at anchor creation regardless
                    # of whether a frame ever arrives.
                    if anchor["agc_cache_hit"]:
                        cache_hits += 1
                    else:
                        cache_misses += 1
                    if anchor["pll_drift"] not in (None, 0):
                        pll_drift_nonzero += 1
                    # NOTE: agc_drift is structurally always nonzero in
                    # the current HDL — lsm_agc.py:722 clobbers gain_dbg
                    # to 0 in the same reset block that loads the gain
                    # register from seed_in, so the post-reset readback
                    # is taken before any strobe re-populates the dbg
                    # tap. The actual gain register IS correctly seeded;
                    # only the diagnostic readback is broken. We still
                    # count it for visibility but flag the caveat.
                    if anchor["agc_drift"] not in (None, 0) and anchor["agc_seed"]:
                        agc_drift_nonzero += 1
                elif is_real_frame(e) and pending:
                    # First real frame closes the OLDEST pending anchor.
                    # The chain is single-tuner, so FIFO matching is correct.
                    anchor = pending.pop(0)
                    latency_ms = ts_ms - anchor["ts_ms"]
                    kind = (e.get("fields") or {}).get("event_type", "")
                    _emit(writer, anchor, latency_ms, kind, "FRAME")
                    completed_latencies.append(latency_ms)

            # Heartbeat poll: counters change much faster than the
            # retune cadence (43-200 ms to first sync hit per
            # memory). One snap per loop iteration drives deltas
            # for ALL pending anchors — back-to-back anchors share
            # the same chain state but each has its own baseline,
            # so each gets its own first-delta time.
            if not args.no_heartbeat and pending:
                snap = fetch_dibit_dump(args.board)
                if snap is not None:
                    now_ms = time.time() * 1000
                    for p in pending:
                        base = p.get("hb_baseline")
                        if base is None:
                            continue
                        rel = now_ms - p["ts_ms"]
                        if p["sync_first_ms"] is None and snap[0] > base[0]:
                            p["sync_first_ms"] = rel
                        if p["nid_first_ms"] is None and snap[1] > base[1]:
                            p["nid_first_ms"] = rel

            # Expire pending anchors that never got a frame
            now_ms = time.time() * 1000
            kept = []
            for p in pending:
                if (now_ms - p["ts_ms"]) / 1000.0 > MAX_PENDING_AGE_S:
                    _emit(writer, p, None, "", "NO_FRAME")
                    timed_out += 1
                else:
                    kept.append(p)
            pending = kept
            fh.flush()

            time.sleep(POLL_INTERVAL_S)

    except KeyboardInterrupt:
        print("\n^C caught")
    finally:
        restore_setup(args.board, prior_setup)
        fh.close()

    # Summary
    n = len(completed_latencies)
    print()
    print(f"=== summary ({csv_path}) ===")
    print(f"retunes with frame:   {n}")
    print(f"retunes with no frame:{timed_out}")
    if n:
        median = statistics.median(completed_latencies)
        p90 = statistics.quantiles(completed_latencies, n=10)[8] if n >= 10 else max(completed_latencies)
        print(f"first-frame latency:  median={median:.0f} ms  p90={p90:.0f} ms  max={max(completed_latencies):.0f} ms")
    # Heartbeat-stage medians help isolate which stage owns the budget.
    # Read from the CSV we just wrote so we don't have to thread a
    # separate list through the loop.
    try:
        sync_vals, nid_vals = [], []
        with open(csv_path) as ch:
            for row in csv.DictReader(ch):
                if row.get("sync_first_ms"):
                    try: sync_vals.append(float(row["sync_first_ms"]))
                    except ValueError: pass
                if row.get("nid_first_ms"):
                    try: nid_vals.append(float(row["nid_first_ms"]))
                    except ValueError: pass
        if sync_vals:
            print(f"sync_first  latency:  median={statistics.median(sync_vals):.0f} ms  n={len(sync_vals)}")
        if nid_vals:
            print(f"nid_first   latency:  median={statistics.median(nid_vals):.0f} ms  n={len(nid_vals)}")
        if completed_latencies and sync_vals:
            print(f"  framer-stage delta: ~{statistics.median(completed_latencies) - statistics.median(sync_vals):.0f} ms (median first_frame - sync_first)")
    except Exception as e:
        print(f"(stage-stat readback failed: {e})")
    total_anchors = cache_hits + cache_misses
    if total_anchors:
        print(f"agc_cache_hit rate:   {cache_hits}/{total_anchors} ({100*cache_hits/total_anchors:.0f}%)")
    print(f"pll_drift!=0:         {pll_drift_nonzero} (expect 0 — non-zero = HDL seed-load bug)")
    print(f"agc_drift!=0 w/seed:  {agc_drift_nonzero} (expected nonzero — gain_dbg readback is")
    print(f"                      clobbered to 0 by reset block in lsm_agc.py:722. The actual")
    print(f"                      gain register IS seeded correctly; only the diag is broken.)")
    print()
    print("Decision rule:")
    print("  p90 < 500 ms on cache hits  -> Stage 0 sufficient, defer channelizer")
    print("  p90 >= 500 ms or many NO_FRAME -> proceed to Stage 1 (IQ ring lookahead)")


def _emit(writer, anchor, latency_ms, kind, result):
    sync_ms = anchor.get("sync_first_ms")
    nid_ms = anchor.get("nid_first_ms")
    row = {
        "wall": datetime.fromtimestamp(anchor["ts_ms"] / 1000.0)
                        .isoformat(timespec="milliseconds"),
        "tg": anchor["tg"],
        "freq_mhz": round(anchor["freq_hz"] / 1e6, 4) if anchor["freq_hz"] else 0.0,
        "anchor": anchor["anchor"],
        "agc_cache_hit": anchor["agc_cache_hit"],
        "pll_seed": anchor["pll_seed"],
        "pll_pre": anchor["pll_pre"],
        "pll_post": anchor["pll_post"],
        "pll_drift": anchor["pll_drift"],
        "agc_seed": anchor["agc_seed"],
        "agc_pre": anchor["agc_pre"],
        "agc_post": anchor["agc_post"],
        "agc_drift": anchor["agc_drift"],
        "sync_first_ms": round(sync_ms, 0) if sync_ms is not None else "",
        "nid_first_ms": round(nid_ms, 0) if nid_ms is not None else "",
        "first_frame_ms": round(latency_ms, 0) if latency_ms is not None else "",
        "first_frame_kind": kind,
        "result": result,
    }
    writer.writerow(row)
    print(f"{datetime.fromtimestamp(anchor['ts_ms']/1000).strftime('%H:%M:%S.%f')[:12]:>12s} "
          f"{anchor['tg']:>4d} {row['freq_mhz']:>9.4f} {anchor['anchor']:>9s} "
          f"{'Y' if anchor['agc_cache_hit'] else 'n':>5s} "
          f"{fmt_ms(sync_ms)} {fmt_ms(nid_ms)} "
          f"{fmt_ms(latency_ms)} {kind:>13s}  {result}")


if __name__ == "__main__":
    main()
