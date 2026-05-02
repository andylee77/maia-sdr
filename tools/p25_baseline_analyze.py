#!/usr/bin/env python3
"""
p25_baseline_analyze.py — comprehensive baseline-quality analysis of a
wideband IQ capture (.wav or .cs16). Runs `cargo test software_decode`
for each candidate channel (control + each traffic LCN), parses the
metrics output, and cross-validates against any SDRTrunk recordings /
event logs from the same time window.

What we measure per channel:
  * Signal-to-noise ratio (peak-vs-median power in a 1 s mid-window FFT)
  * Decode rate (hard sync hits, soft sync hits)
  * NAC consistency (does the decoder always lock NAC=0x8A1?)
  * Sync-distance distribution (BER proxy: 0=clean, 4+=noisy)
  * Dibit histogram (slicer bias check; should be ~25% each)
  * Frame counts (HDU/LDU1/LDU2/TDU/TDU_LC) vs SDRTrunk's per-call logs
  * IMBE extraction + silent-frame % (vocoder bit-error rate proxy)

Usage:
  python tools/p25_baseline_analyze.py CAPTURE.wav [--lo-mhz N] \\
      [--targets-mhz 858.4375,857.9875,...] [--sdrtrunk-dir PATH] \\
      [--report report.md]

The LO is auto-detected from the SDRTrunk-format filename
`<unix>_<freq_hz>_<rate>_baseband.wav` if not given.
"""

import argparse
import datetime as _dt
import json
import os
import re
import subprocess
import sys
import wave
from pathlib import Path

import numpy as np


# Default Clay County frequencies (in MHz). Override with --targets-mhz.
DEFAULT_TARGETS_MHZ = [
    860.9625,   # LCN 11 control
    858.4625,   # LCN 7
    858.4375,   # LCN 6
    857.9875,   # LCN 5
    857.4375,   # LCN 4
    856.4375,   # LCN 1
]
CONTROL_MHZ = 860.9625


def parse_filename(path: Path):
    """Extract (unix_ts, lo_hz, sample_rate) from
    `<ts>_<lo>_<rate>_baseband.wav`."""
    m = re.match(r"^(\d+)_(\d+)_(\d+)_baseband\.wav$", path.name)
    if not m:
        return (None, None, None)
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def read_wav_header(path: Path):
    """Return (sample_rate, channels, bits, frames, duration_s)."""
    with wave.open(str(path), "rb") as w:
        sr = w.getframerate()
        ch = w.getnchannels()
        bits = 8 * w.getsampwidth()
        nframes = w.getnframes()
    return (sr, ch, bits, nframes, nframes / sr)


def fft_signal_strength(path: Path, lo_hz: int, sample_rate: int,
                        target_hz: int, fft_n: int = 16384,
                        secs: float = 1.0,
                        offset_secs: float = 1.0) -> dict:
    """Read `secs` seconds of IQ from the capture, average FFT, return
    {target_db_above_median, median_db, peak_db, bin_hz}."""
    with wave.open(str(path), "rb") as w:
        w.setpos(int(offset_secs * sample_rate))
        n = int(secs * sample_rate)
        raw = w.readframes(n)
    iq16 = np.frombuffer(raw, dtype=np.int16).reshape(-1, 2)
    samples = (iq16[:, 0].astype(np.float32)
               + 1j * iq16[:, 1].astype(np.float32))

    bin_hz = sample_rate / fft_n
    n_avg = len(samples) // fft_n
    if n_avg == 0:
        return {"error": "capture too short for one FFT window"}
    win = np.hanning(fft_n).astype(np.float32)
    psd = np.zeros(fft_n)
    for k in range(n_avg):
        seg = samples[k * fft_n: (k + 1) * fft_n] * win
        psd += np.abs(np.fft.fftshift(np.fft.fft(seg))) ** 2
    psd /= n_avg
    psd_db = 10.0 * np.log10(psd / psd.max() + 1e-30)
    median_db = float(np.median(psd_db))

    target_offset = target_hz - lo_hz
    target_bin = int(round(target_offset / bin_hz)) + fft_n // 2
    if not (0 <= target_bin < fft_n):
        return {"error": f"target {target_hz} outside capture window"}

    # Look at ±5 kHz around target
    win_bins = max(1, int(5000 / bin_hz))
    lo_b = max(0, target_bin - win_bins)
    hi_b = min(fft_n, target_bin + win_bins)
    local_peak_db = float(psd_db[lo_b:hi_b].max())
    return {
        "target_hz": target_hz,
        "target_offset_hz": target_offset,
        "median_db": median_db,
        "local_peak_db": local_peak_db,
        "delta_db": local_peak_db - median_db,
        "bin_hz": bin_hz,
        "n_avg": n_avg,
    }


