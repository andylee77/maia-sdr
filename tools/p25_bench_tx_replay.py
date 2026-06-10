#!/usr/bin/env python3
"""Replay a recorded IQ capture through a PlutoSDR TX for bench testing.

Part of the live-glitch validation campaign
(doc/LIVE_GLITCH_VALIDATION_PLAN.md, "Bench rig" section): a second
Pluto + attenuators, CABLED to the Fishball RX, replays a wideband
site capture so the full chain (CC decode -> grant -> retune ->
traffic decode) sees identical, ground-truth-known RF every run.

SAFETY: cabled + attenuated ONLY. Never attach an antenna — the
replay content is public-safety band RF and over-the-air transmission
is illegal. Start with ~60 dB total attenuation (TX gain + pads) and
trim until the Fishball front end sees off-air-like levels.

Input formats:
  *.wav            2-channel 16-bit PCM I/Q (SDRTrunk baseband style)
  *.cs16 / *.iq    raw interleaved int16 I/Q, little-endian

SDRTrunk-style filenames (<unix>_<freqHz>_<rateHz>_baseband.wav and
the matching .cs16 convention) are parsed for default center
frequency and sample rate, so usually only --tx-gain needs thought:

  python tools/p25_bench_tx_replay.py \
      "C:/Users/Andy/SDRTrunk/my_captures/1777801424_859212970_4000000_baseband.wav" \
      --start 30 --duration 60 --cyclic

Modes:
  --cyclic     load one clip into the Pluto's DMA buffer and let the
               device loop it forever, gap-free (recommended; clip
               must fit in memory — keep it under ~30 s at 4 MSPS).
  (default)    stream the file from the host in chunks, optionally
               --loop. Simpler for very long files but network hiccups
               become TX gaps — do NOT use streaming mode while
               measuring glitch rates.

Requires: pyadi-iio + numpy  (pip install pyadi-iio numpy)
Default URI ip:pluto.local — the USB-attached Pluto's mDNS name. The
Fishball owns 192.168.2.1, so never point this at that address.
"""

import argparse
import re
import struct
import sys
import time
import wave
from pathlib import Path

import numpy as np

# DAC full scale for pyadi-iio's float TX path is 2**14. Back off a
# little so filter overshoot can't clip.
TX_FULL_SCALE = 2 ** 14 * 0.9

FILENAME_RE = re.compile(r"^(\d+)_(\d+)_(\d+)_")


def parse_filename_defaults(path: Path):
    """Return (freq_hz, rate_hz) parsed from an SDRTrunk-style name, or
    (None, None)."""
    m = FILENAME_RE.match(path.name)
    if not m:
        return None, None
    _, freq, rate = m.groups()
    return int(freq), int(rate)


def wav_reader(path: Path, start_s: float, duration_s: float, chunk_samples: int):
    """Yield complex64 chunks from a 2-channel 16-bit WAV."""
    wf = wave.open(str(path), "rb")
    try:
        if wf.getnchannels() != 2 or wf.getsampwidth() != 2:
            sys.exit(
                f"error: {path.name} is not 2-channel 16-bit "
                f"(got {wf.getnchannels()} ch, {wf.getsampwidth() * 8} bit)"
            )
        rate = wf.getframerate()
        wf.setpos(int(start_s * rate))
        remaining = (
            int(duration_s * rate) if duration_s else wf.getnframes() - wf.tell()
        )
        while remaining > 0:
            n = min(chunk_samples, remaining)
            raw = wf.readframes(n)
            if not raw:
                break
            got = len(raw) // 4
            remaining -= got
            pairs = np.frombuffer(raw, dtype=np.int16).astype(np.float32)
            yield (pairs[0::2] + 1j * pairs[1::2]).astype(np.complex64)
    finally:
        wf.close()


def cs16_reader(path: Path, rate: int, start_s: float, duration_s: float,
                chunk_samples: int):
    """Yield complex64 chunks from raw interleaved int16 I/Q."""
    total_samples = path.stat().st_size // 4
    start = min(int(start_s * rate), total_samples)
    remaining = total_samples - start
    if duration_s:
        remaining = min(remaining, int(duration_s * rate))
    with open(path, "rb") as f:
        f.seek(start * 4)
        while remaining > 0:
            n = min(chunk_samples, remaining)
            raw = f.read(n * 4)
            if not raw:
                break
            got = len(raw) // 4
            remaining -= got
            pairs = np.frombuffer(raw, dtype=np.int16).astype(np.float32)
            yield (pairs[0::2] + 1j * pairs[1::2]).astype(np.complex64)


