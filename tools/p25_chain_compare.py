#!/usr/bin/env python3
"""p25_chain_compare.py -- Track 2 of the 2026-05-03 three-track plan.

Pair tool for `tools/p25_chain_forensics_capture.py`. Given a run dir
produced by the capture tool, runs `cargo test --release software_decode`
against `wideband.cs16` to produce SW dibits, then slide-aligns the SW
and HDL dibit streams to locate where they diverge.

The HDL chain decodes ~57 % of LDUs on the same wideband IQ where the
SW reference hits 98.96 % bit-exact agreement with SDRTrunk (see
project memory `project_2026_05_02_sw_demod_session_close.md` and
`project_2026_05_03_session_pickup_three_tracks.md`). The comparison
output is the diagnostic that pinpoints where in the LSM chain
(Costas / AGC / Gardner / Q-format conversions) the HDL diverges.

Usage:
  python tools/p25_chain_compare.py <run_dir>
      [--no-cargo]       # skip cargo run, expect sw_dibits.bits to exist
      [--halfband]       # use SOFTDEC_HALFBAND=1 (SDRTrunk-bit-exact oracle)
      [--multistage]     # use SOFTDEC_MULTISTAGE=1 (sharper Kaiser DDC)
      [--streaming]      # use SOFTDEC_STREAMING=1
      [--probe-len N]    # alignment probe length (default 1024)
      [--max-search N]   # max slide offset (default 16384)
      [--window N]       # per-window agreement size (default 200)
      [--threshold P]    # below P: flagged as divergent (default 0.85)
      [--ref-bits PATH]  # also diff against an SDRTrunk .bits reference

Outputs (all written into <run_dir>):
  sw_decoded.wav        -- SW pipeline's decoded audio
  sw_dibits.bits        -- SW dibits in SDRTrunk MSB-first packing
  sw_metrics.json       -- vector_stats for PLL/timing/AGC + counters
  compare_report.txt    -- alignment + per-window agreement summary
  compare_windows.csv   -- per-window data for plotting
"""
from __future__ import annotations

import argparse
import csv
import json
import os
import subprocess
import sys
from pathlib import Path

# Walk up from this file to find the repo root (parent of `tools/`).
# `software_decode` lives in the `p25-httpd` cargo crate; cargo needs
# to be invoked from there since the workspace root has no Cargo.toml.
REPO_ROOT = Path(__file__).resolve().parent.parent
CARGO_DIR = REPO_ROOT / "p25-httpd"

DIBIT_RATE = 4800.0


def unpack_dibits(packed: bytes) -> list[int]:
    """SDRTrunk-style 4-dibits-per-byte MSB-first unpacking."""
    out = []
    for b in packed:
        out.append((b >> 6) & 0x3)
        out.append((b >> 4) & 0x3)
        out.append((b >> 2) & 0x3)
        out.append(b & 0x3)
    return out


def slide_align(short: list[int], long: list[int],
                probe_len: int, max_search: int):
    """Find the offset in `long` at which `short[:probe_len]` matches
    best. Returns (best_offset, agreement_rate).
    """
    if len(short) < probe_len:
        probe_len = len(short)
    if probe_len <= 0:
        return 0, 0.0
    probe = short[:probe_len]
    upper = max(0, min(max_search, len(long) - probe_len))
    best_off = 0
    best_match = -1
    for off in range(upper + 1):
        m = 0
        for i in range(probe_len):
            if long[off + i] == probe[i]:
                m += 1
        if m > best_match:
            best_match = m
            best_off = off
    return best_off, best_match / probe_len


def per_window_agreement(a: list[int], b: list[int], window: int):
    n = min(len(a), len(b)) // window
    rows = []
    for w in range(n):
        s = w * window
        agree = 0
        for i in range(window):
            if a[s + i] == b[s + i]:
                agree += 1
        rows.append((w, s, agree, window))
    return rows


def detect_divergence_runs(rows, threshold):
    runs = []
    in_bad = False
    bad_start = 0
    bad_end = 0
    for (w, s, agree, win) in rows:
        rate = agree / win
        if rate < threshold:
            if not in_bad:
                in_bad = True
                bad_start = s
            bad_end = s + win
        else:
            if in_bad:
                in_bad = False
                runs.append((bad_start, bad_end))
    if in_bad:
        runs.append((bad_start, bad_end))
    return runs


