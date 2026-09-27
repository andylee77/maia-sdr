"""Scoring P25 replays against SDRTrunk's decode of the same air.

Inputs recorded during a replay (all host-clock stamped):

- IMBE tap: successive ``/api/imbe_dump`` rings (last 128 raw 144-bit frames,
  oldest first, with the talkgroup context). :func:`merge_dumps` stitches them
  into one frame sequence by overlap (frames arrive in LDU batches of 9); the
  ``/api/traffic`` counter is read only at item boundaries, because that
  endpoint clears the traffic ``nid_event`` sticky bit p25-httpd's heartbeat
  uses.
- ``/api/ui/calls`` items (per-call ``imbe``, ``tg``, ``source``,
  ``started_unix_ms`` on the DUT clock).
- ``/ws/audio`` PCM chunks (8 kHz, 20 ms) for the audio-continuity check.

Truth: ``.mbe`` frames (epoch ms, raw hex). :func:`vote_offset` finds the
host-minus-air offset from identical high-entropy frames, then
:func:`match_frames` aligns each transmission's truth frames with the decoded
frames of the same window in order, a frame counting as recovered when its
Hamming distance is at most ``max_bits`` (raw codewords: our demod and
SDRTrunk's can differ in bits the IMBE FEC corrects).
"""

from __future__ import annotations

import collections
from typing import Any, Iterable

import numpy as np

FRAME_HZ = 50.0  # IMBE frames per second of voice
MAX_BITS = 16  # raw-codeword Hamming distance still counted as the same frame
RING = 128  # /api/imbe_dump ring length


# ---------------------------------------------------------------------------
# IMBE tap
# ---------------------------------------------------------------------------


def _key(f: dict[str, Any]) -> tuple[Any, str]:
    return (f.get("talkgroup"), str(f.get("hex", "")).lower())


def merge_dumps(dumps: list[dict[str, Any]], skip_first: bool = False) -> dict[str, Any]:
    """Stitch ``[{"t": host s, "frames": [...]}, ...]`` into indexed frames.

    ``skip_first``: the first dump is a baseline taken before the stream (its
    frames are older content of the ring) and only anchors the overlap.

    Returns ``{"frames": [{"i", "t", "tg", "enc", "hex"}], "gaps": n, "ambiguous": n}``.
    ``gaps`` counts polls with no overlap (more than a ring of new frames: the tap
    may have missed some); ``ambiguous`` counts polls whose shift was not unique
    (repetitive content, resolved by the expected frame rate).
    """
    out: list[dict[str, Any]] = []
    prev: list[tuple[Any, str]] = []
    prev_t: float | None = None
    last_d = 0
    gaps = amb = 0
    base = 0
    for dump in dumps:
        fr = dump.get("frames") or []
        cur = [_key(f) for f in fr]
        t = float(dump["t"])
        n = len(cur)
        if prev_t is None:
            d = n
        else:
            m = len(prev)
            if n < RING and n >= m and cur[:m] == prev:
                d = n - m  # ring not full yet: nothing was evicted
                cands = [d]
            else:
                # Shifts with at least one overlapping frame (an empty overlap always
                # "matches" and would double-count).
                cands = [d for d in range(0, n) if 0 < n - d <= m and
                         cur[:n - d] == prev[m - (n - d):]]
            if not cands:
                d = n
                gaps += bool(n)
            elif len(cands) == 1:
                d = cands[0]
            else:
                amb += 1
                want = FRAME_HZ * (t - prev_t) if last_d else 0.0
                d = min(cands, key=lambda c: (abs(c - want), c))
        if prev_t is None and skip_first:
            d = 0
        for k in range(n - d, n):
            f = fr[k]
            out.append({"i": base, "t": t, "tg": f.get("talkgroup"),
                        "enc": bool(f.get("encrypted")), "hex": str(f.get("hex", "")).lower()})
            base += 1
        last_d = d
        prev, prev_t = cur, t
    return {"frames": out, "gaps": gaps, "ambiguous": amb}


def _hexbits(h: str) -> np.ndarray:
    return np.unpackbits(np.frombuffer(bytes.fromhex(h.ljust(36, "0")[:36]), dtype=np.uint8))


def hamming(a: str, b: str) -> int:
    if a == b:
        return 0
    try:
        return int(np.count_nonzero(_hexbits(a) != _hexbits(b)))
    except ValueError:
        return 144


