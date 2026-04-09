#!/usr/bin/env python3
"""p25_lsm_demod.py -- Python reference port of SDRTrunk's P25 LSM demod chain.

Phase 1 of the Fishball P25 LSM development plan:

    SDRTrunk Java reference  ──port──>  Python reference (THIS FILE)
                                            │
                                            │ validates against
                                            ▼
                                  Captured baseband .wav
                                            │
                                            │ + truth from
                                            ▼
                                SDRTrunk decoded_messages.log

If this Python prototype's NID frame-sync hits and recovered NACs match
SDRTrunk's truth log on the same recording, we have proof that:

    1. The algorithm port is faithful
    2. We can iterate fixes here in seconds instead of FPGA rebuilds
    3. The next porting step (-> Rust on PS, then -> HDL) starts from a
       known-good reference instead of guesswork

The chain mirrors `P25P1DecoderLSM.receive()` + `P25P1DemodulatorLSM.process()`
from SDRTrunk (read directly from C:\\Users\\Andy\\Projects\\SDRTrunk\\sdrtrunk
on 2026-04-09; the user's `phase4-refactor` fork is unchanged from upstream
for these specific files):

    raw IQ (50 kSPS from .wav, post-channelizer)
        ↓ Stage 1: half-band decimation by 2 -> 25 kSPS (~5.21 sps)
        ↓ Stage 2: baseband Parks-McClellan LPF (passband 7250, stopband 8000)
        ↓ Stage 3: RRC matched filter (alpha=0.2, 16-symbol kernel)
        ↓ Stage 4: P25P1DemodulatorLSM
        │   - per-symbol AGC toward |z|=1.0, slewed at 5%/symbol
        │   - linear-interp between samples for fractional sample point
        │   - differential demod z[k] * conj(z[k-1])
        │   - rotate by tracked PLL phase
        │   - atan2 slicer with pi/2 quadrant boundaries
        │   - Gardner timing error on 2D demodulated symbols
        │   - decision-directed PI phase loop, bounded ±pi/3
        ↓ soft + hard symbols
        ↓ Stage 5: NID frame sync correlator (Hamming distance to 0x5575F5FF77FF)
        ↓ Stage 6: NID extractor (NAC + DUID, no FEC -- same approximation we
                    use in p25-httpd today)
        ↓ list of (timestamp, nac, duid) sync events

Then we diff that list against SDRTrunk's truth log:

    {sync events the prototype found} vs {NAC/TSBK count in the .log}

Expected result on a faithful port + clean recording:
    - Sync hit count within ~5% of SDRTrunk's PASSED+FAILED NID count
    - >99% of recovered NACs == 0x8A1 (the site's NAC)
    - Best frame-sync Hamming distance <= 4 most of the time

Usage:
    python tools/p25_lsm_demod.py \\
        --wav "C:\\Users\\Andy\\SDRTrunk\\recordings\\<file>.wav" \\
        --truth "C:\\Users\\Andy\\SDRTrunk\\event_logs\\<file>.log"

    # Run without truth diff (just demod and report stats):
    python tools/p25_lsm_demod.py --wav <wav>

    # Plot the constellation and PLL trace at the end (needs matplotlib):
    python tools/p25_lsm_demod.py --wav <wav> --plot

Dependencies: numpy, scipy. matplotlib only if --plot is given.
"""
from __future__ import annotations

import argparse
import math
import re
import sys
import time
import wave
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np
from scipy import signal


# ============================================================================
# Constants from SDRTrunk's P25 LSM chain (verbatim)
# ============================================================================
# These come straight out of:
#   io.github.dsheirer.module.decode.p25.phase1.P25P1DecoderLSM
#   io.github.dsheirer.module.decode.p25.phase1.P25P1DemodulatorLSM
#   io.github.dsheirer.dsp.symbol.Dibit
# If a porting bug crops up, the first thing to suspect is one of these
# values. Do not change them without consulting the Java source.

P25_SYMBOL_RATE = 4800       # symbols / sec
RRC_ROLLOFF = 0.2            # alpha for the matched filter
RRC_SYMBOL_LENGTH = 16       # filter length in symbols
LPF_PASSBAND_HZ = 7250       # baseband LPF passband edge
LPF_STOPBAND_HZ = 8000       # baseband LPF stopband edge
LPF_PASS_RIPPLE = 0.01
LPF_STOP_RIPPLE = 0.01

# Demodulator loop constants (P25P1DemodulatorLSM.java fields)
PLL_GAIN = 0.1               # phase loop step coefficient
PLL_MAX_ERROR = 0.3          # phaseError clamp before applying gain
MAX_PLL_ABS = math.pi / 3    # ±pi/3 == ±800 Hz at 4800 sym/s
OBJECTIVE_MAGNITUDE = 1.0    # AGC target for |IQ|
AGC_SLEW = 0.05              # AGC update fraction per symbol
AGC_MAX = 500.0              # max gain to prevent runaway

# Slicer ideal phases (Dibit.java)
DIBIT_PHASE = {
    0b00: math.pi / 4,        # +1
    0b01: 3 * math.pi / 4,    # +3
    0b10: -math.pi / 4,       # -1
    0b11: -3 * math.pi / 4,   # -3
}

# P25 frame sync (24 dibits = 48 bits), packed dibits-low-first into u64.
# This is the same constant SDRTrunk uses (P25P1SyncDetector.SYNC_PATTERN)
# and matches TIA-102.BAAA.
FRAME_SYNC_DIBIT_PATTERN = 0x5575_F5FF_77FF
FRAME_SYNC_MASK = 0xFFFF_FFFF_FFFF
FRAME_SYNC_DIBITS = 24

