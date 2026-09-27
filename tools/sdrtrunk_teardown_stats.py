#!/usr/bin/env python3
"""Measure P25 traffic-channel teardown / call-close timing from SDRTrunk logs (+ optional p25-httpd dumps).

Distributions (n, min, p10..p99, max, bucket fractions) for: terminators after
the last LDU, system channel hang, same-channel turnaround (by TG and by CC
grant presence), grant->voice latency, same-TG re-grant after the hang,
SDRTrunk call-event end vs last voice, CC GRP_VCH_GRANT/GRNT_UPD keep-alive
cadence, mid-transmission decode dropouts, and .mbe voice spans / turnaround.

Timing: SDRTrunk decoded_messages.log lines are stamped to 1 s only, but the
P25P1 framer accounts for every bit (HDU 792, LDU 1728, TDU 144, TDULC 432,
TSDU 360/576/720, plus "SYNC LOSS [n]" / "DROPPED SAMPLES [n]"), so each log
gets a 9600 bit/s clock whose offset is fitted to the 1 s stamps (max-coverage
fit per 60 s window). Relative times within one log are bit-exact; CC vs
traffic alignment is ~+-30 ms (checked against .mbe epoch-ms frame times).
Message times are END times; a transmission starts at the start of its HDU
(or first LDU) and ends at the end of its last LDU.

Usage:
  python tools/sdrtrunk_teardown_stats.py --recordings C:/Users/Andy/SDRTrunk/recordings \\
      --out report.md --json raw.json
  python tools/sdrtrunk_teardown_stats.py --since 2026-05-02 --freqs 857987500 858437500
  python tools/sdrtrunk_teardown_stats.py --event-logs NONE --p25-calls calls.jsonl --p25-log log.json
"""
from __future__ import annotations

import argparse
import bisect
import collections
import csv
import functools
import glob
import json
import os
import re
import sys
from datetime import datetime

RATE = 9600.0
BITS = {"H": 792, "L": 1728, "T": 144, "LC": 432}
TSDU_BITS = {"1": 360, "2": 216, "3": 144}  # cumulative 360 / 576 / 720 for 1..3 TSBKs
EDGES = (0.5, 1, 2, 3, 5, 10)
REGRANT_EDGES = (1, 2, 3, 5, 10, 30, 60)
LINE_RE = re.compile(r"^(\d{8} \d{6}),(\w+),(.*)$")
FN_RE = re.compile(r"^(\d{8})_(\d{6})\.(\d{3})_(\d+)_Hz_(.+)_(decoded_messages|call_events)\.log$")
APP_RE = re.compile(r"^(\d{8} \d{6}\.\d{3}) \[sdrtrunk channel \[[^\]]*\] (\d+) thread")
MBE_RE = re.compile(r"^(\d{8})_(\d{6})_(\d+)_(\d+)_(\d+)_(\d+)(_encrypted)?\.mbe$")
FMTO_RE = re.compile(r"FM:(\d+)\S* TO:(\d+)")


@functools.lru_cache(maxsize=None)
def stamp(s):
    return datetime.strptime(s, "%Y%m%d %H%M%S").timestamp()


# ---------------------------------------------------------------- statistics
def quant(xs, q):
    xs = sorted(xs)
    k = (len(xs) - 1) * q
    f = int(k)
    return xs[f] + (xs[min(f + 1, len(xs) - 1)] - xs[f]) * (k - f)


def dist(xs):
    xs = [x for x in xs if x is not None]
    d = {"n": len(xs)}
    if xs:
        d.update(min=min(xs), max=max(xs), **{"p%d" % q: quant(xs, q / 100) for q in (10, 50, 90, 95, 99)})
    return d


def dist_table(rows, unit="s"):
    out = ["| metric | n | min | p10 | p50 | p90 | p95 | p99 | max |", "|---|---|---|---|---|---|---|---|---|"]
    for name, xs in rows:
        d = dist(xs)
        vals = ["%.3f" % d[k] if d["n"] else "" for k in ("min", "p10", "p50", "p90", "p95", "p99", "max")]
        out.append("| %s | %d | %s |" % (name, d["n"], " | ".join(vals)))
    return "\n".join(out) + "\n\nUnit: %s.\n" % unit


def bucket_table(rows, edges=EDGES):
    out = ["| split | n | " + " | ".join("<%gs" % e for e in edges) + " |", "|---|---" + "|---" * len(edges) + "|"]
    for name, xs in rows:
        xs = [x for x in xs if x is not None]
        cells = ["%.1f%%" % (100.0 * sum(1 for x in xs if x < e) / len(xs)) if xs else "-" for e in edges]
        out.append("| %s | %d | %s |" % (name, len(xs), " | ".join(cells)))
    return "\n".join(out) + "\n"


def count_table(counter, head=("value", "count")):
    tot = sum(counter.values()) or 1
    rows = ["| %s | %d | %.1f%% |" % (k, v, 100.0 * v / tot) for k, v in counter.most_common()]
    return "\n".join(["| %s | %s | share |" % head, "|---|---|---|"] + rows) + "\n"


# ---------------------------------------------------------------- log parsing
class Msg:
    __slots__ = ("t", "dur", "k", "sub", "body", "tg", "src", "enc", "bits", "ok")