def vote_offset(decoded: list[dict[str, Any]], truth: Iterable[tuple[float, str]],
                max_repeat: int = 2, bin_s: float = 0.25) -> dict[str, Any] | None:
    """Host-minus-air offset from identical frames (hex seen at most ``max_repeat``
    times in the truth, so silence / tone codewords do not vote)."""
    by_hex: dict[str, list[float]] = collections.defaultdict(list)
    for t, h in truth:
        by_hex[h].append(t)
    diffs = [f["t"] - ta for f in decoded for ta in by_hex.get(f["hex"], ())
             if len(by_hex[f["hex"]]) <= max_repeat]
    if not diffs:
        return None
    d = np.asarray(diffs)
    edges = np.arange(d.min(), d.max() + bin_s * 1.01, bin_s)
    if edges.size < 2:
        return {"offset_s": float(np.median(d)), "votes": int(d.size), "spread_s": 0.0}
    h, e = np.histogram(d, bins=edges)
    k = int(np.argmax(h))
    near = d[(d >= e[k] - 1.0) & (d <= e[k + 1] + 1.0)]
    return {"offset_s": round(float(np.median(near)), 3), "votes": int(near.size),
            "spread_s": round(float(np.percentile(near, 90) - np.percentile(near, 10)), 3)}


def match_frames(truth: list[tuple[float, str]], decoded: list[dict[str, Any]],
                 max_bits: int = MAX_BITS, look: int = 60) -> dict[str, Any]:
    """In-order alignment of truth frames with decoded frames (both time sorted)."""
    i = j = 0
    matched = exact = 0
    bits: list[int] = []
    while i < len(truth) and j < len(decoded):
        best = None
        for k in range(j, min(len(decoded), j + look)):
            hd = hamming(truth[i][1], decoded[k]["hex"])
            if hd <= max_bits:  # earliest acceptable (codewords repeat in silence)
                best = (k, hd)
                break
        if best is None:
            i += 1  # truth frame not decoded (missed)
            continue
        matched += 1
        exact += best[1] == 0
        bits.append(best[1])
        i += 1
        j = best[0] + 1
    return {"truth": len(truth), "recovered": matched, "exact": exact,
            "mean_bit_diff": round(float(np.mean(bits)), 3) if bits else None}


def score_transmissions(txs: list[dict[str, Any]], truth: dict[str, list[tuple[float, str]]],
                        decoded: list[dict[str, Any]], offset_s: float, *,
                        window_pad_s: float = 1.5, use_tg: bool = True,
                        max_bits: int = MAX_BITS) -> list[dict[str, Any]]:
    """Per transmission: recovered frames (hex-matched) in its host-time window."""
    out = []
    ts = np.array([f["t"] for f in decoded]) if decoded else np.zeros(0)
    for tx in txs:
        fr = truth.get(tx["id"], [])
        lo = tx["t0"] + offset_s - window_pad_s
        hi = tx["t1"] + offset_s + window_pad_s + 1.2  # tap poll period + decode latency
        a, b = int(np.searchsorted(ts, lo)), int(np.searchsorted(ts, hi, side="right"))
        cand = decoded[a:b]
        if use_tg:
            same = [f for f in cand if f["tg"] in (tx["tg"], None, 0)]
            cand = same if same else cand
        m = match_frames(fr, cand, max_bits)
        out.append({"id": tx["id"], "call": tx["call"], "tg": tx["tg"], "src": tx["src"],
                    "freq_hz": tx["freq_hz"], "encrypted": tx["encrypted"],
                    "t0": tx["t0"], "seconds": round(tx["t1"] - tx["t0"], 2), **m,
                    "recovery_pct": round(100.0 * m["recovered"] / m["truth"], 1)
                    if m["truth"] else None})
    return out


def conflicts(txs: list[dict[str, Any]]) -> dict[str, list[str]]:
    """Transmissions overlapping in time on another frequency: a single traffic
    chain can follow only one of them."""
    out: dict[str, list[str]] = {}
    for a in txs:
        others = [b["id"] for b in txs if b is not a and b["freq_hz"] != a["freq_hz"]
                  and b["t0"] < a["t1"] and a["t0"] < b["t1"]]
        if others:
            out[a["id"]] = others
    return out


GRANT_LEAD_S = 0.5  # CC grant -> first voice frame on the traffic channel (0.45-0.6 s here)


