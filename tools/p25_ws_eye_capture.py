#!/usr/bin/env python3
"""Capture raw /ws/iq samples (post_ddc or post_lsm) and render offline eye plots.

Bypasses the browser so the eye is driven by the canonical firmware
sample stream, not by any dashboard JS state. Strides each overlay by
exactly one symbol with fractional tracking.

Usage:
    python tools/p25_ws_eye_capture.py --host 192.168.2.1:8080 \
        --source post_lsm --chain control --secs 12 \
        --out doc/diagnostics/2026-04-18/eye
"""
from __future__ import annotations

import argparse
import json
import os
import time

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import websocket


SYMBOL_RATE = 4800  # P25 Phase 1 symbol rate


def capture(host: str, chain: str, source: str, secs: float) -> tuple[dict, np.ndarray, np.ndarray]:
    url = f"ws://{host}/ws/iq?chain={chain}&source={source}"
    ws = websocket.create_connection(url, timeout=10)
    ws.settimeout(10)
    hello = json.loads(ws.recv())
    t_start = time.monotonic()
    buf = bytearray()
    while time.monotonic() - t_start < secs:
        try:
            m = ws.recv()
        except Exception:
            break
        if isinstance(m, (bytes, bytearray)):
            buf.extend(m)
    ws.close()
    # i16le interleaved: re, im, re, im, ...
    arr = np.frombuffer(bytes(buf), dtype=np.int16).astype(np.float32)
    if arr.size % 2:
        arr = arr[:-1]
    re = arr[0::2]
    im = arr[1::2]
    return hello, re, im


def draw_eye(
    re: np.ndarray,
    im: np.ndarray,
    sps: float,
    nsym: int,
    title: str,
    outpath: str,
    traces: str = "iq",
    max_overlays: int = 500,
) -> None:
    """Overlay N-symbol windows with 1-symbol stride.

    At high sample counts we cap overlays (random stride-preserving
    subsample) + use per-trace alpha so the eye openings stay visible
    instead of saturating into a solid blob.
    """
    win_len = nsym * sps
    step = int(round(win_len))
    # Fractional start positions so stride is exactly `sps` per overlay.
    starts = []
    s_f = 0.0
    while int(s_f) + step <= re.size:
        starts.append(int(s_f))
        s_f += sps
    n_total = len(starts)
    # Downsample overlays uniformly across the capture to preserve the
    # long-time-window variance rather than clustering at the start.
    if n_total > max_overlays:
        idx = np.linspace(0, n_total - 1, max_overlays, dtype=int)
        starts = [starts[i] for i in idx]
    t_axis = np.arange(step) / sps  # x-axis in units of symbol periods

    # Alpha calibrated for visible-but-not-saturated traces. At 200
    # overlays ≈ 0.20 alpha; at 500 ≈ 0.08; clamped to a floor of 0.05
    # so sparse captures are still visible.
    alpha = min(0.35, max(0.05, 40.0 / max(1, len(starts))))

    fig, ax = plt.subplots(figsize=(10, 5), dpi=120)
    ax.set_facecolor("#0a1929")
    fig.patch.set_facecolor("#0a1929")
    for s_idx in range(0, step + 1):
        ax.axvline(s_idx, color="#1e2a3a", lw=0.7)

    if "i" in traces:
        for start in starts:
            ax.plot(t_axis, re[start : start + step],
                    color="#22c55e", alpha=alpha, lw=0.7)
    if "q" in traces:
        for start in starts:
            ax.plot(t_axis, im[start : start + step],
                    color="#60a5fa", alpha=alpha, lw=0.7)

    amax = max(np.abs(re).max(), np.abs(im).max(), 1.0)
    ax.set_xlim(0, nsym)
    ax.set_ylim(-amax * 1.1, amax * 1.1)
    ax.set_xlabel("Symbol periods", color="#cbd5e1")
    ax.set_ylabel("Amplitude", color="#cbd5e1")
    ax.set_title(
        f"{title}  •  nsym={nsym}  •  sps={sps:.2f}  •  overlays={len(starts)}/{n_total}  •  alpha={alpha:.3f}  •  |max|={amax:.0f}",
        color="#cbd5e1", fontsize=10,
    )
    ax.tick_params(colors="#cbd5e1")
    for spine in ax.spines.values():
        spine.set_color("#1e2a3a")
    fig.tight_layout()
    fig.savefig(outpath, facecolor=fig.get_facecolor())
    plt.close(fig)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--chain", default="control", choices=["control", "traffic"])
    ap.add_argument("--source", default="post_lsm", choices=["post_ddc", "post_lsm"])
    ap.add_argument("--secs", type=float, default=10.0)
    ap.add_argument("--out", required=True, help="output directory")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)

    print(f"capturing {args.secs:.1f}s of {args.source} on chain={args.chain}...")
    hello, re, im = capture(args.host, args.chain, args.source, args.secs)
    sample_rate = hello["sample_rate_hz"]
    sps = sample_rate / SYMBOL_RATE
    dur = re.size / sample_rate
    print(f"  hello: sample_rate={sample_rate} sps={sps:.3f}")
    print(f"  captured samples={re.size} ({dur:.2f}s, {re.size/sps:.0f} symbols)")
    if re.size == 0:
        print("ERROR: no samples received")
        return 2

    # Save raw IQ as npz for offline replay.
    tag = f"{args.chain}_{args.source}"
    raw_path = os.path.join(args.out, f"iq_raw_{tag}.npz")
    np.savez_compressed(raw_path, re=re, im=im, hello=json.dumps(hello))
    print(f"  raw -> {raw_path}")

    # Render eyes at nsym=2 and nsym=4, I-only and I+Q.
    for nsym in (2, 4):
        for traces, lbl in (("iq", "I+Q"), ("i", "I-only")):
            path = os.path.join(args.out, f"eye_{tag}_nsym{nsym}_{traces}.png")
            draw_eye(re, im, sps, nsym, f"{tag} {lbl}", path, traces=traces)
            print(f"  eye -> {path}")

    # Summary stats: RMS I/Q, I/Q imbalance, DC offset
    stats = {
        "chain": args.chain,
        "source": args.source,
        "sample_rate_hz": sample_rate,
        "sps": sps,
        "samples": int(re.size),
        "duration_s": float(dur),
        "rms_i": float(np.sqrt(np.mean(re**2))),
        "rms_q": float(np.sqrt(np.mean(im**2))),
        "dc_i": float(re.mean()),
        "dc_q": float(im.mean()),
        "peak_abs": float(max(np.abs(re).max(), np.abs(im).max())),
    }
    summary_path = os.path.join(args.out, f"summary_{tag}.json")
    with open(summary_path, "w") as f:
        json.dump(stats, f, indent=2)
    print(f"  stats -> {summary_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