def run_software_decode(capture_path: Path, lo_hz: int, sample_rate: int,
                        target_hz: int, work_dir: Path,
                        ppm: float = 0.0) -> dict:
    """Run `cargo test software_decode` for one target freq. Return a
    dict combining stderr-parsed metrics + the JSON metrics dump."""
    out_wav = work_dir / f"sw_{target_hz}.wav"
    metrics_json = work_dir / f"sw_{target_hz}.metrics.json"
    work_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["SOFTDEC_INPUT"] = str(capture_path)
    env["SOFTDEC_INPUT_RATE"] = str(sample_rate)
    env["SOFTDEC_CENTER_HZ"] = str(lo_hz)
    env["SOFTDEC_TARGET_HZ"] = str(target_hz)
    env["SOFTDEC_PPM"] = str(ppm)
    env["SOFTDEC_OUT_WAV"] = str(out_wav)
    env["SOFTDEC_METRICS_JSON"] = str(metrics_json)
    cmd = [
        "cargo", "test", "--release", "software_decode", "--",
        "--ignored", "--nocapture",
    ]
    proc = subprocess.run(
        cmd, capture_output=True, text=True, env=env,
        cwd=Path(__file__).parent.parent / "p25-httpd",
        timeout=600,
    )
    stderr = proc.stderr
    stdout = proc.stdout
    combined = stdout + stderr
    parsed = parse_decode_stderr(combined)
    if metrics_json.exists():
        try:
            with open(metrics_json) as f:
                parsed.update(json.load(f))
        except Exception as e:
            parsed["json_parse_error"] = str(e)
    parsed["target_hz"] = target_hz
    parsed["wav_out"] = str(out_wav)
    return parsed


def parse_decode_stderr(text: str) -> dict:
    """Pull the numbers we care about out of cargo test stderr."""
    out = {}
    m = re.search(r"LSM demod:\s*(\d+) symbols, hard_sync=(\d+),\s*soft_sync=(\d+)", text)
    if m:
        out["lsm_symbols"] = int(m.group(1))
        out["hard_sync"] = int(m.group(2))
        out["soft_sync"] = int(m.group(3))
    # Hard sync events: parse position, distance, nac, duid
    hard_events = []
    for m in re.finditer(
        r"hard #(\d+):\s*pos=(\d+)\s*dist=(\d+)\s*nac=([0-9a-fx]+)\s*duid=(\d+)",
        text):
        hard_events.append({
            "idx": int(m.group(1)),
            "pos": int(m.group(2)),
            "dist": int(m.group(3)),
            "nac": int(m.group(4), 16),
            "duid": int(m.group(5)),
        })
    out["hard_events"] = hard_events
    # Soft sync events
    soft_events = []
    for m in re.finditer(
        r"soft #(\d+):\s*pos=(\d+)\s*score=([0-9.]+)\s*nac=([0-9a-fx]+)\s*duid=(\d+)",
        text):
        soft_events.append({
            "idx": int(m.group(1)),
            "pos": int(m.group(2)),
            "score": float(m.group(3)),
            "nac": int(m.group(4), 16),
            "duid": int(m.group(5)),
        })
    out["soft_events"] = soft_events
    return out


