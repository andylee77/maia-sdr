#!/usr/bin/env python3
"""Replay IQ through the traffic LSM chain: bit-true front end + the real gateware.

Front end in numpy, matching the HDL arithmetic:

- DDC (8 MSPS captures only): NCO + the three FIR stages of p25-httpd's
  ``PRESET_8M`` (coefficients parsed from ``p25-httpd/src/hardware/ddc_presets.rs``),
  normalised to unity DC gain -> 50 kSPS;
- ``LsmDecimator2``: even samples;
- ``LsmFir(LPF_TAPS_25K)`` and ``LsmFir(RRC_TAPS_25K)``: taps quantised to
  Q1.17 as ``LsmFir`` does, products summed exactly, arithmetic shift by 17,
  saturated to int16.

Then the post-RRC samples drive the Amaranth ``LsmDemod`` (DC blockers, AGC,
timing interpolator + Gardner, CORDIC diff demod, PLL, slicer, sync + NID) in
the Amaranth simulator, one strobe every ``--cycles`` clocks. Per symbol it
records ``pll_dbg``, ``sample_point_dbg``, ``agc_gain_dbg``, ``agc_mag_dbg``,
the AGC gate count, the PLL/timing hold flag (when the gateware has one) and
the dibit; frame syncs are found in the dibit stream (48-bit pattern, <= 4 bit
errors) with the four quadrant rotations tried separately, so a chain whose
PLL sits a quadrant away shows up as rotated syncs.

Inputs: ``--npy`` (complex 50 kSPS, e.g. a DDC dump), ``--wav`` (SDRTrunk
channel recording, 50 kSPS) or ``--cs16`` (8 MSPS capture; ``--centre`` and
``--freq`` select the channel, ``--ref-ppm`` is the receiver's reference error).

Scenario building (no resets are added by any of these):

- every signal part is scaled to ``--rms`` (RMS of its louder half, Q1.15 at
  50 kSPS); noise parts are complex white Gaussian at ``--noise-db`` below that;
- ``--prepend-noise S``: S seconds of noise first;
- ``--gap-at T:S``: insert S seconds of noise at T seconds into the primary
  input (repeatable), i.e. a carrier gap inside the same transmission;
- ``--append-wav``/``--append-npy`` (+ ``--append-start``/``--append-seconds``)
  with ``--gap S``: a second recording after S seconds of noise, i.e. the next
  transmission on the same channel.

The summary has one row per part (signal or gap) with syncs per rotation, valid
NIDs, the first rotation-0 sync after the part starts (acquisition), PLL
start/end/min/max, the share of symbols gated by the AGC and held.

Gateware selection: ``--hdl-root DIR`` simulates the ``p25_hdl`` package in DIR
(e.g. an export of an older commit, for before/after runs); ``--legacy`` builds
the working-tree gateware with the no-signal hold removed and the pi/3 clamp
(bit-identical to the pre-hold design); ``--hold-enter``/``--hold-exit``/
``--pll-clamp-q13`` override the hold hysteresis and PLL clamp.

Examples:
  python tools/p25_lsm_hdl_replay.py --wav REC.wav --seconds 3 --out run.json
  python tools/p25_lsm_hdl_replay.py --cs16 a_tone.cs16 --centre 858100000 \\
      --freq 858437500 --ref-ppm -0.548 --seconds 3 --pll-seed 8579
  python tools/p25_lsm_hdl_replay.py --wav A.wav --seconds 1.5 --gap 3 \\
      --append-wav B.wav --append-seconds 1.5 --noise-db 20 --rms 1230
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
from pathlib import Path

import numpy as np

REPO = Path(__file__).resolve().parents[1]

FS_DDC = 50000.0
FS_LSM = 25000.0
SYNC = 0x5575F5FF77FF
# p25 dibit -> C4FM symbol (+3 +1 -1 -3): 01 +3, 00 +1, 10 -1, 11 -3. A PLL a
# quadrant off maps every dibit through one of these rotations.
ROTATIONS = {0: (0, 1, 2, 3), 1: (1, 3, 0, 2), 2: (3, 2, 1, 0), 3: (2, 0, 3, 1)}
LEGACY_CLAMP_Q13 = 8579  # round(pi/3 * 8192), the pre-2026-09-27 MAX_PLL_ABS_Q13


def use_hdl(root: str | Path | None = None) -> None:
    """Put the gateware package on ``sys.path`` (first import wins)."""
    p = str(Path(root) if root else REPO / "maia-hdl")
    if p not in sys.path:
        sys.path.insert(0, p)


# ---------------------------------------------------------------------------
# Front end
# ---------------------------------------------------------------------------


def ddc_8m(x: np.ndarray, fs: float, f_off: float) -> np.ndarray:
    from scipy.signal import lfilter

    src = (REPO / "p25-httpd/src/hardware/ddc_presets.rs").read_text(encoding="utf-8")

    def arr(name: str) -> np.ndarray:
        m = re.search(r"const %s: &\[i32\] = &\[(.*?)\];" % name, src, re.S)
        return np.array([int(v) for v in re.findall(r"-?\d+", m.group(1))], dtype=float)
    if abs(fs - 8e6) > 1:
        raise SystemExit("the DDC model covers the 8M preset only")
    y = x * np.exp(-2j * np.pi * f_off / fs * np.arange(x.size))
    for name, d in (("P25_FIR1_8M", 8), ("P25_FIR2_8M", 4), ("P25_FIR3_8M", 5)):
        h = arr(name)
        y = lfilter(h / h.sum(), 1, y)[::d]
    return y


def to_q15(x: np.ndarray) -> np.ndarray:
    return np.clip(np.round(x), -32768, 32767).astype(np.int64)


def lsm_fir(re: np.ndarray, im: np.ndarray, taps: list[float]) -> tuple[np.ndarray, np.ndarray]:
    """``LsmFir``: Q1.17 taps, exact accumulate, >> 17 (floor), saturate to int16."""
    q = np.array([max(-(1 << 17), min((1 << 17) - 1, int(round(t * (1 << 17))))) for t in taps],
                 dtype=np.int64)

    def one(x: np.ndarray) -> np.ndarray:
        acc = np.convolve(x, q)[:x.size]  # causal, index 0 = newest tap first
        return np.clip(acc >> 17, -32768, 32767)
    return one(re), one(im)


def level(x: np.ndarray) -> float:
    """RMS of the louder half of the samples (ignores fades and gaps)."""
    p = np.abs(x) ** 2
    return float(np.sqrt(np.mean(p[p > np.percentile(p, 50)]))) if p.size else 1.0


def front_end(iq50: np.ndarray, rms: float | None) -> tuple[np.ndarray, np.ndarray]:
    """50 kSPS complex -> post-RRC int16 at 25 kSPS. ``rms=None``: already scaled."""
    from p25_hdl.lsm_fir import LPF_TAPS_25K, RRC_TAPS_25K

    x = iq50 if rms is None else iq50 * (rms / max(level(iq50), 1e-9))
    re, im = to_q15(x.real), to_q15(x.imag)
    re, im = re[0::2], im[0::2]  # LsmDecimator2
    re, im = lsm_fir(re, im, LPF_TAPS_25K)
    re, im = lsm_fir(re, im, RRC_TAPS_25K)
    return re, im


# ---------------------------------------------------------------------------
# Gateware
# ---------------------------------------------------------------------------


def build_demod(*, legacy: bool = False, hold_enter: int | None = None,
                hold_exit: int | None = None, pll_clamp_q13: int = 0):
    """``LsmDemod`` as the traffic chain builds it, with optional overrides.
    Gateware without the hold (an older commit) accepts no overrides except
    the clamp, which is then patched into the module constant."""
    import inspect

    import p25_hdl.lsm_pll_update as pu
    from p25_hdl.lsm_demod import LsmDemod

    params = inspect.signature(LsmDemod.__init__).parameters
    kw: dict = {"sample_rate_hz": 25_000}
    clamp = LEGACY_CLAMP_Q13 if legacy and not pll_clamp_q13 else pll_clamp_q13
    if "hold_exit_symbols" in params:
        if legacy:
            kw["hold_exit_symbols"] = 0
        if hold_exit is not None:
            kw["hold_exit_symbols"] = hold_exit
        if hold_enter is not None:
            kw["hold_enter_symbols"] = hold_enter
        if clamp:
            kw["max_pll_abs_q13"] = int(clamp)
    else:
        if hold_enter is not None or hold_exit is not None:
            raise SystemExit("this gateware has no PLL/timing hold")
        if clamp:
            pu.MAX_PLL_ABS_Q13 = int(clamp)
    dut = LsmDemod(**kw)
    dut.effective_clamp_q13 = int(kw.get("max_pll_abs_q13") or pu.MAX_PLL_ABS_Q13)
    return dut


def run_hdl(re: np.ndarray, im: np.ndarray, *, cycles: int = 16, reset_at: list[int] = (0,),
            pll_seed: int = 0, agc_seed: int = 0, timing_seed: int = 0,
            agc_threshold: int = 256, dc_block: bool = True, agc: bool = True,
            pll_clamp_q13: int = 0, legacy: bool = False, hold_enter: int | None = None,
            hold_exit: int | None = None) -> dict:
    """Seeds apply to the first reset; later resets are cold (seeds 0), as a PS
    watchdog reset would be. Per symbol: (k, dibit, pll, sample_point,
    agc_gain, agc_mag, agc_gate_count, hold)."""
    from amaranth.sim import Simulator

    dut = build_demod(legacy=legacy, hold_enter=hold_enter, hold_exit=hold_exit,
                      pll_clamp_q13=pll_clamp_q13)
    has_hold = hasattr(dut, "hold_dbg")
    sim = Simulator(dut)
    sim.add_clock(1 / 62.5e6)
    sym: list[tuple] = []
    nids: list[dict] = []
    resets = set(int(r) for r in reset_at)

    async def bench(ctx):
        ctx.set(dut.agc_mag_update_threshold_in, agc_threshold)
        ctx.set(dut.dc_block_enable, int(dc_block))
        ctx.set(dut.agc_enable, int(agc))
        ctx.set(dut.pll_seed_in, pll_seed)
        ctx.set(dut.agc_seed_in, agc_seed)
        ctx.set(dut.timing_seed_in, timing_seed)
        first = min(resets) if resets else None
        for k in range(re.size):
            if k in resets:
                ctx.set(dut.reset_in, 1)
                await ctx.tick()
                ctx.set(dut.reset_in, 0)
                if k == first:
                    ctx.set(dut.pll_seed_in, 0)
                    ctx.set(dut.agc_seed_in, 0)
                    ctx.set(dut.timing_seed_in, 0)
            ctx.set(dut.re_in, int(re[k]))
            ctx.set(dut.im_in, int(im[k]))
            ctx.set(dut.strobe_in, 1)
            await ctx.tick()
            ctx.set(dut.strobe_in, 0)
            for _ in range(cycles - 1):
                if ctx.get(dut.symbol_strobe):
                    sym.append((k, ctx.get(dut.dibit_out), ctx.get(dut.pll_dbg),
                                ctx.get(dut.sample_point_dbg), ctx.get(dut.agc_gain_dbg),
                                ctx.get(dut.agc_mag_dbg), ctx.get(dut.agc_gate_dbg),
                                ctx.get(dut.hold_dbg) if has_hold else 0))
                if ctx.get(dut.nid_event_strobe):
                    nids.append({"k": k, "nac": ctx.get(dut.nac_out), "duid": ctx.get(dut.duid_out),
                                 "valid": ctx.get(dut.valid_out),
                                 "n_errors": ctx.get(dut.n_errors_out),
                                 "sync_distance": ctx.get(dut.sync_distance_out)})
                await ctx.tick()
    sim.add_testbench(bench)
    t0 = time.time()
    sim.run()
    a = np.array(sym, dtype=np.int64) if sym else np.zeros((0, 8), dtype=np.int64)
    return {"sym": a, "nids": nids, "sim_s": time.time() - t0, "has_hold": has_hold,
            "clamp_q13": dut.effective_clamp_q13}


def find_syncs(dibits: np.ndarray, max_err: int = 4) -> dict[int, list[int]]:
    out: dict[int, list[int]] = {}
    for r, rot in ROTATIONS.items():
        d = np.array([rot[int(v)] for v in dibits], dtype=np.int64)
        reg, hits = 0, []
        for i, v in enumerate(d):
            reg = ((reg << 2) | int(v)) & ((1 << 48) - 1)
            if i >= 23 and bin(reg ^ SYNC).count("1") <= max_err:
                hits.append(i)
        out[r] = hits
    return out


def gated_flags(gate_count: np.ndarray) -> np.ndarray:
    """Per-symbol 'AGC gated' from the cumulative 16-bit gate counter."""
    if gate_count.size == 0:
        return np.zeros(0, dtype=bool)
    d = np.diff(np.concatenate([[0], gate_count])) % 65536
    return d > 0


def part_rows(res: dict, parts: list[dict], fs_in: float = FS_LSM) -> list[dict]:
    """One summary row per scenario part (signal or noise)."""
    a = res["sym"]
    if a.size == 0:
        return []
    t = a[:, 0] / fs_in
    pll = a[:, 2]
    syncs = find_syncs(a[:, 1])
    gated = gated_flags(a[:, 6])
    rows = []
    for p in parts:
        m = (t >= p["t0"]) & (t < p["t1"])
        if not m.any():
            continue
        idx = np.flatnonzero(m)
        row = {"part": p["label"], "kind": p["kind"], "t0": round(p["t0"], 3),
               "t1": round(p["t1"], 3),
               "syncs": {str(r): int(sum(1 for i in v if m[i])) for r, v in syncs.items()},
               "nid_valid": sum(1 for n in res["nids"]
                                if n["valid"] and p["t0"] <= n["k"] / fs_in < p["t1"]),
               "pll_start": int(pll[idx[0]]), "pll_end": int(pll[idx[-1]]),
               "pll_min": int(pll[m].min()), "pll_max": int(pll[m].max()),
               "pinned_pct": round(100.0 * float(
                   (np.abs(pll[m]) >= res.get("clamp_q13", LEGACY_CLAMP_Q13) - 10).mean()), 1),
               "agc_mag_median": int(np.median(a[m, 5])),
               "gated_pct": round(100.0 * float(gated[m].mean()), 1),
               "hold_pct": round(100.0 * float(a[m, 7].mean()), 1)}
        s0 = [i for i in syncs[0] if m[i]]
        row["first_sync_s"] = round(float(t[s0[0]] - p["t0"]), 3) if s0 else None
        rows.append(row)
    return rows


def summarize(res: dict, parts: list[dict] | None = None, fs_in: float = FS_LSM) -> dict:
    a = res["sym"]
    if a.size == 0:
        return {"symbols": 0}
    t = a[:, 0] / fs_in
    pll = a[:, 2].astype(float)
    syncs = find_syncs(a[:, 1])
    clamp = int(res.get("clamp_q13", LEGACY_CLAMP_Q13))
    pinned = np.abs(pll) >= clamp - 10
    gated = gated_flags(a[:, 6])
    seg = []
    for s0 in np.arange(0, t[-1] + 1e-9, 0.25):
        m = (t >= s0) & (t < s0 + 0.25)
        if m.any():
            seg.append({"t": round(float(s0), 2), "pll_mean": round(float(pll[m].mean())),
                        "pll_min": int(pll[m].min()), "pll_max": int(pll[m].max()),
                        "pinned_pct": round(100.0 * float(pinned[m].mean()), 1),
                        "sample_point": int(np.median(a[m, 3])),
                        "agc_gain": int(np.median(a[m, 4])), "agc_mag": int(np.median(a[m, 5])),
                        "gated_pct": round(100.0 * float(gated[m].mean()), 1),
                        "hold_pct": round(100.0 * float(a[m, 7].mean()), 1)})
    valid = [n for n in res["nids"] if n["valid"]]
    out = {"symbols": int(a.shape[0]), "sim_s": round(res["sim_s"], 1),
           "has_hold": res.get("has_hold", False), "clamp_q13": clamp,
           "syncs_by_rotation": {str(k): len(v) for k, v in syncs.items()},
           "first_sync_s": {str(k): round(float(t[v[0]]), 3) for k, v in syncs.items() if v},
           "nid_events": len(res["nids"]), "nid_valid": len(valid),
           "duids": sorted({n["duid"] for n in valid}),
           "pll_pinned_pct": round(100.0 * float(pinned.mean()), 1),
           "pll_final": int(pll[-1]), "per_250ms": seg}
    if parts:
        out["parts"] = part_rows(res, parts, fs_in)
    return out


# ---------------------------------------------------------------------------
# Scenario
# ---------------------------------------------------------------------------


def read_source(*, npy: str = "", wav: str = "", cs16: str = "", fs: float = 8e6,
                centre: float = 0.0, freq: float = 0.0, ref_ppm: float = 0.0,
                start: float = 0.0, seconds: float = 0.0) -> np.ndarray:
    """50 kSPS complex window [start, start + seconds) (0 = to the end). WAV
    and npy inputs are read for the window only (long CC recordings)."""
    i0 = int(start * FS_DDC)
    n = int(seconds * FS_DDC) if seconds else -1
    if npy:
        x = np.load(npy, mmap_mode="r")
        return np.array(x[i0:] if n < 0 else x[i0:i0 + n])
    if wav:
        use_bench()
        from fbench.analysis import sdrtrunk as st

        info = st.wav_info(wav)
        r = np.fromfile(wav, dtype="<i2", offset=info.data_offset + 4 * i0,
                        count=-1 if n < 0 else 2 * n).astype(float)
        return r[0::2] + 1j * r[1::2]
    r = np.fromfile(cs16, dtype="<i2").astype(float)
    x = r[0::2] + 1j * r[1::2]
    x = ddc_8m(x, fs, freq - centre * (1 + ref_ppm * 1e-6))[i0:]
    return x if n < 0 else x[:n]


def use_bench() -> None:
    p = str(REPO / "bench")
    if p not in sys.path:
        sys.path.insert(0, p)


def shift(x: np.ndarray, cfo: float) -> np.ndarray:
    return x * np.exp(2j * np.pi * cfo / FS_DDC * np.arange(x.size)) if cfo else x


def parse_gap(s: str) -> tuple[float, float]:
    t, d = s.split(":")
    return float(t), float(d)


def build_input(a: argparse.Namespace) -> tuple[np.ndarray, list[dict]]:
    """Scaled 50 kSPS input and its parts (times in seconds on the output)."""
    rng = np.random.default_rng(a.noise_seed)
    sd = a.rms * 10 ** (-a.noise_db / 20) / np.sqrt(2)

    def noise(sec: float) -> np.ndarray:
        n = int(round(sec * FS_DDC))
        return sd * (rng.normal(size=n) + 1j * rng.normal(size=n))

    prim = shift(read_source(npy=a.npy, wav=a.wav, cs16=a.cs16, fs=a.fs, centre=a.centre,
                             freq=a.freq, ref_ppm=a.ref_ppm, start=a.start,
                             seconds=a.seconds), a.cfo)
    prim = prim * (a.rms / max(level(prim), 1e-9))
    chunks: list[tuple[str, str, np.ndarray]] = []
    if a.prepend_noise:
        chunks.append(("noise", "noise0", noise(a.prepend_noise)))
    cut = 0
    for k, (t, d) in enumerate(sorted(parse_gap(g) for g in a.gap_at)):
        i = int(round(t * FS_DDC))
        chunks.append(("signal", f"sig{k}", prim[cut:i]))
        chunks.append(("noise", f"gap{k}", noise(d)))
        cut = i
    chunks.append(("signal", "sig" if not a.gap_at else f"sig{len(a.gap_at)}", prim[cut:]))
    if a.append_wav or a.append_npy:
        app = shift(read_source(npy=a.append_npy, wav=a.append_wav, start=a.append_start,
                                seconds=a.append_seconds), a.cfo)
        app = app * (a.rms / max(level(app), 1e-9))
        if a.gap:
            chunks.append(("noise", "gap", noise(a.gap)))
        chunks.append(("signal", "append", app))
    parts, t = [], 0.0
    for kind, label, c in chunks:
        parts.append({"kind": kind, "label": label, "t0": t, "t1": t + c.size / FS_DDC})
        t += c.size / FS_DDC
    return np.concatenate([c for _, _, c in chunks]), parts


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--npy")
    src.add_argument("--wav")
    src.add_argument("--cs16")
    ap.add_argument("--fs", type=float, default=8e6)
    ap.add_argument("--centre", type=float, default=858.1e6)
    ap.add_argument("--freq", type=float, default=858437500.0)
    ap.add_argument("--ref-ppm", type=float, default=0.0)
    ap.add_argument("--cfo", type=float, default=0.0, help="add a carrier offset (Hz)")
    ap.add_argument("--start", type=float, default=0.0)
    ap.add_argument("--seconds", type=float, default=3.0)
    ap.add_argument("--rms", type=float, default=2500.0, help="signal RMS into the chain (Q1.15)")
    ap.add_argument("--prepend-noise", type=float, default=0.0, help="seconds of noise first")
    ap.add_argument("--noise-db", type=float, default=30.0, help="noise below the signal (dB)")
    ap.add_argument("--noise-seed", type=int, default=1)
    ap.add_argument("--gap-at", action="append", default=[], metavar="T:S",
                    help="insert S s of noise at T s into the primary input (repeatable)")
    ap.add_argument("--append-wav", default="")
    ap.add_argument("--append-npy", default="")
    ap.add_argument("--append-start", type=float, default=0.0)
    ap.add_argument("--append-seconds", type=float, default=0.0)
    ap.add_argument("--gap", type=float, default=0.0, help="noise (s) before the appended input")
    ap.add_argument("--reset-at", type=float, nargs="*", default=[0.0], help="reset pulses (s)")
    ap.add_argument("--pll-seed", type=int, default=0)
    ap.add_argument("--agc-seed", type=int, default=0)
    ap.add_argument("--timing-seed", type=int, default=0)
    ap.add_argument("--agc-threshold", type=int, default=256)
    ap.add_argument("--cycles", type=int, default=16)
    ap.add_argument("--pll-clamp-q13", type=int, default=0,
                    help="PLL clamp in the simulated gateware (8580 = pi/3)")
    ap.add_argument("--hold-enter", type=int, default=None)
    ap.add_argument("--hold-exit", type=int, default=None, help="0 removes the hold")
    ap.add_argument("--legacy", action="store_true",
                    help="working-tree gateware without the hold, pi/3 clamp")
    ap.add_argument("--hdl-root", default="", help="directory holding the p25_hdl to simulate")
    ap.add_argument("--quiet", action="store_true", help="no per-250 ms lines")
    ap.add_argument("--out", default="")
    a = ap.parse_args(argv)
    use_hdl(a.hdl_root or None)
    x, parts = build_input(a)
    re_, im_ = front_end(x, None)
    res = run_hdl(re_, im_, cycles=a.cycles, reset_at=[int(s * FS_LSM) for s in a.reset_at],
                  pll_seed=a.pll_seed, agc_seed=a.agc_seed, timing_seed=a.timing_seed,
                  agc_threshold=a.agc_threshold, pll_clamp_q13=a.pll_clamp_q13,
                  legacy=a.legacy, hold_enter=a.hold_enter, hold_exit=a.hold_exit)
    summ = summarize(res, parts)
    summ["args"] = vars(a)
    print(json.dumps({k: v for k, v in summ.items() if k not in ("per_250ms", "args", "parts")}))
    for p in summ.get("parts", []):
        print("  part", json.dumps(p))
    if not a.quiet:
        for s in summ.get("per_250ms", []):
            print("  ", s)
    if a.out:
        Path(a.out).write_text(json.dumps(summ, indent=1), encoding="utf-8")
        np.save(Path(a.out).with_suffix(".npy"), res["sym"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
