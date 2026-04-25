#!/usr/bin/env python3
"""Offline simulator — replay a SDRTrunk-style timeseries log through
a model of the on-board call pipeline (grant follower -> imbe_forwarder
-> vocoder -> recorder) and print what would have happened.

The goal is to test validity/cooldown/split rule changes against real
recorded traffic *without* a flash cycle. Any rule that produces more
recordings / different recordings / different source stamps than the
board currently emits is immediately visible.

Model assumptions
-----------------

- Grant follower: retunes on every GRP_VCH_GRANT for a non-encrypted
  TG (if not already on it), updates `current_source` on every grant
  (new OR same TG) and pushes into a 4-deep `call_sources_seen` ring.
- ImbeForwarder: submits to the mpsc with (tg, src) captured at send
  time (B fix).
- Vocoder: decodes every LDU, emits AudioChunk(tg, source) using the
  batch labels.
- Recorder: opens a new recording on first chunk; splits on audio-
  chunk TG change OR source change (default — can be tuned with
  --no-split-source).
- SpeakerEnd cooldown: first accepted emission within a window blocks
  any subsequent ones. Defaults to 1500 ms.
- SpeakerEnd validity:
    * bare TDU   : requires current_tg != 0 and recent IMBE (≤ 2 s)
    * MOT_TC     : --rule selects strict | history | plausible
    * CALL_TERM  : requires BY ∈ well-known system controllers
                   AND current_tg != 0

Usage
-----

    python tools/simulate_call_pipeline.py <log>
    python tools/simulate_call_pipeline.py <log> --tg 301
    python tools/simulate_call_pipeline.py <log> --rule history
    python tools/simulate_call_pipeline.py <log> --cooldown-ms 2500
    python tools/simulate_call_pipeline.py <log> --no-split-source

Output includes an annotated event timeline and a per-call summary.
"""
from __future__ import annotations

import argparse
import re
import sys
from collections import deque
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Optional


TS_RE = re.compile(r"^(\d{2}:\d{2}:\d{2})\s+(\S+)")

# CC lines
GRANT_RE = re.compile(
    r"GRP_VCH_GRANT\s+TG:(\d+)\s+SRC:(\d+)\s+->\s+(\S+)(?:\s+\(([\d.]+)\s*MHz\))?(?:\s+\[(ENC)\])?")
GRNT_UPD_RE = re.compile(
    r"GRP_VCH_GRNT_UPD\s+TG:(\d+)\s+->\s+(\S+)")

# T1 / traffic chain
LDU1_LC_RE = re.compile(
    r"LDU1\s+VOICE\s+GROUP\s+VOICE\s+CHANNEL\s+USER\s+FM:(\d+)\s+TO:(\d+)")
MOT_TC_RE = re.compile(
    r"TDULC\s+MOTOROLA\s+TALK\s+COMPLETE\s+BY:(\d+)(?:\s+TG:(\d+))?")
CALLTERM_RE = re.compile(
    r"TDULC\s+CALL\s+TERMINATION\s+BY:(?:MOTOROLA\s+SYS\s+CTRL\s+\(0x([0-9A-Fa-f]+)\)|TIA\s+STANDARD\s+\(0x([0-9A-Fa-f]+)\)|(\d+))")
GVCU_BEACON_RE = re.compile(
    r"TDULC\s+GROUP\s+VOICE\s+CHANNEL\s+USER\s+FM:(\d+)\s+TO:(\d+)")
GVU_UPDATE_RE = re.compile(
    r"TDULC\s+GROUP\s+VOICE\s+CHANNEL\s+UPDATE\s+TG_A:(\d+)")
# PS-framer bare DUID heartbeats: "traffic LDU1 TG=301" / "traffic LDU2"
LDU_CONFIRM_RE = re.compile(
    r"traffic\s+(LDU1|LDU2)\s+TG=(\d+)")
TDU_CONFIRM_RE = re.compile(r"traffic\s+TDU\s")  # bare TDU

# System controller addresses the P25 standard reserves for CALL_TERMINATION
SYS_CONTROLLER_IDS = {0xFFFFFD, 0xFFFFFE, 0xFFFFFF}

COOLDOWN_MS_DEFAULT = 1500
HISTORY_RING_SIZE = 4
IMBE_RECENT_MS = 2_000  # bare-TDU validity window
PLAUSIBLE_RID_MAX = 0xFFFFFD