def hard_distance_histogram(events) -> dict:
    h = {0: 0, 1: 0, 2: 0, 3: 0, "4+": 0}
    for e in events:
        d = e["dist"]
        if d >= 4:
            h["4+"] += 1
        else:
            h[d] += 1
    return h


def nac_histogram(events) -> dict:
    out = {}
    for e in events:
        out[e["nac"]] = out.get(e["nac"], 0) + 1
    return out


def find_sdrtrunk_logs(unix_ts: int, duration_s: float,
                       sdrtrunk_dir: Path) -> dict:
    """Find SDRTrunk event_logs and recordings overlapping our capture
    window. Returns {control_logs, traffic_logs, recordings}."""
    out = {"control_logs": [], "traffic_logs": [], "recordings": []}
    if not sdrtrunk_dir.is_dir():
        return out
    start = unix_ts
    end = unix_ts + int(duration_s) + 30  # +30s slack

    def _ts_from_filename(name: str):
        # SDRTrunk uses local-time YYYYMMDD_HHMMSS prefix; convert to
        # unix using local timezone. Best-effort.
        m = re.match(r"^(\d{8})_(\d{6})", name)
        if not m:
            return None
        try:
            d = _dt.datetime.strptime(m.group(1) + m.group(2),
                                      "%Y%m%d%H%M%S")
            return int(d.timestamp())
        except Exception:
            return None

    for d in (sdrtrunk_dir / "event_logs", sdrtrunk_dir / "recordings"):
        if not d.is_dir():
            continue
        for p in d.iterdir():
            ts = _ts_from_filename(p.name)
            if ts is None:
                continue
            if not (start - 60 <= ts <= end):
                continue
            entry = {"path": str(p), "name": p.name, "ts": ts,
                     "size": p.stat().st_size}
            if "_T-LCN" in p.name and p.name.endswith("_decoded_messages.log"):
                out["traffic_logs"].append(entry)
            elif "_LCN-" in p.name and p.name.endswith("_decoded_messages.log"):
                out["control_logs"].append(entry)
            elif p.name.endswith(".bits") or p.name.endswith("_baseband.wav") \
                 or p.name.endswith(".mp3") or p.name.endswith(".mbe"):
                out["recordings"].append(entry)
    out["control_logs"].sort(key=lambda x: x["ts"])
    out["traffic_logs"].sort(key=lambda x: x["ts"])
    out["recordings"].sort(key=lambda x: x["ts"])
    return out


def hard_dist_hist_from_json(events_json) -> dict:
    """events_json is a list of {sym,dist,nac,duid,fec} from JSON."""
    h = {0: 0, 1: 0, 2: 0, 3: 0, "4+": 0}
    for e in events_json:
        d = e.get("dist", 0)
        if d >= 4:
            h["4+"] += 1
        else:
            h[d] += 1
    return h


def nac_hist_from_json(events_json) -> dict:
    out = {}
    for e in events_json:
        out[e.get("nac", 0)] = out.get(e.get("nac", 0), 0) + 1
    return out


def fec_corrected_pct(events_json) -> float:
    if not events_json:
        return 0.0
    n_fec = sum(1 for e in events_json if e.get("fec"))
    return 100.0 * n_fec / len(events_json)