# Soft sync correlation threshold from SDRTrunk's P25P1MessageFramer.java:55:
#     private static final float SYNC_DETECTION_THRESHOLD = 60;
# The score is sum_{i=0..23}(SYNC_PATTERN_SYMBOLS[i] * received_symbol[i])
# where SYNC_PATTERN_SYMBOLS[i] is one of ±3π/4 and received_symbol[i] is
# the soft (atan2) phase of the demodulated symbol. Perfect lock on the
# sync pattern gives a score of 24 * (3π/4)² ≈ 133. Threshold 60 is
# roughly half of that — generous tolerance for noisy syncs.
SYNC_SCORE_THRESHOLD = 60.0

# NID layout in the dibit stream after sync (from SDRTrunk's
# P25P1MessageFramer.java:54): 32 dibits of NID payload + 1 status
# dibit inserted at index 11 = 33 dibits total. The status dibit is
# part of the global "every 35 dibits" status cycle (which is reset on
# sync detection in the framer), and must be skipped when extracting
# the 64-bit NID payload.
NID_TRANSMITTED_DIBITS = 33
NID_PAYLOAD_DIBITS = 32
NID_STATUS_DIBIT_INDEX = 11


# ============================================================================
# Stage 0: I/Q reader (the .wav SDRTrunk recorded post-channelizer)
# ============================================================================

def load_wav_iq(path: Path) -> tuple[np.ndarray, int]:
    """Read a SDRTrunk baseband .wav into a complex64 array.

    SDRTrunk records post-channelizer IQ as 16-bit PCM, 2-channel (I/Q
    interleaved), little-endian. The wave header reports the channelizer
    output sample rate directly.
    """
    with wave.open(str(path), "rb") as w:
        nch = w.getnchannels()
        sw = w.getsampwidth()
        sr = w.getframerate()
        nframes = w.getnframes()
        if nch != 2:
            raise ValueError(f"expected 2 channels (I/Q), got {nch}")
        if sw != 2:
            raise ValueError(f"expected 16-bit samples, got {sw} bytes")
        raw = w.readframes(nframes)

    # int16 little-endian, interleaved [I0 Q0 I1 Q1 ...]
    pcm = np.frombuffer(raw, dtype="<i2").astype(np.float32)
    pcm /= 32768.0  # normalise to ±1.0 like SDRTrunk's float buffers
    iq = pcm[0::2] + 1j * pcm[1::2]
    return iq.astype(np.complex64), sr


# ============================================================================
# Stage 1: half-band decimation
# ============================================================================
# SDRTrunk picks decimation by doubling while the resulting rate is still
# >= 38400, then KEEPS the higher decimation (the loop's exit value, NOT
# the previous one). See P25P1DecoderLSM.setSampleRate():
#
#   while((sampleRate / decimation) >= 38400)
#       decimation *= 2;
#
# For sr=50000:  50000/1=50000 (>=38400, dec=2)  50000/2=25000 (<38400, exit)
#                              -> decimated rate = 25000 = 5.21 sps  ✓
# For sr=24000:  loop never runs              -> decimated rate = 24000 = 5.0 sps
# For sr=2500000: 2500000/1=2500000 ... -> dec=64 -> 39062 (>=38400, dec=128)
#                                       -> 19531 (<38400, exit) -> 19531 ~4 sps

def select_decimation(sample_rate: float) -> int:
    dec = 1
    while (sample_rate / dec) >= 38400:
        dec *= 2
    return max(1, dec)


def decimate_iq(iq: np.ndarray, factor: int) -> np.ndarray:
    """Decimate an IQ stream by an integer factor with a half-band-style FIR.

    SDRTrunk uses cascaded half-band filters tuned for each power-of-two.
    For our purposes (offline reference) `scipy.signal.decimate` with a
    Chebyshev IIR is functionally equivalent at the symbol-rate scale we
    care about. Apply to real and imaginary parts separately to keep the
    complex array intact.
    """
    if factor == 1:
        return iq
    re = signal.decimate(iq.real, factor, ftype="iir", zero_phase=True)
    im = signal.decimate(iq.imag, factor, ftype="iir", zero_phase=True)
    return (re + 1j * im).astype(np.complex64)


# ============================================================================
# Stage 2: baseband LPF (Parks-McClellan equiripple)
# ============================================================================

def design_baseband_lpf(sample_rate: float) -> np.ndarray:
    """Design the same baseband LPF SDRTrunk uses.

    SDRTrunk's `getBasebandFilter` builds an equiripple FIR via the
    `FilterFactory.getTaps(spec)` path. The spec values are:

        passband:   DC -> 7250 Hz   amplitude 1.0   ripple 0.01
        stopband: 8000 Hz -> Nyquist amplitude 0.0   ripple 0.01

    `scipy.signal.remez` is the Parks-McClellan equivalent. It needs the
    band edges as fractions of the sample rate.
    """
    nyq = sample_rate / 2.0
    bands = [0, LPF_PASSBAND_HZ, LPF_STOPBAND_HZ, nyq]
    desired = [1.0, 0.0]
    weights = [1.0 / LPF_PASS_RIPPLE, 1.0 / LPF_STOP_RIPPLE]
    # Estimate filter order (Bellanger's formula).
    transition = (LPF_STOPBAND_HZ - LPF_PASSBAND_HZ) / sample_rate
    n = int(2.0 / 3.0 * math.log10(1.0 / (10.0 * LPF_PASS_RIPPLE * LPF_STOP_RIPPLE))
            / transition)
    if n % 2 == 0:
        n += 1  # odd length so the filter is symmetric with integer delay
    n = max(n, 31)
    return signal.remez(n, bands, desired, weight=weights, fs=sample_rate)


def apply_real_fir(iq: np.ndarray, taps: np.ndarray) -> np.ndarray:
    """Apply a real-coefficient FIR independently to I and Q."""
    re = signal.lfilter(taps, [1.0], iq.real)
    im = signal.lfilter(taps, [1.0], iq.imag)
    return (re + 1j * im).astype(np.complex64)


