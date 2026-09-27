"""Small P25 signal helpers for replay corpora (numpy/scipy, host only).

- :func:`frame_syncs`: P25 frame-sync positions in a channel baseband (C4FM or
  LSM/CQPSK: the phase advance over one symbol is the dibit for both), used to
  check how well an SDRTrunk channel recording lines up with its log clock.
- :class:`Interpolator`: streaming polyphase interpolation (50 kSPS channel
  recordings up to the wideband replay rate) with state across blocks.
- :func:`pack` / :func:`unpack`: the replay storage formats ``cs16`` (2 x int16),
  ``cs12`` (I and Q as 12-bit two's complement in one little-endian 24-bit word,
  I in bits 0..11) and ``cs8`` (2 x int8). ``fbench-agent replay stream`` expands
  them to the int16 stream ``iio_writedev`` takes.
"""

from __future__ import annotations

from typing import Any

import numpy as np

SYMBOL_RATE = 4800.0
# 0x5575F5FF77FF as C4FM symbols (+3 +3 +3 +3 +3 -3 ...).
FS_SYMBOLS = np.array([3, 3, 3, 3, 3, -3, 3, 3, -3, -3, 3, 3, -3, -3, -3, -3, 3, -3, 3, -3,
                       -3, -3, -3, -3], dtype=np.float64)
FORMATS = {"cs16": 4, "cs12": 3, "cs8": 2}  # bytes per complex sample
FULL_SCALE = {"cs16": 32767, "cs12": 2047, "cs8": 127}


def frame_syncs(iq: np.ndarray, fs: float, threshold: float = 0.75,
                min_spacing_s: float = 0.02) -> list[tuple[float, float]]:
    """``[(time s, correlation)]`` of P25 frame syncs in a channel baseband."""
    from scipy.signal import firwin, lfilter

    x = np.asarray(iq, dtype=np.complex128)
    if x.size < int(fs * 0.1):
        return []
    h = firwin(101, 6500.0, fs=fs)
    y = lfilter(h, 1.0, x)
    lag = max(1, int(round(fs / SYMBOL_RATE)))
    d = np.angle(y[lag:] * np.conj(y[:-lag]))  # phase advance over one symbol
    sps = fs / SYMBOL_RATE
    n = int(round(24 * sps))
    tmpl = FS_SYMBOLS[np.minimum((np.arange(n) / sps).astype(int), 23)]
    tmpl = tmpl - tmpl.mean()
    tmpl /= np.linalg.norm(tmpl)
    num = np.correlate(d, tmpl, mode="valid")
    ones = np.ones(n)
    s1 = np.correlate(d, ones, mode="valid")
    s2 = np.correlate(d * d, ones, mode="valid")
    var = np.maximum(s2 - s1 * s1 / n, 1e-12)
    rho = num / np.sqrt(var)
    out: list[tuple[float, float]] = []
    gap = int(min_spacing_s * fs)
    idx = np.flatnonzero(rho > threshold)
    i = 0
    while i < idx.size:
        j = i
        while j + 1 < idx.size and idx[j + 1] - idx[i] < gap:
            j += 1
        seg = idx[i:j + 1]
        k = int(seg[np.argmax(rho[seg])])
        # group delay of the FIR (50 taps) and the differentiator (lag / 2)
        out.append(((k + 50 + lag / 2.0) / fs, float(rho[k])))
        i = j + 1
    return out


def carrier_offset(iq: np.ndarray, fs: float) -> float | None:
    """Carrier offset (Hz) of a P25 channel recording: the power-weighted mean phase
    advance of its louder half (C4FM and CQPSK symbols average to zero)."""
    from scipy.signal import firwin, lfilter

    x = np.asarray(iq, dtype=np.complex128)
    if x.size < int(fs * 0.5):
        return None
    y = lfilter(firwin(101, 7000.0, fs=fs), 1.0, x)
    p = np.abs(y) ** 2
    w = (p * (p > np.percentile(p, 50)))[1:]
    if not np.any(w):
        return None
    d = np.angle(y[1:] * np.conj(y[:-1]))
    return float(np.sum(d * w) / np.sum(w) * fs / (2 * np.pi))