class Color:
    RESET = "\033[0m"
    DIM = "\033[2m"
    BOLD = "\033[1m"
    RED = "\033[31m"
    GREEN = "\033[32m"
    YELLOW = "\033[33m"
    BLUE = "\033[34m"
    MAGENTA = "\033[35m"
    CYAN = "\033[36m"


def parse_ts_ms(ts: str) -> int:
    h, m, s = (int(x) for x in ts.split(":"))
    return ((h * 60 + m) * 60 + s) * 1000


class EventKind(Enum):
    GRANT = "grant"
    GRANT_UPDATE = "grant_upd"
    LDU_BARE = "ldu"
    LDU1_LC = "ldu1_lc"
    MOT_TC = "mot_tc"
    CALL_TERM = "call_term"
    TDU_BARE = "tdu"
    BEACON = "beacon"
    GVU_UPDATE = "gvu_upd"


@dataclass
class Event:
    ts_ms: int
    ts_str: str
    kind: EventKind
    tg: Optional[int] = None
    src: Optional[int] = None        # grant SRC / LDU1 FM / MOT_TC BY
    channel: Optional[str] = None    # "0-1117" etc.
    mhz: Optional[float] = None
    encrypted: bool = False
    sys_ctrl: Optional[int] = None   # CALL_TERM BY system address (for validation)
    raw: str = ""


def parse_log(path: Path) -> list[Event]:
    events: list[Event] = []
    with path.open("r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.rstrip()
            m = TS_RE.match(line)
            if not m:
                continue
            ts = m.group(1)
            now_ms = parse_ts_ms(ts)

            if "GRP_VCH_GRANT " in line:
                g = GRANT_RE.search(line)
                if g:
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.GRANT,
                        tg=int(g.group(1)), src=int(g.group(2)),
                        channel=g.group(3),
                        mhz=float(g.group(4)) if g.group(4) else None,
                        encrypted=(g.group(5) == "ENC"),
                        raw=line))
                    continue

            if "GRP_VCH_GRNT_UPD" in line:
                g = GRNT_UPD_RE.search(line)
                if g:
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.GRANT_UPDATE,
                        tg=int(g.group(1)),
                        channel=g.group(2),
                        raw=line))
                    continue

            if "TDULC MOTOROLA TALK COMPLETE" in line:
                t = MOT_TC_RE.search(line)
                if t:
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.MOT_TC,
                        tg=int(t.group(2)) if t.group(2) else None,
                        src=int(t.group(1)),
                        raw=line))
                    continue

            if "TDULC CALL TERMINATION" in line:
                ct = CALLTERM_RE.search(line)
                if ct:
                    sys_ctrl = None
                    if ct.group(1):
                        sys_ctrl = int(ct.group(1), 16)
                    elif ct.group(2):
                        sys_ctrl = int(ct.group(2), 16)
                    elif ct.group(3):
                        sys_ctrl = int(ct.group(3))
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.CALL_TERM,
                        sys_ctrl=sys_ctrl,
                        raw=line))
                    continue

            if "TDULC GROUP VOICE CHANNEL UPDATE" in line:
                g = GVU_UPDATE_RE.search(line)
                if g:
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.GVU_UPDATE,
                        tg=int(g.group(1)),
                        raw=line))
                    continue

            if "TDULC GROUP VOICE CHANNEL USER" in line:
                g = GVCU_BEACON_RE.search(line)
                if g:
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.BEACON,
                        src=int(g.group(1)),
                        tg=int(g.group(2)),
                        raw=line))
                    continue

            if "LDU1 VOICE GROUP VOICE CHANNEL USER" in line:
                l = LDU1_LC_RE.search(line)
                if l:
                    events.append(Event(
                        ts_ms=now_ms, ts_str=ts,
                        kind=EventKind.LDU1_LC,
                        src=int(l.group(1)), tg=int(l.group(2)),
                        raw=line))
                    continue

            # Bare LDU1/LDU2 confirmation (PS-framer PASSED events w/ TG)
            l = LDU_CONFIRM_RE.search(line)
            if l:
                events.append(Event(
                    ts_ms=now_ms, ts_str=ts,
                    kind=EventKind.LDU_BARE,
                    tg=int(l.group(2)),
                    raw=line))
                continue

            if TDU_CONFIRM_RE.search(line):
                events.append(Event(
                    ts_ms=now_ms, ts_str=ts,
                    kind=EventKind.TDU_BARE,
                    raw=line))
                continue

    return events


