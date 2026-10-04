"""Replay corpus runtime: items, rendering, SD/RAM staging, the relay and the taps.

An *item* is one continuous stream the TX board plays in a single pass:

- mode A ``whole``: one wideband capture (its 1 GiB ``cs12`` segments, played
  back to back), ``window``: a slice of a capture;
- mode B: one scene (a CC recording plus the overlapping traffic recordings,
  up-converted from 50 kSPS and mixed at their RF offsets).

Stream files are rendered on the host while they upload (no local copies), named
by a key over their recipe, and cached on ``/mnt/sd/bench/corpus`` (or
``/root/fbench_corpus`` for the RAM path), checked by size with ``ls -ln``.
"""

from __future__ import annotations

import hashlib
import json
import re
import shlex
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator

import numpy as np

from .analysis import p25_corpus as pc
from .analysis import p25_dsp
from .analysis import sdrtrunk as st
from .errors import FbenchError, PreconditionError, UsageError

P25_TX_FULL_SCALE = 2 ** 14 * 0.9  # the DAC's 2**14 full scale, backed off for filter overshoot
CORPUS_SD = "/mnt/sd/bench/corpus"
CORPUS_RAM = "/root/fbench_corpus"
RELAY_DIR = "/tmp/fbench_relay"
RENDER_VERSION = 1
CH_RMS_FRACTION = 0.12  # per-channel RMS relative to the format's full scale
CAPTURE_GAIN_CS12 = 5.0  # 12-bit captures: |2048 + 2048j| x 5.0 = 14482 <= P25_TX_FULL_SCALE
TAIL_ZEROS_S = 0.3  # flushes iio_writedev's last block


@dataclass
class StreamFile:
    name: str
    fmt: str
    samples: int
    recipe: dict[str, Any]
    render: Callable[[int, int], Iterator[bytes]]  # (start sample, count) -> bytes

    @property
    def nbytes(self) -> int:
        return self.samples * pc.BYTES_PER_SAMPLE[self.fmt]


@dataclass
class Item:
    id: str
    mode: str
    rate_hz: float
    centre_hz: float
    fmt: str
    gain: float
    files: list[StreamFile]
    playlist: list[dict[str, Any]]
    seconds: float
    timeline: list[dict[str, float]]  # [{"s0", "s1", "air0"}]: stream s -> air time
    transmissions: list[str]
    recorder: str  # unit whose reference error the content carries, or "true"
    focus: bool = False
    out_of_band: list[str] = field(default_factory=list)

    def spec(self) -> dict[str, Any]:
        return {"id": self.id, "mode": self.mode, "rate_hz": self.rate_hz,
                "centre_hz": self.centre_hz, "format": self.fmt, "gain": self.gain,
                "seconds": round(self.seconds, 3), "timeline": self.timeline,
                "transmissions": self.transmissions, "recorder": self.recorder,
                "focus": self.focus, "out_of_band": self.out_of_band,
                "files": [{"name": f.name, "bytes": f.nbytes, "samples": f.samples}
                          for f in self.files],
                "playlist": self.playlist}


def _key(recipe: dict[str, Any]) -> str:
    return hashlib.sha256(json.dumps(recipe, sort_keys=True).encode()).hexdigest()[:12]


def stream_to_air(timeline: list[dict[str, float]], s: float) -> float | None:
    for seg in timeline:
        if seg["s0"] <= s < seg["s1"]:
            return seg["air0"] + (s - seg["s0"])
    return None


def air_to_stream(timeline: list[dict[str, float]], t: float, pad: float = 1.0) -> float | None:
    for seg in timeline:
        s = seg["s0"] + (t - seg["air0"])
        if seg["s0"] - pad <= s < seg["s1"] + pad:
            return s
    return None


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def _wav_iq(path: Path, info: st.WavInfo, first: int, count: int) -> np.ndarray:
    first = max(0, min(first, info.frames))
    count = max(0, min(count, info.frames - first))
    a = np.fromfile(path, dtype="<i2", count=2 * count,
                    offset=info.data_offset + 4 * first).astype(np.float64)
    return a[0::2] + 1j * a[1::2]