# ============================================================================
# Stage 3: RRC matched filter
# ============================================================================

def design_rrc(samples_per_symbol: float, num_symbols: int, alpha: float) -> np.ndarray:
    """Root raised cosine impulse response.

    Direct port of `FilterFactory.getRootRaisedCosine(sps, num_symbols, rolloff)`.
    The Java implementation is a textbook closed-form formula -- this Python
    one is the same formula, just unrolled into numpy.
    """
    n = int(round(num_symbols * samples_per_symbol))
    # Force odd length so the filter is symmetric with an integer delay.
    if n % 2 == 0:
        n += 1
    t = (np.arange(n) - (n - 1) / 2.0) / samples_per_symbol
    h = np.zeros_like(t)
    pi = math.pi
    for i, ti in enumerate(t):
        if abs(ti) < 1e-9:
            h[i] = (1.0 - alpha) + (4.0 * alpha / pi)
        elif abs(abs(4.0 * alpha * ti) - 1.0) < 1e-9:
            h[i] = (alpha / math.sqrt(2.0)) * (
                (1.0 + 2.0 / pi) * math.sin(pi / (4.0 * alpha))
                + (1.0 - 2.0 / pi) * math.cos(pi / (4.0 * alpha))
            )
        else:
            num = math.sin(pi * ti * (1.0 - alpha)) + \
                  4.0 * alpha * ti * math.cos(pi * ti * (1.0 + alpha))
            den = pi * ti * (1.0 - (4.0 * alpha * ti) ** 2)
            h[i] = num / den
    h /= np.sqrt(np.sum(h ** 2))   # unit-energy normalisation
    return h.astype(np.float32)


# ============================================================================
# Stage 4: LSM demodulator -- direct port of P25P1DemodulatorLSM.process()
# ============================================================================
# This is the line-by-line port of the Java loop. Variable names match the
# original (mPLL -> pll, mSamplePoint -> sample_point, etc.) so a side-by-side
# diff is straightforward. The only differences from Java:
#   - we precompute lerp via numpy where possible to keep the loop fast
#   - we collect debug traces (constellation points, PLL phase) for plotting
#   - we run on the entire input at once instead of incremental buffers

@dataclass
class DemodResult:
    """Output of one full demod run over an IQ buffer."""
    soft_symbols: np.ndarray   # complex constellation points (post-PLL)
    soft_phases: np.ndarray    # atan2 of soft_symbols (used by soft sync detector)
    hard_dibits: np.ndarray    # uint8 dibit values (0..3)
    pll_trace: np.ndarray      # tracked PLL phase per symbol (radians)
    timing_trace: np.ndarray   # sample_point per symbol (for diagnostics)
    samples_per_symbol: float
    n_symbols: int


def lerp(a: float, b: float, mu: float) -> float:
    """Linear interpolation between two adjacent samples."""
    return a + (b - a) * mu


def to_dibit(soft_symbol: float) -> int:
    """Map a soft phase to a 4-PSK quadrant. Mirrors Dibit.toDibit()."""
    if soft_symbol > 0:
        return 0b01 if soft_symbol > math.pi / 2 else 0b00
    else:
        return 0b11 if soft_symbol < -math.pi / 2 else 0b10


