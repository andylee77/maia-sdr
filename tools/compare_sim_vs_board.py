#!/usr/bin/env python3
"""Side-by-side diff: simulator predicted recordings vs board's
actual /api/recordings ring, scoped to the captured log window.

Workflow:
  1. Snapshot board: /api/recordings + /api/log (in that order, fast).
  2. Run simulate_call_pipeline.py against the exported log.
  3. Diff predicted recordings vs board recordings within the log
     window. Pair by (tg, src, time-proximity).

Use --board-mode (default True here) to run the simulator against the
current board's code path so divergences narrow to genuine model
errors instead of LDU1-LC-source-stamping differences.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REC_RE = re.compile(
    r"^\s*(\d+)\s+(\d+)\s+(\S+)\s+(\d+)\s+([\d.]+)\s+(\d+)\s+(\d+)s\s+(\S+)")


def parse_sim_output(text: str) -> list[dict]:
    rows = []
    in_table = False
    for line in text.splitlines():
        if "=== Recording list ===" in line:
            in_table = True
            continue
        if not in_table:
            continue
        m = REC_RE.match(line)
        if not m:
            continue
        sim_id, tg, src, ldus, dur_s, sp_end, opened_s, reason = m.groups()
        rows.append({
            "sim_id":      int(sim_id),
            "tg":          int(tg),
            "src":         None if src == "-" else int(src),
            "ldus":        int(ldus),
            "duration_ms": int(float(dur_s) * 1000),
            "opened_s":    int(opened_s),
            "reason":      reason,
        })
    return rows


def fmt_clock(ms: int) -> str:
    s = ms // 1000
    return f"{(s//3600)%24:02d}:{(s//60)%60:02d}:{s%60:02d}"


def diff(board_recs: list[dict], sim_recs: list[dict],
         log_min_ms: int, log_max_ms: int) -> None:
    # Filter board recs to log window.
    board_in = sorted(
        [r for r in board_recs
         if log_min_ms <= r["started_unix_ms"] <= log_max_ms],
        key=lambda r: r["started_unix_ms"])

    # Sim opened_s is local-time seconds-of-day (the SDRTrunk log
    # text uses local HH:MM:SS, not UTC). Map by computing the local
    # time-of-day of log_min and offsetting from there. Use the time
    # module's localtime() so we don't have to know the offset
    # explicitly.
    import time
    lt = time.localtime(log_min_ms / 1000)
    log_min_localtime_s = lt.tm_hour * 3600 + lt.tm_min * 60 + lt.tm_sec
    for s in sim_recs:
        delta_s = s["opened_s"] - log_min_localtime_s
        # Wrap past midnight: if delta_s is hugely negative, the log
        # spans a day boundary — add 86400.
        if delta_s < -3600:
            delta_s += 86400
        s["started_unix_ms"] = log_min_ms + delta_s * 1000

    # Greedy pair: for each board rec (oldest first), find the nearest
    # sim rec with same (tg, src) within ±5s.
    matched_sim_ids = set()
    pairs = []  # (board, sim_or_None)
    for b in board_in:
        best = None
        best_dt = 5000
        for s in sim_recs:
            if s["sim_id"] in matched_sim_ids:
                continue
            if s["tg"] != b["talkgroup"]:
                continue
            if s["src"] != b.get("source"):
                continue
            dt = abs(s["started_unix_ms"] - b["started_unix_ms"])
            if dt <= best_dt:
                best = s
                best_dt = dt
        if best:
            matched_sim_ids.add(best["sim_id"])
        pairs.append((b, best))
    orphan_sim = [s for s in sim_recs if s["sim_id"] not in matched_sim_ids
                  and log_min_ms <= s["started_unix_ms"] <= log_max_ms + 5000]

    # Render.
    print(f"\n=== Diff: simulator vs board (log window [{fmt_clock(log_min_ms)}, {fmt_clock(log_max_ms)}]) ===")
    print(f"{'time':>8}  {'tg':>4}  {'src':>10}  {'board':>10}  {'sim':>10}  {'ddur_ms':>8}  match")
    print("-" * 80)
    for b, s in pairs:
        bdur = b["duration_ms"]
        sdur = s["duration_ms"] if s else None
        ddur = (sdur - bdur) if s else None
        src = b.get("source") or "--"
        if s is None:
            print(f"  {fmt_clock(b['started_unix_ms']):>8}  {b['talkgroup']:>4}  {src:>10}  {bdur:>8}ms  {'(none)':>10}  {'--':>8}  BOARD-ONLY")
        else:
            mark = "OK" if abs(ddur) < 1000 else f"d{ddur:+}ms"
            print(f"  {fmt_clock(b['started_unix_ms']):>8}  {b['talkgroup']:>4}  {src:>10}  {bdur:>8}ms  {sdur:>8}ms  {ddur:+8}  {mark}")
    if orphan_sim:
        print()
        print("--- SIM-ONLY (sim predicted, board didn't produce) ---")
        for s in sorted(orphan_sim, key=lambda x: x["started_unix_ms"]):
            src = s["src"] if s["src"] is not None else "--"
            print(f"  {fmt_clock(s['started_unix_ms']):>8}  {s['tg']:>4}  {src:>10}  {'(none)':>10}  {s['duration_ms']:>8}ms  reason={s['reason']}")

    print()
    print(f"summary: board={len(board_in)} sim={len(sim_recs)} matched={len(pairs) - sum(1 for _,s in pairs if s is None)} board-only={sum(1 for _,s in pairs if s is None)} sim-only={len(orphan_sim)}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--validation-dir", default="_validation",
                    help="Directory holding recs.json + log_all_sdrtrunk_style.log + log.json")
    ap.add_argument("--no-board-mode", action="store_true",
                    help="Run sim with LDU1 LC source stamping enabled (predicts what board WOULD produce if stamping were re-added)")
    args = ap.parse_args()

    vdir = Path(args.validation_dir)
    recs_path = vdir / "recs.json"
    log_path = vdir / "log.json"
    sdrtrunk_log = vdir / "log_all_sdrtrunk_style.log"

    if not recs_path.exists() or not sdrtrunk_log.exists() or not log_path.exists():
        print(f"missing snapshot files in {vdir}/. Run snapshot first.", file=sys.stderr)
        return 1

    board_recs = json.loads(recs_path.read_text())["items"]
    log_data = json.loads(log_path.read_text(encoding="utf-8", errors="replace"))
    ts = [e["timestamp_ms"] for e in log_data["entries"]]
    log_min, log_max = min(ts), max(ts)

    sim_args = ["python", "tools/simulate_call_pipeline.py",
                str(sdrtrunk_log), "--quiet"]
    if not args.no_board_mode:
        sim_args.append("--board-mode")
    proc = subprocess.run(sim_args, capture_output=True, text=True,
                          encoding="utf-8", errors="replace")
    if proc.returncode != 0:
        print(f"sim failed: {proc.stderr}", file=sys.stderr)
        return 1
    sim_recs = parse_sim_output(proc.stdout)

    diff(board_recs, sim_recs, log_min, log_max)
    return 0


if __name__ == "__main__":
    sys.exit(main())