def capture_segment_render(path: Path, info: st.WavInfo, sample0: int, fmt: str,
                           chunk: int = 1 << 23) -> Callable[[int, int], Iterator[bytes]]:
    """Raw capture samples (12-bit AD9361 values in int16) repacked, unscaled."""

    def render(start: int, count: int) -> Iterator[bytes]:
        done = 0
        while done < count:
            n = min(chunk, count - done)
            a = np.fromfile(path, dtype="<i2", count=2 * n,
                            offset=info.data_offset + 4 * (sample0 + start + done))
            if a.size < 2 * n:
                a = np.concatenate([a, np.zeros(2 * n - a.size, dtype="<i2")])
            yield p25_dsp.pack_int16(a, fmt)
            done += n
    return render


@dataclass
class MixSource:
    file: str
    freq_hz: float
    at_s: float  # stream time of the recording's sample 0
    from_s: float  # recording span used [from_s, to_s)
    to_s: float
    gain: float  # linear gain to the target channel level
    correct_hz: float = 0.0  # the recording's carrier offset, removed when placing it


class Mixer:
    """Up-converts 50 kSPS channel recordings to their offsets and mixes them."""

    def __init__(self, recordings: Path, sources: list[MixSource], rate_hz: float,
                 centre_hz: float, fmt: str, noise_db: float, seed: int,
                 taps_per_phase: int = 24) -> None:
        from scipy.signal import firwin

        self.dir = Path(recordings)
        self.sources = sources
        self.rate = float(rate_hz)
        self.centre = float(centre_hz)
        self.fmt = fmt
        self.seed = seed
        self.info: dict[str, st.WavInfo] = {}
        self.up: dict[str, int] = {}
        for s in sources:
            info = st.wav_info(self.dir / s.file)
            self.info[s.file] = info
            up = self.rate / info.rate
            if abs(up - round(up)) > 1e-9:
                raise UsageError(f"{s.file}: {info.rate} Hz does not divide {self.rate} Hz")
            self.up[s.file] = int(round(up))
        self._h: dict[int, np.ndarray] = {}
        for up in set(self.up.values()):
            n = up * taps_per_phase + 1
            self._h[up] = firwin(n, 20e3, fs=self.rate, window=("kaiser", 8.0)) * up
        fs_full = p25_dsp.FULL_SCALE[fmt]
        self.ch_rms = CH_RMS_FRACTION * fs_full
        # AWGN: noise_db below one channel's power in 12.5 kHz.
        self.noise_sigma = self.ch_rms * np.sqrt(self.rate / 12.5e3 / 10 ** (noise_db / 10.0))
        self.clipped = 0

    def block(self, b0: int, b1: int) -> np.ndarray:
        out = np.zeros(b1 - b0, dtype=np.complex128)
        n_idx = np.arange(b0, b1, dtype=np.float64)
        for s in self.sources:
            info, up = self.info[s.file], self.up[s.file]
            h = self._h[up]
            L = h.size
            D = (L - 1) // 2
            o_s = int(round(s.at_s * self.rate))
            lo = max(b0, o_s + int(round(s.from_s * self.rate)))
            hi = min(b1, o_s + int(round(min(s.to_s, info.seconds) * self.rate)))
            if hi <= lo:
                continue
            j0, j1 = lo - o_s, hi - o_s  # output samples relative to the recording
            k_lo = max(0, -(-(j0 + D - L + 1) // up))
            k_hi = min(info.frames - 1, (j1 - 1 + D) // up)
            if k_hi < k_lo:
                continue
            from scipy.signal import upfirdn

            x = _wav_iq(self.dir / s.file, info, k_lo, k_hi - k_lo + 1)
            yy = upfirdn(h, x, up=up)
            m0 = j0 + D - k_lo * up
            seg = yy[m0:m0 + (j1 - j0)]
            if seg.size < j1 - j0:
                seg = np.concatenate([seg, np.zeros(j1 - j0 - seg.size)])
            f_off = s.freq_hz - s.correct_hz - self.centre
            ph = 2 * np.pi * f_off / self.rate * n_idx[lo - b0:hi - b0]
            out[lo - b0:hi - b0] += s.gain * seg * np.exp(1j * ph)
        if self.noise_sigma > 0:
            rng = np.random.default_rng([self.seed, b0])
            out += (rng.normal(0, self.noise_sigma / np.sqrt(2), out.size)
                    + 1j * rng.normal(0, self.noise_sigma / np.sqrt(2), out.size))
        fs_full = p25_dsp.FULL_SCALE[self.fmt]
        self.clipped += int(np.count_nonzero((np.abs(out.real) > fs_full) |
                                             (np.abs(out.imag) > fs_full)))
        return out

    def render(self, start: int, count: int, block: int | None = None) -> Iterator[bytes]:
        blk = int(block or self.rate // 2)
        pos = start
        while pos < start + count:
            n = min(blk, start + count - pos)
            yield p25_dsp.pack(self.block(pos, pos + n), self.fmt)
            pos += n


def source_gain(recordings: Path, file: str, from_s: float, to_s: float, target_rms: float
                ) -> float:
    info = st.wav_info(Path(recordings) / file)
    k0 = int(from_s * info.rate)
    n = int(min(max(to_s - from_s, 0.1), 60.0) * info.rate)
    x = _wav_iq(Path(recordings) / file, info, k0, n)
    rms = float(np.sqrt(np.mean(np.abs(x) ** 2))) if x.size else 0.0
    return target_rms / rms if rms > 0 else 0.0


def _split_file(prefix: str, fmt: str, samples: int, recipe: dict[str, Any],
                render: Callable[[int, int], Iterator[bytes]]) -> list[StreamFile]:
    per = pc.SEG_BYTES // pc.BYTES_PER_SAMPLE[fmt]
    out = []
    key = _key({**recipe, "render_version": RENDER_VERSION})
    for k in range((samples + per - 1) // per):
        s0 = k * per
        n = min(per, samples - s0)

        def part(start: int, count: int, _s0: int = s0) -> Iterator[bytes]:
            return render(_s0 + start, count)
        out.append(StreamFile(f"{prefix}_{key}.{k}.{fmt}", fmt, n,
                              {**recipe, "part": k, "part_sample0": s0}, part))
    return out


# ---------------------------------------------------------------------------
# Items
# ---------------------------------------------------------------------------


def item_ids(man: dict[str, Any], mode: str, a_unit: str = "whole") -> list[str]:
    plans = man["plans"]
    if mode == "A":
        if a_unit == "window":
            return [w["id"] for w in plans["A"]["windows"]]
        return [c["id"] for c in plans["A"]["captures"] if c["transmissions"]]
    if mode == "B":
        return [s["id"] for s in plans["B"]["scenes"]]
    raise UsageError(f"mode must be A or B, not {mode!r}")


def build_item(man: dict[str, Any], mode: str, item_id: str, *, a_unit: str = "whole",
               a_recorder: str = "A", noise_db: float = 35.0) -> Item:
    src = man["sources"]
    plans = man["plans"]
    if mode == "A":
        return _build_a(man, plans["A"], item_id, a_unit, a_recorder, Path(src["captures"]))
    if mode == "B":
        return _build_b(man, plans["B"], item_id, Path(src["recordings"]), noise_db)
    raise UsageError(f"mode must be A or B, not {mode!r}")


def _build_a(man: dict[str, Any], plan: dict[str, Any], item_id: str, a_unit: str,
             recorder: str, captures: Path) -> Item:
    fmt = plan["format"]
    if a_unit == "window":
        w = next((x for x in plan["windows"] if x["id"] == item_id), None)
        if w is None:
            raise UsageError(f"no mode A window {item_id!r} in the manifest")
        cap = next(c for c in plan["captures"] if c["id"] == w["capture"])
        s0, secs, txs = float(w["start_s"]), float(w["seconds"]), list(w["transmissions"])
    else:
        cap = next((c for c in plan["captures"] if c["id"] == item_id), None)
        if cap is None:
            raise UsageError(f"no mode A capture {item_id!r} in the manifest")
        s0, secs, txs = 0.0, float(cap["seconds"]), list(cap["transmissions"])
    path = captures / cap["file"]
    if not path.exists():
        raise PreconditionError(f"capture {path} not found")
    info = st.wav_info(path)
    rate = float(cap["rate_hz"])
    first = int(round(s0 * rate))
    n = min(int(round(secs * rate)), info.frames - first)
    recipe = {"kind": "capture", "file": cap["file"], "size": cap["size"],
              "mtime": cap["mtime"], "sample0": first, "samples": n, "format": fmt}
    files = _split_file(f"A_{cap['id']}" + (f"_w{int(s0)}" if a_unit == "window" else ""),
                        fmt, n, recipe, capture_segment_render(path, info, first, fmt))
    playlist: list[dict[str, Any]] = [{"file": f.name} for f in files]
    playlist.append({"zeros": int(TAIL_ZEROS_S * rate)})
    return Item(id=item_id, mode="A", rate_hz=rate, centre_hz=float(cap["centre_hz"]), fmt=fmt,
                gain=CAPTURE_GAIN_CS12 if fmt == "cs12" else P25_TX_FULL_SCALE / 2896.0,
                files=files, playlist=playlist, seconds=n / rate,
                timeline=[{"s0": 0.0, "s1": n / rate, "air0": cap["start_unix"] + s0}],
                transmissions=txs, recorder=recorder,
                out_of_band=list(cap.get("out_of_band", [])))


def _mixed_item(mode: str, item_id: str, recordings: Path, sources: list[MixSource],
                rate: float, centre: float, fmt: str, seconds: float, noise_db: float,
                recipe: dict[str, Any]) -> tuple[list[StreamFile], list[dict[str, Any]], float]:
    n = int(round(seconds * rate))
    seed = int(_key(recipe), 16) & 0x7FFFFFFF
    mixer = Mixer(recordings, sources, rate, centre, fmt, noise_db, seed)
    full = p25_dsp.FULL_SCALE[fmt]
    gain = P25_TX_FULL_SCALE / (full * np.sqrt(2))
    files = _split_file(f"{mode}_{item_id}", fmt, n,
                        {**recipe, "noise_db": noise_db, "rate_hz": rate, "centre_hz": centre},
                        mixer.render)
    playlist: list[dict[str, Any]] = [{"file": f.name} for f in files]
    playlist.append({"zeros": int(TAIL_ZEROS_S * rate)})
    return files, playlist, float(gain)


def _build_b(man: dict[str, Any], plan: dict[str, Any], item_id: str, recordings: Path,
             noise_db: float) -> Item:
    sc = next((x for x in plan["scenes"] if x["id"] == item_id), None)
    if sc is None:
        raise UsageError(f"no mode B scene {item_id!r} in the manifest")
    fmt, rate, centre = plan["format"], float(sc["rate_hz"]), float(sc["centre_hz"])
    target = CH_RMS_FRACTION * p25_dsp.FULL_SCALE[fmt]
    sources = []
    for s in sc["sources"]:
        at = s["start_unix"] - sc["t0"]
        fr = max(0.0, -at)
        to = min(float(s["seconds"]), sc["t1"] - s["start_unix"])
        if to <= fr:
            continue
        sources.append(MixSource(s["file"], float(s["freq_hz"]), at, fr, to,
                                 source_gain(recordings, s["file"], fr, to, target),
                                 float(s.get("correct_hz") or 0.0)))
    recipe = {"kind": "scene", "scene": {k: sc[k] for k in ("t0", "t1", "sources")},
              "format": fmt}
    files, playlist, gain = _mixed_item("B", item_id, recordings, sources, rate, centre, fmt,
                                        float(sc["seconds"]), noise_db, recipe)
    return Item(id=item_id, mode="B", rate_hz=rate, centre_hz=centre, fmt=fmt, gain=gain,
                files=files, playlist=playlist, seconds=float(sc["seconds"]),
                timeline=[{"s0": 0.0, "s1": float(sc["seconds"]), "air0": sc["t0"]}],
                transmissions=list(sc["transmissions"]), recorder="true",
                focus=bool(sc.get("focus")))


def truth_stream_frames(item: Item, man: dict[str, Any]
                        ) -> dict[str, list[tuple[float, str]]]:
    """Truth frames of the item's transmissions on the stream clock."""
    txs = pc.tx_index(man)
    truth_dir = Path(man["sources"]["recordings"])
    out: dict[str, list[tuple[float, str]]] = {}
    for tid in item.transmissions:
        tx = txs.get(tid)
        if tx is None:
            continue
        fr = st.truth_frames(truth_dir, tx)
        rows = []
        for t, h in fr:
            s = air_to_stream(item.timeline, t - pc.MBE_LAG_S)
            if s is not None:
                rows.append((round(s + pc.MBE_LAG_S, 3), h))
        out[tid] = rows
    return out


# ---------------------------------------------------------------------------
# Staging on the TX board
# ---------------------------------------------------------------------------


def parse_ls(out: str) -> dict[str, int]:
    """``ls -ln`` (BusyBox) -> {name: size}."""
    sizes = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 9 and parts[0].startswith("-"):
            try:
                sizes[parts[-1]] = int(parts[4])
            except ValueError:
                continue
    return sizes


class Stager:
    """Uploads stream files once; later runs find them by name and size."""

    def __init__(self, ctx: Any, unit: str, root: str, cache_file: Path) -> None:
        self.ctx, self.unit, self.root = ctx, unit, root
        self.ssh = ctx.services.ssh(unit)
        self.cache_file = cache_file
        try:
            self.cache = json.loads(cache_file.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self.cache = {}

    def listing(self) -> dict[str, int]:
        _, out, _ = self.ssh.run(f"mkdir -p {self.root}; ls -ln {self.root} 2>/dev/null", 20.0)
        return parse_ls(out)

    def free_bytes(self) -> int | None:
        if self.root.startswith("/mnt/sd"):
            _, out, _ = self.ssh.run(f"df -k {self.root} | tail -1", 15.0)
            parts = out.split()
            try:
                return int(parts[3]) * 1024
            except (IndexError, ValueError):
                return None
        _, out, _ = self.ssh.run("grep MemAvailable /proc/meminfo", 15.0)
        m = re.search(r"(\d+)", out)
        return int(m.group(1)) * 1024 if m else None

    def ensure(self, files: list[StreamFile], reserve: int = 0) -> list[dict[str, Any]]:
        have = self.listing()
        need = [f for f in files if have.get(f.name) != f.nbytes]
        rows = [{"name": f.name, "bytes": f.nbytes, "uploaded": False,
                 "sha256": (self.cache.get(f.name) or {}).get("sha256")}
                for f in files if f not in need]
        if need:
            free = self.free_bytes()
            total = sum(f.nbytes for f in need)
            if free is not None and total + reserve + (64 << 20) > free:
                raise PreconditionError(
                    f"{total / 1e9:.2f} GB to stage on {self.unit}:{self.root} but only "
                    f"{free / 1e9:.2f} GB free (reserve {reserve >> 20} MiB): purge older corpus "
                    "files (-p purge=true) or run fewer items", unit=self.unit)
        for f in need:
            t0 = self.ctx.services.monotonic()
            sha = hashlib.sha256()

            def gen(_f: StreamFile = f) -> Iterator[bytes]:
                for blk in _f.render(0, _f.samples):
                    sha.update(blk)
                    yield blk
            part = f"{self.root}/{f.name}.part"
            self.ctx.log.info("staging %s (%.0f MiB) on %s", f.name, f.nbytes / 2 ** 20,
                              self.unit)
            n = self.ssh.put_stream(gen(), part, max(600.0, f.nbytes / 2e6))
            if n != f.nbytes:
                raise FbenchError(f"rendered {n} B for {f.name}, expected {f.nbytes}")
            _, out, _ = self.ssh.run(f"ls -ln {part}", 15.0)
            got = parse_ls(out).get(Path(part).name)
            if got is not None and got != f.nbytes:
                raise FbenchError(f"upload of {f.name} to {self.unit} truncated ({got} of "
                                  f"{f.nbytes} B)")
            self.ssh.run(f"mv -f {part} {self.root}/{f.name}", 30.0)
            secs = self.ctx.services.monotonic() - t0
            digest = sha.hexdigest()
            self.cache[f.name] = {"sha256": digest, "bytes": f.nbytes, "recipe": f.recipe,
                                  "unit": self.unit}
            self._save()
            meta = json.dumps({"sha256": digest, "bytes": f.nbytes, "recipe": f.recipe},
                              default=str).encode()
            self.ssh.put_stream([meta], f"{self.root}/{f.name}.json", 60.0)
            rows.append({"name": f.name, "bytes": f.nbytes, "uploaded": True, "sha256": digest,
                         "seconds": round(secs, 1),
                         "mbs": round(f.nbytes / 1e6 / max(secs, 1e-6), 2)})
        return rows

    def purge(self, keep: set[str]) -> list[str]:
        have = self.listing()
        drop = [n for n in have if n not in keep and not any(n == k + ".json" for k in keep)]
        for n in drop:
            self.ssh.run(f"rm -f {self.root}/{shlex.quote(n)}", 30.0)
        return drop

    def _save(self) -> None:
        self.cache_file.parent.mkdir(parents=True, exist_ok=True)
        self.cache_file.write_text(json.dumps(self.cache, indent=1, default=str),
                                   encoding="utf-8")


# ---------------------------------------------------------------------------
# The relay on the TX board
# ---------------------------------------------------------------------------


class Relay:
    """``fbench-agent replay stream | iio_writedev`` in its own session."""

    def __init__(self, ctx: Any, unit: str, root: str, *, block_samples: int, ring_mb: int,
                 prefill_mb: int | None = None, on_underrun: str = "wait") -> None:
        self.ctx, self.unit, self.root = ctx, unit, root
        self.ssh = ctx.services.ssh(unit)
        self.block, self.ring_mb, self.prefill_mb = block_samples, ring_mb, prefill_mb
        self.on_underrun = on_underrun
        self.started = False
        self.status_path = f"{RELAY_DIR}/status.json"
        self.report_path = f"{RELAY_DIR}/report.json"
        self.pidfile = f"{RELAY_DIR}/stream.pid"
        self.log = f"{RELAY_DIR}/stream.log"

    def playlist_doc(self, item: Item) -> dict[str, Any]:
        items = []
        for p in item.playlist:
            if "zeros" in p:
                items.append({"zeros": int(p["zeros"])})
            else:
                items.append({"path": f"{self.root}/{p['file']}"})
        return {"id": item.id, "format": item.fmt, "rate_hz": item.rate_hz, "gain": item.gain,
                "items": items}

    def start(self, item: Item) -> None:
        doc = json.dumps(self.playlist_doc(item)).encode()
        self.ssh.run(f"mkdir -p {RELAY_DIR}; rm -f {self.status_path} {self.report_path} "
                     f"{self.pidfile}", 15.0)
        self.ssh.put_stream([doc], f"{RELAY_DIR}/playlist.json", 60.0)
        args = ["replay", "stream", "--playlist", f"{RELAY_DIR}/playlist.json", "--ring-mb",
                str(self.ring_mb), "--status", self.status_path, "--report", self.report_path,
                "--on-underrun", self.on_underrun]
        if self.prefill_mb is not None:
            args += ["--prefill-mb", str(self.prefill_mb)]
        agent = self.ctx.agent.command(args)
        dev = self.ctx.cfg.iio.tx_device
        inner = (f"echo $$ > {self.pidfile}; {agent} 2> {self.log} | iio_writedev -u local: "
                 f"-b {self.block} {dev} voltage0 voltage1 2>> {self.log}.iio")
        self.ssh.run(f"setsid sh -c {shlex.quote(inner)} > /dev/null 2>&1 < /dev/null &", 15.0)
        self.started = True

    def status(self) -> dict[str, Any] | None:
        rc, out, _ = self.ssh.run(f"cat {self.status_path} 2>/dev/null", 15.0)
        if rc != 0 or not out.strip():
            return None
        try:
            return json.loads(out)
        except ValueError:
            return None

    def report(self) -> dict[str, Any] | None:
        rc, out, _ = self.ssh.run(f"cat {self.report_path} 2>/dev/null; echo; "
                                  f"tail -5 {self.log}.iio 2>/dev/null", 15.0)
        text = out.strip().splitlines()
        for line in text:
            if line.startswith("{"):
                try:
                    return json.loads(line)
                except ValueError:
                    pass
        return None

    def stop(self) -> None:
        if not self.started:
            return
        self.ssh.run(f"P=$(cat {self.pidfile} 2>/dev/null); [ -n \"$P\" ] && "
                     f"kill -TERM -- -$P 2>/dev/null; sleep 1; "
                     f"pkill -f '[f]bench-agent replay stream'; pkill -x iio_writedev; "
                     f"rm -f {self.pidfile}; true", 20.0)
        self.started = False


# ---------------------------------------------------------------------------
# Taps on the DUT (polled from the main loop: sim-clock friendly, no threads)
# ---------------------------------------------------------------------------


class Taps:
    def __init__(self, ctx: Any, unit: str, *, calls_every: int = 15) -> None:
        self.ctx, self.http = ctx, ctx.http(unit)
        self.dumps: list[dict[str, Any]] = []
        self.calls: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.calls_every = max(1, calls_every)
        self.n = 0
        self.has_baseline = False

    def poll(self, calls: bool | None = None) -> None:
        t = self.ctx.services.monotonic()
        try:
            d = self.http.get_json("/api/imbe_dump", None, 5.0) or {}
            self.dumps.append({"t": t, "frames": d.get("frames") or []})
        except FbenchError as exc:
            self.errors.append(f"imbe_dump: {exc.message}")
        if calls or (calls is None and self.n % self.calls_every == 0):
            self.poll_calls()
        self.n += 1

    def baseline(self) -> int:
        """First dump before the stream starts: the ring's older frames, excluded."""
        self.poll(calls=False)
        self.has_baseline = bool(self.dumps)
        return len(self.dumps[0]["frames"]) if self.dumps else 0

    def poll_calls(self) -> None:
        try:
            snap = self.http.get_json("/api/ui/calls", {"limit": 250}, 10.0) or {}
            self.calls.append({"t": self.ctx.services.monotonic(),
                               "items": snap.get("items") or []})
        except FbenchError as exc:
            self.errors.append(f"ui/calls: {exc.message}")

    def dut_clock(self) -> dict[str, Any] | None:
        """DUT wall clock minus host monotonic (maps started_unix_ms to the tap clock)."""
        try:
            m0 = self.ctx.services.monotonic()
            st_ = self.http.get_json("/api/ui/state", None, 5.0) or {}
            m1 = self.ctx.services.monotonic()
        except FbenchError:
            return None
        now = st_.get("now_unix_ms")
        if now is None:
            return None
        return {"dut_minus_mono_s": now / 1000.0 - (m0 + m1) / 2,
                "clock_valid": st_.get("clock_valid")}