class Interpolator:
    """Integer-factor polyphase interpolation of a complex stream, block by block."""

    def __init__(self, up: int, cutoff_hz: float, fs_out: float, taps_per_phase: int = 16) -> None:
        from scipy.signal import firwin

        self.up = int(up)
        ntaps = self.up * taps_per_phase
        self.h = firwin(ntaps, cutoff_hz, fs=fs_out, window=("kaiser", 8.0)) * self.up
        self.hist = np.zeros(taps_per_phase - 1, dtype=np.complex128)
        self.taps = taps_per_phase

    def process(self, x: np.ndarray) -> np.ndarray:
        from scipy.signal import upfirdn

        x = np.asarray(x, dtype=np.complex128)
        if x.size == 0:
            return np.zeros(0, dtype=np.complex128)
        buf = np.concatenate([self.hist, x])
        y = upfirdn(self.h, buf, up=self.up)
        start = self.hist.size * self.up
        out = y[start:start + x.size * self.up]
        self.hist = buf[-(self.taps - 1):] if self.taps > 1 else self.hist
        return out

    @property
    def delay_out(self) -> float:
        """Group delay in output samples."""
        return (len(self.h) - 1) / 2.0


def pack(iq: np.ndarray, fmt: str) -> bytes:
    """Complex samples (already scaled to the format's full scale) to bytes."""
    fs = FULL_SCALE[fmt]
    i = np.clip(np.round(np.real(iq)), -fs - 1, fs).astype(np.int32)
    q = np.clip(np.round(np.imag(iq)), -fs - 1, fs).astype(np.int32)
    if fmt == "cs16":
        out = np.empty(2 * i.size, dtype="<i2")
        out[0::2], out[1::2] = i, q
        return out.tobytes()
    if fmt == "cs8":
        out8 = np.empty(2 * i.size, dtype=np.int8)
        out8[0::2], out8[1::2] = i, q
        return out8.tobytes()
    if fmt == "cs12":
        w = (i & 0xFFF).astype(np.uint32) | ((q & 0xFFF).astype(np.uint32) << 12)
        b = np.empty((w.size, 3), dtype=np.uint8)
        b[:, 0] = w & 0xFF
        b[:, 1] = (w >> 8) & 0xFF
        b[:, 2] = (w >> 16) & 0xFF
        return b.tobytes()
    raise ValueError(f"unknown format {fmt}")


def pack_int16(inter: np.ndarray, fmt: str) -> bytes:
    """Interleaved int16 I/Q (e.g. a WAV's data) to ``fmt`` without scaling."""
    a = np.asarray(inter).astype(np.int32)
    return pack(a[0::2] + 1j * a[1::2], fmt)


def unpack(data: bytes, fmt: str) -> np.ndarray:
    """Bytes of ``fmt`` to complex samples in format counts."""
    if fmt == "cs16":
        a = np.frombuffer(data, dtype="<i2").astype(np.float64)
        return a[0::2] + 1j * a[1::2]
    if fmt == "cs8":
        a = np.frombuffer(data, dtype=np.int8).astype(np.float64)
        return a[0::2] + 1j * a[1::2]
    if fmt == "cs12":
        b = np.frombuffer(data, dtype=np.uint8).reshape(-1, 3).astype(np.uint32)
        w = b[:, 0] | (b[:, 1] << 8) | (b[:, 2] << 16)
        i = (w & 0xFFF).astype(np.int32)
        q = ((w >> 12) & 0xFFF).astype(np.int32)
        i = np.where(i >= 2048, i - 4096, i)
        q = np.where(q >= 2048, q - 4096, q)
        return i.astype(np.float64) + 1j * q.astype(np.float64)
    raise ValueError(f"unknown format {fmt}")


def tone_frames(pcm: np.ndarray, fs: float = 8000.0, frame: int = 160,
                nfft: int = 4096) -> list[dict[str, Any]]:
    """Per-frame dominant frequency (parabolic peak) and RMS of 8 kHz PCM."""
    x = np.asarray(pcm, dtype=np.float64)
    out = []
    w = np.hanning(frame)
    for k in range(x.size // frame):
        s = x[k * frame:(k + 1) * frame]
        rms = float(np.sqrt(np.mean(s * s)))
        spec = np.abs(np.fft.rfft((s - s.mean()) * w, nfft))
        i = int(np.argmax(spec[1:-1])) + 1
        a, b, c = np.log(spec[i - 1:i + 2] + 1e-12)
        den = a - 2 * b + c
        delta = 0.5 * (a - c) / den if den != 0 else 0.0
        out.append({"k": k, "freq_hz": (i + delta) * fs / nfft, "rms": rms})
    return out