def classify(body):
    """(kind, sub-kind, bits consumed on the bit clock) for one decoded_messages body."""
    if "SYNC LOSS" in body:
        return "SL", "", int(re.search(r"\[(\d+)\]", body).group(1))
    if "DROPPED SAMPLES" in body:
        return "DS", "", -int(re.search(r"\[(\d+)\]", body).group(1))
    b = re.sub(r"^NAC:\S+\s+", "", body)
    if b.startswith("LDU"):
        return "L", "", BITS["L"]
    if b.startswith("HDU"):
        return "H", "", BITS["H"]
    if b.startswith("TDULC"):
        sub = ("TALK_COMPLETE" if "TALK COMPLETE" in b else "CALL_TERMINATION" if "CALL TERMINATION" in b
               else "CHANNEL_USER" if "CHANNEL USER" in b else "CHANNEL_UPDATE" if "CHANNEL UPDATE" in b
               else "TDULC_OTHER")
        return "LC", sub + ("(crc)" if "CRC FAIL" in b or "CRC-FAIL" in b else ""), BITS["LC"]
    if b.startswith("TDU"):
        return "T", "TDU", BITS["T"]
    m = re.match(r"TSBK([123])", b)
    if m:
        return "TSBK", "", TSDU_BITS[m.group(1)]
    return "O", b.split(" ")[0], 360  # PDU/IPPKT/...: length unknown, the offset fit absorbs it


def fit_offset(los):
    """Point covered by the most intervals [lo, lo+1): clock offset agreeing with most 1 s stamps."""
    ev = sorted([(x, 1) for x in los] + [(x + 1.0, -1) for x in los])
    best = cur = 0
    lo = hi = los[0]
    for i, (x, d) in enumerate(ev):
        cur += d
        if cur > best:
            best, lo, hi = cur, x, ev[i + 1][0] if i + 1 < len(ev) else x
    return (lo + hi) / 2


def parse_log(path, window_s=60.0):
    raw, cum = [], 0
    with open(path, encoding="utf-8", errors="replace") as fh:
        for ln in fh:
            m = LINE_RE.match(ln.rstrip("\r\n"))
            if m:
                body = m.group(3).strip()
                k, sub, nb = classify(body)
                cum += nb
                raw.append((stamp(m.group(1)), cum, k, sub, body, m.group(2) == "PASSED", nb))
    msgs, i = [], 0
    while i < len(raw):  # piecewise fit absorbs clock drift and sample drops
        j = i
        while j < len(raw) and raw[j][1] - raw[i][1] < window_s * RATE:
            j += 1
        j = len(raw) if len(raw) - j < 20 else j
        a = fit_offset([raw[x][0] - raw[x][1] / RATE for x in range(i, j)])
        for s, b, k, sub, body, ok, nb in raw[i:j]:
            mm = Msg()
            mm.t, mm.k, mm.sub, mm.body, mm.ok, mm.bits = a + b / RATE, k, sub, body, ok, nb
            mm.dur = BITS.get(k, 0) / RATE
            mm.tg = mm.src = None
            mm.enc = "ENCRYPT" in body and "UNENCRYPTED" not in body
            f = FMTO_RE.search(body)
            if "CRC FAIL" in body or "CRC-FAIL" in body:
                pass  # the last LDU1 of a tx often carries a CRC-failed LC (FM:532610 TO:8322): ignore ids
            elif f and "CHANNEL USER" in body:
                mm.src, mm.tg = int(f.group(1)) or None, int(f.group(2))
            elif k == "H":
                g = re.search(r"TALKGROUP:(\d+)", body)
                mm.tg = int(g.group(1)) if g else None
            msgs.append(mm)
        i = j
    return msgs


def scan_files(evdir, since, until):
    out = []
    for p in glob.glob(os.path.join(evdir, "*.log")):
        m = FN_RE.match(os.path.basename(p))
        if not m or (since and m.group(1) < since) or (until and m.group(1) > until):
            continue
        t0 = datetime.strptime(m.group(1) + m.group(2), "%Y%m%d%H%M%S").timestamp() + int(m.group(3)) / 1000
        out.append(dict(path=p, t0=t0, freq=int(m.group(4)), kind=m.group(6), traffic=m.group(5).startswith("T-")))
    return sorted(out, key=lambda f: f["t0"])


def nac_ok(msgs, nac):
    tagged = [m for m in msgs if m.body.startswith("NAC:")]
    return bool(tagged) and sum(1 for m in tagged if ("/x" + nac) in m.body) >= 0.5 * len(tagged)