def run_cargo_software_decode(meta, run_dir, halfband, multistage, streaming):
    cs16 = (run_dir / "wideband.cs16").resolve()
    if not cs16.exists():
        raise SystemExit(
            f"missing wideband capture: {cs16}\n"
            f"re-run forensics_capture without --no-wideband, or pass "
            f"--no-cargo and a pre-existing sw_dibits.bits.")
    rate = meta.get("wideband_rate_hz") or 4_000_000
    rx_lo = meta.get("rx_lo_hz")
    target = meta.get("target_freq_hz")
    if rx_lo is None or target is None:
        raise SystemExit("meta.json missing rx_lo_hz or target_freq_hz")

    env = os.environ.copy()
    env["SOFTDEC_INPUT"] = str(cs16)
    env["SOFTDEC_INPUT_RATE"] = str(rate)
    env["SOFTDEC_CENTER_HZ"] = str(int(rx_lo))
    env["SOFTDEC_TARGET_HZ"] = str(int(target))
    env["SOFTDEC_OUT_WAV"] = str((run_dir / "sw_decoded.wav").resolve())
    env["SOFTDEC_DIBITS_OUT"] = str((run_dir / "sw_dibits.bits").resolve())
    env["SOFTDEC_METRICS_JSON"] = str((run_dir / "sw_metrics.json").resolve())
    if halfband:
        env["SOFTDEC_HALFBAND"] = "1"
    if multistage:
        env["SOFTDEC_MULTISTAGE"] = "1"
    if streaming:
        env["SOFTDEC_STREAMING"] = "1"
    cmd = ["cargo", "test", "--release", "--bin", "p25-httpd",
           "software_decode", "--",
           "--exact", "lsm::software_decode_tests::software_decode",
           "--ignored", "--nocapture"]
    print(f"# running: {' '.join(cmd)}")
    print(f"#   cwd: {CARGO_DIR}")
    print(f"#   SOFTDEC_INPUT={cs16}")
    print(f"#   SOFTDEC_INPUT_RATE={rate}")
    print(f"#   SOFTDEC_CENTER_HZ={rx_lo}")
    print(f"#   SOFTDEC_TARGET_HZ={target}")
    r = subprocess.run(cmd, cwd=str(CARGO_DIR), env=env)
    if r.returncode != 0:
        raise SystemExit(f"cargo test failed (exit {r.returncode})")


