"""P25 replay corpus: inventory, alignment and replay plans (``fbench.p25corpus/1``).

Built by ``tools/p25_corpus_index.py``; read by ``rf.p25_corpus``. Two modes:

- **A** real wideband air (``my_captures``): whole captures single-pass from the
  TX board's SD card through ``fbench-agent replay stream`` (``cs12``, lossless
  for the 12-bit AD9361 samples), or short windows from RAM.
- **B** synthetic full system: a control-channel recording plus the traffic
  recordings that overlap it, each up-converted from 50 kSPS to its RF offset
  and mixed. Every channel recording starts at its ``decoded_messages`` bit-clock
  origin + 10..13 ms (checked by frame-sync matching on every CC and a sample of
  traffic recordings), and ``.mbe`` frame times agree with the log LDU times to
  +-27 ms (p10..p90 of 332 transmissions), so CC-to-traffic alignment is
  ~+-30 ms. Recordings without a log fall back to the 1 s file-name stamp
  (``align: name``).
"""

from __future__ import annotations

import collections
import time
from pathlib import Path
from typing import Any

from . import sdrtrunk as st

SCHEMA = "fbench.p25corpus/1"
UPLOAD_MBS = 9.0  # host -> B, measured (USB to A, then GbE)
SEG_BYTES = 1 << 30  # FAT32 caps files at 4 GiB; 1 GiB segments upload/resume well
LOG_TO_WAV_S = 0.012  # wav sample 0 = log bit 0 + this (frame-sync matching)
MBE_LAG_S = 0.186  # .mbe frame time - LDU start on the log clock (median)
BYTES_PER_SAMPLE = {"cs16": 4, "cs12": 3, "cs8": 2}
DEFAULT_FOCUS = ("20260503_091125_858437500_1_300_1014",)


def _pct(xs: list[Any]) -> dict[str, float] | None:
    import numpy as np

    v = [float(x) for x in xs if x is not None]
    if not v:
        return None
    return {"p10": round(float(np.percentile(v, 10)), 1), "median": round(float(np.median(v)), 1),
            "p90": round(float(np.percentile(v, 90)), 1)}


def _band_ok(freq: float, centre: float, rate: float, guard: float = 0.08) -> bool:
    return abs(freq - centre) <= rate / 2 * (1 - guard)


def _cov(txs: list[dict[str, Any]], seconds: float, fmt: str, rate: float) -> dict[str, Any]:
    clear = [t for t in txs if not t["encrypted"]]
    nbytes = int(seconds * rate) * BYTES_PER_SAMPLE[fmt]
    return {
        "transmissions": len(txs), "clear": len(clear), "encrypted": len(txs) - len(clear),
        "calls": len({t["call"] for t in txs}), "calls_clear": len({t["call"] for t in clear}),
        "frames_clear": sum(t["frames"] for t in clear), "frames": sum(t["frames"] for t in txs),
        "tgs": dict(collections.Counter(str(t["tg"]) for t in txs).most_common()),
        "stream_s": round(seconds, 1), "format": fmt, "rate_hz": rate, "bytes": nbytes,
        "upload_min": round(nbytes / UPLOAD_MBS / 1e6 / 60, 1),
    }


# ---------------------------------------------------------------------------
# Alignment of channel recordings to their decoded_messages logs
# ---------------------------------------------------------------------------


def align_recordings(recs: list[dict[str, Any]], logs: list[dict[str, Any]]) -> None:
    """Set ``start_unix`` / ``align`` / ``log`` on every channel recording."""
    for r in recs:
        cands = [lg for lg in logs if lg["freq_hz"] == r["freq_hz"]
                 and lg["traffic"] == (r["kind"] == "traffic")
                 and -0.5 <= lg["t_name"] - r["t_name"] <= 1.5]
        r["start_unix"] = r["t_name"] + 0.5
        r["align"] = "name"
        r["align_err_s"] = 0.5
        r["log"] = None
        if not cands:
            continue
        lg = min(cands, key=lambda c: abs(c["t_name"] - r["t_name"] - 0.5))
        clk = st.log_clock(lg["path"])
        if clk is None:
            continue
        r["log"] = lg["file"]
        r["start_unix"] = round(clk["bit0_unix"] + LOG_TO_WAV_S, 4)
        r["align"] = "log"
        r["align_err_s"] = 0.03


