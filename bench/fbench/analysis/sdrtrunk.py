"""SDRTrunk recordings and event logs: names, ``.mbe`` truth, WAV headers, log clocks.

File names use local time (``YYYYMMDD_HHMMSS``), captures use unix seconds:

- wideband capture  ``<unix>_<centreHz>_<rateHz>_baseband.wav`` (2 x int16)
- channel baseband  ``<date>_<time>_<freq>_<system>_<site>_[T-]LCN-<n>_<k>_baseband.wav``
- demodulated bits  ``<date>_<time>_<freq>_9600BPS_APCO25PHASE1_..._[T-]LCN-<n>_<k>.bits``
- per-call IMBE     ``<date>_<time>_<freq>_<seq>_<to>_<from>[_encrypted].mbe`` (JSON)
- per-call audio    ``<date>_<time>_<system>_<site>_T-LCN-<n>__TO_<to>_FROM_<from>.mp3``
- event logs        ``<date>_<time>.<ms>_<freq>_Hz_<channel>_decoded_messages.log``

``decoded_messages`` lines are stamped to 1 s only, but the P25 framer accounts
for every bit (HDU 792, LDU 1728, TDU 144, TDULC 432, TSDU 360/576/720, plus
``SYNC LOSS [n]``), so each log gets a 9600 bit/s clock whose offset is fitted
to the 1 s stamps (the method of ``tools/sdrtrunk_teardown_stats.py``).
"""

from __future__ import annotations

import json
import re
import struct
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

BIT_RATE = 9600.0
SPLIT_S = 0.5  # a frame gap longer than this ends a transmission (PTT)
FRAME_S = 0.02  # one IMBE frame

CAPTURE_NAME = re.compile(r"^(\d{9,11})_(\d+)_(\d+)_baseband\.wav$")
CHANNEL_WAV = re.compile(r"^(\d{8})_(\d{6})_(\d+)_(.+)_((?:T-)?LCN-\d+)_(\d+)_baseband\.wav$")
BITS_NAME = re.compile(r"^(\d{8})_(\d{6})_(\d+)_9600BPS_APCO25PHASE1_(.+)_((?:T-)?LCN-\d+)_(\d+)"
                       r"\.bits$")
MBE_NAME = re.compile(r"^(\d{8})_(\d{6})_(\d+)_(\d+)_(\d+)_(\d+)(_encrypted)?\.mbe$")
MP3_NAME = re.compile(r"^(\d{8})_(\d{6})_(.+)_((?:T-)?LCN-\d+)__TO_(\d+)_FROM_(\d+)(.*)\.mp3$")
LOG_NAME = re.compile(r"^(\d{8})_(\d{6})\.(\d{3})_(\d+)_Hz_(.+)_(decoded_messages|call_events)"
                      r"\.log$")
LOG_LINE = re.compile(r"^(\d{8} \d{6}),(\w+),(.*)$")
FS_BITS = {"H": 792, "L": 1728, "T": 144, "LC": 432}
TSDU_BITS = {"1": 360, "2": 216, "3": 144}  # cumulative 360 / 576 / 720 for 1..3 TSBKs


def local_epoch(date8: str, time6: str) -> float:
    """Unix seconds of a local-time ``YYYYMMDD``, ``HHMMSS`` pair."""
    return time.mktime(time.strptime(date8 + time6, "%Y%m%d%H%M%S"))


def local_stamp(t: float) -> str:
    return time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(t))


# ---------------------------------------------------------------------------
# WAV headers (SDRTrunk writes > 4 GiB files whose RIFF sizes wrap)
# ---------------------------------------------------------------------------


@dataclass
class WavInfo:
    path: str
    rate: int
    channels: int
    sampwidth: int
    data_offset: int
    data_bytes: int

    @property
    def frame_bytes(self) -> int:
        return self.channels * self.sampwidth

    @property
    def frames(self) -> int:
        return self.data_bytes // self.frame_bytes

    @property
    def seconds(self) -> float:
        return self.frames / float(self.rate)


