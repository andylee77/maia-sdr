#!/usr/bin/env python3
"""Replay a SDRTrunk-style P25 timeseries log and evaluate several
TDULC SpeakerEnd-validity rule variants against it.

Goal: figure out which rule set correctly accepts real end-of-call
events and rejects false-BCH-corrected decodes, WITHOUT needing a
flash cycle on the board.

Usage:

    python tools/replay_tdulc_validity.py <path-to-log>
    python tools/replay_tdulc_validity.py <path> --per-call
    python tools/replay_tdulc_validity.py <path> --cutoff 100000

Rules tested
------------

  baseline     : accept every TDULC close candidate (current on-board
                 behaviour BEFORE any 2026-04-24 gating).
  cooldown     : 1500 ms cooldown — first one in a burst wins,
                 duplicates dropped. Matches the `2026-04-24-...-gate`
                 build behaviour.
  strict_src   : cooldown + `BY == current_source` (most recent
                 grant SRC for the active TG). What the latest
                 edit does.
  history_src  : cooldown + `BY in last N sources seen on this TG
                 since HDU` (proposal A in the review).
  plausible    : cooldown + `BY` is a plausible RID (non-zero, not
                 a system controller). No cross-check.

Each TalkComplete is evaluated against every rule — count per rule
printed at the end, plus optionally the per-call trace so you can
eyeball which calls swing which way.
"""
from __future__ import annotations

import argparse
import re
import sys
from collections import deque, Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


# --- Regex patterns matched against the SDRTrunk-style log ----------

TS_RE = re.compile(r"^(\d{2}:\d{2}:\d{2})\s+(\S+)")

# CC: TSBKN GRP_VCH_GRANT TG:00402 SRC:3412599 -> 0-1117 (857.9875 MHz)
GRANT_RE = re.compile(
    r"GRP_VCH_GRANT\s+TG:(\d+)\s+SRC:(\d+)\s+->\s+(\S+)")

# T1: LDU1 VOICE GROUP VOICE CHANNEL USER FM:3599082 TO:301 SERVICE ...
LDU1_LC_RE = re.compile(
    r"LDU1\s+VOICE\s+GROUP\s+VOICE\s+CHANNEL\s+USER\s+FM:(\d+)\s+TO:(\d+)")

# T1: TDULC MOTOROLA TALK COMPLETE BY:3599082 TG:301
MOT_TC_RE = re.compile(
    r"TDULC\s+MOTOROLA\s+TALK\s+COMPLETE\s+BY:(\d+)(?:\s+TG:(\d+))?")

# T1: TDULC CALL TERMINATION BY:MOTOROLA SYS CTRL (0xFFFFFD)
CALLTERM_RE = re.compile(
    r"TDULC\s+CALL\s+TERMINATION\s+BY:(?:MOTOROLA\s+SYS\s+CTRL\s+\(0x([0-9A-Fa-f]+)\)|(\d+))")

# T1: TDULC GROUP VOICE CHANNEL USER FM:0 TO:301   (beacon, no close)
GVCU_BEACON_RE = re.compile(
    r"TDULC\s+GROUP\s+VOICE\s+CHANNEL\s+USER\s+FM:(\d+)\s+TO:(\d+)")

# T1: NAC:NNNN/xNN TDU_LC TG=301 NAC=0xNNN   (HDL-level DUID, pre-LCW-parse)
TDULC_DUID_RE = re.compile(r"\bTDU_LC\s+TG=")

# T1: ...LDU1 TG=XXX or LDU2 TG=XXX (PASSED / INFO level)
LDU_DUID_RE = re.compile(r"\b(LDU1|LDU2)\s+TG=(\d+)")


COOLDOWN_MS = 1500
HISTORY_RING_SIZE = 4

# System-controller addresses used by P25 CALL_TERMINATION
SYS_CONTROLLER_IDS = {0xFFFFFD, 0xFFFFFE, 0xFFFFFF}


def parse_ts_ms(ts: str) -> int:
    """Convert HH:MM:SS -> total ms since midnight. The log's
    timestamp lacks sub-second resolution; ms-field is always 0."""
    h, m, s = (int(x) for x in ts.split(":"))
    return ((h * 60 + m) * 60 + s) * 1000


