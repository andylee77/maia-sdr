"""Minimal SigMF reader/writer (``.sigmf-data`` + ``.sigmf-meta``).

Supports ``ci16_le`` (AD9361 raw captures) and ``cf32_le``. Also reads raw
``.cs16`` and 2-channel 16-bit ``.wav`` clips (SDRTrunk baseband style) so
stimulus files can be used without conversion.
"""

from __future__ import annotations

import json
import re
import wave
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import numpy as np

SDRTRUNK_NAME = re.compile(r"^(\d+)_(\d+)_(\d+)_")


def _base(path: str | Path) -> Path:
    p = Path(path)
    if p.suffix in (".sigmf-data", ".sigmf-meta"):
        return p.with_suffix("")
    return p


def write(path: str | Path, data: np.ndarray | bytes, sample_rate: float,
          center_freq: float | None = None, datatype: str = "ci16_le", description: str = "",
          extra: dict[str, Any] | None = None) -> tuple[Path, Path]:
    """Write a recording. ``data`` is raw bytes, interleaved int16 or complex."""
    base = _base(path)
    base.parent.mkdir(parents=True, exist_ok=True)
    data_path = base.with_suffix(".sigmf-data")
    meta_path = base.with_suffix(".sigmf-meta")
    if isinstance(data, (bytes, bytearray)):
        data_path.write_bytes(bytes(data))
    else:
        arr = np.asarray(data)
        if datatype == "ci16_le":
            if np.iscomplexobj(arr):
                inter = np.empty(arr.size * 2, dtype="<i2")
                inter[0::2] = np.clip(np.round(arr.real), -32768, 32767)
                inter[1::2] = np.clip(np.round(arr.imag), -32768, 32767)
                arr = inter
            arr.astype("<i2").tofile(data_path)
        elif datatype == "cf32_le":
            arr.astype(np.complex64).tofile(data_path)
        else:
            raise ValueError(f"unsupported datatype {datatype}")
    glob: dict[str, Any] = {
        "core:datatype": datatype,
        "core:sample_rate": float(sample_rate),
        "core:version": "1.0.0",
        "core:recorder": "fbench",
        "core:description": description,
    }
    glob.update(extra or {})
    capture: dict[str, Any] = {
        "core:sample_start": 0,
        "core:datetime": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }
    if center_freq is not None:
        capture["core:frequency"] = float(center_freq)
    meta = {"global": glob, "captures": [capture], "annotations": []}
    meta_path.write_text(json.dumps(meta, indent=2), encoding="utf-8")
    return data_path, meta_path


def read(path: str | Path) -> tuple[np.ndarray, dict[str, Any]]:
    """Read a SigMF recording as complex128 in native units (int16 counts for ci16)."""
    base = _base(path)
    meta = json.loads(base.with_suffix(".sigmf-meta").read_text(encoding="utf-8"))
    dt = meta["global"]["core:datatype"]
    raw_path = base.with_suffix(".sigmf-data")
    if dt == "ci16_le":
        a = np.fromfile(raw_path, dtype="<i2").astype(np.float64)
        iq = a[0::2] + 1j * a[1::2]
    elif dt == "cf32_le":
        iq = np.fromfile(raw_path, dtype=np.complex64).astype(np.complex128)
    else:
        raise ValueError(f"unsupported datatype {dt}")
    return iq, meta


def read_clip(path: str | Path, rate_hz: float | None = None,
              freq_hz: float | None = None) -> tuple[np.ndarray, float, float | None]:
    """Load a stimulus clip: SigMF, 2-channel int16 WAV or raw .cs16.

    Returns ``(iq in int16 counts, sample_rate, centre_freq)``; the rate and
    frequency default to the SigMF metadata or an SDRTrunk-style file name
    ``<unix>_<freqHz>_<rateHz>_…``.
    """
    p = Path(path)
    m = SDRTRUNK_NAME.match(p.name)
    name_freq = float(m.group(2)) if m else None
    name_rate = float(m.group(3)) if m else None
    if p.suffix in (".sigmf-meta", ".sigmf-data") or p.with_suffix(".sigmf-meta").exists():
        iq, meta = read(p)
        caps = meta.get("captures") or [{}]
        return (iq, float(rate_hz or meta["global"]["core:sample_rate"]),
                freq_hz or caps[0].get("core:frequency") or name_freq)
    if p.suffix.lower() == ".wav":
        with wave.open(str(p), "rb") as wf:
            if wf.getnchannels() != 2 or wf.getsampwidth() != 2:
                raise ValueError(f"{p.name}: need 2-channel 16-bit WAV")
            rate = float(wf.getframerate())
            a = np.frombuffer(wf.readframes(wf.getnframes()), dtype="<i2").astype(np.float64)
        return a[0::2] + 1j * a[1::2], float(rate_hz or rate), freq_hz or name_freq
    a = np.fromfile(p, dtype="<i2").astype(np.float64)
    rate = rate_hz or name_rate
    if not rate:
        raise ValueError(f"{p.name}: sample rate unknown (pass rate_hz)")
    return a[0::2] + 1j * a[1::2], float(rate), freq_hz or name_freq


