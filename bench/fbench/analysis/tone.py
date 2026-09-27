"""CW tone analysis: frequency/ppm, level, SNR, spurs, phase continuity.

Frequency estimation: Blackman-Harris FFT peak, parabolic interpolation on
the log spectrum, then a phase-slope refinement (mix down by the coarse
estimate, block-average, linear fit of the unwrapped phase). On a clean tone
this reaches millihertz accuracy on 10^5..10^6 samples.

IQ is complex with full scale 1.0 (``iq_from_ci16`` divides AD9361 12-bit
samples by 2048), so levels are in dBFS.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Sequence

import numpy as np
from scipy.signal import windows

AD9361_FULL_SCALE = 2048.0


def iq_from_ci16(raw: bytes | np.ndarray, full_scale: float = AD9361_FULL_SCALE) -> np.ndarray:
    """Interleaved little-endian int16 I/Q -> complex128 (full scale 1.0)."""
    a = np.frombuffer(raw, dtype="<i2") if isinstance(raw, (bytes, bytearray)) else np.asarray(raw)
    a = a[: (len(a) // 2) * 2].astype(np.float64)
    return (a[0::2] + 1j * a[1::2]) / full_scale


def load_ci16(path: Path, full_scale: float = AD9361_FULL_SCALE) -> np.ndarray:
    return iq_from_ci16(np.fromfile(path, dtype="<i2"), full_scale)


@dataclass
class ToneEstimate:
    freq_hz: float
    coarse_hz: float
    power_dbfs: float
    snr_db: float
    noise_dbfs_per_hz: float
    n: int
    fs: float
    spurs: list[dict] = field(default_factory=list)
    clip_fraction: float = 0.0

    def to_dict(self) -> dict:
        return asdict(self)


def _spectrum(iq: np.ndarray) -> tuple[np.ndarray, np.ndarray, float, float]:
    """Windowed power spectrum (fftshifted) and window sums."""
    n = len(iq)
    w = windows.blackmanharris(n, sym=False)
    spec = np.fft.fftshift(np.fft.fft(iq * w))
    p = np.abs(spec) ** 2
    return p, w, float(w.sum()), float((w ** 2).sum())


def _parabolic(y: np.ndarray, k: int) -> float:
    if k <= 0 or k >= len(y) - 1:
        return 0.0
    a, b, c = y[k - 1], y[k], y[k + 1]
    denom = a - 2 * b + c
    return 0.0 if denom == 0 else 0.5 * (a - c) / denom


def refine_phase_slope(iq: np.ndarray, fs: float, f0: float, blocks: int = 256) -> float:
    """Refine ``f0`` by fitting the phase slope of the mixed-down signal."""
    n = len(iq)
    t = np.arange(n) / fs
    y = iq * np.exp(-2j * np.pi * f0 * t)
    blen = max(1, n // blocks)
    nb = n // blen
    if nb < 4:
        return f0
    b = y[: nb * blen].reshape(nb, blen).mean(axis=1)
    ph = np.unwrap(np.angle(b))
    tc = (np.arange(nb) * blen + (blen - 1) / 2.0) / fs
    weights = np.abs(b)
    slope = np.polyfit(tc, ph, 1, w=weights if weights.any() else None)[0]
    return f0 + slope / (2 * np.pi)


def estimate_tone(iq: np.ndarray, fs: float, search: tuple[float, float] | None = None,
                  guard_hz: float | None = None, spur_threshold_db: float = 10.0,
                  max_spurs: int = 20, exclude_dc_hz: float = 0.0) -> ToneEstimate:
    """Estimate the strongest tone (optionally within ``search=(centre, half_width)``)."""
    iq = np.asarray(iq, dtype=np.complex128)
    n = len(iq)
    if n < 64:
        raise ValueError("need at least 64 samples")
    p, w, wsum, w2sum = _spectrum(iq)
    freqs = np.fft.fftshift(np.fft.fftfreq(n, 1.0 / fs))
    mask = np.ones(n, dtype=bool)
    if search is not None:
        mask &= np.abs(freqs - search[0]) <= search[1]
    if exclude_dc_hz > 0:
        mask &= np.abs(freqs) > exclude_dc_hz
    if not mask.any():
        raise ValueError("empty search window")
    logp = np.log(p + 1e-300)
    idx = np.flatnonzero(mask)
    k = int(idx[np.argmax(p[idx])])
    coarse = freqs[k] + _parabolic(logp, k) * fs / n
    f = refine_phase_slope(iq, fs, coarse)
    t = np.arange(n) / fs
    amp = np.mean(iq * np.exp(-2j * np.pi * f * t))
    sig_pow = float(np.abs(amp) ** 2)

    bin_hz = fs / n
    guard = guard_hz if guard_hz is not None else max(16 * bin_hz, 1e-4 * fs)
    noise_mask = np.abs(freqs - f) > guard
    noise_bin = float(np.median(p[noise_mask]) / np.log(2)) if noise_mask.any() else 1e-30
    sigma2 = max(noise_bin / w2sum, 1e-30)  # per-sample noise power
    snr_db = 10 * np.log10(max(sig_pow, 1e-30) / sigma2)
    noise_dbfs_hz = 10 * np.log10(sigma2 / fs)

    # Tone-normalised spectrum: a tone of amplitude A reads 20log10|A| dBFS.
    pdb = 10 * np.log10(p / (wsum ** 2) + 1e-30)
    floor_db = 10 * np.log10(noise_bin / (wsum ** 2) + 1e-30)
    main_db = 10 * np.log10(max(sig_pow, 1e-30))
    spurs = find_spurs(pdb, freqs, f, guard, floor_db + spur_threshold_db, main_db, max_spurs)
    clip = float(np.mean((np.abs(iq.real) >= 0.999) | (np.abs(iq.imag) >= 0.999)))
    return ToneEstimate(
        freq_hz=float(f), coarse_hz=float(coarse), power_dbfs=float(main_db),
        snr_db=float(snr_db), noise_dbfs_per_hz=float(noise_dbfs_hz), n=n, fs=float(fs),
        spurs=spurs, clip_fraction=clip,
    )


def find_spurs(pdb: np.ndarray, freqs: np.ndarray, main_hz: float, guard_hz: float,
               threshold_db: float, main_db: float, max_spurs: int = 20) -> list[dict]:
    """Local maxima above ``threshold_db`` outside ``main_hz ± guard_hz``."""
    k = np.arange(2, len(pdb) - 2)
    local = (pdb[k] >= pdb[k - 1]) & (pdb[k] >= pdb[k + 1]) & \
            (pdb[k] >= pdb[k - 2]) & (pdb[k] >= pdb[k + 2])
    cand = k[local & (pdb[k] > threshold_db) & (np.abs(freqs[k] - main_hz) > guard_hz)]
    order = cand[np.argsort(pdb[cand])[::-1]][:max_spurs]
    return [{"freq_hz": float(freqs[i]), "dbfs": float(pdb[i]), "dbc": float(pdb[i] - main_db)}
            for i in order]


def spectrum_db(iq: np.ndarray, fs: float, nfft: int = 8192) -> tuple[np.ndarray, np.ndarray]:
    """Averaged (Welch-style) tone-normalised spectrum in dBFS, fftshifted."""
    iq = np.asarray(iq, dtype=np.complex128)
    nfft = min(nfft, len(iq))
    segs = len(iq) // nfft
    w = windows.blackmanharris(nfft, sym=False)
    acc = np.zeros(nfft)
    for s in range(segs):
        acc += np.abs(np.fft.fft(iq[s * nfft:(s + 1) * nfft] * w)) ** 2
    acc /= max(segs, 1)
    return (np.fft.fftshift(np.fft.fftfreq(nfft, 1.0 / fs)),
            np.fft.fftshift(10 * np.log10(acc / w.sum() ** 2 + 1e-30)))


def noise_and_spurs(iq: np.ndarray, fs: float, threshold_db: float = 10.0, nfft: int = 8192,
                    max_spurs: int = 30, dc_guard_hz: float = 0.0) -> dict:
    """Noise floor (median, dBFS/bin and dBFS/Hz) and spur list with no reference tone."""
    freqs, pdb = spectrum_db(iq, fs, nfft)
    floor_bin = float(np.median(pdb))
    spurs = find_spurs(pdb, freqs, 0.0, dc_guard_hz, floor_bin + threshold_db, 0.0, max_spurs)
    for s in spurs:
        s.pop("dbc", None)
        s["above_floor_db"] = s["dbfs"] - floor_bin
    # In the tone-normalised spectrum a noise bin reads sigma^2 x ENBW / fs, so
    # per-Hz density = bin level / ENBW (Blackman-Harris ENBW = 2.0044 bins).
    enbw_hz = 2.0044 * fs / nfft
    return {"floor_dbfs_per_bin": floor_bin,
            "floor_dbfs_per_hz": floor_bin - 10 * np.log10(enbw_hz),
            "nfft": nfft, "spurs": spurs}


def ppm(freq_err_hz: float, carrier_hz: float) -> float:
    return freq_err_hz / carrier_hz * 1e6


def linear_slope(t: Sequence[float], y: Sequence[float]) -> float:
    """Least-squares slope dy/dt (0 for fewer than two points)."""
    t_arr, y_arr = np.asarray(t, float), np.asarray(y, float)
    if len(t_arr) < 2 or np.ptp(t_arr) == 0:
        return 0.0
    return float(np.polyfit(t_arr, y_arr, 1)[0])


def phase_continuity(iq: np.ndarray, fs: float, f0: float, block_s: float = 1e-3,
                     step_threshold_deg: float = 10.0) -> dict:
    """Phase residual of a CW after removing the linear trend; flag steps.

    The tone is mixed down by ``f0``, averaged in ``block_s`` blocks, the
    unwrapped phase is detrended (linear fit) and consecutive-block
    differences larger than ``step_threshold_deg`` are reported as steps.
    """
    iq = np.asarray(iq, dtype=np.complex128)
    n = len(iq)
    t = np.arange(n) / fs
    y = iq * np.exp(-2j * np.pi * f0 * t)
    blen = max(1, int(round(block_s * fs)))
    nb = n // blen
    if nb < 4:
        return {"blocks": nb, "max_step_deg": 0.0, "steps": [], "residual_rms_deg": 0.0}
    b = y[: nb * blen].reshape(nb, blen).mean(axis=1)
    ph = np.unwrap(np.angle(b))
    tc = (np.arange(nb) * blen + (blen - 1) / 2.0) / fs
    coef = np.polyfit(tc, ph, 1)
    resid = np.degrees(ph - np.polyval(coef, tc))
    # Step size at each block boundary = median(right window) - median(left
    # window), so a step that straddles a block is reported at full size and
    # isolated noise spikes are suppressed.
    w = max(1, min(5, nb // 4))
    step = np.zeros(nb)
    for k in range(w, nb - w + 1):
        step[k] = np.median(resid[k:k + w]) - np.median(resid[k - w:k])
    mag = np.abs(step)
    steps = []
    k = 0
    while k < nb:
        if mag[k] > step_threshold_deg:
            hi = min(nb, k + w)
            j = k + int(np.argmax(mag[k:hi]))
            steps.append({"t_s": float(tc[j]), "step_deg": float(step[j])})
            k = j + w
        else:
            k += 1
    return {
        "blocks": int(nb),
        "block_s": blen / fs,
        "freq_hz": float(f0 + coef[0] / (2 * np.pi)),
        "max_step_deg": float(mag.max()) if nb else 0.0,
        "n_steps": len(steps),
        "steps": steps[:50],
        "residual_rms_deg": float(np.sqrt(np.mean(resid ** 2))),
        "residual_pp_deg": float(np.ptp(resid)),
    }


def harmonic_aliases(fundamentals_hz: Sequence[float], rx_lo_hz: float, fs: float,
                     rf_bw_hz: float, max_harmonic: int = 64,
                     dc_guard_hz: float = 0.0) -> list[dict]:
    """Where harmonics of e.g. 25/125 MHz Ethernet clocks land in a capture.

    Two coupling paths are listed: ``rf`` (harmonic inside the RF passband,
    appears at ``h - rx_lo``) and ``alias`` (harmonic folded by the ADC sample
    clock, ``((h + fs/2) mod fs) - fs/2``). Candidates within ``dc_guard_hz``
    of 0 Hz are dropped: there a spur cannot be told from DC offset / LO
    leakage (e.g. every 25 MHz harmonic aliases to DC when fs divides 25 MHz).
    """
    out: list[dict] = []
    seen: set[tuple[str, int]] = set()
    for f0 in fundamentals_hz:
        for m in range(1, max_harmonic + 1):
            h = m * f0
            bb = h - rx_lo_hz
            if abs(bb) <= min(rf_bw_hz, fs) / 2:
                key = ("rf", int(round(bb)))
                if key not in seen:
                    seen.add(key)
                    out.append({"path": "rf", "fundamental_hz": f0, "harmonic": m,
                                "baseband_hz": float(bb)})
            al = ((h + fs / 2) % fs) - fs / 2
            key = ("alias", int(round(al)))
            if key not in seen:
                seen.add(key)
                out.append({"path": "alias", "fundamental_hz": f0, "harmonic": m,
                            "baseband_hz": float(al)})
    return [c for c in out if abs(c["baseband_hz"]) > dc_guard_hz]


def match_spurs(spurs: list[dict], candidates: list[dict], tol_hz: float) -> list[dict]:
    """Spurs within ``tol_hz`` of a candidate frequency (annotated copies)."""
    hits = []
    for s in spurs:
        for c in candidates:
            if abs(s["freq_hz"] - c["baseband_hz"]) <= tol_hz:
                hits.append({**s, "match": c})
                break
    return hits