@dataclass
class CallState:
    """Active call as reconstructed from the log stream."""
    tg: int
    first_seen_ms: int
    grants: list[tuple[int, int]] = field(default_factory=list)  # (ms, src)
    ldu1_fm: list[int] = field(default_factory=list)
    speaker_ends: list[dict] = field(default_factory=list)
    # Most recent grant SRC observed — analog of on-board current_source.
    current_source: int = 0
    # Ring of last N sources seen since this "call" started — for
    # history-match rule.
    source_history: deque[int] = field(default_factory=
        lambda: deque(maxlen=HISTORY_RING_SIZE))
    # Cooldown — ms of last accepted SpeakerEnd.
    last_accepted_end_ms: Optional[int] = None


@dataclass
class RuleCounts:
    accepted: int = 0
    rejected_cooldown: int = 0
    rejected_src: int = 0
    rejected_plausible: int = 0


def evaluate_rules(
    call: CallState,
    by_radio_id: int,
    now_ms: int,
    dispatch_cutoff: int,
) -> dict[str, str]:
    """Return each rule's verdict for this TalkComplete candidate."""
    verdicts: dict[str, str] = {}

    # baseline: always accept
    verdicts["baseline"] = "accept"

    # cooldown only
    if call.last_accepted_end_ms is not None \
       and (now_ms - call.last_accepted_end_ms) < COOLDOWN_MS:
        verdicts["cooldown"] = "reject:cooldown"
    else:
        verdicts["cooldown"] = "accept"

    # strict_src: cooldown + BY == current_source
    if verdicts["cooldown"] == "reject:cooldown":
        verdicts["strict_src"] = "reject:cooldown"
    elif call.current_source == 0:
        # No source known — strict can't apply, accept (fallback)
        verdicts["strict_src"] = "accept:no-current-src"
    elif call.current_source == by_radio_id:
        verdicts["strict_src"] = "accept"
    else:
        verdicts["strict_src"] = (
            f"reject:src-mismatch(cur={call.current_source})")

    # history_src: cooldown + BY in source_history
    if verdicts["cooldown"] == "reject:cooldown":
        verdicts["history_src"] = "reject:cooldown"
    elif not call.source_history:
        verdicts["history_src"] = "accept:no-history"
    elif by_radio_id in call.source_history:
        verdicts["history_src"] = "accept"
    else:
        verdicts["history_src"] = (
            f"reject:not-in-history({list(call.source_history)})")

    # plausible: cooldown + BY is a plausible RID
    if verdicts["cooldown"] == "reject:cooldown":
        verdicts["plausible"] = "reject:cooldown"
    elif by_radio_id == 0 or by_radio_id >= 0xFFFFFD:
        verdicts["plausible"] = f"reject:implausible(BY={by_radio_id:#x})"
    else:
        verdicts["plausible"] = "accept"

    # Tag dispatcher vs field for the trace
    source_class = "console" if by_radio_id < dispatch_cutoff else "field"
    verdicts["_source_class"] = source_class

    return verdicts


def classify_rid(rid: int, cutoff: int) -> str:
    if rid == 0:
        return "zero"
    if rid in SYS_CONTROLLER_IDS:
        return "sysctl"
    if rid < cutoff:
        return "console"
    return "field"