def _clip_chunks(p: Path, rate_hz: float | None, start_s: float, seconds: float | None,
                 chunk: int) -> tuple[Any, float]:
    """Interleaved int16 chunks of a clip window, plus the sample rate."""
    if p.suffix in (".sigmf-meta", ".sigmf-data") or p.with_suffix(".sigmf-meta").exists():
        iq, meta = read(p)
        rate = float(rate_hz or meta["global"]["core:sample_rate"])
        s0 = int(start_s * rate)
        iq = iq[s0:s0 + int(seconds * rate)] if seconds else iq[s0:]
        inter = np.empty(2 * len(iq), dtype="<i2")
        inter[0::2] = np.clip(np.round(iq.real), -32768, 32767)
        inter[1::2] = np.clip(np.round(iq.imag), -32768, 32767)
        return iter([inter[i:i + 2 * chunk] for i in range(0, len(inter), 2 * chunk)]), rate
    if p.suffix.lower() == ".wav":
        wf = wave.open(str(p), "rb")
        if wf.getnchannels() != 2 or wf.getsampwidth() != 2:
            wf.close()
            raise ValueError(f"{p.name}: need 2-channel 16-bit WAV")
        rate = float(rate_hz or wf.getframerate())
        wf.setpos(min(int(start_s * wf.getframerate()), wf.getnframes()))
        left = int(seconds * wf.getframerate()) if seconds else wf.getnframes() - wf.tell()

        def wav_gen() -> Any:
            nonlocal left
            try:
                while left > 0:
                    raw = wf.readframes(min(chunk, left))
                    if not raw:
                        break
                    left -= len(raw) // 4
                    yield np.frombuffer(raw, dtype="<i2")
            finally:
                wf.close()
        return wav_gen(), rate
    m = SDRTRUNK_NAME.match(p.name)
    rate = float(rate_hz or (float(m.group(3)) if m else 0.0))
    if not rate:
        raise ValueError(f"{p.name}: sample rate unknown (pass rate_hz)")
    total = p.stat().st_size // 4
    first = min(int(start_s * rate), total)
    n = min(total - first, int(seconds * rate)) if seconds else total - first

    def raw_gen() -> Any:
        with open(p, "rb") as f:
            f.seek(first * 4)
            left = n
            while left > 0:
                a = np.fromfile(f, dtype="<i2", count=2 * min(chunk, left))
                if a.size == 0:
                    break
                left -= a.size // 2
                yield a
    return raw_gen(), rate


def prepare_replay(path: str | Path, out: str | Path, *, start_s: float = 0.0,
                   seconds: float | None = None, rate_hz: float | None = None,
                   freq_hz: float | None = None, full_scale: float = 2 ** 14 * 0.9,
                   chunk: int = 1 << 22) -> dict[str, Any]:
    """Cut a clip window to interleaved int16 scaled to ``full_scale`` peak.

    Streams in chunks (two passes: peak, then scale + write + hash), so multi-GB
    captures never load into memory. Returns rate, centre frequency, sample
    count, source peak (counts) and the output's sha256.
    """
    import hashlib

    p = Path(path)
    m = SDRTRUNK_NAME.match(p.name)
    freq = freq_hz or (float(m.group(2)) if m else None)
    if freq is None and p.with_suffix(".sigmf-meta").exists():
        meta = json.loads(_base(p).with_suffix(".sigmf-meta").read_text(encoding="utf-8"))
        freq = (meta.get("captures") or [{}])[0].get("core:frequency")
    peak = 0.0
    chunks, rate = _clip_chunks(p, rate_hz, start_s, seconds, chunk)
    for a in chunks:
        f = a.astype(np.float32)
        peak = max(peak, float(np.sqrt(np.max(f[0::2] ** 2 + f[1::2] ** 2))) if a.size else 0.0)
    scale = full_scale / (peak or 1.0)
    sha = hashlib.sha256()
    n = 0
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    chunks, _ = _clip_chunks(p, rate_hz, start_s, seconds, chunk)
    with open(out, "wb") as fh:
        for a in chunks:
            y = np.clip(np.round(a.astype(np.float32) * scale), -32768, 32767).astype("<i2")
            b = y.tobytes()
            sha.update(b)
            fh.write(b)
            n += y.size // 2
    return {"rate_hz": rate, "freq_hz": freq, "samples": n, "peak_counts": peak,
            "sha256": sha.hexdigest(), "path": str(out)}