# ---------------------------------------------------------------- control channel
class CC:
    def __init__(self, f, msgs):
        self.f, self.t0, self.t1 = f, msgs[0].t - 1, msgs[-1].t
        self.times = [m.t for m in msgs if m.ok and m.k == "TSBK"]
        idens = {0: (851006250, 6250, 1)}
        for m in msgs:
            g = re.search(r"IDEN_UPDATE(_TDMA)? ID:(\d+) \S+ SPACING:(\d+) BASE:(\d+)", m.body)
            if g:
                ts = re.search(r"TIMESLOTS:(\d+)", m.body)
                idens[int(g.group(2))] = (int(g.group(4)), int(g.group(3)), int(ts.group(1)) if ts else 1)

        def hz(i, c):
            return idens[i][0] + (c // idens[i][2]) * idens[i][1] if i in idens else None
        self.grants, self.ment = collections.defaultdict(list), collections.defaultdict(list)
        self.grants_tg = collections.defaultdict(list)
        for m in msgs:
            if not m.ok or "CRC-FAIL" in m.body or m.k != "TSBK":
                continue
            g = re.search(r"GRP_VCH_GRANT FM:(\d+) TO:(\d+) CHAN:(\d+)-(\d+)", m.body)
            if g:
                key = (hz(int(g.group(3)), int(g.group(4))), int(g.group(2)))
                self.grants[key].append((m.t, int(g.group(1))))
                self.grants_tg[key[1]].append((m.t, key[0]))
                self.ment[key].append(m.t)
            elif "GRP_VCH_GRNT_UPD" in m.body:
                for _, tg, i, c in re.findall(r"GROUP ([AB]):(\d+) CHAN \1:(\d+)-(\d+)", m.body):
                    self.ment[(hz(int(i), int(c)), int(tg))].append(m.t)
        for d in (self.grants, self.grants_tg, self.ment):
            for v in d.values():
                v.sort()
        self.times.sort()

    def healthy(self, a, b, max_gap=0.35):
        """CC log covers [a, b] and decoded a TSBK at least every max_gap seconds."""
        pts = [a] + self.times[bisect.bisect_left(self.times, a):bisect.bisect_right(self.times, b)] + [b]
        return self.t0 <= a and b <= self.t1 and max(y - x for x, y in zip(pts, pts[1:])) <= max_gap

    def grants_in(self, key, a, b):
        """[(time, source)] of GRP_VCH_GRANTs for (freq, tg) with a <= time <= b."""
        g = self.grants.get(key, [])
        return g[bisect.bisect_left(g, (a, -1)):bisect.bisect_right(g, (b, 1 << 40))]


# ---------------------------------------------------------------- traffic channel
class Tx:
    def __init__(self, f, m):
        self.f, self.freq, self.tg, self.src, self.enc = f, f["freq"], None, None, False
        self.t0, self.first_end, self.hdu, self.last_voice = m.t - m.dur, m.t, m.k == "H", m.t
        self.last, self.n_ldu, self.drops, self.post, self.mid_tdu = None, 0, [], [], []
        self.term = self.term_delay = self.next = None
        self.hend = None  # carrier drop (last tx) or next tx start


def segment(f, msgs):
    """Split one traffic-channel log into voice transmissions (HDU/first LDU -> last LDU)."""
    txs, cur, since = [], None, []
    for m in msgs:
        if m.k in ("H", "L"):
            tg = None if "CHANNEL UPDATE" in m.body else m.tg
            if (m.k == "L" and cur is not None and cur.term == "TDU" and not (tg and cur.tg and tg != cur.tg)
                    and all(x.k in ("T", "SL", "DS") for x in since) and (m.t - m.dur) - cur.last <= 0.2):
                # TDU inside voice (LDU -> TDU -> LDU, no HDU): phantom sync (DROPPED SAMPLES around it,
                # zero net time) or a talker hand-over; the voice stream continues, so keep one transmission
                cur.mid_tdu.append(any(x.k == "DS" for x in since))
                del cur.post[len(cur.post) - len(since):]
                cur.term = cur.term_delay = None
            if cur is None or m.k == "H" or cur.term is not None or (tg and cur.tg and tg != cur.tg):
                cur = Tx(f, m)
                txs.append(cur)
            elif m.k == "L" and (m.t - m.dur) - cur.last_voice > 0.03:
                cur.drops.append((m.t - m.dur) - cur.last_voice)
            if m.k == "L":
                cur.n_ldu, cur.last = cur.n_ldu + 1, m.t
            cur.last_voice, since = m.t, []
            cur.tg, cur.src, cur.enc = cur.tg or tg, m.src or cur.src, cur.enc or m.enc
        elif cur is not None:
            cur.post.append(m)
            since.append(m)
            if m.k in ("T", "LC") and cur.term is None and cur.n_ldu:
                cur.term, cur.term_delay = m.sub, (m.t - m.dur) - cur.last
    txs = [t for t in txs if t.n_ldu]
    for a, b in zip(txs, txs[1:] + [None]):
        a.next, a.hend = b, (b.t0 if b else hang_end(a))
    return txs


def hang_end(tx, quiet_bits=4800):
    """Last decoded message after tx before a >=0.5 s SYNC LOSS (carrier drop) or log end."""
    end = tx.last
    for m in tx.post:
        if m.k == "SL" and m.bits >= quiet_bits:
            break
        if m.k not in ("SL", "DS"):
            end = m.t
    return end


# ---------------------------------------------------------------- other inputs
def parse_call_events(paths):
    """Final state per EVENT_ID of Group Call rows in CC call_events.log files."""
    ev = {}
    for p in paths:
        with open(p, encoding="utf-8", errors="replace", newline="") as fh:
            for row in csv.DictReader(fh):
                try:
                    t = datetime.strptime(row["TIMESTAMP"], "%Y:%m:%d:%H:%M:%S").timestamp()
                    freq = int(round(float(row["FREQUENCY"]) * 1e6))
                except (ValueError, KeyError, TypeError):
                    continue
                if "Group Call" not in (row.get("EVENT") or ""):
                    continue
                e = ev.setdefault(row.get("EVENT_ID"), dict(t=t, freq=freq, dur=0, det=set(), tg=None))
                e["dur"] = max(e["dur"], int(row.get("DURATION_MS") or 0))
                e["det"].add(row.get("DETAILS") or "")
                g = re.search(r"\((\d+)\)\s*$", row.get("TO") or "")
                e["tg"] = int(g.group(1)) if g else e["tg"]
    for e in ev.values():
        e["kind"] = "call" if any(d.startswith("PHASE 1 CALL") for d in e["det"]) else "grant"
    return list(ev.values())


def parse_mbe(rdir, since, until, freqs, split_s=0.5):
    """Voice segments from .mbe files (frames[].time epoch ms), split at frame gaps > split_s."""
    segs = []
    for p in glob.glob(os.path.join(rdir, "*.mbe")):
        m = MBE_RE.match(os.path.basename(p))
        if not m or (since and m.group(1) < since) or (until and m.group(1) > until):
            continue
        if freqs and int(m.group(3)) not in freqs:
            continue
        try:
            with open(p, encoding="utf-8") as fh:
                times = sorted(x["time"] / 1000.0 for x in json.load(fh).get("frames", []))
        except (ValueError, OSError, KeyError, TypeError, AttributeError):
            continue
        start = 0
        for i in range(1, len(times) + 1):
            if i == len(times) or times[i] - times[i - 1] > split_s:
                segs.append(dict(freq=int(m.group(3)), tg=int(m.group(5)), file=os.path.basename(p),
                                 t0=times[start], t1=times[i - 1] + 0.02))
                start = i
    return sorted(segs, key=lambda s: (s["freq"], s["t0"]))


def load_json_docs(path):
    with open(path, encoding="utf-8") as fh:
        txt = fh.read()
    try:
        return [json.loads(txt)]
    except ValueError:  # JSONL of snapshots
        return [json.loads(ln) for ln in txt.splitlines() if ln.strip().startswith(("{", "["))]


def items_of(doc):
    if isinstance(doc, list):
        return doc
    for k in ("items", "entries", "calls"):
        if isinstance(doc, dict) and isinstance(doc.get(k), list):
            return doc[k]
    return []


def p25_section(calls_paths, log_paths, J):
    R = ["## p25-httpd comparison\n"]
    num = lambda c, k: c.get(k) if isinstance(c.get(k), (int, float)) else None  # noqa: E731
    if calls_paths:
        calls = {}
        for p in calls_paths:
            for d in load_json_docs(p):
                for it in items_of(d):
                    if isinstance(it, dict) and it.get("call_id") is not None:
                        calls[it["call_id"]] = it  # later snapshot wins
        cl = sorted(calls.values(), key=lambda c: num(c, "started_unix_ms") or 0)
        voiced = [c for c in cl if not c.get("not_followed") and num(c, "voice_ms") and num(c, "first_voice_ms") is not None
                  and num(c, "started_unix_ms") is not None]
        tear = [(c["open_ms"] - c["first_voice_ms"] - c["voice_ms"]) / 1000 for c in voiced if num(c, "open_ms") is not None]
        fv = [c["first_voice_ms"] / 1000 for c in cl if num(c, "first_voice_ms") is not None]
        ta, by_f = collections.defaultdict(list), collections.defaultdict(list)
        for c in cl:
            by_f[c.get("freq_hz")].append(c)
        for lst in by_f.values():
            for a, b in zip(lst, lst[1:]):
                if a in voiced and num(b, "started_unix_ms") is not None:
                    g = (b["started_unix_ms"] - a["started_unix_ms"] - a["first_voice_ms"] - a["voice_ms"]) / 1000
                    if g < 60:
                        ta["same TG" if a.get("tg") == b.get("tg") else "different TG"].append(g)
        J["p25_calls"] = dict(teardown=tear, first_voice=fv, turnaround=dict(ta))
        R.append("%d unique call_id in %d file(s); %d followed with voice.\n" % (len(cl), len(calls_paths), len(voiced)))
        for k in ("close_reason", "not_followed", "audio_status"):
            R.append(count_table(collections.Counter(str(c.get(k)) for c in cl), (k, "calls")))
        R.append(dist_table([("teardown = open_ms - (first_voice_ms + voice_ms)", tear), ("first_voice_ms", fv)]
                            + [("turnaround, " + k, v) for k, v in sorted(ta.items())]))
        R.append(bucket_table([("turnaround, " + k, v) for k, v in sorted(ta.items())]))
    for p in log_paths:
        ents = [e for d in load_json_docs(p) for e in items_of(d) if isinstance(e, dict)]
        reasons = collections.Counter(str((e.get("fields") or {}).get("reason")) for e in ents
                                      if e.get("message") == "call_closing")
        R.append("Log `%s`: %d entries; categories %s.\n" % (os.path.basename(p), len(ents),
                                                           dict(collections.Counter(str(e.get("category")) for e in ents))))
        R.append(count_table(reasons, ("call_closing reason", "count")) if reasons else "No call_closing entries.\n")
    return "\n".join(R)


# ---------------------------------------------------------------- analysis
def load(a):
    files = scan_files(a.event_logs, a.since, a.until)
    ccs, tfiles, skipped = [], [], collections.Counter()
    for f in files:
        is_cc = f["freq"] == a.cc_freq and not f["traffic"]
        if f["kind"] != "decoded_messages" or not (is_cc or (f["traffic"] and (not a.freqs or f["freq"] in a.freqs))):
            continue
        msgs = parse_log(f["path"])
        if not nac_ok(msgs, a.nac):
            skipped["CC" if is_cc else "traffic"] += 1
        elif is_cc:
            ccs.append(CC(f, msgs))
        else:
            f["msgs"], f["txs"] = msgs, segment(f, msgs)
            f["cc"] = next((c for c in ccs if c.t0 - 2 <= f["t0"] <= c.t1), None)
            f["own"] = [m for m in msgs if m.k not in ("SL", "DS") and ("/x" + a.nac) in m.body]
            tfiles.append(f)
    return files, ccs, tfiles, skipped


def analyse(a, J):
    files, ccs, tfiles, skipped = load(a)
    live = [f for f in tfiles if f["txs"]]
    txs = [t for f in live for t in f["txs"]]
    by_freq = collections.defaultdict(list)
    for t in txs:
        by_freq[t.freq].append(t)
    days = sorted({datetime.fromtimestamp(f["t0"]).strftime("%Y-%m-%d") for f in live})
    R = ["# SDRTrunk traffic-channel teardown statistics\n",
         "Generated %s by `tools/sdrtrunk_teardown_stats.py %s`; NAC %s, CC %d Hz.\n" % (
             datetime.now().strftime("%Y-%m-%d %H:%M"), " ".join(sys.argv[1:]), a.nac, a.cc_freq),
         "## Coverage\n",
         "- Days with traffic logs: %s\n- CC decoded_messages sessions: %d (%.2f h)\n"
         "- Traffic-channel logs (one per SDRTrunk allocation): %d, %d with voice, %d inside a CC session\n"
         "- Voice transmissions: %d (%d encrypted, %d with HDU), %d LDUs\n- Logs skipped (other NAC / empty): %s\n" % (
             ", ".join(days) or "-", len(ccs), sum(c.t1 - c.t0 for c in ccs) / 3600, len(tfiles), len(live),
             sum(1 for f in live if f["cc"]), len(txs), sum(t.enc for t in txs), sum(t.hdu for t in txs),
             sum(t.n_ldu for t in txs), dict(skipped) or "none"),
         count_table(collections.Counter(t.freq for t in txs), ("freq Hz", "transmissions"))]

    # 1. terminators
    kinds = collections.Counter(t.term or ("NONE, next tx follows" if t.next else "NONE, log ends") for t in txs)
    by_kind = collections.defaultdict(list)
    for t in txs:
        if t.term:
            by_kind["all"].append(t.term_delay)
            by_kind[t.term].append(t.term_delay)
    cls = lambda s: "unknown src" if not s else "console src<100000" if s < 100000 else "subscriber"  # noqa: E731
    tc = collections.Counter("%s, %s" % (cls(t.src), "TALK COMPLETE" if any(m.sub.startswith("TALK_COMPLETE")
                             for m in t.post) else "no TALK COMPLETE") for t in txs)
    mid = [x for t in txs for x in t.mid_tdu]
    J["term_delay"] = dict(by_kind)
    R += ["## 1. Terminators after the last LDU\n", "Delay = start of first TDU/TDULC - end of last LDU "
          "(0 = back-to-back; 0.18 = one LDU missed).\n", count_table(kinds, ("first terminator", "transmissions")),
          dist_table(sorted(by_kind.items())), count_table(tc, ("source class, TALK COMPLETE before next tx", "tx")),
          "TDU inside voice (LDU, TDU, LDU without HDU; not counted as an end): %d (%d flanked by DROPPED SAMPLES "
          "= phantom sync, %d clean).\n" % (len(mid), sum(mid), len(mid) - sum(mid))]

    # 2. hang + traffic decode silence while the allocation is live
    hang, n_user, endk, tdu_h, live_max, live_gaps = [], [], collections.Counter(), 0, [], []
    for f in live:
        t = f["txs"][-1]
        dec = [m for m in t.post if m.k not in ("SL", "DS")]
        hang.append(t.hend - t.last)
        n_user.append(sum(1 for m in dec if m.sub == "CHANNEL_USER"))
        endk[dec[-1].sub if dec else "(nothing after last LDU)"] += 1
        tdu_h += any(m.k == "T" for m in dec)
        ms = [m for m in f["own"] if f["txs"][0].t0 < m.t <= t.hend + 1e-6]
        g = [(y.t - y.dur) - x.t for x, y in zip(ms, ms[1:])]
        live_max.append(max(g, default=0.0))
        live_gaps += [x for x in g if x > 0.03]
    J.update(hang=hang, live_silence_max=live_max, live_silence_gaps=live_gaps)
    R += ["## 2. System channel hang (last transmission of each allocation)\n",
          dist_table([("last LDU -> last msg before >=0.5 s SYNC LOSS (carrier drop)", hang),
                      ("TDULC CHANNEL USER count in hang", n_user),
                      ("max own-NAC decode silence per allocation (voice start -> drop)", live_max),
                      ("all own-NAC decode silences >30 ms while live", live_gaps)]),
          count_table(endk, ("last message of allocation", "allocations")),
          "Bare TDU inside the final hang: %d of %d allocations.\n" % (tdu_h, len(live))]

    # 3. turnaround, grants, re-grant
    ta, st_cnt, tdu_gap = collections.defaultdict(list), collections.Counter(), collections.Counter()
    lat_c, lat_last, lat_new, tdu_lead, g_lead = [], [], [], [], []
    for t in txs:
        n = t.next
        if n is None:
            continue
        gap = n.t0 - t.last
        same = "TG unknown" if not (t.tg and n.tg) else "same TG" if n.tg == t.tg else "different TG"
        tdu = [n.t0 - m.t for m in t.post if m.k == "T"]
        tdu_lead += tdu
        tdu_gap["%s, %s, %s" % (same, "TDU in gap" if tdu else "no TDU", "HDU" if n.hdu else "no HDU")] += 1
        ta["all"].append(gap)
        ta[same].append(gap)
        cc = t.f["cc"]
        if cc is None or not n.tg:
            st = "no CC log" if cc is None else "grant check n/a"
        else:  # N's own grant burst ends ~0.3 s after N starts; a queued talker's grant can come any time after
            g = [x for x, s in cc.grants_in((n.freq, n.tg), t.t0 - 0.2, n.t0 + 0.5)
                 if x > t.t0 + 0.3 or (n.src and s == n.src != t.src)]
            st = "grant" if g else "no grant, CC healthy" if cc.healthy(t.last, n.t0) else "no grant, CC gap"
            if g:
                i = len(g) - 1  # last grant burst (repeats <= 0.5 s apart) before N+1 voice
                while i > 0 and g[i] - g[i - 1] <= 0.5:
                    i -= 1
                lat_c.append(n.t0 - g[i])
                lat_last.append(n.t0 - g[-1])
                g_lead.append(g[0] - t.last)
        st_cnt[st] += 1
        ta["%s, %s" % (same, st)].append(gap)
    regrant, regrant_drop = collections.defaultdict(list), []
    for f in live:
        t, cc = f["txs"][0], f["cc"]
        if cc is None:
            continue
        g = [x for x, _ in cc.grants_in((t.freq, t.tg), f["t0"] - 10, t.t0 + 0.5)]
        if g:
            lat_new.append(t.t0 - [x for x in g if x >= g[-1] - 3][0])
        t = f["txs"][-1]
        nxt = [(x, fq) for x, fq in cc.grants_tg.get(t.tg, []) if x > t.last + 0.05]
        if not nxt:
            regrant["none, >=60 s of CC log after" if cc.t1 >= t.hend + 60 else "none, CC log ends <60 s"].append(None)
            continue
        regrant["all"].append(nxt[0][0] - t.last)
        regrant["same channel" if nxt[0][1] == t.freq else "other channel"].append(nxt[0][0] - t.last)
        regrant_drop.append(nxt[0][0] - t.hend)
    J.update(turnaround=dict(ta), grant_to_voice_continuation=lat_c, grant_to_voice_new_alloc=lat_new,
             regrant_same_tg_after_last_ldu=dict(regrant), regrant_after_carrier_drop=regrant_drop)
    none60 = len(regrant["none, >=60 s of CC log after"])
    R += ["## 3. Same-channel turnaround (end of last LDU of N -> start of HDU/first LDU of N+1, same log)\n",
          "Grant check: CC GRP_VCH_GRANT for (N+1 TG, channel) between N start + 0.3 s (earlier only if FM is "
          "N+1's source) and N+1 voice start + 0.5 s; 'CC healthy' = CC decoded a TSBK at least every 0.35 s.\n",
          dist_table(sorted(ta.items())), bucket_table(sorted(ta.items())),
          count_table(st_cnt, ("CC grant for N+1", "turnarounds")),
          count_table(tdu_gap, ("TG, bare TDU between N and N+1, N+1 starts with", "turnarounds")),
          "Grants for N+1 issued while N was still talking (queued): %d of %d.\n" % (
              sum(1 for x in g_lead if x < 0), len(g_lead)),
          dist_table([("grant burst (first) -> N+1 voice start, same-channel continuation", lat_c),
                      ("grant burst (last repeat) -> N+1 voice start", lat_last),
                      ("first grant for N+1 - end of N (negative = queued during N)", g_lead),
                      ("grant -> first decoded voice, new allocation (incl. SDRTrunk tune)", lat_new),
                      ("bare TDU in gap -> N+1 voice start", tdu_lead)]),
          "### Same-TG re-grant after the allocation's last LDU (any channel)\n",
          dist_table([("last LDU -> next GRP_VCH_GRANT same TG", regrant["all"]),
                      ("carrier drop -> next GRP_VCH_GRANT same TG", regrant_drop)]),
          bucket_table([("all (+%d none within 60 s)" % none60, regrant["all"] + [1e9] * none60)]
                       + [(k, regrant[k]) for k in ("same channel", "other channel")], REGRANT_EDGES),
          count_table(collections.Counter({k: len(v) for k, v in regrant.items() if k != "all"}),
                      ("next same-TG grant", "allocations"))]

    # 4. SDRTrunk call events + channel stop
    cev = parse_call_events([f["path"] for f in files if f["kind"] == "call_events" and f["freq"] == a.cc_freq])
    ev_gap, ev_span, ev_endk, cnt, covered = (collections.defaultdict(list), collections.Counter(),
                                              collections.Counter(), collections.Counter(), set())
    cover = collections.defaultdict(list)
    for f in tfiles:
        cover[f["freq"]].append((f["t0"] - 3, (f["msgs"][-1].t if f["msgs"] else f["t0"]) + 1))
    for e in cev:
        txl = by_freq.get(e["freq"])
        if not txl or not any(x <= e["t"] <= y for x, y in cover[e["freq"]]):
            continue
        if e["kind"] == "call":
            cands = [t.first_end for t in txl if t.hdu]
        else:
            cc = next((c for c in ccs if c.t0 <= e["t"] <= c.t1 + 1), None)
            cands = [x for x, _ in cc.grants.get((e["freq"], e["tg"]), [])] if cc else []
        cands = sorted(x for x in cands if e["t"] - 0.05 <= x < e["t"] + 1.05)
        end = cands[0] + e["dur"] / 1000.0 if cands else None
        span = [t for t in txl if cands and t.first_end >= cands[0] - 0.2 and t.t0 < end]
        cnt[e["kind"] + (": spans voice" if span else ": ended before voice" if cands else ": no logged start")] += 1
        if not span:
            continue
        covered.update(id(t) for t in span)
        ev_span[len(span)] += 1
        last = max(span, key=lambda t: t.t0)
        ev_gap["all"].append(end - last.last)
        ev_gap[e["kind"]].append(end - last.last)
        near = min(last.f["own"], key=lambda m: abs(m.t - end))
        ev_endk[(near.sub or near.k) if abs(near.t - end) < 0.1 else "none within 100 ms"] += 1
    stops, stop_d = collections.defaultdict(list), []
    for p in (glob.glob(os.path.join(a.app_logs, "*sdrtrunk_app*.log")) if a.app_logs else []):
        with open(p, encoding="utf-8", errors="replace") as fh:
            for ln in fh:
                m = APP_RE.match(ln) if "DataCaptureModule stopped" in ln else None
                if m:
                    stops[int(m.group(2))].append(datetime.strptime(m.group(1), "%Y%m%d %H%M%S.%f").timestamp())
    for f in tfiles:
        last = f["own"][-1].t if f["own"] else None
        s_ = [x for x in stops.get(f["freq"], []) if last and last - 0.5 <= x <= last + 15]
        if s_:
            stop_d.append(min(s_) - last)
    J.update(call_event_end_minus_last_ldu=dict(ev_gap), sdrtrunk_channel_stop_after_last_msg=stop_d)
    R += ["## 4. SDRTrunk call events (CC call_events.log Group Call rows) vs decoded voice\n",
          "Event start is pinned to the HDU (`PHASE 1 CALL`) or CC grant (`PHASE 1 CHANNEL GRANT`) in the same "
          "second; end = start + final DURATION_MS. Transmissions covered by an event: %d of %d.\n" % (
              len(covered), len(txs)), count_table(cnt, ("event (inside traffic-log coverage)", "events")),
          dist_table(sorted(("event end - last LDU end (%s)" % k, v) for k, v in ev_gap.items())),
          count_table(ev_span, ("transmissions per event", "events")),
          count_table(ev_endk, ("decoded message nearest the event end", "events"))]
    if a.app_logs:
        R += ["SDRTrunk traffic-channel stop (app log `P25DataCaptureModule stopped`, ms stamps) vs the channel's "
              "last decoded message: %d of %d allocations matched.\n" % (len(stop_d), len(tfiles)),
              dist_table([("channel stop - last decoded message", stop_d)])]

    # 5. CC keep-alive
    gh, gb, gv, ghang, after_ldu, after_drop, cc_max, any_max = [], [], [], [], [], [], [], []
    for c in ccs:
        for key, ts in c.ment.items():
            act = [(t.t0, t.last, t.hend) for t in by_freq.get(key[0], []) if t.tg == key[1] and c.t0 <= t.t0 <= c.t1]
            for x, y in zip(ts, ts[1:]):
                if y - x > 10 or y == x:
                    continue
                ok = c.healthy(x, y)
                (gh if ok else gb).append(y - x)
                if ok and any(v0 <= (x + y) / 2 <= v1 for v0, v1, _ in act):
                    gv.append(y - x)
                elif ok and any(v1 < (x + y) / 2 <= h for _, v1, h in act):
                    ghang.append(y - x)
    for f in live:
        t0, t = f["txs"][0].t0, f["txs"][-1]
        if f["cc"] is None or not t.tg:
            continue
        ment = f["cc"].ment.get((t.freq, t.tg), [])
        win = [x for x in ment if t0 <= x <= t.hend]
        pts = sorted([t0, t.hend] + win)
        cc_max.append(max(y - x for x, y in zip(pts, pts[1:])))
        pts = sorted(set(pts + [m.t for m in f["own"] if t0 <= m.t <= t.hend]))
        any_max.append(max(y - x for x, y in zip(pts, pts[1:])))
        bound = min([g["t0"] for g in live if g["freq"] == t.freq and g["t0"] > f["t0"]] + [1e18])
        ts = [x for x in ment if t0 <= x < bound - 0.2]
        j = 0
        while j + 1 < len(ts) and ts[j + 1] - ts[j] < 3:
            j += 1
        if ts:
            after_ldu.append(ts[j] - t.last)
            after_drop.append(ts[j] - t.hend)
    J.update(upd_gap_cc_healthy=gh, upd_gap_cc_gap=gb, upd_gap_voice=gv, upd_gap_hang=ghang,
             alloc_max_gap_cc=cc_max, alloc_max_gap_any=any_max,
             last_mention_minus_last_ldu=after_ldu, last_mention_minus_carrier_drop=after_drop)
    R += ["## 5. CC keep-alive: GRP_VCH_GRANT + GRP_VCH_GRNT_UPD (A and B) mentions of the same (TG, channel)\n",
          "'voice' / 'hang' = gap midpoint inside a logged transmission / between its last LDU and the next "
          "transmission or carrier drop (CC healthy only). Per-allocation maxima are over [first voice start, "
          "carrier drop].\n",
          dist_table([("gap <=10 s, CC decoding continuously", gh), ("gap spanning a CC decode gap", gb),
                      ("gap during voice", gv), ("gap during hang / between tx", ghang),
                      ("max CC-mention gap per allocation", cc_max),
                      ("max gap in (own-NAC traffic decode + CC mention) per allocation", any_max),
                      ("last mention - last LDU of allocation", after_ldu),
                      ("last mention - carrier drop", after_drop)]),
          bucket_table([("gap, CC healthy", gh), ("gap during voice", gv), ("max CC gap per allocation", cc_max)],
                       (0.4, 0.5, 0.75, 1, 1.5, 2))]

    # 6. dropouts inside transmissions
    drops = [g for t in txs for g in t.drops]
    lost = sum(round(g / 0.18) for g in drops)
    J.update(dropout_gap=drops, voice_span=[t.last - t.t0 for t in txs])
    R += ["## 6. Decode dropouts inside a live transmission (LDU -> LDU gap, no terminator between)\n",
          "%d of %d transmissions (%.1f%%) have >=1 dropout; %d dropouts; %.2f%% of LDU slots lost.\n" % (
              sum(1 for t in txs if t.drops), len(txs), 100.0 * sum(1 for t in txs if t.drops) / max(len(txs), 1),
              len(drops), 100.0 * lost / max(lost + sum(t.n_ldu for t in txs), 1)),
          dist_table([("dropout gap", drops), ("voice span per transmission", J["voice_span"]),
                      ("LDUs per transmission", [t.n_ldu for t in txs])])]

    # 7. .mbe recordings
    if a.recordings:
        segs = [s for s in parse_mbe(a.recordings, a.since, a.until, set(a.freqs or []) or set(by_freq))
                if s["freq"] != a.cc_freq]
        mt, match, dta = collections.defaultdict(list), {}, []
        for s in segs:
            c = [t for t in by_freq.get(s["freq"], []) if abs(s["t0"] - t.t0 - 0.25) < 0.4]
            if c:
                match[id(s)] = min(c, key=lambda t: abs(s["t0"] - t.t0 - 0.25))
        m_ = [(s, match[id(s)]) for s in segs if id(s) in match]
        for x, y in zip(segs, segs[1:]):
            if x["freq"] == y["freq"] and y["t0"] - x["t1"] < 10:
                g = y["t0"] - x["t1"]
                mt["all <10 s"].append(g)
                mt["same TG" if x["tg"] == y["tg"] else "different TG"].append(g)
                tx_, ty = match.get(id(x)), match.get(id(y))
                if tx_ is not None and ty is not None and tx_.next is ty:
                    dta.append(g - (ty.t0 - tx_.last))
        mdays = {datetime.fromtimestamp(s["t0"]).date() for s in segs}
        inr = {id(t) for t in txs if datetime.fromtimestamp(t.t0).date() in mdays}
        J.update(mbe_turnaround=dict(mt), mbe_span=[s["t1"] - s["t0"] for s in segs],
                 mbe_clock_offset=[s["t0"] - t.t0 for s, t in m_])
        R += ["## 7. .mbe recordings cross-check\n",
              "%d voice segments from %d .mbe files (split at >0.5 s frame gaps); %d matched to a logged "
              "transmission; %d of %d logged transmissions on .mbe days have a segment.\n" % (
                  len(segs), len({s["file"] for s in segs}), len(m_), len({id(t) for _, t in m_} & inr), len(inr)),
              dist_table([("voice span (.mbe)", J["mbe_span"]),
                          ("first frame - log voice start (expect ~0.26 = HDU + 1 LDU)", J["mbe_clock_offset"]),
                          ("turnaround(.mbe) - turnaround(log), same pair", dta)]
                         + [("turnaround (.mbe), " + k, v) for k, v in sorted(mt.items())]),
              bucket_table([("turnaround (.mbe), " + k, v) for k, v in sorted(mt.items())])]
    return "\n".join(R)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--event-logs", default="C:/Users/Andy/SDRTrunk/event_logs", help="SDRTrunk event_logs dir; NONE skips")
    ap.add_argument("--recordings", help="SDRTrunk recordings dir with .mbe files (optional)")
    ap.add_argument("--app-logs", help="SDRTrunk logs dir (*sdrtrunk_app*.log) for traffic-channel stop times")
    ap.add_argument("--cc-freq", type=int, default=860962500)
    ap.add_argument("--nac", default="8A1", help="site NAC (hex) used to select logs")
    ap.add_argument("--freqs", type=int, nargs="*", help="traffic frequencies in Hz (default: all at the site)")
    ap.add_argument("--since", help="YYYY-MM-DD, inclusive, by log/recording file name")
    ap.add_argument("--until", help="YYYY-MM-DD, inclusive")
    ap.add_argument("--p25-calls", nargs="*", default=[], help="GET /api/ui/calls dumps (JSON or JSONL snapshots)")
    ap.add_argument("--p25-log", nargs="*", default=[], help="GET /api/log dumps (JSON)")
    ap.add_argument("--out", help="Markdown report path (default stdout)")
    ap.add_argument("--json", help="write the raw distributions as JSON")
    a = ap.parse_args(argv)
    a.since, a.until = [x.replace("-", "") if x else None for x in (a.since, a.until)]
    J, parts = {}, []
    if a.event_logs and a.event_logs.upper() != "NONE":
        parts.append(analyse(a, J))
    if a.p25_calls or a.p25_log:
        parts.append(p25_section(a.p25_calls, a.p25_log, J))
    rep = "\n".join(parts)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as fh:
            fh.write(rep)
    else:
        sys.stdout.write(rep)
    if a.json:
        with open(a.json, "w", encoding="utf-8") as fh:
            json.dump(J, fh, indent=1)


if __name__ == "__main__":
    main()
