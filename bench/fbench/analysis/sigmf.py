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
