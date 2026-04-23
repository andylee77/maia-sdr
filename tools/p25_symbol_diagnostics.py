#!/usr/bin/env python3
"""
Per-symbol diagnostic plots from the Phase 10.7 HDL post-PLL ring.

For each decoded symbol we extract a short windowed trace around its
decision instant, group traces by the four P25 symbol values (+3, +1,
−1, −3), and render:

  1. Small-multiples: one panel per symbol value, all traces for
     that value overlaid. Lets you see whether one decision level is
     consistently cleaner than another (AGC bias, quadrant imbalance,
     I/Q imbalance).
  2. Stats table: per-symbol hit count, mean decision-time deviation,
     stdev, and "clean fraction" (how many traces land within a
     tight margin of the nominal ±1 / ±3 rail).

Input: /ws/iq?source=pre_diff from the board. The HDL stream is
interleaved (mid, sym, mid, sym, ...) at 9.6 kSPS (2 samples per
symbol). Decisions are made from sign(I), sign(Q) at the sym samples
(same rule the HDL slicer uses — see
maia-hdl/p25_hdl/lsm_demod_loop.py "rotated_dibit" signal).

P25 dibit → symbol deviation mapping (from the diff-demod phase
change):
  dibit 00 (I>0, Q>0) → angle +π/4  → +1
  dibit 10 (I<0, Q>0) → angle +3π/4 → +3
  dibit 11 (I<0, Q<0) → angle −3π/4 → −3
  dibit 01 (I>0, Q<0) → angle −π/4  → −1

(Check this against SDRTrunk's P25P1DemodulatorLSM.java if porting.)
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import struct
import sys
from collections import Counter, defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

try:
    import websockets
except ImportError:
    print("websockets package required: pip install websockets", file=sys.stderr)
    sys.exit(1)


Q13_SCALE = 1.0 / 8192.0
UPSAMPLE = 5                   # 2 sps → 10 sps via linear interp
SPS = 2 * UPSAMPLE             # post-upsample samples per symbol
WINDOW_SYMS = 5                # 5-symbol wide trace per symbol
WINDOW_SAMPLES = SPS * WINDOW_SYMS
CENTER = SPS * (WINDOW_SYMS // 2)   # decision instant inside the window


# dibit = (sign(I), sign(Q)) → P25 symbol value {+1, +3, −3, −1}.
def dibit_to_symbol(i: float, q: float) -> int:
    if i > 0 and q > 0:
        return +1           # +π/4
    if i < 0 and q > 0:
        return +3           # +3π/4
    if i < 0 and q < 0:
        return -3           # −3π/4
    return -1               # −π/4


async def stream(host: str, chain: str, seconds: float) -> np.ndarray:
    uri = f"ws://{host}/ws/iq?source=pre_diff&chain={chain}"
    print(f"connecting {uri}", file=sys.stderr)
    pairs: list[tuple[int, int]] = []
    async with websockets.connect(uri, max_size=None) as ws:
        deadline = asyncio.get_event_loop().time() + seconds
        try:
            while asyncio.get_event_loop().time() < deadline:
                remaining = deadline - asyncio.get_event_loop().time()
                if remaining <= 0:
                    break
                frame = await asyncio.wait_for(ws.recv(), timeout=remaining + 0.5)
                if isinstance(frame, str):
                    continue
                n = len(frame) // 4
                for k in range(n):
                    re, im = struct.unpack_from("<hh", frame, k * 4)
                    pairs.append((re, im))
        except asyncio.TimeoutError:
            pass
    if not pairs:
        raise SystemExit("no post-PLL data received")
    return np.array(pairs, dtype=np.int32).astype(np.float32) * Q13_SCALE


def upsample(arr: np.ndarray) -> np.ndarray:
    """Linear interp 2 sps → 10 sps. Phase-space would be cleaner but
    requires unwrap handling; IQ linear is fine for ±3/±1 rendering
    at this signal density."""
    n0 = arr.shape[0]
    t0 = np.arange(n0)
    t1 = np.linspace(0, n0 - 1, (n0 - 1) * UPSAMPLE + 1)
    i_up = np.interp(t1, t0, arr[:, 0])
    q_up = np.interp(t1, t0, arr[:, 1])
    return np.column_stack([i_up, q_up])


def extract_symbol_traces(arr: np.ndarray) -> list[tuple[int, np.ndarray]]:
    """For each sym sample in the interleaved stream, pull a
    WINDOW_SYMS-wide slice around it and return (symbol_value, phase_trace).

    arr is the ORIGINAL 2-sps stream (pre-upsample); we locate the
    sym-sample boundary in original-sample space (odd indices) and
    then convert to upsampled-space offsets.
    """
    arr_up = upsample(arr)
    phase_up = np.arctan2(arr_up[:, 1], arr_up[:, 0]) * (4.0 / np.pi)

    # In the 2-sps stream, odd indices are sym samples. After UPSAMPLE
    # = 5, the sym-sample k in 2-sps sits at upsampled index k * UPSAMPLE.
    sym_indices_orig = np.arange(1, arr.shape[0], 2)
    sym_indices_up = sym_indices_orig * UPSAMPLE

    traces: list[tuple[int, np.ndarray]] = []
    for k_up in sym_indices_up:
        start = k_up - CENTER
        end = start + WINDOW_SAMPLES
        if start < 0 or end > phase_up.size:
            continue
        trace = phase_up[start:end]
        # Decide the symbol from the raw sym sample in the 2-sps stream.
        orig_idx = k_up // UPSAMPLE
        i_dec = arr[orig_idx, 0]
        q_dec = arr[orig_idx, 1]
        sym = dibit_to_symbol(i_dec, q_dec)
        traces.append((sym, trace))
    return traces


def render_smallmultiples(traces: list[tuple[int, np.ndarray]],
                          out_path: str,
                          meta: dict) -> dict:
    by_sym: dict[int, list[np.ndarray]] = defaultdict(list)
    for sym, tr in traces:
        by_sym[sym].append(tr)

    fig, axes = plt.subplots(2, 2, figsize=(10.0, 6.0), dpi=110,
                             sharex=True, sharey=True)
    # Ordering: +3 top-left, +1 top-right, −1 bottom-right, −3 bottom-left.
    grid = {(0, 0): +3, (0, 1): +1, (1, 0): -3, (1, 1): -1}
    colors = {+3: "#4c72b0", +1: "#dd8452",
              -1: "#55a467", -3: "#c44e52"}
    x = np.arange(WINDOW_SAMPLES) / SPS - WINDOW_SYMS // 2

    stats_out: dict[int, dict] = {}
    for (r, c), sym in grid.items():
        ax = axes[r][c]
        ax.set_facecolor("#1c1c1c")
        tracs = by_sym.get(sym, [])
        for tr in tracs[:200]:
            ax.plot(x, tr, color=colors[sym], lw=0.6, alpha=0.12)
        # Mean trace on top, solid.
        if tracs:
            mean_tr = np.mean(np.stack(tracs), axis=0)
            ax.plot(x, mean_tr, color="#fff", lw=1.2)
        # Rails + decision grid.
        for lvl in (+3, +1, -1, -3):
            ax.axhline(lvl, color="#333", lw=0.5, ls="-")
        ax.axvline(0, color="#444", lw=0.5, ls="--")
        ax.set_ylim(-4.2, 4.2)
        ax.set_yticks([-3, -1, 0, 1, 3])
        ax.set_title(f"symbol {sym:+d}  n={len(tracs)}",
                     fontsize=10, color="#ddd")
        ax.tick_params(colors="#ddd", labelsize=8)
        for spine in ax.spines.values():
            spine.set_color("#444")

        # Stats: decision-instant value (CENTER sample in the trace),
        # mean and stdev, and "clean fraction" = fraction within ±0.5
        # of the nominal rail.
        if tracs:
            decision_vals = np.array([tr[CENTER] for tr in tracs])
            mean = float(decision_vals.mean())
            stdev = float(decision_vals.std())
            target = float(sym)
            clean_frac = float(np.mean(np.abs(decision_vals - target) < 0.5))
            stats_out[sym] = dict(
                count=len(tracs),
                mean=mean,
                stdev=stdev,
                nominal=target,
                clean_frac=clean_frac,
            )

    axes[1][0].set_xlabel("symbol periods (decision at 0)", color="#ddd")
    axes[1][1].set_xlabel("symbol periods (decision at 0)", color="#ddd")
    axes[0][0].set_ylabel("deviation", color="#ddd")
    axes[1][0].set_ylabel("deviation", color="#ddd")
    fig.suptitle(f"per-symbol traces  "
                 f"({meta['total_syms']} symbols, {meta['duration']:.1f} s)",
                 color="#ddd", fontsize=11)
    fig.patch.set_facecolor("#1c1c1c")
    fig.tight_layout()
    fig.savefig(out_path, facecolor="#1c1c1c", dpi=110)
    plt.close(fig)
    return stats_out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--chain", choices=["control", "traffic"], default="control")
    ap.add_argument("--duration", type=float, default=10.0)
    ap.add_argument("--outdir", default="symbol_diagnostics")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    raw = asyncio.run(stream(args.host, args.chain, args.duration))
    print(f"collected {raw.shape[0]} samples "
          f"({raw.shape[0] // 2} symbols estimated)", file=sys.stderr)

    traces = extract_symbol_traces(raw)
    counts = Counter(s for s, _ in traces)
    meta = dict(total_syms=len(traces), duration=args.duration)
    out_png = os.path.join(args.outdir, f"per_symbol_{args.chain}.png")
    stats = render_smallmultiples(traces, out_png, meta)

    # Print stats + write JSON.
    print(f"\n{'sym':>5} {'count':>6} {'mean':>8} {'stdev':>7} "
          f"{'|mean-nom|':>10} {'clean_frac':>11}")
    total_clean = 0
    total_n = 0
    for sym in (+3, +1, -1, -3):
        s = stats.get(sym, dict(count=0, mean=0.0, stdev=0.0,
                                nominal=float(sym), clean_frac=0.0))
        dev = abs(s["mean"] - s["nominal"]) if s["count"] else 0.0
        print(f"{sym:>+5} {s['count']:>6} {s['mean']:>+8.3f} "
              f"{s['stdev']:>7.3f} {dev:>10.3f} {s['clean_frac']:>11.3f}")
        total_clean += s["clean_frac"] * s["count"]
        total_n += s["count"]
    if total_n:
        print(f"\noverall decision clean_frac (|soft-nom|<0.5): "
              f"{total_clean / total_n:.3f}  "
              f"(n={total_n})")
    # Quadrant balance / bias check.
    print(f"\nsymbol counts: {dict(counts)}")
    if counts:
        ratio = max(counts.values()) / max(1, min(counts.values()))
        print(f"max/min ratio: {ratio:.2f}x  "
              f"(balanced random dibits should be ~1.0, "
              f"P25 control-channel varies by signalling content)")

    summary = dict(
        host=args.host, chain=args.chain, duration=args.duration,
        total_samples=int(raw.shape[0]),
        total_symbols=len(traces),
        counts={str(k): int(v) for k, v in counts.items()},
        per_symbol_stats={str(k): v for k, v in stats.items()},
    )
    with open(os.path.join(args.outdir, f"summary_{args.chain}.json"), "w") as f:
        json.dump(summary, f, indent=2)
    print(f"\nwrote {out_png}")
    print(f"wrote {os.path.join(args.outdir, f'summary_{args.chain}.json')}")


if __name__ == "__main__":
    main()