def demod_lsm(iq: np.ndarray, sample_rate: float) -> DemodResult:
    """Demodulate filtered LSM I/Q -> dibit symbols.

    Direct port of `P25P1DemodulatorLSM.process(float[] i, float[] q)`.
    Reads the entire incoming buffer in one pass (no incremental state
    handover, since this is offline batch processing).
    """
    sps = sample_rate / P25_SYMBOL_RATE
    half_sps = sps / 2.0
    ted_gain = sps / 4.0
    max_timing_adj = sps / 25.0

    # Loop state
    sample_point = sps                # countdown to next decision
    pll = 0.0
    sample_gain = 1.0
    prev_middle_i = 0.0
    prev_middle_q = 0.0
    prev_current_i = 0.0
    prev_current_q = 0.0
    prev_sym_i = 0.7                  # SDRTrunk's init values
    prev_sym_q = 0.7

    # Output collectors
    soft_symbols: list[complex] = []
    soft_phases: list[float] = []
    hard_dibits: list[int] = []
    pll_trace: list[float] = []
    timing_trace: list[float] = []

    # SDRTrunk works against pre-loaded float[] buffers; we mirror that
    # by extracting real/imag arrays once.
    bufI = iq.real.astype(np.float64)
    bufQ = iq.imag.astype(np.float64)
    n = len(bufI)

    # Walk the buffer, decrementing sample_point each step. When it goes
    # below 1, we land on a fractional sample for the symbol decision.
    bp = 0   # buffer pointer (Java's bufferPointer)
    while bp < n - int(math.ceil(sps)) - 2:
        bp += 1
        sample_point -= 1.0

        if sample_point >= 1.0:
            continue

        # ----- midpoint sample (between prev and current symbol) -----
        i_mid = lerp(bufI[bp], bufI[bp + 1], sample_point)
        q_mid = lerp(bufQ[bp], bufQ[bp + 1], sample_point)

        # ----- current symbol sample, half a symbol ahead of midpoint -----
        ptr = bp + sample_point + half_sps
        offset = int(math.floor(ptr))
        residual = ptr - offset
        if offset + 1 >= n:
            break
        i_cur = lerp(bufI[offset], bufI[offset + 1], residual)
        q_cur = lerp(bufQ[offset], bufQ[offset + 1], residual)

        # ----- AGC: scale toward unit magnitude, slewed -----
        magnitude = math.hypot(i_cur, q_cur)
        if magnitude > 0 and not math.isinf(magnitude):
            required_gain = OBJECTIVE_MAGNITUDE / magnitude
            required_gain = min(required_gain, AGC_MAX)
            sample_gain += (required_gain - sample_gain) * AGC_SLEW
            sample_gain = min(sample_gain, required_gain)
            sample_gain = min(sample_gain, AGC_MAX)
        i_mid *= sample_gain
        q_mid *= sample_gain
        i_cur *= sample_gain
        q_cur *= sample_gain

        # ----- current PLL state as a complex rotation -----
        pll_i = math.cos(pll)
        pll_q = math.sin(pll)

        # ----- differential demod of MIDDLE sample -----
        # z_mid * conj(z_prev_mid):
        i_mid_demod = (prev_middle_i * i_mid) + (prev_middle_q * q_mid)
        q_mid_demod = (prev_middle_i * q_mid) - (prev_middle_q * i_mid)
        # rotate by PLL
        tmp = (i_mid_demod * pll_i) - (q_mid_demod * pll_q)
        q_mid_demod = (q_mid_demod * pll_i) + (i_mid_demod * pll_q)
        i_mid_demod = tmp

        # ----- differential demod of SYMBOL sample -----
        i_sym = (prev_current_i * i_cur) + (prev_current_q * q_cur)
        q_sym = (prev_current_i * q_cur) - (prev_current_q * i_cur)
        tmp = (i_sym * pll_i) - (q_sym * pll_q)
        q_sym = (q_sym * pll_i) + (i_sym * pll_q)
        i_sym = tmp

        # ----- slice -----
        soft_symbol = math.atan2(q_sym, i_sym)

        # ----- Gardner TED on 2D demodulated symbols -----
        timing_adj = ((prev_sym_i - i_sym) * i_mid_demod
                      + (prev_sym_q - q_sym) * q_mid_demod)
        if timing_adj > max_timing_adj:
            timing_adj = max_timing_adj
        elif timing_adj < -max_timing_adj:
            timing_adj = -max_timing_adj
        timing_adj *= ted_gain
        sample_point += timing_adj

        # ----- decision-directed PLL update -----
        if soft_symbol != 0.0:
            hard = to_dibit(soft_symbol)
            phase_error = soft_symbol - DIBIT_PHASE[hard]
            if phase_error > PLL_MAX_ERROR:
                phase_error = PLL_MAX_ERROR
            elif phase_error < -PLL_MAX_ERROR:
                phase_error = -PLL_MAX_ERROR
            pll -= phase_error * PLL_GAIN
            if pll > MAX_PLL_ABS:
                pll = MAX_PLL_ABS
            elif pll < -MAX_PLL_ABS:
                pll = -MAX_PLL_ABS
        else:
            hard = 0b00

        # ----- record outputs -----
        soft_symbols.append(complex(i_sym, q_sym))
        soft_phases.append(soft_symbol)
        hard_dibits.append(hard)
        pll_trace.append(pll)
        timing_trace.append(sample_point)

        # ----- shuffle history for next iteration -----
        prev_sym_i = i_sym
        prev_sym_q = q_sym
        prev_middle_i = i_mid
        prev_middle_q = q_mid
        prev_current_i = i_cur
        prev_current_q = q_cur

        # Add another symbol period to the countdown
        sample_point += sps

    return DemodResult(
        soft_symbols=np.array(soft_symbols, dtype=np.complex64),
        soft_phases=np.array(soft_phases, dtype=np.float32),
        hard_dibits=np.array(hard_dibits, dtype=np.uint8),
        pll_trace=np.array(pll_trace, dtype=np.float32),
        timing_trace=np.array(timing_trace, dtype=np.float32),
        samples_per_symbol=sps,
        n_symbols=len(hard_dibits),
    )


# ============================================================================
# Stage 5: NID frame sync detector + status-aware NID extractor
# ============================================================================
# We provide TWO sync detectors so we can A/B compare them:
#
#   find_sync_events_hard()  -- the old hard-dibit Hamming-distance correlator
#                               (what p25-httpd uses, what our HDL would use)
#   find_sync_events_soft()  -- a port of SDRTrunk's P25P1SoftSyncDetector
#                               that correlates ideal sync phases against the
#                               soft (atan2) symbols. Much more sensitive on
#                               noisy data because it uses sub-bit information.
#
# The soft detector is the one SDRTrunk's LSM decoder actually uses
# (P25P1MessageFramer.processWithSoftSyncDetect). Our 339/1005 = 34% recall
# in the previous run was because the hard detector simply doesn't see syncs
# that the soft detector would.
#
# Both extract the NID using the same status-dibit-aware logic: they read
# 33 dibits after the sync hit and SKIP index 11 (the position where the
# 35-dibit-cycle status symbol falls inside the NID block, given that the
# status counter is reset on sync detection -- per SDRTrunk's framer).

SYNC_THRESHOLD = 4   # Hamming distance for hard correlator (used by hard path)


@dataclass
class SyncEvent:
    """One frame-sync match -> NID extraction."""
    symbol_idx: int       # symbol index of the dibit immediately after sync
    distance: int         # Hamming distance of the sync match (hard detector)
    score: float          # soft correlation score (soft detector)
    nac: int              # 12-bit NAC, no FEC
    duid: int             # 4-bit DUID, no FEC


