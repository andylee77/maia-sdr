#!/usr/bin/env python3
"""TDU_LC forensic analyzer — distinguish real-but-corrupted TDU_LCs
from pure-noise false decodes.

Polls /api/hdl_lsm.nid_ring at ~5 Hz, dedupes by sequence number, then
partitions NID events by DUID and summarizes the BCH error distribution
per DUID. If TDU_LC's (DUID=0xF) error profile looks like LDU1/LDU2's,
they're the same "quality" of decode and probably real. If TDU_LC is
shifted toward the BCH-correction-limit tail, they're noise-driven.

Also emits a correlation table of `pll_dbg` and `sync_distance` at the
time of each TDU_LC vs the same-window average, to test whether the
phase-slip hypothesis (excursion → misdecode) holds.

Usage:
    python tools/p25_tdu_lc_forensics.py --host 192.168.2.1:8080 \\
        --secs 120 --out doc/diagnostics/2026-04-19/tdu_lc

The output dir gets:
    nid_events.jsonl        — every deduped NID observed
    summary.json            — per-DUID histograms + correlation stats
    summary.md              — human-readable report
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import time
import urllib.request

DUID_LABELS = {
    0x0: "HDU",
    0x3: "TDU",
    0x5: "LDU1",
    0x7: "TSDU",
    0xA: "LDU2",
    0xC: "PDU",
    0xF: "TDU_LC",
}


def fetch(host: str, timeout: float = 3.0) -> dict | None:
    url = f"http://{host}/api/hdl_lsm"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except Exception as e:
        print(f"  fetch fail: {e}")
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--secs", type=float, default=120.0)
    ap.add_argument("--out", required=True)
    ap.add_argument("--poll-hz", type=float, default=5.0)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)

    deadline = time.monotonic() + args.secs
    seen_seqs: set[int] = set()
    events: list[dict] = []
    poll_interval = 1.0 / args.poll_hz

    print(f"polling {args.host} for {args.secs:.0f}s at {args.poll_hz} Hz...")
    while time.monotonic() < deadline:
        d = fetch(args.host)
        if d:
            ring = d.get("nid_ring") or []
            for ev in ring:
                seq = ev.get("seq")
                if seq is None or seq in seen_seqs:
                    continue
                seen_seqs.add(seq)
                events.append(ev)
        time.sleep(poll_interval)

    print(f"collected {len(events)} unique NID events")

    with open(os.path.join(args.out, "nid_events.jsonl"), "w") as f:
        for ev in events:
            f.write(json.dumps(ev) + "\n")

    # Partition by DUID and summarise.
    by_duid: dict[int, list[dict]] = {}
    for ev in events:
        by_duid.setdefault(ev.get("duid", -1), []).append(ev)

    def dist(values: list[float]) -> dict:
        if not values:
            return {"n": 0}
        vs = sorted(values)
        return {
            "n": len(vs),
            "min": vs[0],
            "max": vs[-1],
            "median": statistics.median(vs),
            "mean": statistics.fmean(vs),
            "stdev": statistics.pstdev(vs) if len(vs) > 1 else 0.0,
            "p90": vs[int(0.9 * (len(vs) - 1))],
            "p99": vs[int(0.99 * (len(vs) - 1))],
        }

    summary = {}
    for duid, evs in sorted(by_duid.items()):
        label = DUID_LABELS.get(duid, f"DUID=0x{duid:X}")
        summary[label] = {
            "duid_hex": f"0x{duid:X}",
            "count": len(evs),
            "valid_pct": 100.0 * sum(1 for e in evs if e.get("valid")) / max(1, len(evs)),
            "n_errors":    dist([e.get("n_errors", 0) for e in evs]),
            "sync_dist":   dist([e.get("sync_distance", 0) for e in evs]),
            "pll_dbg":     dist([e.get("pll_dbg", 0) for e in evs]),
            "sp_dbg":      dist([e.get("sp_dbg", 0) for e in evs]),
            "pll_dbg_abs": dist([abs(e.get("pll_dbg", 0)) for e in evs]),
        }

    # Baseline — merge LDU1 + LDU2 as the "definitely real packets" group
    ldu_errors = []
    for d in (0x5, 0xA):
        ldu_errors.extend(e.get("n_errors", 0) for e in by_duid.get(d, []))
    tdu_lc_errors = [e.get("n_errors", 0) for e in by_duid.get(0xF, [])]

    verdict = ""
    if ldu_errors and tdu_lc_errors:
        ldu_mean = statistics.fmean(ldu_errors)
        tdu_mean = statistics.fmean(tdu_lc_errors)
        ldu_p99 = sorted(ldu_errors)[int(0.99 * (len(ldu_errors) - 1))]
        tdu_p99 = sorted(tdu_lc_errors)[int(0.99 * (len(tdu_lc_errors) - 1))]
        gap = tdu_mean - ldu_mean
        if gap < 0.5:
            verdict = "LIKELY REAL — TDU_LC n_errors distribution matches LDU1/LDU2"
        elif gap < 1.5:
            verdict = "MIXED — small shift toward noise; probably real with occasional phase-slip"
        elif gap < 3.0:
            verdict = "SUSPECT — meaningful shift toward the BCH correction limit; phase-slip or noise is contributing"
        else:
            verdict = "LIKELY NOISE — TDU_LC error distribution is far from LDU1/LDU2 baseline"
        summary["verdict"] = {
            "text": verdict,
            "ldu_mean_errors": ldu_mean,
            "tdu_lc_mean_errors": tdu_mean,
            "ldu_p99_errors": ldu_p99,
            "tdu_lc_p99_errors": tdu_p99,
            "mean_error_gap": gap,
        }

    with open(os.path.join(args.out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=2)

    # Human-readable markdown.
    lines = [
        "# TDU_LC forensics",
        "",
        f"Host: `{args.host}`  duration: `{args.secs:.0f} s`  poll: `{args.poll_hz:g} Hz`  events: `{len(events)}`",
        "",
        "## Verdict",
        "",
        verdict or "(no LDU or TDU_LC events collected; rerun during traffic)",
        "",
        "## Error distribution by DUID",
        "",
        "| DUID | Count | valid % | n_errors mean | median | p90 | p99 | max |",
        "|---|--:|--:|--:|--:|--:|--:|--:|",
    ]
    for label, s in summary.items():
        if label == "verdict":
            continue
        n = s["n_errors"]
        lines.append(
            f"| {label} ({s['duid_hex']}) | {s['count']} | {s['valid_pct']:.1f}% | "
            f"{n.get('mean', 0):.2f} | {n.get('median', 0):.1f} | "
            f"{n.get('p90', 0):.0f} | {n.get('p99', 0):.0f} | {n.get('max', 0):.0f} |"
        )
    lines += [
        "",
        "## PLL excursion at time of event",
        "",
        "| DUID | |pll_dbg| mean | median | p99 | max |",
        "|---|--:|--:|--:|--:|",
    ]
    for label, s in summary.items():
        if label == "verdict":
            continue
        p = s["pll_dbg_abs"]
        lines.append(
            f"| {label} | {p.get('mean', 0):.0f} | {p.get('median', 0):.0f} | "
            f"{p.get('p99', 0):.0f} | {p.get('max', 0):.0f} |"
        )
    lines += [
        "",
        "## Interpretation guide",
        "",
        "- If TDU_LC's `n_errors` distribution is close to LDU1/LDU2's: "
        "TDU_LCs are real packets, decoded with the same BCH correction "
        "budget — the phantom bursts are NOT noise.",
        "- If TDU_LC's `n_errors` is shifted toward the p99/max of the "
        "BCH-correction limit (typically 11 for BCH(63,16,23) with t=11): "
        "TDU_LCs are being invented by the corrector from near-random bit "
        "patterns.",
        "- If TDU_LC's `|pll_dbg|` p99 is much higher than LDU's: events "
        "coincide with PLL excursions — phase-slip hypothesis is supported.",
    ]
    with open(os.path.join(args.out, "summary.md"), "w") as f:
        f.write("\n".join(lines))

    print(f"\n{verdict}")
    print(f"artifacts -> {args.out}/")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