def write_compare_report(run_dir, meta, hdl_dibits, sw_dibits,
                         offset_dir, best_off, agree_rate,
                         rows, threshold, divergence_runs,
                         hdl_lead, sw_lead):
    """offset_dir = 'sw_in_hdl' or 'hdl_in_sw' depending on which is the
    longer stream we slid into."""
    rep = run_dir / "compare_report.txt"
    with rep.open("w") as f:
        f.write(f"# p25_chain_compare report -- {meta.get('run_id')}\n")
        f.write(f"# build={meta.get('build_tag')}  tg={meta.get('target_tg')}"
                f"  freq={meta.get('target_freq_hz')} Hz\n")
        f.write(f"# encrypted={meta.get('encrypted')}\n")
        f.write(f"\n")
        f.write(f"hdl_dibits: {len(hdl_dibits)} ({len(hdl_dibits)/DIBIT_RATE:.2f}s)\n")
        f.write(f"sw_dibits:  {len(sw_dibits)} ({len(sw_dibits)/DIBIT_RATE:.2f}s)\n")
        f.write(f"alignment: {offset_dir} offset={best_off} "
                f"({best_off/DIBIT_RATE:.3f}s)  probe agreement="
                f"{agree_rate*100:.1f}%\n")
        if hdl_lead is not None:
            f.write(f"# HDL leads SW by {hdl_lead} dibits "
                    f"({hdl_lead/DIBIT_RATE:.3f}s) "
                    f"= HDL chain pre-grant history\n")
        if sw_lead is not None:
            f.write(f"# SW leads HDL by {sw_lead} dibits "
                    f"({sw_lead/DIBIT_RATE:.3f}s) "
                    f"= SW DDC settling on the wideband window\n")
        if agree_rate < 0.6:
            f.write("# WARNING: low alignment agreement -- alignment may be wrong;"
                    " expand --max-search or sanity-check inputs.\n")
        f.write(f"\n")
        n = len(rows)
        below = sum(1 for r in rows if r[2] / r[3] < threshold)
        f.write(f"windows: {n}  threshold={threshold:.2f}  "
                f"below_threshold={below}\n")
        f.write(f"\n")
        f.write(f"{'idx':>5s} {'sym':>8s} {'time':>7s} "
                f"{'agree':>7s} {'rate':>6s}  marker\n")
        for (w, s, agree, win) in rows:
            rate = agree / win
            marker = "<<<" if rate < threshold else ""
            f.write(f"{w:>5d} {s:>8d} {s/DIBIT_RATE:>6.2f}s "
                    f"{agree:>4d}/{win:<3d} "
                    f"{rate*100:>5.1f}%  {marker}\n")
        if divergence_runs:
            f.write(f"\n# divergence runs (sym_start, sym_end, time):\n")
            for (s0, s1) in divergence_runs:
                f.write(f"   {s0:>7d} - {s1:>7d}   "
                        f"{s0/DIBIT_RATE:>6.2f}s - "
                        f"{s1/DIBIT_RATE:>6.2f}s\n")
    print(f"# wrote {rep}")


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("run_dir", type=Path,
                    help="forensics run dir (output of "
                         "p25_chain_forensics_capture.py)")
    ap.add_argument("--no-cargo", action="store_true",
                    help="skip cargo run; expect sw_dibits.bits to exist")
    ap.add_argument("--halfband", action="store_true",
                    help="use SOFTDEC_HALFBAND=1 (SDRTrunk-bit-exact oracle)")
    ap.add_argument("--multistage", action="store_true",
                    help="use SOFTDEC_MULTISTAGE=1 (sharper Kaiser DDC)")
    ap.add_argument("--streaming", action="store_true",
                    help="use SOFTDEC_STREAMING=1")
    ap.add_argument("--probe-len", type=int, default=1024)
    ap.add_argument("--max-search", type=int, default=16384)
    ap.add_argument("--window", type=int, default=200,
                    help="per-window agreement size (default 200 dibits = ~42 ms)")
    ap.add_argument("--threshold", type=float, default=0.85,
                    help="windows below this rate flagged as divergent")
    ap.add_argument("--ref-bits", type=Path, default=None,
                    help="optional SDRTrunk reference .bits to diff against "
                         "(runs the same alignment against ref instead)")
    args = ap.parse_args()

    if not args.run_dir.is_dir():
        print(f"run dir not found: {args.run_dir}", file=sys.stderr)
        return 1
    meta_path = args.run_dir / "meta.json"
    if not meta_path.is_file():
        print(f"missing meta.json in {args.run_dir}", file=sys.stderr)
        return 1
    meta = json.loads(meta_path.read_text())
    print(f"# run {meta.get('run_id')}  tg={meta.get('target_tg')}  "
          f"freq={meta.get('target_freq_hz')}")

    hdl_path = args.run_dir / "hdl_dibits.bits"
    sw_path = args.run_dir / "sw_dibits.bits"
    if not hdl_path.is_file():
        print(f"missing {hdl_path}", file=sys.stderr)
        return 1

    if args.halfband and args.multistage:
        print("# warning: both --halfband and --multistage set; "
              "halfband path takes precedence in the cargo test "
              "(see software_decode_tests.rs).", file=sys.stderr)

    if not args.no_cargo:
        run_cargo_software_decode(
            meta, args.run_dir,
            args.halfband, args.multistage, args.streaming)
    if not sw_path.is_file():
        print(f"missing {sw_path} after cargo (or --no-cargo "
              f"with no pre-existing dibits)", file=sys.stderr)
        return 1

    hdl_dibits = unpack_dibits(hdl_path.read_bytes())
    sw_dibits = unpack_dibits(sw_path.read_bytes())
    print(f"# hdl: {len(hdl_dibits)} dibits "
          f"({len(hdl_dibits)/DIBIT_RATE:.2f}s)")
    print(f"# sw:  {len(sw_dibits)} dibits "
          f"({len(sw_dibits)/DIBIT_RATE:.2f}s)")

    # Slide whichever is shorter into the longer one. Common case: HDL
    # has more dibits than SW because we polled the rolling buffer
    # before the capture window started, while SW only sees the cs16
    # window. The diff tool's design assumed SDRTrunk reference (short)
    # inside our long stream; we mirror that.
    if len(hdl_dibits) <= len(sw_dibits):
        short = hdl_dibits
        long_ = sw_dibits
        offset_dir = "hdl_in_sw"
    else:
        short = sw_dibits
        long_ = hdl_dibits
        offset_dir = "sw_in_hdl"

    best_off, agree_rate = slide_align(
        short, long_, args.probe_len, args.max_search)
    print(f"# alignment ({offset_dir}): offset={best_off} "
          f"({best_off/DIBIT_RATE:.3f}s) probe={agree_rate*100:.1f}%")

    if offset_dir == "hdl_in_sw":
        hdl_a = hdl_dibits
        sw_a = sw_dibits[best_off:best_off + len(hdl_dibits)]
        hdl_lead = None
        sw_lead = best_off
    else:
        sw_a = sw_dibits
        hdl_a = hdl_dibits[best_off:best_off + len(sw_dibits)]
        hdl_lead = best_off
        sw_lead = None

    overlap = min(len(hdl_a), len(sw_a))
    hdl_a = hdl_a[:overlap]
    sw_a = sw_a[:overlap]

    rows = per_window_agreement(hdl_a, sw_a, args.window)
    runs = detect_divergence_runs(rows, args.threshold)

    # Persist a CSV for plotting.
    csv_path = args.run_dir / "compare_windows.csv"
    with csv_path.open("w", newline="") as fh:
        wr = csv.writer(fh)
        wr.writerow(["idx", "sym_start", "time_s", "agree", "window", "rate"])
        for (w, s, agree, win) in rows:
            wr.writerow([w, s, round(s/DIBIT_RATE, 3),
                         agree, win, round(agree/win, 4)])
    print(f"# wrote {csv_path}")

    write_compare_report(args.run_dir, meta, hdl_dibits, sw_dibits,
                         offset_dir, best_off, agree_rate,
                         rows, args.threshold, runs,
                         hdl_lead, sw_lead)

    n = len(rows)
    below = sum(1 for r in rows if r[2] / r[3] < args.threshold)
    pct = 100.0 * below / n if n else 0.0
    print(f"# windows={n} below_threshold={below} ({pct:.1f}%)")
    print(f"# divergence_runs={len(runs)}")
    for (s0, s1) in runs[:8]:
        print(f"   {s0:>7d} - {s1:>7d}   "
              f"{s0/DIBIT_RATE:>6.2f}s - {s1/DIBIT_RATE:>6.2f}s")

    if args.ref_bits is not None:
        # Bonus: diff SW against an SDRTrunk reference too. Reuses the
        # same machinery; written to compare_ref_*.* so it doesn't clobber
        # the HDL-vs-SW report.
        ref_dibits = unpack_dibits(args.ref_bits.read_bytes())
        if len(sw_dibits) <= len(ref_dibits):
            ss, ll, dirn = sw_dibits, ref_dibits, "sw_in_ref"
        else:
            ss, ll, dirn = ref_dibits, sw_dibits, "ref_in_sw"
        bo, ar = slide_align(ss, ll, args.probe_len, args.max_search)
        if dirn == "sw_in_ref":
            sw_b = sw_dibits
            ref_b = ref_dibits[bo:bo + len(sw_dibits)]
        else:
            ref_b = ref_dibits
            sw_b = sw_dibits[bo:bo + len(ref_dibits)]
        ov = min(len(sw_b), len(ref_b))
        sw_b = sw_b[:ov]
        ref_b = ref_b[:ov]
        rows2 = per_window_agreement(sw_b, ref_b, args.window)
        below2 = sum(1 for r in rows2 if r[2] / r[3] < args.threshold)
        print(f"# SW-vs-REF: {dirn} offset={bo} "
              f"probe={ar*100:.1f}% below_threshold={below2}/{len(rows2)}")
        with (args.run_dir / "compare_ref_windows.csv").open(
                "w", newline="") as fh:
            wr = csv.writer(fh)
            wr.writerow(["idx", "sym_start", "time_s", "agree", "window", "rate"])
            for (w, s, agree, win) in rows2:
                wr.writerow([w, s, round(s/DIBIT_RATE, 3),
                             agree, win, round(agree/win, 4)])

    return 0


if __name__ == "__main__":
    sys.exit(main())