def _build_sync_pattern_phases() -> np.ndarray:
    """Convert the 48-bit sync pattern into 24 ideal symbol phases.

    Mirrors P25P1SyncDetector.syncPatternToSymbols() in SDRTrunk:
    extract dibits MSB-first, map 01 -> +3π/4 and 11 -> -3π/4 (the sync
    is all outer ±3 symbols, no inner ±1).
    """
    out = np.zeros(24, dtype=np.float32)
    for x in range(24):
        # Extract dibit at position x (MSB-first within the 48 bits)
        shift = (23 - x) * 2
        dibit = (FRAME_SYNC_DIBIT_PATTERN >> shift) & 0x3
        if dibit == 0b01:        # +3
            out[x] = 3 * math.pi / 4
        elif dibit == 0b11:      # -3
            out[x] = -3 * math.pi / 4
        else:
            raise ValueError(
                f"sync pattern dibit {x} = {dibit:02b}; "
                "expected only ±3 symbols in P25 sync"
            )
    return out


SYNC_PATTERN_PHASES = _build_sync_pattern_phases()


def _extract_nid_skipping_status(
    dibits: np.ndarray, start_idx: int
) -> tuple[int, int] | None:
    """Read 33 dibits starting at start_idx, skip index 11 (status), return (nac, duid).

    Returns None if the buffer doesn't have enough room past start_idx.
    The 33-dibit window matches SDRTrunk's DIBIT_LENGTH_NID; the skip-11
    matches checkNID()'s `if(i != 11)` branch in P25P1MessageFramer.
    """
    end = start_idx + NID_TRANSMITTED_DIBITS
    if end > len(dibits):
        return None
    nid_bits = 0
    for j in range(NID_TRANSMITTED_DIBITS):
        if j == NID_STATUS_DIBIT_INDEX:
            continue
        nid_bits = (nid_bits << 2) | int(dibits[start_idx + j])
    # nid_bits is now exactly 64 bits = NAC[12] || DUID[4] || parity[48]
    nac = (nid_bits >> 52) & 0xFFF
    duid = (nid_bits >> 48) & 0xF
    return nac, duid


def find_sync_events_hard(dibits: np.ndarray) -> list[SyncEvent]:
    """Hard-decision Hamming-distance sync correlator. The simple, HDL-friendly
    version. Uses status-aware NID extraction now that we know the layout.
    """
    sync_register = 0
    out: list[SyncEvent] = []
    i = 0
    while i < len(dibits):
        d = int(dibits[i])
        sync_register = ((sync_register << 2) | d) & FRAME_SYNC_MASK
        i += 1
        if i < FRAME_SYNC_DIBITS:
            continue
        dist = bin(sync_register ^ FRAME_SYNC_DIBIT_PATTERN).count("1")
        if dist <= SYNC_THRESHOLD:
            nid = _extract_nid_skipping_status(dibits, i)
            if nid is None:
                break
            nac, duid = nid
            out.append(SyncEvent(
                symbol_idx=i,
                distance=dist,
                score=0.0,
                nac=nac,
                duid=duid,
            ))
            # Skip past this NID's 33 dibits before scanning again, exactly
            # like SDRTrunk's framer suppresses sync detection during message
            # assembly. Without this skip we'd false-trigger on dibit content
            # immediately following the sync.
            i += NID_TRANSMITTED_DIBITS
            sync_register = 0
    return out


def find_sync_events_soft(
    soft_phases: np.ndarray, hard_dibits: np.ndarray
) -> list[SyncEvent]:
    """Soft-symbol sync correlator. Direct port of P25P1SoftSyncDetectorScalar.

    For every soft symbol we compute:
        score = sum_{x=0..23}(SYNC_PATTERN_PHASES[x] * soft_phases[i-23+x])
    A perfect lock at the sync pattern gives score = 24 * (3π/4)² ≈ 133.
    SDRTrunk uses threshold 60 (about half the maximum possible value).

    Once a sync is detected we use the SAME hard dibits for NID extraction
    (with status-dibit skip), so we can compare hard vs soft fairly.
    """
    n = len(soft_phases)
    out: list[SyncEvent] = []
    if n < FRAME_SYNC_DIBITS + 1:
        return out

    # Vectorise the correlation: convolve sync pattern (reversed) against
    # the soft phase stream. scipy/numpy correlate is the same operation
    # SDRTrunk's scalar+vector implementations do internally.
    pattern = SYNC_PATTERN_PHASES.astype(np.float64)
    sp = soft_phases.astype(np.float64)
    # `np.correlate(sp, pattern, "valid")` produces an array of length
    # n - 24 + 1 where element k is sum(pattern[j] * sp[k+j], j=0..23).
    # Element k corresponds to a sync window ENDING at index k+24-1.
    scores = np.correlate(sp, pattern, mode="valid")
    above = np.where(scores > SYNC_SCORE_THRESHOLD)[0]
    if above.size == 0:
        return out

    # Walk through above-threshold windows and pick local maxima so we
    # don't double-trigger on the same sync. The window is 24 dibits wide
    # so adjacent above-threshold positions are very likely the same event.
    last_emit = -10**9
    for k in above:
        sync_end = int(k) + FRAME_SYNC_DIBITS - 1   # last dibit of sync window
        # Suppress if we just emitted very recently
        if sync_end - last_emit < FRAME_SYNC_DIBITS:
            continue
        # Local-maximum check: only emit if this is the peak of its
        # immediate neighbours (this is a cheap proxy for the proper
        # peak-finder; good enough for our 27-second test recording)
        prev = scores[k - 1] if k > 0 else -1e9
        nxt = scores[k + 1] if k + 1 < scores.shape[0] else -1e9
        if not (scores[k] >= prev and scores[k] >= nxt):
            continue
        first_nid_idx = sync_end + 1
        nid = _extract_nid_skipping_status(hard_dibits, first_nid_idx)
        if nid is None:
            break
        nac, duid = nid
        out.append(SyncEvent(
            symbol_idx=first_nid_idx,
            distance=-1,            # not used by soft detector
            score=float(scores[k]),
            nac=nac,
            duid=duid,
        ))
        last_emit = sync_end + NID_TRANSMITTED_DIBITS

    return out