def wav_info(path: str | Path) -> WavInfo:
    """Header of a PCM WAV. The data size comes from the file size (the RIFF
    ``data`` length is 32-bit and wraps above 4 GiB)."""
    p = Path(path)
    size = p.stat().st_size
    with open(p, "rb") as fh:
        head = fh.read(4096)
    if head[:4] != b"RIFF" or head[8:12] != b"WAVE":
        raise ValueError(f"{p.name}: not a RIFF/WAVE file")
    pos, fmt = 12, None
    while pos + 8 <= len(head):
        cid, clen = head[pos:pos + 4], struct.unpack("<I", head[pos + 4:pos + 8])[0]
        if cid == b"fmt ":
            _, ch, rate, _, _, bits = struct.unpack("<HHIIHH", head[pos + 8:pos + 24])
            fmt = (ch, rate, bits // 8)
        elif cid == b"data":
            if fmt is None:
                raise ValueError(f"{p.name}: data chunk before fmt")
            off = pos + 8
            return WavInfo(str(p), fmt[1], fmt[0], fmt[2], off, size - off)
        pos += 8 + clen + (clen & 1)
    raise ValueError(f"{p.name}: no data chunk in the first 4 KiB")


# ---------------------------------------------------------------------------
# .mbe ground truth
# ---------------------------------------------------------------------------


def load_mbe(path: str | Path) -> dict[str, Any]:
    """A ``.mbe`` document with its ``frames`` in file (decode) order.

    Not sorted by time: SDRTrunk's frame stamps step back by up to ~150 ms at LDU
    boundaries in 220 of the 367 files, and sorting would interleave two LDUs.
    """
    doc = json.loads(Path(path).read_text(encoding="utf-8"))
    doc["frames"] = [f for f in doc.get("frames", []) if "time" in f and "hex" in f]
    return doc


def split_transmissions(times_s: list[float], split_s: float = SPLIT_S) -> list[tuple[int, int]]:
    """Index ranges ``[lo, hi)`` (file order) of frame runs separated by forward gaps
    > ``split_s`` (backward steps are stamp jitter, not gaps)."""
    out, lo = [], 0
    for i in range(1, len(times_s) + 1):
        if i == len(times_s) or times_s[i] - times_s[i - 1] > split_s:
            out.append((lo, i))
            lo = i
    return [r for r in out if r[1] > r[0]]


def mbe_record(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]]] | None:
    """(call, transmissions) of one ``.mbe`` file, or None if unreadable."""
    m = MBE_NAME.match(path.name)
    if not m:
        return None
    try:
        doc = load_mbe(path)
    except (OSError, ValueError):
        return None
    times = [f["time"] / 1000.0 for f in doc["frames"]]
    enc = bool(doc.get("encrypted")) or bool(m.group(7))
    tg = int(doc.get("to") or m.group(5))
    try:
        src = int(doc.get("from") or m.group(6))
    except ValueError:
        src = 0
    call = {"id": path.stem, "file": path.name, "freq_hz": int(m.group(3)), "seq": int(m.group(4)),
            "tg": tg, "src": src, "encrypted": enc, "frames": len(times),
            "t0": min(times) if times else local_epoch(m.group(1), m.group(2)),
            "t1": max(times) + FRAME_S if times else local_epoch(m.group(1), m.group(2)),
            "transmissions": []}
    txs = []
    for k, (lo, hi) in enumerate(split_transmissions(times)):
        run = times[lo:hi]
        tx = {"id": f"{path.stem}#{k}", "call": path.stem, "file": path.name,
              "freq_hz": call["freq_hz"], "tg": tg, "src": src, "encrypted": enc,
              "frame_lo": lo, "frame_hi": hi, "frames": hi - lo,
              "t0": round(min(run), 3), "t1": round(max(run) + FRAME_S, 3)}
        txs.append(tx)
        call["transmissions"].append(tx["id"])
    return call, txs