def replay(path: Path, per_call: bool, cutoff: int, tg_filter: Optional[int]):
    # Per-TG state. A real call's lifecycle here is roughly:
    #   grant -> (optional LDU1 FM) -> TalkComplete / CallTerm / bare TDU
    # We treat each new grant for a TG as extending the current call if
    # within a short gap, otherwise as a fresh call. Gap threshold = 30 s
    # — bigger than the longest dispatcher↔field handoff but smaller
    # than inter-call idle.
    CALL_GAP_MS = 30_000

    calls: dict[int, CallState] = {}
    completed_calls: list[CallState] = []

    # Rule vote tallies
    rules = ["baseline", "cooldown", "strict_src", "history_src", "plausible"]
    rule_totals: dict[str, Counter] = {r: Counter() for r in rules}

    # Global counters
    total_grants = 0
    total_mot_tc = 0
    total_callterm = 0
    total_beacons = 0
    total_ldu1_lc = 0

    # Source classification for MOT_TC BYs
    by_class_counter: Counter = Counter()

    # Per-call trace collector (list of tuples for printing)
    per_call_trace: list[tuple[int, int, list[str]]] = []

    def end_call(tg: int):
        if tg in calls:
            completed_calls.append(calls.pop(tg))

    def rotate_call_if_stale(tg: int, now_ms: int) -> CallState:
        if tg in calls:
            last_ms = calls[tg].grants[-1][0] if calls[tg].grants \
                else calls[tg].first_seen_ms
            if now_ms - last_ms > CALL_GAP_MS:
                end_call(tg)
        if tg not in calls:
            calls[tg] = CallState(tg=tg, first_seen_ms=now_ms)
        return calls[tg]

    with path.open("r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.rstrip()
            m = TS_RE.match(line)
            if not m:
                continue
            ts = m.group(1)
            now_ms = parse_ts_ms(ts)

            # GRP_VCH_GRANT
            g = GRANT_RE.search(line)
            if g:
                tg = int(g.group(1))
                src = int(g.group(2))
                if tg_filter is not None and tg != tg_filter:
                    continue
                total_grants += 1
                c = rotate_call_if_stale(tg, now_ms)
                c.grants.append((now_ms, src))
                c.current_source = src
                if src != 0 and src not in c.source_history:
                    c.source_history.append(src)
                continue

            # LDU1 LC (FM:NNN TO:NNN) — confirms source + TG
            l = LDU1_LC_RE.search(line)
            if l:
                fm = int(l.group(1))
                to = int(l.group(2))
                if tg_filter is not None and to != tg_filter:
                    continue
                total_ldu1_lc += 1
                c = rotate_call_if_stale(to, now_ms)
                c.ldu1_fm.append(fm)
                # LDU1 FM is authoritative speaker evidence — treat
                # like a grant source for history.
                if fm != 0 and fm not in c.source_history:
                    c.source_history.append(fm)
                continue

            # MOTOROLA TALK COMPLETE
            t = MOT_TC_RE.search(line)
            if t:
                by = int(t.group(1))
                tg = int(t.group(2)) if t.group(2) else None
                if tg_filter is not None and tg != tg_filter:
                    continue
                total_mot_tc += 1
                by_class_counter[classify_rid(by, cutoff)] += 1
                if tg is None:
                    # Log line didn't carry TG — try most recent open call
                    if not calls:
                        continue
                    tg = next(iter(calls))
                c = rotate_call_if_stale(tg, now_ms)
                verdicts = evaluate_rules(c, by, now_ms, cutoff)
                for r in rules:
                    rule_totals[r][verdicts[r].split(":")[0]] += 1
                # Cooldown uses "first accepted wins" per call — if
                # cooldown accepted this one, note the timestamp.
                if verdicts["cooldown"] == "accept":
                    c.last_accepted_end_ms = now_ms
                c.speaker_ends.append({
                    "kind": "mot_tc",
                    "by": by,
                    "by_class": verdicts["_source_class"],
                    "ts": now_ms,
                    "verdicts": {r: verdicts[r] for r in rules},
                })
                if per_call:
                    per_call_trace.append((now_ms, tg, [
                        f"MOT_TC BY:{by}({verdicts['_source_class']})",
                        f"  history={list(c.source_history)} cur_src={c.current_source}",
                        *[f"  {r:<13} {verdicts[r]}" for r in rules],
                    ]))
                continue

            # CALL TERMINATION
            ct = CALLTERM_RE.search(line)
            if ct:
                total_callterm += 1
                # Apply cooldown-only and plausible-validator flavours.
                # A CALL_TERM that fires first BLOCKS a later MOT_TC
                # within the cooldown window — that's the dedup race
                # we care about. CALL_TERM always carries a system
                # address so strict_src/history_src don't apply.
                if not calls:
                    continue
                # Use most recent open call by time; log doesn't give
                # us TG on CALL_TERM lines.
                tg = max(calls.keys(),
                         key=lambda t: calls[t].grants[-1][0]
                                        if calls[t].grants
                                        else calls[t].first_seen_ms)
                c = calls[tg]
                under_cooldown = (c.last_accepted_end_ms is not None
                    and (now_ms - c.last_accepted_end_ms) < COOLDOWN_MS)
                # Cooldown variant: accepts first, rejects duplicates.
                if not under_cooldown:
                    # Record acceptance so later MOT_TCs in the same
                    # burst see a cooldown lockout.
                    c.last_accepted_end_ms = now_ms
                    c.speaker_ends.append({
                        "kind": "call_term",
                        "ts": now_ms,
                    })
                continue

            # TDULC GVCU beacon (explicitly NOT a close)
            if GVCU_BEACON_RE.search(line):
                total_beacons += 1
                continue

    # Flush any still-open calls
    for tg in list(calls.keys()):
        end_call(tg)

    # ---------- Report --------------------------------------------------
    print(f"=== Replay: {path} ===")
    if tg_filter is not None:
        print(f"Filter: TG={tg_filter}")
    print()
    print(f"Total grants             : {total_grants}")
    print(f"Total LDU1 LCs (FM:TO)   : {total_ldu1_lc}")
    print(f"Total TDULC MOT_TC       : {total_mot_tc}")
    print(f"Total TDULC CALL_TERM    : {total_callterm}")
    print(f"Total TDULC GVCU beacons : {total_beacons}")
    print(f"Calls (gap > {CALL_GAP_MS} ms): {len(completed_calls)}")
    print()
    print(f"MOT_TC BY classification (cutoff {cutoff}):")
    for cls, n in by_class_counter.most_common():
        print(f"  {cls:>8}: {n}")
    print()

    # Build accept/reject totals across rules
    print(f"{'Rule':<14} {'accept':>8} {'reject':>8}  (reject reasons)")
    print("-" * 70)
    for r in rules:
        totals = rule_totals[r]
        acc = totals.get("accept", 0)
        rej = total_mot_tc - acc
        # Break down reject reasons for clarity — keys other than
        # 'accept' under each rule represent reject flavors.
        reasons = {k: v for k, v in totals.items() if k != "accept"}
        reason_str = ", ".join(f"{k}={v}" for k, v in
                                sorted(reasons.items(),
                                       key=lambda kv: -kv[1])) \
                     if reasons else "-"
        print(f"{r:<14} {acc:>8} {rej:>8}  ({reason_str})")
    print()

    # Per-call trace (optional)
    if per_call and per_call_trace:
        print(f"=== Per-MOT_TC trace ({len(per_call_trace)} entries) ===")
        for ts_ms, tg, lines in per_call_trace:
            h = ts_ms // 3_600_000
            m = (ts_ms % 3_600_000) // 60_000
            s = (ts_ms % 60_000) // 1000
            print(f"\n[{h:02d}:{m:02d}:{s:02d}] TG:{tg}")
            for l in lines:
                print(l)

    # Per-call summary for calls with multiple sources (dispatcher
    # handoff candidates)
    print("\n=== Calls with multi-source handoffs ===")
    handoff_calls = [c for c in completed_calls if len(set(c.source_history)) >= 2]
    if not handoff_calls:
        print("(none)")
    else:
        for c in handoff_calls[:20]:
            srcs = [f"{s}({classify_rid(s, cutoff)})" for s in c.source_history]
            print(f"  TG:{c.tg:>4} sources={srcs} "
                  f"mot_tc_count={sum(1 for e in c.speaker_ends if e['kind']=='mot_tc')}")
        if len(handoff_calls) > 20:
            print(f"  ... + {len(handoff_calls) - 20} more")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("log", type=Path,
                    help="Path to SDRTrunk-style timeseries log")
    ap.add_argument("--per-call", action="store_true",
                    help="Print per-MOT_TC verdicts across all rules")
    ap.add_argument("--cutoff", type=int, default=100_000,
                    help="RID < cutoff = console/dispatcher (default 100000)")
    ap.add_argument("--tg", type=int, default=None,
                    help="Only analyse a single TG (e.g. --tg 301)")
    args = ap.parse_args()

    if not args.log.exists():
        print(f"Log file not found: {args.log}", file=sys.stderr)
        sys.exit(1)

    replay(args.log, args.per_call, args.cutoff, args.tg)


if __name__ == "__main__":
    main()