# Backwards-compatible alias for old call sites in this file's main()
def find_sync_events(dibits: np.ndarray) -> list[SyncEvent]:
    """Default to the hard sync detector (matches what p25-httpd does today)."""
    return find_sync_events_hard(dibits)


# ============================================================================
# Stage 6: truth-log parser + diff
# ============================================================================
# SDRTrunk's decoded_messages.log format (one line per TSBK):
#
#   20260409 163748,PASSED,NAC:2209/x8A1 TSBK1 IDEN_UPDATE ID:4 ...
#   ^date    ^time  ^status ^NAC dec/hex  ^block ^opcode ...
#
# We only need the (status, NAC) pair to validate the demod produced the
# right NAC for the right number of NID detections. Higher-level TSBK
# parsing will be a future phase.

@dataclass
class TruthEntry:
    timestamp: str
    status: str            # "PASSED" or "FAILED"
    nac: int               # 12-bit numeric
    opcode: str            # e.g. "IDEN_UPDATE"
    raw: str

    @property
    def passed(self) -> bool:
        return self.status == "PASSED"


_TRUTH_RE = re.compile(
    r"^(?P<ts>\d{8} \d{6}),"
    r"(?P<status>PASSED|FAILED),"
    r"(?:NAC:\d+/x(?P<nac>[0-9A-Fa-f]+)\s+)?"
    r"(?:TSBK\d\s+(?P<opcode>\S+))?"
)


def parse_truth_log(path: Path) -> list[TruthEntry]:
    out: list[TruthEntry] = []
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        m = _TRUTH_RE.match(raw)
        if not m:
            continue
        nac_hex = m.group("nac")
        if nac_hex is None:
            # SYNC LOSS lines etc. -- skip; we count those separately
            continue
        out.append(TruthEntry(
            timestamp=m.group("ts"),
            status=m.group("status"),
            nac=int(nac_hex, 16),
            opcode=m.group("opcode") or "",
            raw=raw,
        ))
    return out


# ============================================================================
# Reporting
# ============================================================================

def _hist_top(values: list[int], n: int = 5) -> list[tuple[int, int]]:
    counts: dict[int, int] = {}
    for v in values:
        counts[v] = counts.get(v, 0) + 1
    return sorted(counts.items(), key=lambda kv: -kv[1])[:n]


def report(
    iq_len: int,
    sample_rate_in: float,
    sample_rate_after_dec: float,
    n_dibits: int,
    sync_events: list[SyncEvent],
    truth: list[TruthEntry] | None,
) -> None:
    duration_sec = iq_len / sample_rate_in
    print()
    print("=" * 72)
    print(" P25 LSM demod prototype -- run report")
    print("=" * 72)
    print(f" Input IQ          : {iq_len} samples at {sample_rate_in:.0f} Hz "
          f"({duration_sec:.2f} sec)")
    print(f" After decimation  : {int(sample_rate_after_dec)} Hz "
          f"(~{sample_rate_after_dec / P25_SYMBOL_RATE:.2f} sps)")
    print(f" Dibits produced   : {n_dibits} "
          f"({n_dibits / max(duration_sec, 1e-9):.1f} sym/s)")
    print(f"   expected ~      : {int(P25_SYMBOL_RATE * duration_sec)} "
          f"(at 4800 sym/s)")
    print()
    print(f" Sync events       : {len(sync_events)} "
          f"(threshold dist <= {SYNC_THRESHOLD})")
    if sync_events:
        dist_hist = _hist_top([e.distance for e in sync_events], n=10)
        print(f"   distances       : "
              + ", ".join(f"d{d}={c}" for d, c in dist_hist))
        nac_hist = _hist_top([e.nac for e in sync_events], n=5)
        print(f"   NACs (top 5)    : "
              + ", ".join(f"0x{n:03X}={c}" for n, c in nac_hist))
        duid_hist = _hist_top([e.duid for e in sync_events], n=8)
        print(f"   DUIDs (top 8)   : "
              + ", ".join(f"0x{d:X}={c}" for d, c in duid_hist))
        n_tsdu = sum(1 for e in sync_events if e.duid == 0x7)
        print(f"   DUID==7 (TSDU)  : {n_tsdu} "
              f"({100.0 * n_tsdu / len(sync_events):.1f}%)")

    if truth is None:
        print()
        return

    truth_passed = [t for t in truth if t.passed]
    truth_failed = [t for t in truth if not t.passed]
    # The truth log contains one line per *TSBK*, but each TSDU carries
    # up to 3 TSBKs (TSBK1, TSBK2, TSBK3). So `len(truth)` is the total
    # decoded TSBKs, NOT the number of frame-syncs that produced them.
    # The actual number of frame syncs == number of TSBK1 entries
    # because every TSDU starts with TSBK1.
    truth_tsbk1 = sum(1 for t in truth if " TSBK1 " in t.raw)
    truth_tsbk2 = sum(1 for t in truth if " TSBK2 " in t.raw)
    truth_tsbk3 = sum(1 for t in truth if " TSBK3 " in t.raw)
    truth_syncs = max(truth_tsbk1, 1)  # avoid div-by-zero on empty logs

    print()
    print(" Truth log         :")
    print(f"   PASSED TSBKs    : {len(truth_passed)}")
    print(f"   FAILED CRC      : {len(truth_failed)}")
    print(f"   TSBK1/2/3       : {truth_tsbk1}/{truth_tsbk2}/{truth_tsbk3}")
    print(f"   actual syncs    : {truth_syncs} (= TSBK1 count, "
          f"since every TSDU starts with TSBK1)")
    print(f"   total TSBKs     : {len(truth)} "
          f"({len(truth) / max(truth_syncs, 1):.2f} per sync)")
    if truth:
        truth_nac_hist = _hist_top([t.nac for t in truth], n=3)
        print(f"   Truth NAC top   : "
              + ", ".join(f"0x{n:03X}={c}" for n, c in truth_nac_hist))

    print()
    print(" Comparison vs truth syncs:")
    print(f"   prototype syncs : {len(sync_events)}")
    print(f"   truth syncs     : {truth_syncs}")
    if truth_syncs > 0:
        ratio = len(sync_events) / truth_syncs
        print(f"   ratio           : {ratio:.2%}")
        # ASCII-only verdict markers; Windows default cp1252 stdout chokes
        # on tick/cross glyphs.
        if 0.95 <= ratio <= 1.10:
            print("   verdict         : [PASS] within +/-10% of truth -- "
                  "demod is producing the right number of syncs")
        elif ratio < 0.5:
            print("   verdict         : [FAIL] FAR below truth -- "
                  "demod is missing most syncs (algorithm bug?)")
        elif ratio > 1.5:
            print("   verdict         : [WARN] above truth -- "
                  "false positives (sync threshold too loose?)")
        else:
            print("   verdict         : [WARN] in same order of magnitude, "
                  "iterate")

        if sync_events:
            target_nac = max(set(t.nac for t in truth),
                             key=lambda n: sum(1 for t in truth if t.nac == n))
            n_nac_match = sum(1 for e in sync_events if e.nac == target_nac)
            n_duid_match = sum(1 for e in sync_events if e.duid == 0x7)
            print(f"   target NAC      : 0x{target_nac:03X} "
                  f"(most common in truth)")
            print(f"   NAC match (raw) : {n_nac_match}/{len(sync_events)} = "
                  f"{100.0 * n_nac_match / len(sync_events):.1f}% "
                  f"(no FEC; BCH(64,16) would correct most residual errors)")
            print(f"   DUID==7 (TSDU)  : {n_duid_match}/{len(sync_events)} = "
                  f"{100.0 * n_duid_match / len(sync_events):.1f}% "
                  f"(control channel should be ~100% TSDU)")
    print()