def normalise(iq: np.ndarray) -> np.ndarray:
    """Scale a clip so its peak hits TX_FULL_SCALE."""
    peak = float(np.max(np.abs(iq))) or 1.0
    return iq * (TX_FULL_SCALE / peak)


def main():
    ap = argparse.ArgumentParser(
        description="Replay an IQ capture through a PlutoSDR TX (bench rig).",
    )
    ap.add_argument("file", type=Path, help=".wav (2ch int16) or .cs16/.iq")
    ap.add_argument("--uri", default="ip:pluto.local",
                    help="Pluto IIO URI (default ip:pluto.local; NOT the "
                         "Fishball's 192.168.2.1)")
    ap.add_argument("--freq", type=float, default=None,
                    help="TX LO in Hz (default: parsed from filename)")
    ap.add_argument("--rate", type=float, default=None,
                    help="sample rate in Hz (default: parsed from filename, "
                         "or WAV header)")
    ap.add_argument("--tx-gain", type=float, default=-30.0,
                    help="tx_hardwaregain in dB, -89.75..0 (default -30; "
                         "more negative = quieter)")
    ap.add_argument("--start", type=float, default=0.0,
                    help="seconds into the file to start (default 0)")
    ap.add_argument("--duration", type=float, default=0.0,
                    help="seconds to play per pass (default: to EOF)")
    ap.add_argument("--cyclic", action="store_true",
                    help="device-side gap-free loop of one clip "
                         "(recommended for measurements)")
    ap.add_argument("--loop", action="store_true",
                    help="streaming mode: restart from --start at EOF")
    args = ap.parse_args()

    if not args.file.exists():
        sys.exit(f"error: {args.file} not found")
    if "192.168.2.1" in args.uri:
        sys.exit("error: --uri points at the Fishball, not the TX Pluto")

    name_freq, name_rate = parse_filename_defaults(args.file)
    is_wav = args.file.suffix.lower() == ".wav"
    if is_wav and args.rate is None:
        with wave.open(str(args.file), "rb") as wf:
            name_rate = wf.getframerate()
    freq = args.freq or name_freq
    rate = args.rate or name_rate
    if not freq or not rate:
        sys.exit("error: --freq/--rate required (filename gave no defaults)")
    freq, rate = int(freq), int(rate)

    try:
        import adi
    except ImportError:
        sys.exit("error: pyadi-iio not installed (pip install pyadi-iio)")

    chunk = 1 << 20  # 1M samples per streaming push
    reader = (
        wav_reader(args.file, args.start, args.duration, chunk)
        if is_wav
        else cs16_reader(args.file, rate, args.start, args.duration, chunk)
    )

    print(f"Pluto TX: {args.uri}  LO={freq / 1e6:.6f} MHz  "
          f"rate={rate / 1e6:.3f} MSPS  gain={args.tx_gain:+.2f} dB")
    print("REMINDER: cabled + attenuated only. No antenna.")

    sdr = adi.Pluto(uri=args.uri)
    sdr.tx_destroy_buffer()
    sdr.sample_rate = rate
    sdr.tx_rf_bandwidth = rate
    sdr.tx_lo = freq
    sdr.tx_hardwaregain_chan0 = args.tx_gain

    try:
        if args.cyclic:
            clip = np.concatenate(list(reader))
            secs = len(clip) / rate
            mib = clip.nbytes / 2 ** 20
            print(f"cyclic clip: {len(clip)} samples ({secs:.1f} s, "
                  f"{mib:.0f} MiB host-side)")
            if secs > 35:
                print("warning: long cyclic clips may exhaust Pluto DMA "
                      "memory; consider --duration <=30")
            sdr.tx_cyclic_buffer = True
            sdr.tx(normalise(clip))
            print("device looping gap-free — Ctrl-C to stop")
            while True:
                time.sleep(1)
        else:
            sdr.tx_cyclic_buffer = False
            passes = 0
            while True:
                sent = 0
                t0 = time.time()
                for chunk_iq in reader:
                    sdr.tx(normalise(chunk_iq))
                    sent += len(chunk_iq)
                passes += 1
                print(f"pass {passes}: {sent} samples in "
                      f"{time.time() - t0:.1f} s")
                if not args.loop:
                    break
                reader = (
                    wav_reader(args.file, args.start, args.duration, chunk)
                    if is_wav
                    else cs16_reader(args.file, rate, args.start,
                                     args.duration, chunk)
                )
    except KeyboardInterrupt:
        print("\nstopping")
    finally:
        try:
            sdr.tx_destroy_buffer()
            sdr.tx_hardwaregain_chan0 = -89.0
        except Exception:
            pass
        print("TX buffer destroyed, gain floored")


if __name__ == "__main__":
    main()
