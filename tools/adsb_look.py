#!/usr/bin/env python3
"""What a raw capture at 1090 MHz holds of Mode S: the extended squitters (ADS-B) and the short
all-call replies with a good parity, and what the squitters say (no scanner needed).

    adsb_look.py FILE.cs16 [--messages [LIST]]

The capture is 16-bit little-endian I and Q at 4 MSPS with its LO at 1090 MHz, as `iio_readdev`
takes it on a unit with the scanner stopped (doc/changes/113 has the commands). Prints how many
messages have a good parity, how strong they are, and each aircraft heard: its address, its
callsign, its altitudes, a position where an even and an odd half of one were both heard, and
its speed.

It finds the messages as the scanner does (scanner/src/protocol/adsb/mode_s.rs), sample for
sample and in whole numbers, so the two can be held against each other: `--messages` writes
each message with the sample its preamble begins at (to LIST, or beside the capture as
FILE.messages), which is what the scanner's test reads (ADSB_CAPTURE_DIR).

The facts it is written from: pulse-position keying at 1 Mbit/s; a preamble of four
half-microsecond pulses starting 0, 1.0, 3.5 and 4.5 us in; 56 or 112 bits after 8 us, a pulse in
a bit's first half a one; the first five bits the format (17 and 18: an extended squitter, 112
bits; 11: an all-call reply, 56 bits); the last 24 bits a parity by the generator 0xFFF409, which
leaves nothing over a whole message; a squitter's bits 9 to 32 the aircraft's address, 33 to 88
its message, whose first five bits are its type (1 to 4: a callsign of eight six-bit characters;
9 to 18: an altitude and half a position; 19: a velocity). dump1090 is the reference such a
reader is to be held against; none of its code is here.

The method: each sample as its amplitude in sixteenths of the ADC's step; a preamble where the
eight samples of its pulses hold more than a third of its 32 samples' sum (their mean one and a
half times the others'), at the sample where they hold the most; each bit by which of its halves
holds more; no bit corrected; the same message found again within 200 samples is one.
"""

import argparse
import math
import sys
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

CHARS = "#ABCDEFGHIJKLMNOPQRSTUVWXYZ#####_###############0123456789######"
RATE = 4_000_000
SPAN = 32 + 112 * 4
TABLE = []
for i in range(256):
    c = i << 16
    for _ in range(8):
        c = ((c << 1) ^ 0xFFF409) if c & 0x800000 else (c << 1)
        c &= 0xFFFFFF
    TABLE.append(c)
TABLE = np.array(TABLE, dtype=np.uint32)


def parity(data):
    """The 24-bit remainder of each row of bytes."""
    crc = np.zeros(data.shape[0], dtype=np.uint32)
    for k in range(data.shape[1]):
        crc = ((crc << 8) & 0xFFFFFF) ^ TABLE[((crc >> 16) ^ data[:, k]) & 0xFF]
    return crc


def amplitude(block):
    """Each sample's amplitude in sixteenths of the ADC's step, a whole number."""
    i = np.clip(block[:, 0].astype(np.int64), -2048, 2048)
    q = np.clip(block[:, 1].astype(np.int64), -2048, 2048)
    return np.floor(np.sqrt((i * i + q * q).astype(np.float32)) * np.float32(16) + np.float32(0.5)).astype(np.int64)


def nl(lat):
    """The number of longitude zones at a latitude."""
    if lat == 0:
        return 59
    if abs(lat) >= 87:
        return 1 if abs(lat) > 87 else 2
    a = 1 - math.cos(math.pi / 30)
    b = math.cos(math.pi / 180 * abs(lat)) ** 2
    return int(math.floor(2 * math.pi / math.acos(1 - a / b)))