def maybe_plot(
    result: DemodResult,
    hard_events: list[SyncEvent],
    soft_events: list[SyncEvent],
    save_path: Path | None = None,
) -> None:
    """Visual diagnostic dashboard.

    Six panels:
        (0,0) Constellation (post-PLL soft symbols, every Nth point)
        (0,1) PLL phase trace over time
        (0,2) Hard-sync Hamming distance histogram
        (1,0) Symbol-timing samplePoint trace
        (1,1) Sync hits over time (both hard + soft)
        (1,2) NAC histogram across all sync events (top 10)
    """
    try:
        import matplotlib.pyplot as plt
    except ImportError:
        print("matplotlib not installed; skipping --plot", file=sys.stderr)
        return

    fig, axs = plt.subplots(2, 3, figsize=(15, 8))

    # ----- (0,0) Constellation -----
    # Plot every 4th symbol so the figure isn't a mud puddle. Use a small
    # alpha so density is visible. We expect 4 lobes at ±π/4 and ±3π/4
    # if the PLL is locked.
    sym = result.soft_symbols[::4]
    axs[0, 0].scatter(sym.real, sym.imag, s=1, alpha=0.3, color="steelblue")
    axs[0, 0].set_title("Constellation (every 4th symbol)")
    axs[0, 0].set_xlabel("I (in-phase)"); axs[0, 0].set_ylabel("Q (quadrature)")
    axs[0, 0].axhline(0, color="gray", lw=0.5)
    axs[0, 0].axvline(0, color="gray", lw=0.5)
    axs[0, 0].set_aspect("equal")

    # ----- (0,1) PLL phase trace -----
    axs[0, 1].plot(result.pll_trace, lw=0.5)
    axs[0, 1].set_title("Tracked PLL phase (radians)")
    axs[0, 1].set_xlabel("symbol index")
    axs[0, 1].axhline(MAX_PLL_ABS, color="r", lw=0.5, linestyle="--",
                      label=f"+pi/3 ({MAX_PLL_ABS:.2f})")
    axs[0, 1].axhline(-MAX_PLL_ABS, color="r", lw=0.5, linestyle="--")
    axs[0, 1].axhline(0, color="gray", lw=0.3)
    axs[0, 1].legend(fontsize=8)

    # ----- (0,2) Hard-sync Hamming distance histogram -----
    if hard_events:
        dists = [e.distance for e in hard_events]
        max_d = max(dists)
        axs[0, 2].hist(dists, bins=range(0, max_d + 2),
                       align="left", edgecolor="black", color="seagreen")
        axs[0, 2].set_title(
            f"Hard sync distances ({len(hard_events)} events)")
        axs[0, 2].set_xlabel("Hamming distance to sync (lower = better)")
        axs[0, 2].set_ylabel("count")
    else:
        axs[0, 2].text(0.5, 0.5, "no hard sync events",
                       ha="center", va="center", transform=axs[0, 2].transAxes)

    # ----- (1,0) Symbol timing trace -----
    axs[1, 0].plot(result.timing_trace, lw=0.5)
    axs[1, 0].set_title("Gardner samplePoint over time")
    axs[1, 0].set_xlabel("symbol index")
    axs[1, 0].axhline(result.samples_per_symbol, color="r", lw=0.5,
                      linestyle="--",
                      label=f"sps={result.samples_per_symbol:.2f}")
    axs[1, 0].legend(fontsize=8)

    # ----- (1,1) Sync hits over time -----
    # X axis: symbol index. Y axis: 0 or 1, jittered slightly so hard and
    # soft don't fully overlap. Tells us if syncs are evenly distributed
    # (good) or clustered/burst-y (bad — means demod loses lock).
    if hard_events:
        x_h = [e.symbol_idx for e in hard_events]
        axs[1, 1].vlines(x_h, 0, 1, color="seagreen", lw=0.5,
                         label=f"hard ({len(hard_events)})")
    if soft_events:
        x_s = [e.symbol_idx for e in soft_events]
        axs[1, 1].vlines(x_s, 1.1, 2.1, color="darkorange", lw=0.5,
                         label=f"soft ({len(soft_events)})")
    axs[1, 1].set_title("Sync hits over time")
    axs[1, 1].set_xlabel("symbol index")
    axs[1, 1].set_yticks([0.5, 1.6])
    axs[1, 1].set_yticklabels(["hard", "soft"])
    axs[1, 1].legend(fontsize=8, loc="upper right")

    # ----- (1,2) NAC histogram (top N from hard sync events) -----
    if hard_events:
        nac_counts: dict[int, int] = {}
        for e in hard_events:
            nac_counts[e.nac] = nac_counts.get(e.nac, 0) + 1
        top = sorted(nac_counts.items(), key=lambda kv: -kv[1])[:10]
        labels = [f"0x{n:03X}" for n, _ in top]
        counts = [c for _, c in top]
        colors = ["seagreen" if n == 0x8A1 else "lightgray"
                  for n, _ in top]
        axs[1, 2].bar(range(len(top)), counts, color=colors,
                      edgecolor="black")
        axs[1, 2].set_xticks(range(len(top)))
        axs[1, 2].set_xticklabels(labels, rotation=45, ha="right")
        axs[1, 2].set_title("Top 10 NACs (raw, no FEC) -- target highlighted")
        axs[1, 2].set_ylabel("count")
    else:
        axs[1, 2].text(0.5, 0.5, "no events", ha="center", va="center",
                       transform=axs[1, 2].transAxes)

    plt.tight_layout()
    if save_path is not None:
        plt.savefig(str(save_path), dpi=120, bbox_inches="tight")
        print(f"# saved plot to {save_path}")
    plt.show()


