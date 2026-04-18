#!/usr/bin/env python3
"""Compute the same audio-quality metrics we used in 2026-04-17 perf analysis.

Proxy for "robotic audio" severity:
- silent_frames_pct: % of 100ms frames whose |x| < 100 (near-silence)
- rms_std_over_mean:  std/mean of per-100ms RMS values (voice stationarity)
- peak, rms, dc, zcr
"""
from __future__ import annotations

import argparse
import json
import sys
import wave

import numpy as np


def analyze(path: str, frame_ms: int = 100, silence_thresh: int = 100) -> dict:
    with wave.open(path, "rb") as wf:
        sr = wf.getframerate()
        ch = wf.getnchannels()
        sw = wf.getsampwidth()
        nframes = wf.getnframes()
        raw = wf.readframes(nframes)
    assert ch == 1 and sw == 2, f"expected 8 kHz 16-bit mono, got ch={ch} sw={sw}"
    x = np.frombuffer(raw, dtype=np.int16).astype(np.float64)
    dur = x.size / sr
    frame_len = sr * frame_ms // 1000
    # Drop trailing partial frame so indices line up.
    x_trimmed = x[: (x.size // frame_len) * frame_len]
    frames = x_trimmed.reshape(-1, frame_len)
    per_frame_rms = np.sqrt(np.mean(frames**2, axis=1))
    silent = np.mean(np.abs(frames).max(axis=1) < silence_thresh) * 100.0
    zcr = float(np.sum(np.diff(np.signbit(x).astype(int)) != 0)) / dur
    return {
        "path": path,
        "sample_rate": int(sr),
        "duration_s": float(dur),
        "samples": int(x.size),
        "peak_abs": float(np.abs(x).max()),
        "rms": float(np.sqrt(np.mean(x**2))),
        "dc_offset": float(x.mean()),
        "zcr_per_s": zcr,
        "frame_ms": frame_ms,
        "frames": int(frames.shape[0]),
        "silent_frames_pct": float(silent),
        "silence_thresh": silence_thresh,
        "per_frame_rms_mean": float(per_frame_rms.mean()),
        "per_frame_rms_std": float(per_frame_rms.std()),
        "per_frame_rms_min": float(per_frame_rms.min()),
        "per_frame_rms_max": float(per_frame_rms.max()),
        "rms_std_over_mean": float(
            per_frame_rms.std() / per_frame_rms.mean() if per_frame_rms.mean() > 0 else 0.0
        ),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()
    results = []
    for p in args.files:
        try:
            r = analyze(p)
            results.append(r)
        except Exception as e:
            print(f"ERR {p}: {e}", file=sys.stderr)
            continue
    # Markdown-style table for easy paste into docs.
    hdrs = ["file", "dur_s", "peak", "rms", "silent%", "std/mean", "min_rms", "zcr/s"]
    widths = [28, 7, 7, 7, 8, 9, 8, 8]
    print("| " + " | ".join(f"{h:<{w}}" for h, w in zip(hdrs, widths)) + " |")
    print("|" + "|".join("-" * (w + 2) for w in widths) + "|")
    for r in results:
        row = [
            r["path"].split("/")[-1][:28],
            f'{r["duration_s"]:.2f}',
            f'{r["peak_abs"]:.0f}',
            f'{r["rms"]:.0f}',
            f'{r["silent_frames_pct"]:.1f}',
            f'{r["rms_std_over_mean"]:.2f}',
            f'{r["per_frame_rms_min"]:.0f}',
            f'{r["zcr_per_s"]:.0f}',
        ]
        print("| " + " | ".join(f"{c:<{w}}" for c, w in zip(row, widths)) + " |")
    print()
    print(json.dumps(results, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
