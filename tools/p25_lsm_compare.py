"""Compare the scanner's LSM port with SDRTrunk's LSM decoder on the same recordings (change 079).

Per recording, three dibit streams in SDRTrunk's `.bits` packing:
  ours      the scanner's `lsm_wavs` test (P25_LSM_OUT: <stem>.bits and summary.jsonl)
  sdrtrunk  SDRTrunk's P25P1DecoderLSM on the same WAV (tools/sdrtrunk_lsm_reference.py decode)
  live      the `.bits` SDRTrunk wrote while it recorded the call (its live decoder, with tuner
            feedback and loop state from before the recording started)

Each reference is aligned against ours in blocks of half a second, the offset allowed to move by a
few dibits from block to block (a timing slip), and the dibits that agree are counted after the
first second. Frame counts come from summary.jsonl (ours) and SDRTrunk's message lines.

    python tools/p25_lsm_compare.py --ours runs/079/ours --sdrtrunk runs/079/sdrtrunk \\
        --messages runs/079/sdrtrunk_messages.txt --live C:/Users/Andy/SDRTrunk/recordings
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

SETTLE = 4800          # dibits skipped at the start (1 s)
BLOCK = 2400           # dibits per alignment block (0.5 s)
SLIP = 6               # how far the offset may move between blocks


def load_bits(path: Path) -> np.ndarray:
    raw = np.frombuffer(path.read_bytes(), dtype=np.uint8)
    out = np.empty(raw.size * 4, dtype=np.uint8)
    for k, shift in enumerate((6, 4, 2, 0)):
        out[k::4] = (raw >> shift) & 3
    return out


def coarse_offset(ours: np.ndarray, ref: np.ndarray) -> tuple[int, float]:
    """Offset into ref of ours[SETTLE:SETTLE+BLOCK], over every position (FFT correlation)."""
    probe = ours[SETTLE:SETTLE + BLOCK]
    if probe.size < BLOCK or ref.size < BLOCK:
        return 0, 0.0
    n = 1 << int(np.ceil(np.log2(ref.size + probe.size)))
    score = np.zeros(n)
    for v in range(4):
        a = np.fft.rfft((ref == v).astype(float), n)
        b = np.fft.rfft((probe == v).astype(float)[::-1], n)
        score += np.fft.irfft(a * b, n)
    valid = score[probe.size - 1:ref.size]
    pos = int(np.argmax(valid))
    return pos - SETTLE, float(valid[pos]) / probe.size


def agreement(ours: np.ndarray, ref: np.ndarray) -> dict:
    offset, coarse = coarse_offset(ours, ref)
    agree = total = slips = 0
    t = SETTLE
    while t + BLOCK <= ours.size:
        best, best_off = -1, offset
        for off in range(offset - SLIP, offset + SLIP + 1):
            r = t + off
            if r < 0 or r + BLOCK > ref.size:
                continue
            m = int(np.count_nonzero(ours[t:t + BLOCK] == ref[r:r + BLOCK]))
            if m > best:
                best, best_off = m, off
        if best < 0:
            break
        slips += best_off != offset
        offset = best_off
        agree += best
        total += BLOCK
        t += BLOCK
    return {"agree": agree, "total": total, "slips": slips, "coarse": round(coarse, 3)}


def live_bits_for(stem: str, live_dir: Path) -> Path | None:
    """`<date>_<time>_<hz>_<rest>_baseband` -> `<date>_<time>_<hz>_9600BPS_APCO25PHASE1_<rest>.bits`."""
    m = re.match(r"(\d{8}_\d{6}_\d+)_(.*)_baseband$", stem)
    if not m:
        return None
    p = live_dir / f"{m.group(1)}_9600BPS_APCO25PHASE1_{m.group(2)}.bits"
    return p if p.exists() else None


def sdrtrunk_counts(messages: Path) -> dict[str, Counter]:
    counts: dict[str, Counter] = defaultdict(Counter)
    for line in messages.read_text(encoding="utf-8", errors="replace").splitlines():
        parts = line.split("|", 4)
        if len(parts) < 5:
            continue
        stem = parts[0][:-4] if parts[0].endswith(".wav") else parts[0]
        valid, cls, text = parts[2] == "true", parts[3], parts[4]
        if cls.startswith("LDU1"):
            counts[stem]["ldu1s"] += 1
        elif cls.startswith("LDU2"):
            counts[stem]["ldu2s"] += 1
        elif cls.startswith("HDU"):
            counts[stem]["hdus"] += 1
        elif re.search(r" TSBK\d ", text):
            counts[stem]["tsbk_ok" if valid else "tsbk_bad"] += 1
        counts[stem][cls] += 1
    return counts


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--ours", type=Path, required=True)
    ap.add_argument("--sdrtrunk", type=Path, required=True)
    ap.add_argument("--messages", type=Path)
    ap.add_argument("--live", type=Path, help="the SDRTrunk recordings directory (live .bits)")
    ap.add_argument("--json", type=Path, help="write per-recording results here")
    args = ap.parse_args()

    summary = [json.loads(l) for l in (args.ours / "summary.jsonl").read_text().splitlines() if l.strip()]
    sdr_counts = sdrtrunk_counts(args.messages) if args.messages else {}
    rows = []
    for s in summary:
        stem = s["file"]
        ours = load_bits(args.ours / f"{stem}.bits")
        row = {"file": stem, "ours": {k: s[k] for k in ("ldu1s", "ldu2s", "hdus", "tsbk_ok", "nid_ok")}}
        ref = args.sdrtrunk / f"{stem}.bits"
        if ref.exists():
            row["sdrtrunk"] = agreement(ours, load_bits(ref))
        live = live_bits_for(stem, args.live) if args.live else None
        if live:
            row["live"] = agreement(ours, load_bits(live))
        if stem in sdr_counts:
            row["sdrtrunk_counts"] = dict(sdr_counts[stem])
        rows.append(row)

    def pct(rows, key):
        a = sum(r[key]["agree"] for r in rows if key in r)
        t = sum(r[key]["total"] for r in rows if key in r)
        return 100.0 * a / t if t else float("nan"), t

    for key in ("sdrtrunk", "live"):
        p, t = pct(rows, key)
        n = sum(1 for r in rows if key in r)
        slips = sum(r[key]["slips"] for r in rows if key in r)
        print(f"{key:9s} recordings {n:4d}  dibits compared {t:10d}  agreement {p:7.3f} %  slips {slips}")
    ours_tot = Counter()
    sdr_tot = Counter()
    for r in rows:
        ours_tot.update(r["ours"])
        sdr_tot.update({k: v for k, v in r.get("sdrtrunk_counts", {}).items() if k in ("ldu1s", "ldu2s", "hdus", "tsbk_ok")})
    print("frames    ours", dict(ours_tot))
    print("frames    sdrtrunk", dict(sdr_tot))
    worst = sorted((r for r in rows if "sdrtrunk" in r and r["sdrtrunk"]["total"]),
                   key=lambda r: r["sdrtrunk"]["agree"] / r["sdrtrunk"]["total"])[:10]
    print("lowest agreement with SDRTrunk on the same WAV:")
    for r in worst:
        a = r["sdrtrunk"]
        print(f"  {100.0 * a['agree'] / a['total']:7.3f} %  slips {a['slips']:3d}  {r['file']}")
    if args.json:
        args.json.write_text(json.dumps(rows, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