# ============================================================================
# Main
# ============================================================================

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Python reference port of SDRTrunk's P25 LSM demod chain",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--wav", type=Path, required=True,
                   help="SDRTrunk baseband .wav recording (50 kSPS post-channelizer)")
    p.add_argument("--truth", type=Path, default=None,
                   help="optional SDRTrunk decoded_messages.log to validate against")
    p.add_argument("--plot", action="store_true",
                   help="plot constellation, PLL trace, timing trace, sync histogram")
    p.add_argument("--plot-save", type=Path, default=None,
                   help="if --plot is given, also save the figure to this path")
    return p.parse_args()


def main() -> int:
    args = parse_args()

    print(f"# loading {args.wav.name} ...")
    iq, sr_in = load_wav_iq(args.wav)
    print(f"  {len(iq)} samples at {sr_in} Hz "
          f"({len(iq) / sr_in:.2f} sec)")

    print("# stage 1: decimation ...")
    dec = select_decimation(sr_in)
    iq_dec = decimate_iq(iq, dec)
    sr = sr_in / dec
    print(f"  factor={dec} -> {sr:.0f} Hz, {len(iq_dec)} samples")

    print("# stage 2: baseband LPF (Parks-McClellan) ...")
    lpf = design_baseband_lpf(sr)
    iq_lpf = apply_real_fir(iq_dec, lpf)
    print(f"  designed {len(lpf)}-tap LPF (passband<{LPF_PASSBAND_HZ}, "
          f"stopband>{LPF_STOPBAND_HZ})")

    print("# stage 3: RRC matched filter ...")
    sps = sr / P25_SYMBOL_RATE
    rrc = design_rrc(sps, RRC_SYMBOL_LENGTH, RRC_ROLLOFF)
    iq_rrc = apply_real_fir(iq_lpf, rrc)
    print(f"  designed {len(rrc)}-tap RRC (alpha={RRC_ROLLOFF}, "
          f"~{sps:.2f} sps)")

    print("# stage 4: LSM demodulator ...")
    t0 = time.monotonic()
    result = demod_lsm(iq_rrc, sr)
    elapsed = time.monotonic() - t0
    print(f"  produced {result.n_symbols} dibits in {elapsed:.2f}s "
          f"({result.n_symbols / max(elapsed, 1e-9):.0f} sym/s of CPU)")

    print("# stage 5a: HARD sync detector (hamming distance on dibits) ...")
    hard_events = find_sync_events_hard(result.hard_dibits)
    print(f"  found {len(hard_events)} sync events "
          f"(threshold dist <= {SYNC_THRESHOLD})")

    print("# stage 5b: SOFT sync detector (correlation on soft phases) ...")
    soft_events = find_sync_events_soft(result.soft_phases, result.hard_dibits)
    print(f"  found {len(soft_events)} sync events "
          f"(threshold score > {SYNC_SCORE_THRESHOLD})")

    truth = None
    if args.truth is not None:
        print(f"# loading truth log {args.truth.name} ...")
        truth = parse_truth_log(args.truth)
        print(f"  parsed {len(truth)} entries "
              f"({sum(1 for t in truth if t.passed)} PASSED, "
              f"{sum(1 for t in truth if not t.passed)} FAILED)")

    print()
    print("=" * 72)
    print(" HARD SYNC DETECTOR REPORT")
    print("=" * 72)
    report(len(iq), sr_in, sr, result.n_symbols, hard_events, truth)

    print()
    print("=" * 72)
    print(" SOFT SYNC DETECTOR REPORT (SDRTrunk's chosen detector for LSM)")
    print("=" * 72)
    report(len(iq), sr_in, sr, result.n_symbols, soft_events, truth)

    if args.plot:
        maybe_plot(result, hard_events, soft_events, args.plot_save)

    return 0


if __name__ == "__main__":
    sys.exit(main())