def blockers(txs: list[dict[str, Any]], followed: dict[str, bool],
             busy: Iterable[tuple[float, float, float]] = (), lead_s: float = GRANT_LEAD_S,
             eps_s: float = 0.25) -> dict[str, list[str]]:
    """Per transmission T: the transmissions that kept a single, first-come traffic
    chain busy when T was granted (``t0 - lead_s``), so T was not followable.

    O (another frequency) blocks T when O was granted first (``eps_s`` slack for
    near-simultaneous grants), the DUT followed O (``followed``), and at T's grant
    O was still granted / in voice, or - O on another talkgroup - O's channel was
    still allocated in SDRTrunk's session: a traffic recording on O's frequency
    that holds O and runs past T's grant (``busy``: ``(freq_hz, s0, s1)`` on the
    stream clock). SDRTrunk keeps a channel through the system's hang until the
    traffic channel tears down, so with one channel it would reject T as well. A
    lock the DUT holds past that (e.g. its post-TDULC grace) blocks nothing: T
    stays followable and counts as a miss.
    """
    busy = list(busy)
    out: dict[str, list[str]] = {}
    for t in txs:
        gt = t["t0"] - lead_s
        hit = []
        for o in txs:
            if o is t or o["freq_hz"] == t["freq_hz"] or not followed.get(o["id"]):
                continue
            if o["t0"] - lead_s > gt + eps_s:
                continue  # T was granted first
            if gt < o["t1"] or (o["tg"] != t["tg"] and any(
                    f == o["freq_hz"] and s0 <= o["t0"] and o["t1"] <= s1 and gt < s1
                    for f, s0, s1 in busy)):
                hit.append(o["id"])
        if hit:
            out[t["id"]] = hit
    return out


# ---------------------------------------------------------------------------
# p25-httpd calls
# ---------------------------------------------------------------------------