def measure_offsets(recs: list[dict[str, Any]], recordings: str | Path,
                    max_s: float = 10.0) -> None:
    """``cfo_hz`` (own estimate) and ``correct_hz`` (what the mixer removes) per recording.

    SDRTrunk's frequency correction differed by session: its CC recordings sit at
    +110..+140 Hz (2026-04-18 .. 05-02 09:47), +446..+469 Hz (05-02 10:01..10:40, unit
    A's uncorrected +472 Hz) and -6..-27 Hz (from 05-02 10:50). A traffic recording is
    corrected by the offset of the CC session that covers it (scaled to its frequency),
    which is cleaner than its own short, bursty estimate; else by its own estimate.
    """
    import numpy as np

    from .p25_dsp import carrier_offset

    root = Path(recordings)
    for r in recs:
        try:
            info = st.wav_info(root / r["file"])
            n = min(info.frames, int(max_s * info.rate))
            a = np.fromfile(root / r["file"], dtype="<i2", count=2 * n,
                            offset=info.data_offset).astype(np.float64)
            cfo = carrier_offset(a[0::2] + 1j * a[1::2], info.rate)
        except (OSError, ValueError):
            cfo = None
        r["cfo_hz"] = round(cfo, 1) if cfo is not None else None
    ccs = [r for r in recs if r["kind"] == "cc" and r["cfo_hz"] is not None]
    for r in recs:
        if r["kind"] == "cc":
            r["correct_hz"], r["correct_from"] = r["cfo_hz"] or 0.0, "own"
            continue
        c = next((x for x in ccs if x["start_unix"] <= r["start_unix"] <=
                  x["start_unix"] + x["seconds"]), None)
        if c is not None:
            r["correct_hz"] = round(c["cfo_hz"] * r["freq_hz"] / c["freq_hz"], 1)
            r["correct_from"] = c["id"]
        else:
            r["correct_hz"], r["correct_from"] = r["cfo_hz"] or 0.0, "own"


def _inside(tx: dict[str, Any], t0: float, t1: float, lead: float = 0.0, tail: float = 0.0
            ) -> bool:
    return tx["t0"] >= t0 + lead and tx["t1"] <= t1 - tail


def recording_transmissions(rec: dict[str, Any], txs: list[dict[str, Any]]) -> list[str]:
    """Transmissions on the recording's frequency inside its span (.mbe times lag the
    air by ~0.19 s)."""
    t0 = rec["start_unix"]
    t1 = t0 + rec["seconds"]
    return [t["id"] for t in txs if t["freq_hz"] == rec["freq_hz"]
            and t["t0"] - MBE_LAG_S >= t0 - 0.05 and t["t1"] - MBE_LAG_S <= t1 + 0.1]


# ---------------------------------------------------------------------------
# Mode A: wideband captures
# ---------------------------------------------------------------------------