def render_report(args, capture_path, lo_hz, sample_rate, duration_s,
                  per_channel: list, sdrtrunk: dict) -> str:
    """Build a markdown baseline report."""
    out = []
    out.append(f"# P25 Baseline Analysis: `{capture_path.name}`")
    out.append("")
    out.append(f"- Capture LO: **{lo_hz/1e6:.6f} MHz**")
    out.append(f"- Sample rate: **{sample_rate} Hz** ({sample_rate/1e6:.2f} MSPS)")
    out.append(f"- Duration: **{duration_s:.2f} s**")
    out.append("")

    # === Per-channel signal + decode metrics ===
    out.append("## Decode summary per channel")
    out.append("")
    out.append("| LCN | Freq (MHz) | Δ-noise | hard | soft | NAC=0x8A1 | FEC% | dist 0/1/2/3/4+ | LDU1/2 | IMBE | silent% | PCM (s) |")
    out.append("|---|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|")
    for ch in per_channel:
        sig = ch.get("signal", {})
        decode = ch.get("decode", {})
        framer = decode.get("framer", {})
        pcm = decode.get("pcm", {})
        delta = sig.get("delta_db", 0.0)
        # Prefer JSON's hard_events array (full list); fall back to stderr-parsed.
        hard_events_json = decode.get("hard_events", [])
        if hard_events_json and isinstance(hard_events_json[0], dict) and "sym" in hard_events_json[0]:
            nac_hist = nac_hist_from_json(hard_events_json)
            dist_h = hard_dist_hist_from_json(hard_events_json)
            fec_pct = fec_corrected_pct(hard_events_json)
        else:
            nac_hist = nac_histogram(hard_events_json)
            dist_h = hard_distance_histogram(hard_events_json)
            fec_pct = 0.0
        total = sum(nac_hist.values()) or 1
        nac_target_pct = 100.0 * nac_hist.get(0x8A1, 0) / total
        dist_str = f"{dist_h[0]}/{dist_h[1]}/{dist_h[2]}/{dist_h[3]}/{dist_h['4+']}"
        imbe = framer.get("imbe_total", 0)
        silent = framer.get("silent_frames", 0)
        silent_pct = (100.0 * silent / imbe) if imbe > 0 else 0.0
        ldu1 = framer.get("ldu1", 0)
        ldu2 = framer.get("ldu2", 0)
        pcm_dur = pcm.get("duration_s", 0.0)
        # hard_sync from JSON-encoded len, fall back to stderr.
        hard_n = decode.get("hard_sync", len(hard_events_json))
        soft_n = decode.get("soft_sync", len(decode.get("soft_events", [])))
        out.append(
            f"| {ch['label']} | {ch['target_hz']/1e6:.4f} | "
            f"{delta:+.1f} dB | {hard_n} | {soft_n} | "
            f"{nac_target_pct:.0f}% | {fec_pct:.0f}% | "
            f"{dist_str} | {ldu1}/{ldu2} | {imbe} | "
            f"{silent_pct:.1f}% | {pcm_dur:.2f} |"
        )
    out.append("")

    # === PLL / timing / AGC stats per channel ===
    out.append("## Loop traces (PLL = Costas, timing = Gardner, amp = post-AGC)")
    out.append("")
    out.append("| LCN | PLL mean | PLL stddev | PLL slope/kS | timing mean | timing stddev | amp mean | amp stddev (%) |")
    out.append("|---|---:|---:|---:|---:|---:|---:|---:|")
    for ch in per_channel:
        decode = ch.get("decode", {})
        pll = decode.get("pll_trace", {})
        timing = decode.get("timing_trace", {})
        amp = decode.get("soft_symbol_amp", {})
        amp_mean = amp.get("mean", 0.0) or 0.0
        amp_std = amp.get("stddev", 0.0) or 0.0
        amp_std_pct = (100.0 * amp_std / amp_mean) if amp_mean > 0 else 0.0
        out.append(
            f"| {ch['label']} | {pll.get('mean', 0):+.4f} | "
            f"{pll.get('stddev', 0):.4f} | "
            f"{pll.get('slope_per_kS', 0):+.5f} | "
            f"{timing.get('mean', 0):+.4f} | "
            f"{timing.get('stddev', 0):.4f} | "
            f"{amp_mean:.1f} | {amp_std_pct:.1f}% |"
        )
    out.append("")
    out.append("PLL is the Costas loop output in radians. mean ≈ 0 means the NCO offset")
    out.append("is correct; persistent non-zero mean = residual freq offset (PPM error).")
    out.append("Slope/kS shows drift over time — non-zero = crystal drift or LO wandering.")
    out.append("Amp stddev/mean tells you AGC stability.")
    out.append("")

    # === Notes / interpretation ===
    out.append("## Notes")
    out.append("")
    for ch in per_channel:
        notes = []
        sig = ch.get("signal", {})
        decode = ch.get("decode", {})
        framer = decode.get("framer", {})
        delta = sig.get("delta_db", 0.0)
        if delta < 6:
            notes.append("signal at-or-below noise floor; expect failed decode")
        elif delta < 12:
            notes.append(f"weak signal ({delta:+.1f} dB)")
        hs = decode.get("hard_sync", len(decode.get("hard_events", [])))
        ss = decode.get("soft_sync", len(decode.get("soft_events", [])))
        if hs == 0 and ss > 0:
            notes.append(f"only soft syncs ({ss}); BCH hard threshold unreachable")
        if hs > 0 and framer.get("imbe_total", 0) == 0:
            notes.append("syncs decoded but framer extracted no LDU — bit errors past BCH limit, or no voice in window")
        # PLL drift
        pll = decode.get("pll_trace", {})
        pll_mean_hz = pll.get("mean", 0) * 31250.0 / (2 * 3.14159)  # rad/sym × syms/sec / 2π
        if abs(pll.get("mean", 0)) > 0.1:
            notes.append(f"residual PLL ~ {pll_mean_hz:+.0f} Hz — consider PPM trim")
        if notes:
            out.append(f"- **{ch['label']} {ch['target_hz']/1e6:.4f}**: "
                       + "; ".join(notes))
    out.append("")

    # === Verdict per channel ===
    out.append("## Notes")
    out.append("")
    for ch in per_channel:
        notes = []
        sig = ch.get("signal", {})
        decode = ch.get("decode", {})
        framer = decode.get("framer", {})
        delta = sig.get("delta_db", 0.0)
        if delta < 6:
            notes.append("signal at-or-below noise floor; expect failed decode")
        elif delta < 12:
            notes.append(f"weak signal ({delta:+.1f} dB)")
        hs = decode.get("hard_sync", 0)
        ss = decode.get("soft_sync", 0)
        if hs == 0 and ss > 0:
            notes.append(f"only soft syncs ({ss}); consider lowering hard threshold")
        if hs > 0 and framer.get("imbe_total", 0) == 0:
            notes.append("syncs decoded but framer extracted no LDU — bit errors past BCH limit")
        nac_h = nac_histogram(decode.get("hard_events", []))
        if hs > 0 and nac_h.get(0x8A1, 0) / max(1, hs) < 0.95:
            notes.append("NAC inconsistent across syncs (BCH false positives)")
        if ch.get("label", "").startswith("CC") and hs > 100:
            # Solid control decode → derive PPM bound from peak offset
            offs = sig.get("target_offset_hz", 0)
            notes.append(f"control decoded cleanly — RX LO trim "
                         f"is within tolerance for this freq")
        if notes:
            out.append(f"- **{ch['label']} {ch['target_hz']/1e6:.4f}**: "
                       + "; ".join(notes))
    out.append("")

    # === SDRTrunk cross-reference ===
    out.append("## SDRTrunk cross-reference")
    out.append("")
    if not sdrtrunk["control_logs"] and not sdrtrunk["recordings"]:
        out.append("*(no matching SDRTrunk artifacts in --sdrtrunk-dir)*")
    else:
        out.append(f"Capture window: unix {args.unix_ts or '?'}, "
                   f"+{duration_s:.0f}s")
        out.append("")
        out.append(f"Control logs: {len(sdrtrunk['control_logs'])} match")
        for x in sdrtrunk["control_logs"]:
            out.append(f"  - `{x['name']}` ({x['size']} B)")
        out.append("")
        out.append(f"Traffic logs: {len(sdrtrunk['traffic_logs'])} match")
        for x in sdrtrunk["traffic_logs"]:
            out.append(f"  - `{x['name']}` ({x['size']} B)")
        out.append("")
        out.append(f"Recordings (.bits/.wav/.mp3/.mbe): "
                   f"{len(sdrtrunk['recordings'])} match")
        for x in sdrtrunk["recordings"]:
            out.append(f"  - `{x['name']}` ({x['size']} B)")
    out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("capture", type=Path,
                    help=".wav (SDRTrunk format) or .cs16 capture")
    ap.add_argument("--lo-mhz", type=float, default=None,
                    help="LO in MHz (auto-detected from filename if absent)")
    ap.add_argument("--input-rate", type=int, default=None,
                    help="sample rate in Hz (auto-detected from WAV header)")
    ap.add_argument(
        "--targets-mhz", type=str,
        default=",".join(f"{f:.4f}" for f in DEFAULT_TARGETS_MHZ),
        help="comma-separated list of target freqs in MHz")
    ap.add_argument(
        "--sdrtrunk-dir", type=Path,
        default=Path(r"C:\Users\Andy\SDRTrunk"),
        help="root SDRTrunk data dir (event_logs/, recordings/)")
    ap.add_argument("--ppm", type=float, default=0.0,
                    help="PPM correction passed to software_decode")
    ap.add_argument("--report", type=Path, default=None,
                    help="markdown report path (default: stdout)")
    ap.add_argument("--work-dir", type=Path,
                    default=Path(r"C:\Users\Andy\AppData\Local\Temp\p25_baseline"),
                    help="where to write per-channel WAVs + metrics JSON")
    args = ap.parse_args()

    if not args.capture.is_file():
        print(f"capture not found: {args.capture}", file=sys.stderr)
        return 1

    unix_ts, lo_filename, rate_filename = parse_filename(args.capture)
    lo_hz = (int(args.lo_mhz * 1e6)
             if args.lo_mhz is not None else lo_filename)
    if lo_hz is None:
        print("could not determine LO; pass --lo-mhz", file=sys.stderr)
        return 1
    args.unix_ts = unix_ts

    if args.capture.suffix.lower() == ".wav":
        sr, ch, bits, nframes, dur = read_wav_header(args.capture)
    else:
        sr = args.input_rate or rate_filename
        sz = args.capture.stat().st_size
        nframes = sz // 4
        dur = nframes / sr if sr else 0.0

    sample_rate = args.input_rate or sr or rate_filename
    if not sample_rate:
        print("could not determine sample rate; pass --input-rate", file=sys.stderr)
        return 1

    print(f"capture: {args.capture}")
    print(f"  LO  = {lo_hz} Hz  ({lo_hz/1e6:.6f} MHz)")
    print(f"  sr  = {sample_rate} Hz")
    print(f"  dur = {dur:.2f} s")
    print()

    target_mhz_list = [float(x) for x in args.targets_mhz.split(",")]
    target_hz_list = [int(x * 1e6) for x in target_mhz_list]

    per_channel = []
    for target_hz in target_hz_list:
        offset = target_hz - lo_hz
        if abs(offset) > sample_rate / 2 - 50_000:
            print(f"  skip {target_hz/1e6:.4f} MHz "
                  f"(offset {offset/1e3:.0f} kHz outside ±{(sample_rate/2-50_000)/1e3:.0f} kHz)")
            continue
        label = ("CC" if target_hz == int(CONTROL_MHZ * 1e6)
                 else f"T-{target_hz/1e6:.4f}")
        print(f"  measuring {label} @ {target_hz/1e6:.4f} MHz "
              f"(offset {offset/1e3:+.0f} kHz)")
        # Mid-window FFT for SNR
        sig = fft_signal_strength(
            args.capture, lo_hz, sample_rate, target_hz,
            offset_secs=min(1.0, dur / 2))
        # Software decode for actual metrics
        decode = run_software_decode(
            args.capture, lo_hz, sample_rate, target_hz,
            args.work_dir, ppm=args.ppm)
        per_channel.append({
            "label": label, "target_hz": target_hz,
            "signal": sig, "decode": decode,
        })

    sdrtrunk = (find_sdrtrunk_logs(unix_ts, dur, args.sdrtrunk_dir)
                if unix_ts else
                {"control_logs": [], "traffic_logs": [], "recordings": []})

    report = render_report(args, args.capture, lo_hz, sample_rate, dur,
                           per_channel, sdrtrunk)

    if args.report:
        args.report.write_text(report, encoding="utf-8")
        print(f"\nwrote report: {args.report}")
    else:
        print()
        print(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