@dataclass
class Recording:
    id: int
    tg: int
    source: Optional[int]
    opened_ms: int
    closed_ms: Optional[int] = None
    ldu_count: int = 0
    close_reason: Optional[str] = None
    speaker_ends_received: int = 0
    last_chunk_ms: int = 0          # for grace-timeout closure (board parity)

    def duration_ms(self) -> int:
        if self.closed_ms is None:
            return 0
        return self.closed_ms - self.opened_ms


@dataclass
class Simulator:
    rule: str = "history"          # strict | history | plausible
    cooldown_ms: int = COOLDOWN_MS_DEFAULT
    split_on_source: bool = True
    # 2026-04-24: when True, LDU1 LC FM updates current_source per
    # LDU. When False, mirrors the current board behavior where only
    # the grant follower writes current_source (LDU1 LC stamping was
    # removed because LDU1 LC FEC was deemed too weak — but that
    # also defeats per-speaker source-change splits within a TG).
    # Set False to get a faithful prediction of what the live board
    # will produce; set True to predict what would happen with LDU1
    # LC stamping re-enabled.
    ldu1_lc_source: bool = True

    # Follower state
    current_tg: int = 0
    current_source: int = 0
    call_sources_seen: deque = field(default_factory=
        lambda: deque(maxlen=HISTORY_RING_SIZE))
    last_imbe_at_ms: int = 0

    # SpeakerEnd cooldown
    last_accepted_end_ms: Optional[int] = None

    # Recorder state
    active_rec: Optional[Recording] = None
    recordings: list[Recording] = field(default_factory=list)
    next_rec_id: int = 1

    # Counters
    speaker_end_emitted: int = 0
    speaker_end_dedup: int = 0
    speaker_end_invalid: int = 0
    call_term_emitted: int = 0
    ldu_decoded: int = 0
    encrypted_rejected: int = 0

    # Trace lines accumulated during simulation
    trace: list[str] = field(default_factory=list)

    def log(self, ts_str: str, colored: str):
        self.trace.append(f"[{ts_str}] {colored}")

    # Board grace window: recorder finalizes if no audio chunks for
    # FINALIZE_GRACE (5 s on the live board). The simulator advances
    # in event time, so check this lazily before processing each
    # event — if the gap from active_rec.last_chunk_ms exceeds 5 s,
    # close as "grace_timeout" first.
    GRACE_MS = 5000

    def open_recording(self, ts_ms: int, tg: int, src: int, reason: str):
        rec = Recording(
            id=self.next_rec_id,
            tg=tg,
            source=src if src != 0 else None,
            opened_ms=ts_ms,
            last_chunk_ms=ts_ms,
        )
        self.next_rec_id += 1
        self.active_rec = rec
        self.recordings.append(rec)
        return rec

    def maybe_grace_close(self, now_ms: int):
        if self.active_rec is None: return
        gap = now_ms - self.active_rec.last_chunk_ms
        if gap >= self.GRACE_MS:
            self.close_recording(
                self.active_rec.last_chunk_ms + self.GRACE_MS,
                "grace_timeout")

    def close_recording(self, ts_ms: int, reason: str):
        if self.active_rec is not None:
            self.active_rec.closed_ms = ts_ms
            self.active_rec.close_reason = reason
            self.active_rec = None

    # ---- Follower logic --------------------------------------------
    def on_grant(self, ev: Event):
        if ev.encrypted:
            # Follower rejects encrypted grants
            self.encrypted_rejected += 1
            self.log(ev.ts_str,
                f"{Color.DIM}CC grant TG:{ev.tg} SRC:{ev.src} [ENC] -> follower rejects{Color.RESET}")
            return

        tg_changed = ev.tg != self.current_tg
        src_changed = ev.src != self.current_source
        self.current_tg = ev.tg
        self.current_source = ev.src
        if ev.src != 0 and (not self.call_sources_seen
                            or ev.src != self.call_sources_seen[-1]):
            self.call_sources_seen.append(ev.src)
        bits = []
        if tg_changed:
            bits.append(f"{Color.CYAN}retune->TG:{ev.tg}{Color.RESET}")
            # TG change = new call; reset source history to just this one
            if tg_changed:
                self.call_sources_seen.clear()
                if ev.src != 0:
                    self.call_sources_seen.append(ev.src)
        if src_changed:
            bits.append(f"{Color.YELLOW}src->{ev.src}{Color.RESET}")
        detail = " ".join(bits) if bits else "same"
        self.log(ev.ts_str,
            f"{Color.BLUE}CC grant TG:{ev.tg} SRC:{ev.src} ch:{ev.channel}{Color.RESET} -> follower {detail}"
            + f"  (history={list(self.call_sources_seen)})")

    # ---- Recorder logic --------------------------------------------
    def audio_chunk(self, ev: Event, batch_tg: int, batch_src: int):
        """Simulate one LDU decode producing audio chunks."""
        self.ldu_decoded += 1
        # AGC, silence detection etc. omitted — just the routing.
        if self.active_rec is None:
            # First chunk — open
            rec = self.open_recording(ev.ts_ms, batch_tg, batch_src, "first_chunk")
            self.log(ev.ts_str,
                f"  {Color.GREEN}recorder: open rec #{rec.id} TG:{batch_tg} src:{batch_src}{Color.RESET}")
            self.active_rec.ldu_count += 1
            self.active_rec.last_chunk_ms = ev.ts_ms
            return
        # TG changed in chunk stream -> split
        if batch_tg != self.active_rec.tg:
            self.close_recording(ev.ts_ms, "tg_change")
            rec = self.open_recording(ev.ts_ms, batch_tg, batch_src, "tg_change")
            self.log(ev.ts_str,
                f"  {Color.MAGENTA}recorder: SPLIT on tg_change -> rec #{rec.id} TG:{batch_tg} src:{batch_src}{Color.RESET}")
            self.active_rec.ldu_count += 1
            self.active_rec.last_chunk_ms = ev.ts_ms
            return
        # Source changed -> split (if enabled)
        if (self.split_on_source and batch_src != 0
            and self.active_rec.source is not None
            and batch_src != self.active_rec.source):
            self.close_recording(ev.ts_ms, "source_change")
            rec = self.open_recording(ev.ts_ms, batch_tg, batch_src, "source_change")
            self.log(ev.ts_str,
                f"  {Color.MAGENTA}recorder: SPLIT on source_change -> rec #{rec.id} src:{batch_src}{Color.RESET}")
            self.active_rec.ldu_count += 1
            self.active_rec.last_chunk_ms = ev.ts_ms
            return
        # First-known-source stamp on a chunk where we had no source
        if (batch_src != 0 and self.active_rec.source is None):
            self.active_rec.source = batch_src
            self.log(ev.ts_str,
                f"  {Color.DIM}recorder: source_stamp src:{batch_src} on rec #{self.active_rec.id}{Color.RESET}")
        self.active_rec.ldu_count += 1
        self.active_rec.last_chunk_ms = ev.ts_ms

    # ---- SpeakerEnd validity ---------------------------------------
    def under_cooldown(self, ts_ms: int) -> bool:
        return (self.last_accepted_end_ms is not None
                and (ts_ms - self.last_accepted_end_ms) < self.cooldown_ms)

    def mark_accepted(self, ts_ms: int):
        self.last_accepted_end_ms = ts_ms

    def tdu_bare_valid(self, ts_ms: int) -> tuple[bool, str]:
        if self.current_tg == 0:
            return False, "no active TG"
        if (ts_ms - self.last_imbe_at_ms) >= IMBE_RECENT_MS:
            return False, f"no IMBE recent (last {ts_ms - self.last_imbe_at_ms} ms ago)"
        return True, ""

    def mot_tc_valid(self, by: int) -> tuple[bool, str]:
        if self.current_tg == 0:
            return False, "no active TG"
        if by == 0 or by >= PLAUSIBLE_RID_MAX:
            return False, f"implausible RID {by:#x}"
        if self.rule == "plausible":
            return True, ""
        if self.rule == "strict":
            if self.current_source == 0:
                return True, "no current_src (fallback)"
            if by == self.current_source:
                return True, ""
            return False, f"BY:{by} != current_src:{self.current_source}"
        if self.rule == "history":
            if not self.call_sources_seen:
                return True, "no history (fallback)"
            if by in self.call_sources_seen:
                return True, ""
            return False, f"BY:{by} not in history {list(self.call_sources_seen)}"
        raise ValueError(f"unknown rule {self.rule!r}")

    def call_term_valid(self, sys_ctrl: Optional[int]) -> tuple[bool, str]:
        if self.current_tg == 0:
            return False, "no active TG"
        if sys_ctrl is None or sys_ctrl not in SYS_CONTROLLER_IDS:
            return False, f"BY is not a system controller ({sys_ctrl!r})"
        return True, ""

    def fire_speaker_end(self, ev: Event, source: Optional[int], kind: str):
        if self.under_cooldown(ev.ts_ms):
            self.speaker_end_dedup += 1
            self.log(ev.ts_str,
                f"  {Color.DIM}{kind} SpeakerEnd dedup (cooldown){Color.RESET}")
            return
        self.mark_accepted(ev.ts_ms)
        self.speaker_end_emitted += 1
        if self.active_rec is not None:
            self.active_rec.speaker_ends_received += 1
            # Stamp source if we know it and the recording doesn't
            if source is not None and self.active_rec.source is None:
                self.active_rec.source = source
        self.log(ev.ts_str,
            f"  {Color.GREEN}{kind} SpeakerEnd EMIT src={source}{Color.RESET}")

    # ---- Event dispatch --------------------------------------------
    def step(self, ev: Event):
        # Lazy grace-timeout closure: if the gap from last audio
        # chunk to now exceeds GRACE_MS, the board recorder would
        # have closed by now. Apply that here before processing the
        # next event so durations match the live board.
        self.maybe_grace_close(ev.ts_ms)
        if ev.kind == EventKind.GRANT:
            self.on_grant(ev)
        elif ev.kind == EventKind.GRANT_UPDATE:
            # Grant update = same TG, no SRC change; log only.
            self.log(ev.ts_str,
                f"{Color.DIM}CC grant_upd TG:{ev.tg}{Color.RESET}")
        elif ev.kind == EventKind.LDU_BARE:
            # Voice frame without LC. Batch uses current_tg/source.
            if ev.tg != self.current_tg:
                # Drift / race: follower not yet updated. Use chunk's TG.
                batch_tg = ev.tg
            else:
                batch_tg = self.current_tg
            batch_src = self.current_source
            self.last_imbe_at_ms = ev.ts_ms
            self.audio_chunk(ev, batch_tg, batch_src)
        elif ev.kind == EventKind.LDU1_LC:
            # LDU1 LC carries FM (source) + TO (TG). When
            # `ldu1_lc_source` is True (sim default), they are
            # authoritative — drives per-speaker splits inside one
            # TG. When False (matches live board), grant-follower's
            # current_source wins; LDU1 LC is logged but doesn't
            # update the batch source. The False path predicts the
            # merged-WAV behavior the operator currently sees.
            if self.ldu1_lc_source:
                batch_tg = ev.tg
                batch_src = ev.src
                if ev.src != 0 and (not self.call_sources_seen
                                    or ev.src != self.call_sources_seen[-1]):
                    self.call_sources_seen.append(ev.src)
            else:
                batch_tg = ev.tg if ev.tg != 0 else self.current_tg
                batch_src = self.current_source
            self.last_imbe_at_ms = ev.ts_ms
            self.log(ev.ts_str,
                f"{Color.CYAN}T1 LDU1_LC FM:{ev.src} TO:{ev.tg}"
                f"{' (board-mode: src ignored)' if not self.ldu1_lc_source else ''}"
                f"{Color.RESET}")
            self.audio_chunk(ev, batch_tg, batch_src)
        elif ev.kind == EventKind.MOT_TC:
            ok, why = self.mot_tc_valid(ev.src)
            self.log(ev.ts_str,
                f"{Color.YELLOW}T1 MOT_TC BY:{ev.src} TG:{ev.tg or '?'}{Color.RESET}"
                f" validity: {'ok' if ok else f'REJECT ({why})'}")
            if not ok:
                self.speaker_end_invalid += 1
                return
            self.fire_speaker_end(ev, ev.src, "MOT_TC")
        elif ev.kind == EventKind.CALL_TERM:
            ok, why = self.call_term_valid(ev.sys_ctrl)
            sysctl_str = f"0x{ev.sys_ctrl:X}" if ev.sys_ctrl is not None else "?"
            self.log(ev.ts_str,
                f"{Color.YELLOW}T1 CALL_TERM BY:{sysctl_str}{Color.RESET}"
                f" validity: {'ok' if ok else f'REJECT ({why})'}")
            if not ok:
                self.speaker_end_invalid += 1
                return
            self.call_term_emitted += 1
            # CallTermination carries the follower's current_source
            src_for_boundary = (self.current_source
                                if self.current_source != 0 else None)
            self.fire_speaker_end(ev, src_for_boundary, "CALL_TERM")
        elif ev.kind == EventKind.TDU_BARE:
            ok, why = self.tdu_bare_valid(ev.ts_ms)
            self.log(ev.ts_str,
                f"{Color.YELLOW}T1 TDU{Color.RESET}"
                f" validity: {'ok' if ok else f'REJECT ({why})'}")
            if not ok:
                self.speaker_end_invalid += 1
                return
            src_for_boundary = (self.current_source
                                if self.current_source != 0 else None)
            self.fire_speaker_end(ev, src_for_boundary, "TDU")
        elif ev.kind == EventKind.BEACON:
            # Not a close event — noise in the log, skip from the
            # trace unless verbose.
            pass
        elif ev.kind == EventKind.GVU_UPDATE:
            pass