def merge_calls(snapshots: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    """Unique ``/api/ui/calls`` items by call_id (the latest snapshot wins)."""
    calls: dict[Any, dict[str, Any]] = {}
    for snap in snapshots:
        items = snap.get("items") if isinstance(snap, dict) else snap
        for it in items or []:
            if isinstance(it, dict) and it.get("call_id") is not None:
                calls[it["call_id"]] = it
    return sorted(calls.values(), key=lambda c: c.get("started_unix_ms") or 0)


def match_calls(calls: list[dict[str, Any]], txs: list[dict[str, Any]], dut_minus_air_s: float,
                tol_s: float = 3.0) -> dict[str, Any]:
    """Assign each truth transmission to the p25-httpd call (same TG) whose open
    interval covers it; per call the truth frames vs the call's own ``imbe``."""
    assigned: dict[Any, list[str]] = collections.defaultdict(list)
    unmatched = []
    for tx in txs:
        a0 = tx["t0"] + dut_minus_air_s
        a1 = tx["t1"] + dut_minus_air_s
        best, best_ov = None, -1e9
        for c in calls:
            if c.get("tg") != tx["tg"] or c.get("started_unix_ms") is None:
                continue
            c0 = c["started_unix_ms"] / 1000.0
            c1 = c0 + max(float(c.get("open_ms") or 0), float(c.get("voice_ms") or 0)) / 1000.0
            ov = min(a1, c1 + tol_s) - max(a0, c0 - tol_s)
            if ov > best_ov:
                best, best_ov = c, ov
        if best is not None and best_ov > 0:
            assigned[best["call_id"]].append(tx["id"])
        else:
            unmatched.append(tx["id"])
    byid = {t["id"]: t for t in txs}
    rows = []
    for c in calls:
        ids = assigned.get(c["call_id"], [])
        tf = sum(byid[i]["frames"] for i in ids)
        rows.append({"call_id": c["call_id"], "tg": c.get("tg"), "source": c.get("source"),
                     "freq_hz": c.get("freq_hz"), "imbe": c.get("imbe"), "ldu": c.get("ldu"),
                     "voice_ms": c.get("voice_ms"), "close_reason": c.get("close_reason"),
                     "not_followed": c.get("not_followed"), "encrypted": c.get("encrypted"),
                     "truth_transmissions": ids, "truth_frames": tf,
                     "imbe_vs_truth_pct": round(100.0 * float(c.get("imbe") or 0) / tf, 1)
                     if tf else None})
    return {"calls": rows, "unmatched_transmissions": unmatched,
            "close_reasons": dict(collections.Counter(str(c.get("close_reason")) for c in calls))}


def _src_ok(c: dict[str, Any], tx: dict[str, Any]) -> bool:
    srcs = [s for s in [c.get("source"), *(c.get("sources") or [])] if s]
    return not tx.get("src") or not srcs or tx["src"] in srcs


def _call_span(c: dict[str, Any]) -> tuple[float, float]:
    t0 = float(c["started_unix_ms"]) / 1000.0
    dur = max(float(c.get("open_ms") or 0),
              float(c.get("first_voice_ms") or 0) + float(c.get("voice_ms") or 0)) / 1000.0
    return t0, t0 + dur


def _freq_ok(c: dict[str, Any], tx: dict[str, Any]) -> bool:
    cf, tf = c.get("freq_hz"), tx.get("freq_hz")
    return not cf or not tf or abs(float(cf) - float(tf)) < 1000.0


MIN_COVER_S = 0.5  # a call covers a transmission: this much in common (or half of it)


def score_by_calls(txs: list[dict[str, Any]], calls: list[dict[str, Any]],
                   dut_minus_stream: float | None = None, tol_s: float = 3.0,
                   prior: float | None = None) -> dict[str, Any]:
    """Primary score: p25-httpd's own per-call counts (``/api/ui/calls`` ``imbe``,
    exact per call_id since 057) matched to the truth transmissions.

    ``txs``: truth transmissions with ``t0``/``t1`` on the stream clock. The DUT
    clock offset is voted from (TG, source)-matched call starts unless given
    (``prior``: see :func:`vote_call_offset`). A transmission goes to every
    same-TG call on its frequency whose open interval covers it (``MIN_COVER_S``,
    or half of a shorter transmission), whatever source the call carries:
    p25-httpd stamps the grant's source, SDRTrunk the talker it kept for the
    channel, and the two differ on console grants and talker changes; the DUT
    may also split one transmission over two calls. Without such a call it falls
    back to the same-TG, same-source call within ``tol_s``, credited only from
    what the covering matches left. A call's ``imbe`` is shared by its
    transmissions in time order, each credited up to its truth frame count
    (``excess`` keeps the rest: frames SDRTrunk did not have).
    """
    calls = sorted((c for c in calls if c.get("started_unix_ms")),
                   key=lambda c: c["started_unix_ms"])
    off = dut_minus_stream if dut_minus_stream is not None else vote_call_offset(
        calls, txs, prior=prior)
    if off is None:
        return {"offset_s": None, "rows": {t["id"]: {"call_id": None, "recovered": 0,
                                                       "ldu": 0} for t in txs},
                "calls": [], "unmatched_calls": len(calls)}
    cover: dict[Any, list[dict[str, Any]]] = collections.defaultdict(list)
    near: dict[Any, list[dict[str, Any]]] = collections.defaultdict(list)
    rows: dict[str, dict[str, Any]] = {}
    for tx in sorted(txs, key=lambda t: t["t0"]):
        a0, a1 = tx["t0"] + off, tx["t1"] + off
        need = min(MIN_COVER_S, 0.5 * (a1 - a0))
        hits = []
        for c in calls:
            if c.get("tg") != tx["tg"] or not _freq_ok(c, tx):
                continue
            c0, c1 = _call_span(c)
            ov = min(a1, c1) - max(a0, c0)
            if ov > 0 and ov >= need:
                hits.append((ov, c))
        if hits:
            best = max(hits, key=lambda h: (_src_ok(h[1], tx), h[0]))[1]
            for _, c in hits:
                cover[c["call_id"]].append(tx)
            matched = [c for _, c in hits]
        else:
            best, best_ov = None, 0.0
            for c in calls:
                if c.get("tg") != tx["tg"] or not _src_ok(c, tx) or not _freq_ok(c, tx):
                    continue
                c0, c1 = _call_span(c)
                ov = min(a1, c1 + tol_s) - max(a0, c0 - tol_s)
                if ov > best_ov:
                    best, best_ov = c, ov
            if best is not None:
                near[best["call_id"]].append(tx)
            matched = [best] if best is not None else []
        rows[tx["id"]] = {"call_id": best["call_id"] if best else None,
                          "call_ids": [c["call_id"] for c in matched],
                          "match": ("cover" if hits else "near") if best else None,
                          # The chain was on it: a followed call covers it in time.
                          "followed": any(c.get("not_followed") is None and not c.get("encrypted")
                                          for _, c in hits),
                          "recovered": 0, "ldu": 0}
    left = {c["call_id"]: int(c.get("imbe") or 0) for c in calls}
    left_ldu = {c["call_id"]: int(c.get("ldu") or 0) for c in calls}
    for assigned in (cover, near):
        for c in calls:
            cid = c["call_id"]
            for tx in assigned.get(cid, []):
                r = rows[tx["id"]]
                got = min(left[cid], int(tx["frames"]) - r["recovered"])
                ldu = min(left_ldu[cid], -(-int(tx["frames"]) // 9) - r["ldu"])
                r["recovered"] += got
                r["ldu"] += ldu
                left[cid] -= got
                left_ldu[cid] -= ldu
    out_calls = []
    for c in calls:
        mine = cover.get(c["call_id"], []) + near.get(c["call_id"], [])
        left_c = left[c["call_id"]]
        tf = sum(int(t["frames"]) for t in mine)
        out_calls.append({"call_id": c["call_id"], "tg": c.get("tg"), "source": c.get("source"),
                          "freq_hz": c.get("freq_hz"), "imbe": c.get("imbe"), "ldu": c.get("ldu"),
                          "voice_ms": c.get("voice_ms"), "open_ms": c.get("open_ms"),
                          "close_reason": c.get("close_reason"),
                          "not_followed": c.get("not_followed"), "encrypted": c.get("encrypted"),
                          "truth_transmissions": [t["id"] for t in mine], "truth_frames": tf,
                          "excess": left_c if mine else None,
                          "imbe_vs_truth_pct": round(100.0 * float(c.get("imbe") or 0) / tf, 1)
                          if tf else None})
    return {"offset_s": round(off, 3), "rows": rows, "calls": out_calls,
            "unmatched_calls": sum(1 for c in out_calls if not c["truth_transmissions"])}


def align_bits(truth: list[tuple[float, str]], tapped: list[dict[str, Any]],
               max_bits: int = 24, look: int = 60) -> dict[str, Any]:
    """Secondary bit-accuracy check: truth frames (time order) aligned in order
    with the tapped frames (arrival order). Two receivers' raw codewords differ
    in the bits the IMBE FEC corrects; unrelated frames differ in ~72 bits."""
    i = j = 0
    matched: list[int] = []
    bits: list[int] = []
    while i < len(truth) and j < len(tapped):
        # Earliest acceptable match, not the closest: silence / tone codewords repeat,
        # and jumping to the best of several near-identical frames loses the order.
        best = None
        for k in range(j, min(len(tapped), j + look)):
            hd = hamming(truth[i][1], tapped[k]["hex"])
            if hd <= max_bits:
                best = (k, hd)
                break
        if best is None:
            i += 1
            continue
        matched.append(i)
        bits.append(best[1])
        i += 1
        j = best[0] + 1
    return {"truth": len(truth), "aligned": len(matched), "matched_idx": matched,
            "exact": sum(1 for b in bits if b == 0),
            "mean_bit_diff": round(float(np.mean(bits)), 3) if bits else None,
            "bits": bits}


PRIOR_WINDOW_S = (-3.0, 1.0)  # vote minus prior: -2.0..-0.5 s on the bench (2026-09-27)


def vote_call_offset(calls: list[dict[str, Any]], txs: list[dict[str, Any]],
                     bin_s: float = 0.5, prior: float | None = None,
                     window: tuple[float, float] = PRIOR_WINDOW_S) -> float | None:
    """DUT-clock minus stream/air-time offset from (TG, source)-matched call starts.

    ``prior``: the offset from the DUT clock sampled at the item and the stream
    start (host clock); only differences within ``window`` of it vote when any
    do. With a handful of calls every (call, same-source transmission) pair votes
    once, and a wrong pair can win (bench 2026-09-27: two items 5.3 s off)."""
    d = [c["started_unix_ms"] / 1000.0 - t["t0"] for c in calls for t in txs
         if c.get("started_unix_ms") and c.get("tg") == t["tg"] and _src_ok(c, t)]
    if prior is not None:
        d = [x for x in d if prior + window[0] <= x <= prior + window[1]] or d
    if not d:
        return None
    # Densest bin_s-wide window, not a histogram over the whole span: one call
    # stamped before the DUT clock was set spreads the differences over decades.
    arr = np.sort(np.asarray(d))
    counts = np.searchsorted(arr, arr + bin_s, side="right") - np.arange(arr.size)
    lo = float(arr[int(np.argmax(counts))])
    near = arr[(arr >= lo - 1.5) & (arr <= lo + bin_s + 1.5)]
    return float(np.median(near))


# ---------------------------------------------------------------------------
# Audio (tone continuity)
# ---------------------------------------------------------------------------


def tone_reference(pcm: np.ndarray, fs: float = 8000.0) -> dict[str, Any]:
    """Tone frequencies of a reference decode (e.g. SDRTrunk's MP3): the peaks of
    the per-20 ms dominant frequency over loud, tonal frames."""
    from .p25_dsp import tone_frames

    fr = tone_frames(np.asarray(pcm, dtype=float), fs)
    if not fr:
        return {"seconds": 0.0, "tones_hz": [], "tonal_frames": 0, "frames": 0}
    peak = max(f["rms"] for f in fr)
    loud = [f for f in fr if f["rms"] > 0.25 * peak]
    bins = collections.Counter(int(round(f["freq_hz"] / 10.0)) * 10 for f in loud)
    tones = []
    for b, n in bins.most_common():
        if n < 3 or any(abs(b - t) <= 30 for t in tones):
            continue
        tones.append(b)
    tones = sorted(tones[:4])
    refined = []
    for t in tones:
        near = [f["freq_hz"] for f in loud if abs(f["freq_hz"] - t) <= 30]
        refined.append(round(float(np.median(near)), 1))
    tonal = sum(1 for f in loud if any(abs(f["freq_hz"] - t) <= 20 for t in refined))
    return {"seconds": round(len(pcm) / fs, 3), "tones_hz": refined, "tonal_frames": tonal,
            "frames": len(fr)}


def tone_continuity(chunks: list[dict[str, Any]], tones_hz: list[float], *,
                    tol_hz: float = 20.0, min_run: int = 5, max_gap: int = 25,
                    fs: float = 8000.0) -> dict[str, Any]:
    """Tone stability and dropouts in received ``/ws/audio`` chunks.

    ``chunks``: ``[{"t": host s, "pcm": np.ndarray[int16] (160 samples)}]`` in
    arrival order. The tone span is the longest run of frames near a reference
    tone, bridging non-tone gaps of up to ``max_gap`` frames (0.5 s); inside it,
    frames far from every tone or quieter than 10 % of the median tone RMS are
    dropouts.
    """
    from .p25_dsp import tone_frames

    if not chunks or not tones_hz:
        return {"found": False, "frames": 0}
    pcm = np.concatenate([np.asarray(c["pcm"], dtype=float) for c in chunks])
    fr = tone_frames(pcm, fs)
    near = [min(abs(f["freq_hz"] - t) for t in tones_hz) <= tol_hz and f["rms"] > 50 for f in fr]
    best = (0, -1)
    i = 0
    while i < len(fr):
        if not near[i]:
            i += 1
            continue
        j, bad = i, 0
        while j + 1 < len(fr) and (near[j + 1] or bad < max_gap):
            bad = 0 if near[j + 1] else bad + 1
            j += 1
        while j > i and not near[j]:
            j -= 1
        if sum(near[i:j + 1]) > sum(near[best[0]:best[1] + 1]) if best[1] >= 0 else True:
            best = (i, j)
        i = j + 1
    if best[1] < 0 or best[1] - best[0] + 1 < min_run:
        return {"found": False, "frames": len(fr)}
    span = fr[best[0]:best[1] + 1]
    tone_rms = float(np.median([f["rms"] for f in span if near[f["k"]]]))
    dev = [min(abs(f["freq_hz"] - t) for t in tones_hz) for f in span]
    drop = [f["k"] for f, dv in zip(span, dev) if dv > tol_hz or f["rms"] < 0.1 * tone_rms]
    on = [f for f, dv in zip(span, dev) if dv <= tol_hz]
    per_tone = {}
    for t in tones_hz:
        fs_t = [f["freq_hz"] for f in on if abs(f["freq_hz"] - t) <= tol_hz]
        if fs_t:
            per_tone[str(t)] = {"frames": len(fs_t), "mean_hz": round(float(np.mean(fs_t)), 2),
                                "std_hz": round(float(np.std(fs_t)), 2),
                                "max_dev_hz": round(float(max(abs(x - t) for x in fs_t)), 2)}
    arrivals = [c["t"] for c in chunks[best[0]:best[1] + 1]]
    gaps = np.diff(arrivals) if len(arrivals) > 1 else np.zeros(0)
    return {"found": True, "frames": len(fr), "tone_frames": len(span),
            "tone_s": round(len(span) * 0.02, 2), "dropouts": len(drop),
            "dropout_frames": drop[:50], "per_tone": per_tone,
            "rms_cv": round(float(np.std([f["rms"] for f in on]) / max(tone_rms, 1e-9)), 3)
            if on else None,
            "max_arrival_gap_ms": round(float(gaps.max()) * 1000, 1) if gaps.size else None,
            "start_t": chunks[best[0]]["t"] if best[0] < len(chunks) else None}