def scan_mbe(recordings: str | Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    calls, txs = [], []
    for p in sorted(Path(recordings).glob("*.mbe")):
        rec = mbe_record(p)
        if rec is None or not rec[1]:
            continue
        calls.append(rec[0])
        txs.extend(rec[1])
    txs.sort(key=lambda t: (t["t0"], t["freq_hz"]))
    return calls, txs


def truth_frames(truth_dir: str | Path, tx: dict[str, Any]) -> list[tuple[float, str]]:
    """``[(air time s, hex)]`` of one transmission, loaded from its ``.mbe``."""
    doc = load_mbe(Path(truth_dir) / tx["file"])
    fr = doc["frames"][int(tx["frame_lo"]):int(tx["frame_hi"])]
    return [(f["time"] / 1000.0, str(f["hex"]).lower()) for f in fr]


# ---------------------------------------------------------------------------
# Channel recordings (.wav / .bits / .mp3)
# ---------------------------------------------------------------------------


def scan_channel_recordings(recordings: str | Path, cc_freq: int) -> list[dict[str, Any]]:
    """One entry per channel ``*_baseband.wav`` with its ``.bits`` and header info."""
    root = Path(recordings)
    bits: dict[tuple[str, str, str, str], str] = {}
    for p in root.glob("*.bits"):
        m = BITS_NAME.match(p.name)
        if m:
            bits[(m.group(1) + m.group(2), m.group(3), m.group(5), m.group(6))] = p.name
    out = []
    for p in sorted(root.glob("*_baseband.wav")):
        m = CHANNEL_WAV.match(p.name)
        if not m:
            continue
        try:
            info = wav_info(p)
        except (OSError, ValueError):
            continue
        freq = int(m.group(3))
        kind = "cc" if freq == cc_freq and not m.group(5).startswith("T-") else "traffic"
        stamp = m.group(1) + m.group(2)
        out.append({
            "id": p.stem.replace("_baseband", ""), "file": p.name, "kind": kind, "freq_hz": freq,
            "channel": m.group(5), "index": int(m.group(6)),
            "t_name": local_epoch(m.group(1), m.group(2)), "rate_hz": info.rate,
            "channels": info.channels, "seconds": round(info.seconds, 3),
            "data_offset": info.data_offset, "data_bytes": info.data_bytes,
            "bits": bits.get((stamp, m.group(3), m.group(5), m.group(6))),
        })
    return out


def scan_mp3(recordings: str | Path) -> list[dict[str, Any]]:
    out = []
    for p in sorted(Path(recordings).glob("*.mp3")):
        m = MP3_NAME.match(p.name)
        if m:
            out.append({"file": p.name, "t_name": local_epoch(m.group(1), m.group(2)),
                        "channel": m.group(4), "tg": int(m.group(5)), "src": int(m.group(6))})
    return out


def scan_captures(captures: str | Path) -> list[dict[str, Any]]:
    out = []
    for p in sorted(Path(captures).glob("*_baseband.wav")):
        m = CAPTURE_NAME.match(p.name)
        if not m:
            continue
        try:
            info = wav_info(p)
        except (OSError, ValueError):
            continue
        st = p.stat()
        out.append({"id": p.stem.replace("_baseband", ""), "file": p.name, "path": str(p),
                    "start_unix": float(m.group(1)), "centre_hz": int(m.group(2)),
                    "rate_hz": info.rate, "seconds": round(info.seconds, 3),
                    "data_offset": info.data_offset, "data_bytes": info.data_bytes,
                    "size": st.st_size, "mtime": int(st.st_mtime)})
    return out


# ---------------------------------------------------------------------------
# decoded_messages logs: bit clock
# ---------------------------------------------------------------------------


def scan_logs(event_logs: str | Path) -> list[dict[str, Any]]:
    out = []
    root = Path(event_logs)
    if not root.is_dir():
        return out
    for p in root.glob("*_decoded_messages.log"):
        m = LOG_NAME.match(p.name)
        if not m:
            continue
        out.append({"file": p.name, "path": str(p), "freq_hz": int(m.group(4)),
                    "channel": m.group(5), "traffic": m.group(5).startswith("T-"),
                    "t_name": local_epoch(m.group(1), m.group(2)) + int(m.group(3)) / 1000.0})
    return sorted(out, key=lambda f: f["t_name"])


def classify(body: str) -> tuple[str, int]:
    """(kind, bits consumed on the bit clock) of one decoded_messages body."""
    if "SYNC LOSS" in body:
        m = re.search(r"\[(\d+)\]", body)
        return "SL", int(m.group(1)) if m else 0
    if "DROPPED SAMPLES" in body:
        m = re.search(r"\[(\d+)\]", body)
        return "DS", -int(m.group(1)) if m else 0
    b = re.sub(r"^NAC:\S+\s+", "", body)
    for pre, k in (("LDU", "L"), ("HDU", "H"), ("TDULC", "LC"), ("TDU", "T")):
        if b.startswith(pre):
            return k, FS_BITS[k]
    m = re.match(r"TSBK([123])", b)
    if m:
        return "TSBK", TSDU_BITS[m.group(1)]
    return "O", 360


def _fit_offset(los: list[float]) -> float:
    """Point covered by the most intervals ``[lo, lo + 1)``."""
    ev = sorted([(x, 1) for x in los] + [(x + 1.0, -1) for x in los])
    best = cur = 0
    lo = hi = los[0]
    for i, (x, d) in enumerate(ev):
        cur += d
        if cur > best:
            best, lo, hi = cur, x, ev[i + 1][0] if i + 1 < len(ev) else x
    return (lo + hi) / 2


def parse_log(path: str | Path, window_s: float = 60.0,
              max_lines: int | None = None) -> list[dict[str, Any]]:
    """Messages of one decoded_messages log on its fitted bit clock.

    Each message: ``t`` (END time, unix s), ``start`` (t minus its bits),
    ``k`` (kind), ``body``, ``ok``, ``cum`` (bits since the log start).
    """
    raw: list[tuple[float, int, str, str, bool, int]] = []
    cum = 0
    with open(path, encoding="utf-8", errors="replace") as fh:
        for n, ln in enumerate(fh):
            if max_lines is not None and n > max_lines:
                break
            m = LOG_LINE.match(ln.rstrip("\r\n"))
            if not m:
                continue
            body = m.group(3).strip()
            k, nb = classify(body)
            cum += nb
            stamp = time.mktime(time.strptime(m.group(1), "%Y%m%d %H%M%S"))
            raw.append((stamp, cum, k, body, m.group(2) == "PASSED", nb))
    msgs: list[dict[str, Any]] = []
    i = 0
    while i < len(raw):
        j = i
        while j < len(raw) and raw[j][1] - raw[i][1] < window_s * BIT_RATE:
            j += 1
        j = len(raw) if len(raw) - j < 20 else j
        a = _fit_offset([raw[x][0] - raw[x][1] / BIT_RATE for x in range(i, j)])
        for stamp, c, k, body, ok, nb in raw[i:j]:
            t = a + c / BIT_RATE
            msgs.append({"t": t, "start": t - max(nb, 0) / BIT_RATE, "k": k, "body": body,
                         "ok": ok, "cum": c, "clock": a})
        i = j
    return msgs


def log_clock(path: str | Path, max_lines: int | None = 4000) -> dict[str, Any] | None:
    """Bit-clock origin of a log: the unix time of bit 0 (first fit window)."""
    msgs = parse_log(path, max_lines=max_lines)
    if not msgs:
        return None
    first = next((m for m in msgs if m["k"] not in ("SL", "DS")), None)
    return {"bit0_unix": msgs[0]["clock"], "messages": len(msgs),
            "first_frame_start": first["start"] if first else None,
            "first_kind": first["k"] if first else None}