def run(path: Path, args):
    events = parse_log(path)
    if args.tg is not None:
        events = [e for e in events
                  if e.tg is None or e.tg == args.tg]
    sim = Simulator(
        rule=args.rule,
        cooldown_ms=args.cooldown_ms,
        split_on_source=(not args.no_split_source),
        ldu1_lc_source=(not args.board_mode),
    )
    for ev in events:
        sim.step(ev)

    # Close any still-open recording
    if sim.active_rec is not None and events:
        sim.close_recording(events[-1].ts_ms, "eof")

    if not args.quiet:
        print(f"=== Trace ({len(sim.trace)} lines) ===")
        for l in sim.trace:
            print(l)

    print(f"\n=== Simulation summary ===")
    print(f"  rule              : {sim.rule}")
    print(f"  cooldown_ms       : {sim.cooldown_ms}")
    print(f"  split_on_source   : {sim.split_on_source}")
    print(f"  ldu1_lc_source    : {sim.ldu1_lc_source}  (False = board parity)")
    print(f"  events processed  : {len(events)}")
    print(f"  LDUs decoded      : {sim.ldu_decoded}")
    print(f"  encrypted grants  : {sim.encrypted_rejected}")
    print(f"  SpeakerEnd emit   : {sim.speaker_end_emitted}")
    print(f"  SpeakerEnd dedup  : {sim.speaker_end_dedup}")
    print(f"  SpeakerEnd invalid: {sim.speaker_end_invalid}")
    print(f"  CallTerm emit     : {sim.call_term_emitted}")
    print(f"  Recordings        : {len(sim.recordings)}")
    print()

    print(f"=== Recording list ===")
    print(f"{'id':>3} {'tg':>4} {'src':>10} {'ldus':>4} {'dur_s':>6} "
          f"{'sp_end':>6}  opened      close_reason")
    for r in sim.recordings:
        dur = r.duration_ms() / 1000.0
        src = str(r.source) if r.source else "-"
        print(f"{r.id:>3} {r.tg:>4} {src:>10} {r.ldu_count:>4} "
              f"{dur:>6.2f} {r.speaker_ends_received:>6}  "
              f"{r.opened_ms//1000%86400:>5d}s     "
              f"{r.close_reason or '(open)'}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("log", type=Path,
                    help="SDRTrunk-style timeseries log")
    ap.add_argument("--rule", default="history",
                    choices=("strict", "history", "plausible"),
                    help="MOT_TC validity rule (default history)")
    ap.add_argument("--cooldown-ms", type=int, default=COOLDOWN_MS_DEFAULT,
                    help="SpeakerEnd dedup cooldown (default 1500)")
    ap.add_argument("--no-split-source", action="store_true",
                    help="Disable recorder's source-change split")
    ap.add_argument("--board-mode", action="store_true",
                    help=("Mirror live board: ignore LDU1 LC FM for source "
                          "(grant follower is sole writer of current_source). "
                          "Use this to predict what the board will produce "
                          "from a given log. Without it, the simulator uses "
                          "LDU1 LC FM and predicts the per-speaker splits "
                          "that re-enabling LDU1 LC stamping would yield."))
    ap.add_argument("--tg", type=int, default=None,
                    help="Filter to a single TG")
    ap.add_argument("--quiet", action="store_true",
                    help="Skip the event trace; just print summary")
    args = ap.parse_args()

    if not args.log.exists():
        print(f"log not found: {args.log}", file=sys.stderr)
        sys.exit(1)
    run(args.log, args)


if __name__ == "__main__":
    main()
