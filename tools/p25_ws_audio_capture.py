#!/usr/bin/env python3
"""Capture /ws/audio and characterise arrival timing.

Connects to the board's `/ws/audio` WebSocket, records every binary
frame's monotonic arrival time + size, dumps the concatenated PCM as a
WAV, and prints arrival-rate / inter-arrival-gap statistics.

Auto-exits after `--idle-secs` of no incoming binary frames so a single
invocation can capture one call (or many, with a long idle window) and
end without manual stop.

Server format (see p25-httpd/src/audio/mod.rs):
    Binary frames: 320 bytes = 160 i16 LE samples = 20 ms @ 8 kHz mono.
    Text frames:   {"type":"lag","skipped":N} when broadcast lagged.

Outputs (in --outdir, default ./ws_audio_capture/<timestamp>):
    audio.wav            8 kHz 16-bit mono PCM, all chunks concatenated
    arrivals.jsonl       one row per frame: {seq, t_mono, size, kind}
    summary.json         aggregate stats (counts, rate, gap percentiles)

Usage:
    python tools/p25_ws_audio_capture.py
    python tools/p25_ws_audio_capture.py --idle-secs 30 --max-secs 600
    python tools/p25_ws_audio_capture.py --host 192.168.2.1:8080
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import struct
import sys
import time
import wave

import websocket


CHUNK_BYTES = 320
SAMPLES_PER_CHUNK = 160
SAMPLE_RATE = 8000
CHUNK_MS = 1000.0 * SAMPLES_PER_CHUNK / SAMPLE_RATE  # 20 ms


def _percentile(sorted_vals: list[float], p: float) -> float:
    if not sorted_vals:
        return 0.0
    idx = int(round((len(sorted_vals) - 1) * p))
    return sorted_vals[idx]


def capture(host: str, idle_secs: float, max_secs: float, outdir: str) -> dict:
    os.makedirs(outdir, exist_ok=True)
    url = f"ws://{host}/ws/audio"
    print(f"connecting to {url}", flush=True)
    ws = websocket.create_connection(url, timeout=5)
    # Tight per-recv timeout so we can detect idle on the recv side
    # without blocking forever. We re-arm after every frame.
    ws.settimeout(min(idle_secs, 1.0))

    arrivals_path = os.path.join(outdir, "arrivals.jsonl")
    wav_path = os.path.join(outdir, "audio.wav")
    summary_path = os.path.join(outdir, "summary.json")

    arrivals_f = open(arrivals_path, "w", encoding="utf-8")
    wf = wave.open(wav_path, "wb")
    wf.setnchannels(1)
    wf.setsampwidth(2)
    wf.setframerate(SAMPLE_RATE)

    started_mono = time.monotonic()
    last_binary_mono: float | None = None
    seq = 0
    bin_frames = 0
    bin_bytes = 0
    text_frames = 0
    lag_total = 0
    inter_arrivals_ms: list[float] = []
    first_arrival_mono: float | None = None
    last_arrival_mono: float | None = None

    interrupted = False
    try:
        while True:
            now = time.monotonic()
            elapsed = now - started_mono
            # Hard cap.
            if elapsed > max_secs:
                print(
                    f"reached max-secs={max_secs:.0f}, stopping",
                    flush=True,
                )
                break
            # Idle stop: if we've ever received a frame and none in the
            # last idle_secs, exit.
            if (
                last_binary_mono is not None
                and (now - last_binary_mono) >= idle_secs
            ):
                idle_for = now - last_binary_mono
                print(
                    f"idle {idle_for:.1f}s, stopping",
                    flush=True,
                )
                break

            try:
                msg = ws.recv()
            except KeyboardInterrupt:
                # Graceful Ctrl-C: bail out of the loop so the finally
                # block closes resources and the summary still writes.
                print("interrupted, stopping", flush=True)
                interrupted = True
                break
            except websocket.WebSocketTimeoutException:
                # No frame in the last second; loop and re-check idle.
                continue
            except (websocket.WebSocketConnectionClosedException, OSError) as e:
                print(f"connection closed: {e}", flush=True)
                break

            t = time.monotonic()
            if isinstance(msg, (bytes, bytearray)):
                size = len(msg)
                if size != CHUNK_BYTES:
                    # Not fatal but worth noting.
                    print(
                        f"warn: unexpected binary size {size} != {CHUNK_BYTES}",
                        flush=True,
                    )
                wf.writeframesraw(msg)
                bin_frames += 1
                bin_bytes += size
                if first_arrival_mono is None:
                    first_arrival_mono = t
                if last_binary_mono is not None:
                    inter_arrivals_ms.append((t - last_binary_mono) * 1000.0)
                last_binary_mono = t
                last_arrival_mono = t
                arrivals_f.write(
                    json.dumps(
                        {
                            "seq": seq,
                            "t_mono": round(t - started_mono, 6),
                            "size": size,
                            "kind": "pcm",
                        }
                    )
                    + "\n"
                )
                # Live status every 50 frames (~1 s of audio).
                if bin_frames % 50 == 0:
                    rate = bin_frames / max(t - (first_arrival_mono or t), 1e-6)
                    print(
                        f"  frames={bin_frames} "
                        f"bytes={bin_bytes} "
                        f"avg_rate={rate:.1f} fr/s",
                        flush=True,
                    )
            else:
                # Text control frame.
                text_frames += 1
                try:
                    parsed = json.loads(msg)
                except json.JSONDecodeError:
                    parsed = {"_raw": msg}
                if (
                    isinstance(parsed, dict)
                    and parsed.get("type") == "lag"
                ):
                    lag_total += int(parsed.get("skipped", 0))
                arrivals_f.write(
                    json.dumps(
                        {
                            "seq": seq,
                            "t_mono": round(t - started_mono, 6),
                            "kind": "ctrl",
                            "ctrl": parsed,
                        }
                    )
                    + "\n"
                )
            seq += 1
    finally:
        try:
            ws.close()
        except Exception:
            pass
        wf.close()
        arrivals_f.close()

    # Aggregate stats.
    duration_capture = (last_arrival_mono or 0) - (first_arrival_mono or 0)
    audio_seconds = bin_frames * CHUNK_MS / 1000.0
    realtime_ratio = (
        audio_seconds / duration_capture if duration_capture > 0 else 0.0
    )
    sorted_gaps = sorted(inter_arrivals_ms)
    gap_stats = {
        "count":  len(sorted_gaps),
        "min_ms": round(min(sorted_gaps), 3) if sorted_gaps else 0.0,
        "p50_ms": round(_percentile(sorted_gaps, 0.50), 3),
        "p90_ms": round(_percentile(sorted_gaps, 0.90), 3),
        "p99_ms": round(_percentile(sorted_gaps, 0.99), 3),
        "max_ms": round(max(sorted_gaps), 3) if sorted_gaps else 0.0,
        "expected_ms": CHUNK_MS,
    }
    # How many gaps are "burst" (<5 ms — multiple chunks within one tick)?
    bursts = sum(1 for g in sorted_gaps if g < 5.0)
    long_gaps = sum(1 for g in sorted_gaps if g > 100.0)
    summary = {
        "host":              host,
        "started_at":        datetime.datetime.now().isoformat(timespec="seconds"),
        "outdir":            outdir,
        "interrupted":       interrupted,
        "binary_frames":     bin_frames,
        "binary_bytes":      bin_bytes,
        "audio_seconds":     round(audio_seconds, 3),
        "wallclock_seconds": round(duration_capture, 3),
        "realtime_ratio":    round(realtime_ratio, 3),
        "text_frames":       text_frames,
        "lag_total":         lag_total,
        "first_arrival_at":  (
            round(first_arrival_mono - started_mono, 3)
            if first_arrival_mono else None
        ),
        "last_arrival_at": (
            round(last_arrival_mono - started_mono, 3)
            if last_arrival_mono else None
        ),
        "inter_arrival_ms":      gap_stats,
        "burst_gaps_lt_5ms":     bursts,
        "long_gaps_gt_100ms":    long_gaps,
    }
    with open(summary_path, "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2)
    return summary


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.2.1:8080",
                    help="board address (host:port)")
    ap.add_argument("--idle-secs", type=float, default=10.0,
                    help="exit after this many seconds of no binary frames")
    ap.add_argument("--max-secs", type=float, default=600.0,
                    help="hard cap on total capture time")
    ap.add_argument("--outdir", default=None,
                    help="output directory (default: ws_audio_capture/<ts>)")
    args = ap.parse_args()

    if args.outdir is None:
        ts = datetime.datetime.now().strftime("%Y%m%d_%H%M%S")
        args.outdir = os.path.join("ws_audio_capture", ts)

    summary = capture(args.host, args.idle_secs, args.max_secs, args.outdir)
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