def plan_a(captures: list[dict[str, Any]], txs: list[dict[str, Any]], *, fmt: str = "cs12",
           lead_s: float = 4.0, window_s: float = 40.0, min_seconds: float = 5.0
           ) -> dict[str, Any]:
    caps = []
    covered: dict[str, dict[str, Any]] = {}
    for c in captures:
        if c["seconds"] < min_seconds:
            continue
        s, e = c["start_unix"], c["start_unix"] + c["seconds"]
        inband, oob, partial = [], [], []
        for t in txs:
            if t["t1"] <= s or t["t0"] >= e:
                continue
            if not _band_ok(t["freq_hz"], c["centre_hz"], c["rate_hz"]):
                oob.append(t["id"])
            elif _inside(t, s, e, lead_s, 0.5):
                inband.append(t["id"])
                covered[t["id"]] = t
            else:
                partial.append(t["id"])
        nsamp = c["data_bytes"] // 4
        per_seg = SEG_BYTES // BYTES_PER_SAMPLE[fmt]
        segs = [{"index": k, "sample0": k * per_seg, "samples": min(per_seg, nsamp - k * per_seg)}
                for k in range((nsamp + per_seg - 1) // per_seg)]
        byid_ = {t["id"]: t for t in txs}
        caps.append({**c, "samples": nsamp, "segments": segs, "transmissions": inband,
                     "clear": sum(not byid_[i]["encrypted"] for i in inband),
                     "encrypted": sum(byid_[i]["encrypted"] for i in inband),
                     "tgs": dict(collections.Counter(str(byid_[i]["tg"]) for i in inband)),
                     "partial": partial, "out_of_band": oob, "lead_s": lead_s})
    byid = {t["id"]: t for t in txs}
    windows = []
    for c in caps:
        left = [byid[i] for i in c["transmissions"] if not byid[i]["encrypted"]]
        s0, e0 = c["start_unix"], c["start_unix"] + c["seconds"]
        while left:
            best = None
            for t in left:
                ws = max(s0, t["t0"] - lead_s)
                we = min(e0, ws + window_s)
                inside = [x for x in left if _inside(x, ws, we, lead_s if ws > s0 else lead_s,
                                                      0.5)]
                if best is None or len(inside) > len(best[2]):
                    best = (ws, we, inside)
            if best is None or not best[2]:
                break
            ws, we, inside = best
            ids = [x["id"] for x in inside]
            windows.append({"id": f"{c['id']}@{ws - s0:.0f}", "capture": c["id"],
                            "start_s": round(ws - s0, 1), "seconds": round(we - ws, 1),
                            "transmissions": ids})
            left = [x for x in left if x["id"] not in ids]
    truthed = [c for c in caps if c["transmissions"]]  # what items=all plays
    total_s = sum(c["seconds"] for c in truthed)
    win_s = sum(w["seconds"] for w in windows)
    win_txs = [byid[i] for w in windows for i in w["transmissions"]]
    rate = caps[0]["rate_hz"] if caps else 4e6
    whole = _cov(list(covered.values()), total_s, fmt, rate)
    whole.update(bytes=sum(c["samples"] for c in truthed) * BYTES_PER_SAMPLE[fmt],
                 captures=len(truthed), captures_all=len(caps),
                 bytes_all=sum(c["samples"] for c in caps) * BYTES_PER_SAMPLE[fmt])
    whole["upload_min"] = round(whole["bytes"] / UPLOAD_MBS / 1e6 / 60, 1)
    return {"format": fmt, "lead_s": lead_s, "window_s": window_s, "captures": caps,
            "windows": windows,
            "coverage": {"whole": whole, "windows": _cov(win_txs, win_s, fmt, rate)}}


# ---------------------------------------------------------------------------
# Mode B: synthetic full system from channel recordings
# ---------------------------------------------------------------------------


def plan_b(recs: list[dict[str, Any]], txs: list[dict[str, Any]], *, fmt: str = "cs8",
           max_rate_hz: float = 5e6, pre_s: float = 12.0, post_s: float = 4.0,
           merge_s: float = 20.0, focus: tuple[str, ...] = ()) -> dict[str, Any]:
    byid = {t["id"]: t for t in txs}
    ccs = [r for r in recs if r["kind"] == "cc"]
    traffic = [r for r in recs if r["kind"] == "traffic"]
    scenes = []
    dropped: list[str] = []
    for cc in ccs:
        c0, c1 = cc["start_unix"], cc["start_unix"] + cc["seconds"]
        over = []
        for r in sorted((r for r in traffic if r["start_unix"] < c1 and
                         r["start_unix"] + r["seconds"] > c0), key=lambda r: r["start_unix"]):
            if band_rate([cc["freq_hz"], r["freq_hz"]])[1] > max_rate_hz:
                dropped.append(r["id"])  # too far from the CC for one TX band
            else:
                over.append(r)
        clusters: list[list[dict[str, Any]]] = []
        for r in over:
            if clusters and r["start_unix"] - pre_s <= max(
                    x["start_unix"] + x["seconds"] for x in clusters[-1]) + post_s + merge_s:
                clusters[-1].append(r)
            else:
                clusters.append([r])
        for cl in clusters:
            t0 = max(c0, cl[0]["start_unix"] - pre_s)
            t1 = min(c1, max(x["start_unix"] + x["seconds"] for x in cl) + post_s)
            if t1 - t0 < 5.0:
                continue
            ids = sorted({i for r in cl for i in r["transmissions"]
                          if byid[i]["t0"] - MBE_LAG_S >= t0 + 3.0 and
                          byid[i]["t1"] - MBE_LAG_S <= t1},
                         key=lambda i: byid[i]["t0"])
            if not ids:
                continue  # no .mbe truth in this cluster
            srcs = [{"rec": cc["id"], "file": cc["file"], "kind": "cc", "freq_hz": cc["freq_hz"],
                     "start_unix": cc["start_unix"], "seconds": cc["seconds"],
                     "align": cc["align"], "correct_hz": cc.get("correct_hz", 0.0)}]
            srcs += [{"rec": r["id"], "file": r["file"], "kind": "traffic",
                      "freq_hz": r["freq_hz"], "start_unix": r["start_unix"],
                      "seconds": r["seconds"], "align": r["align"],
                      "correct_hz": r.get("correct_hz", 0.0)} for r in cl]
            freqs = [s["freq_hz"] for s in srcs]
            centre, rate = band_rate(freqs)
            scenes.append({
                "id": f"B_{cc['id'][:15]}_{t0 - c0:.0f}", "cc": cc["id"], "t0": round(t0, 3),
                "t1": round(t1, 3), "seconds": round(t1 - t0, 3), "centre_hz": centre,
                "rate_hz": rate, "image_clearance_hz": image_clearance(freqs, centre),
                "sources": srcs,
                "transmissions": ids, "clear": sum(not byid[i]["encrypted"] for i in ids),
                "align_max_err_s": max(0.03 if s["align"] == "log" else 1.0 for s in srcs),
                "focus": any(byid[i]["call"] in focus for i in ids),
            })
    scenes.sort(key=lambda s: (not s["focus"], s["t0"]))
    cov_txs = [byid[i] for s in scenes for i in s["transmissions"]]
    nbytes = sum(int(s["seconds"] * s["rate_hz"]) * BYTES_PER_SAMPLE[fmt] for s in scenes)
    cov = _cov(cov_txs, sum(s["seconds"] for s in scenes), fmt, max_rate_hz)
    cov.update(bytes=nbytes, upload_min=round(nbytes / UPLOAD_MBS / 1e6 / 60, 1),
               rate_hz="per scene (" + ", ".join(
                   f"{r / 1e6:g}" for r in sorted({s["rate_hz"] for s in scenes})) + " MSPS)")
    return {"format": fmt, "max_rate_hz": max_rate_hz, "pre_s": pre_s,
            "post_s": post_s, "scenes": scenes, "dropped_out_of_band": dropped,
            "coverage": cov,
            "alignment": {"method": "decoded_messages bit clock + 12 ms",
                          "residual_s": 0.03,
                          "name_only_sources": sum(1 for s in scenes for x in s["sources"]
                                                   if x["align"] != "log")}}


SYNTH_RATES = (3e6, 3.5e6, 4e6, 5e6, 6e6)  # multiples of the 50 kSPS channel rate
USABLE = 0.40  # channel centres within +-0.40 fs: inside the AD9361 TX interpolator passband
IMAGE_CLEAR_HZ = 100e3


def image_clearance(freqs: list[float], centre: float) -> float:
    """Smallest distance from a channel to a TX image (``2 c - f``) or the LO."""
    fs_ = sorted({float(f) for f in freqs})
    return min(abs(2 * centre - a - b) for a in fs_ for b in fs_)


def band_rate(freqs: list[float], rates: tuple[float, ...] = SYNTH_RATES,
              usable: float = USABLE, min_clear: float = IMAGE_CLEAR_HZ) -> tuple[float, float]:
    """(TX centre, rate) for a synthetic stream.

    The lowest rate whose +-``usable`` fs holds every channel (+-12.5 kHz), with the
    centre chosen so the TX IQ image of every channel (mirrored around the LO at
    ``2 c - f``) and the LO leakage stay at least ``min_clear`` from every channel: a
    centre midway between two channels would put each one's image on the other.
    """
    fs_ = sorted({float(f) for f in freqs})
    lo, hi = fs_[0], fs_[-1]
    mid = (lo + hi) / 2

    def clearance(c: float) -> float:
        return min(abs(2 * c - a - b) for a in fs_ for b in fs_)
    fallback = None
    for r in rates:
        half = usable * r - 12.5e3
        c_min, c_max = hi - half, lo + half
        if c_min > c_max:
            continue
        grid = [round(c / 1e3) * 1e3 for c in
                [c_min + k * 5e3 for k in range(int((c_max - c_min) / 5e3) + 1)]] or [mid]
        ok = [c for c in grid if clearance(c) >= min_clear]
        if ok:
            return min(ok, key=lambda c: abs(c - mid)), r
        if fallback is None:
            fallback = (max(grid, key=clearance), r)
    return fallback or (round(mid / 1e3) * 1e3, rates[-1])


# ---------------------------------------------------------------------------
# Manifest
# ---------------------------------------------------------------------------


def build_manifest(captures_dir: str | Path, recordings_dir: str | Path,
                   event_logs_dir: str | Path, *, cc_freq: int = 860962500, nac: str = "8A1",
                   focus: tuple[str, ...] = DEFAULT_FOCUS, a_format: str = "cs12",
                   b_format: str = "cs8",
                   plan_kw: dict[str, dict[str, Any]] | None = None) -> dict[str, Any]:
    """``plan_kw``: per-mode keyword overrides, e.g. ``{"B": {"pre_s": 3.0}}``."""
    kw = plan_kw or {}
    t_start = time.time()
    calls, txs = st.scan_mbe(recordings_dir)
    caps = st.scan_captures(captures_dir) if Path(captures_dir).is_dir() else []
    recs = st.scan_channel_recordings(recordings_dir, cc_freq)
    logs = st.scan_logs(event_logs_dir)
    align_recordings(recs, logs)
    measure_offsets(recs, recordings_dir)
    mp3s = st.scan_mp3(recordings_dir)
    for r in recs:
        r["transmissions"] = recording_transmissions(r, txs)
        e = r["start_unix"] + r["seconds"]
        r["mp3"] = [m["file"] for m in mp3s if r["kind"] == "traffic" and
                    r["start_unix"] - 1 <= m["t_name"] <= e + 2 and m["channel"] == r["channel"]]
    # CC recordings -> overlapping traffic recordings
    for r in recs:
        if r["kind"] == "cc":
            c0, c1 = r["start_unix"], r["start_unix"] + r["seconds"]
            r["overlapping_traffic"] = [x["id"] for x in recs if x["kind"] == "traffic" and
                                        x["start_unix"] < c1 and x["start_unix"] + x["seconds"] > c0]
    plans = {
        "A": plan_a(caps, txs, fmt=a_format, **kw.get("A", {})),
        "B": plan_b(recs, txs, fmt=b_format, focus=focus, **kw.get("B", {})),
    }
    in_rec = {i for r in recs for i in r["transmissions"]}
    summary = {
        "mbe_calls": len(calls), "transmissions": len(txs),
        "transmissions_clear": sum(not t["encrypted"] for t in txs),
        "frames": sum(t["frames"] for t in txs), "captures": len(caps),
        "capture_seconds": round(sum(c["seconds"] for c in caps), 1),
        "capture_bytes": sum(c["data_bytes"] for c in caps),
        "channel_recordings": len(recs), "cc_recordings": sum(r["kind"] == "cc" for r in recs),
        "traffic_recordings": sum(r["kind"] == "traffic" for r in recs),
        "traffic_with_truth": sum(1 for r in recs if r["kind"] == "traffic" and
                                  r["transmissions"]),
        "log_aligned": sum(r["align"] == "log" for r in recs),
        "offset_hz_cc": _pct([r["cfo_hz"] for r in recs if r["kind"] == "cc"]),
        "offset_hz_traffic": _pct([r["cfo_hz"] for r in recs if r["kind"] == "traffic"]),
        "transmissions_in_channel_recordings": len(in_rec),
        "bits_files": sum(1 for r in recs if r["bits"]),
        "mp3_files": len(mp3s),
        "decoded_messages_logs": len(logs),
        "modes": {"A_whole": plans["A"]["coverage"]["whole"],
                  "A_windows": plans["A"]["coverage"]["windows"],
                  "B": plans["B"]["coverage"]},
    }
    return {
        "schema": SCHEMA, "generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "build_s": round(time.time() - t_start, 1),
        "sources": {"captures": str(captures_dir), "recordings": str(recordings_dir),
                    "event_logs": str(event_logs_dir)},
        "site": {"nac": nac, "cc_freq_hz": cc_freq}, "focus": list(focus),
        "summary": summary, "calls": calls, "transmissions": txs, "recordings": recs,
        "plans": plans,
    }


def load_manifest(path: str | Path) -> dict[str, Any]:
    import json

    doc = json.loads(Path(path).read_text(encoding="utf-8"))
    if doc.get("schema") != SCHEMA:
        raise ValueError(f"{path}: not a {SCHEMA} manifest")
    return doc


def tx_index(manifest: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {t["id"]: t for t in manifest["transmissions"]}


def rec_index(manifest: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {r["id"]: r for r in manifest["recordings"]}