def position(even, odd, newest_odd):
    """A whole position from an even and an odd half (each 17 bits of latitude and longitude)."""
    lat_e, lon_e = even[0] / 131072, even[1] / 131072
    lat_o, lon_o = odd[0] / 131072, odd[1] / 131072
    j = math.floor(59 * lat_e - 60 * lat_o + 0.5)
    le = 6.0 * ((j % 60) + lat_e)
    lo = 360 / 59 * ((j % 59) + lat_o)
    le = le - 360 if le >= 270 else le
    lo = lo - 360 if lo >= 270 else lo
    if nl(le) != nl(lo):
        return None
    lat = lo if newest_odd else le
    n = max(nl(lat) - (1 if newest_odd else 0), 1)
    m = math.floor(lon_e * (nl(lat) - 1) - lon_o * nl(lat) + 0.5)
    lon = 360 / n * ((m % n) + (lon_o if newest_odd else lon_e))
    return lat, lon - 360 if lon >= 180 else lon


def messages(raw):
    """Every message in the capture: (sample, bytes, its pulses over the rest in dB, its pulses'
    amplitude), the long and the short ones, and how many preambles were read. Every sample but
    the first, and the last that leave no room for a long message, is tried once."""
    total = raw.shape[0]
    chunk = RATE
    long_msgs, short_msgs, tried, floor = [], [], 0, []
    for start in range(0, total, chunk):
        lo = max(start - 1, 0)
        p = amplitude(np.asarray(raw[lo:start + chunk + SPAN]))
        first = start - lo if start else 1
        last = min(p.size - SPAN, first + chunk - (0 if start else 1))
        if last <= first:
            break
        floor.append(float(np.median(p)) / 16)
        pair = p[:-1] + p[1:]
        hi = last + 1
        pulses = pair[0:hi + 1] + pair[4:hi + 5] + pair[14:hi + 15] + pair[18:hi + 19]
        csum = np.concatenate(([0], np.cumsum(p)))
        window = csum[32:hi + 33] - csum[0:hi + 1]
        n = np.arange(first, last)
        at = n[(3 * pulses[n] > window[n]) & (pulses[n] >= pulses[n - 1]) & (pulses[n] > pulses[n + 1])]
        tried += at.size
        if not at.size:
            continue
        # Each bit: its first half against its second.
        k = np.arange(112) * 4 + 32
        ones = (p[at[:, None] + k] + p[at[:, None] + k + 1]) > (p[at[:, None] + k + 2] + p[at[:, None] + k + 3])
        data = np.packbits(ones.astype(np.uint8), axis=1)
        df = data[:, 0] >> 3
        over = 20 * np.log10(3 * pulses[at] / np.maximum(window[at] - pulses[at], 1))
        level = pulses[at] / 8 / 16
        for row in np.flatnonzero((parity(data[:, :14]) == 0) & ((df == 17) | (df == 18))):
            long_msgs.append((lo + int(at[row]), bytes(data[row, :14]), float(over[row]), float(level[row])))
        for row in np.flatnonzero((parity(data[:, :7]) == 0) & (df == 11)):
            short_msgs.append((lo + int(at[row]), bytes(data[row, :7]), float(over[row]), float(level[row])))

    # The same message is found at neighbouring samples: one a time.
    def single(msgs):
        out = []
        for msg in sorted(msgs):
            if not out or msg[1] != out[-1][1] or msg[0] - out[-1][0] > 200:
                out.append(msg)
        return out

    return single(long_msgs), single(short_msgs), tried, float(np.median(floor)) if floor else 0.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--rate", type=int, default=RATE, help="the capture's sample rate: 4 MSPS is what this reads")
    ap.add_argument("--messages", nargs="?", const=True, help="write each message with its sample: to this file, or beside the capture as FILE.messages")
    a = ap.parse_args()
    if a.rate != RATE:
        raise SystemExit("written for 4 MSPS: four samples a microsecond")

    raw = np.memmap(a.file, dtype="<i2", mode="r").reshape(-1, 2)
    total = raw.shape[0]
    peak = max(int(np.abs(raw[s:s + RATE]).max()) for s in range(0, total, RATE))
    clipped = sum(int((np.abs(raw[s:s + RATE].astype(np.int32)) >= 2047).sum()) for s in range(0, total, RATE))
    long_msgs, short_msgs, tried, noise = messages(raw)
    seconds = total / a.rate
    print(f"{a.file.replace(chr(92), '/').rsplit('/', 1)[-1]}: {seconds:.1f} s at {a.rate / 1e6:g} MSPS; the largest sample {peak} of 2047, {clipped} at full scale; median amplitude {noise:.1f}")
    print(f"  preambles tried {tried:,}; extended squitters with a good parity {len(long_msgs)} ({len(long_msgs) / seconds:.1f} a second); all-call replies with a good parity {len(short_msgs)}")
    if a.messages:
        listed = Path(a.file).with_suffix(".messages") if a.messages is True else Path(a.messages)
        with open(listed, "w", encoding="ascii", newline="\n") as f:
            for at, msg, _, _ in sorted(long_msgs + short_msgs):
                f.write(f"{at} {msg.hex()}\n")
        print(f"  wrote {listed}")
    if not long_msgs:
        return 0
    amp = np.array([m[3] for m in long_msgs])
    print(f"  their pulses over the median amplitude: weakest {20 * np.log10(amp.min() / noise):.1f} dB, median {20 * np.log10(np.median(amp) / noise):.1f} dB, strongest {20 * np.log10(amp.max() / noise):.1f} dB")
    by = defaultdict(lambda: {"n": 0, "types": Counter(), "callsign": None, "alt": [], "even": None, "odd": None, "pos": None, "speed": None})
    for at, msg, _, _ in short_msgs:
        by[msg[1:4].hex().upper()]["n"] += 1
    for at, msg, _, _ in long_msgs:
        t = at / a.rate
        icao = msg[1:4].hex().upper()
        me = int.from_bytes(msg[4:11], "big")
        tc = me >> 51
        p = by[icao]
        p["n"] += 1
        p["types"][tc] += 1
        if 1 <= tc <= 4:
            p["callsign"] = "".join(CHARS[(me >> (42 - 6 * k)) & 0x3F] for k in range(8)).replace("_", " ").strip()
        elif 9 <= tc <= 18:
            alt = (me >> 36) & 0xFFF
            if alt & 0x10:
                p["alt"].append((((alt & 0xFE0) >> 1) | (alt & 0xF)) * 25 - 1000)
            half = ((me >> 17) & 0x1FFFF, me & 0x1FFFF, t)
            if (me >> 34) & 1:
                p["odd"] = half
            else:
                p["even"] = half
            if p["even"] and p["odd"] and abs(p["even"][2] - p["odd"][2]) < 10:
                p["pos"] = position(p["even"], p["odd"], p["odd"][2] > p["even"][2]) or p["pos"]
        elif tc == 19 and ((me >> 48) & 7) in (1, 2):
            ew, ns = ((me >> 32) & 0x3FF) - 1, ((me >> 21) & 0x3FF) - 1
            if ew >= 0 and ns >= 0:
                p["speed"] = round(math.hypot(ew, ns))
    twice = sum(1 for p in by.values() if p["n"] >= 2)
    print(f"  aircraft {len(by)}, {twice} of them heard twice or more, {sum(1 for p in by.values() if p['pos'])} with a position:")
    for icao, p in sorted(by.items(), key=lambda kv: -kv[1]["n"]):
        alt = f"{min(p['alt']):,} to {max(p['alt']):,} ft" if p["alt"] else "-"
        pos = f"{p['pos'][0]:.2f}, {p['pos'][1]:.2f}" if p["pos"] else "-"
        print(f"    {icao}  {p['n']:4d} messages  callsign {p['callsign'] or '-':8s}  altitude {alt:22s}  position {pos:16s}  speed {p['speed'] or '-'} kt  types {dict(sorted(p['types'].items()))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
