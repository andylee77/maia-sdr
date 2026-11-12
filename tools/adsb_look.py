#!/usr/bin/env python3
"""What a raw capture at 1090 MHz holds of Mode S: the extended squitters (ADS-B) and the short
all-call replies with a good parity, and what the squitters say (no scanner needed).

    adsb_look.py FILE.cs16 [--ratio 1.5]

The capture is 16-bit little-endian I and Q at 4 MSPS with its LO at 1090 MHz, as `iio_readdev`
takes it on a unit with the scanner stopped (doc/changes/112 has the commands). Prints how many
messages have a good parity, how strong they are, and each aircraft heard: its address, its
callsign, its altitudes, a position where an even and an odd half of one were both heard, and
its speed.

The facts it is written from: pulse-position keying at 1 Mbit/s; a preamble of four
half-microsecond pulses starting 0, 1.0, 3.5 and 4.5 us in; 56 or 112 bits after 8 us, a pulse in
a bit's first half a one; the first five bits the format (17 and 18: an extended squitter, 112
bits; 11: an all-call reply, 56 bits); the last 24 bits a parity by the generator 0xFFF409, which
leaves nothing over a squitter's whole message; a squitter's bits 9 to 32 the aircraft's
address, 33 to 88 its message, whose first five bits are its type (1 to 4: a callsign of eight
six-bit characters; 9 to 18: an altitude and half a position; 19: a velocity). dump1090 is the
reference such a reader is to be held against; none of its code is here.
"""

import argparse
import math
import sys
from collections import Counter, defaultdict

import numpy as np

CHARS = "#ABCDEFGHIJKLMNOPQRSTUVWXYZ#####_###############0123456789######"
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


def nl(lat):
    """The number of longitude zones at a latitude."""
    if abs(lat) >= 87:
        return 1
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--rate", type=int, default=4_000_000, help="the capture's sample rate: 4 MSPS is what this reads")
    ap.add_argument("--ratio", type=float, default=1.5, help="a preamble's pulses over what is between them, in amplitude")
    a = ap.parse_args()
    if a.rate != 4_000_000:
        raise SystemExit("written for 4 MSPS: four samples a microsecond")

    raw = np.memmap(a.file, dtype="<i2", mode="r").reshape(-1, 2)
    total = raw.shape[0]
    chunk, lap = 4_000_000, 600
    peak = 0
    clipped = 0
    floor = []
    long_msgs, short_msgs = [], []
    tried = 0
    for start in range(0, total, chunk):
        block = np.asarray(raw[start:start + chunk + lap]).astype(np.float32)
        peak = max(peak, int(np.abs(block).max()))
        clipped += int((np.abs(block) >= 2047).sum())
        i, q = block[:, 0] - block[:, 0].mean(), block[:, 1] - block[:, 1].mean()
        m = np.sqrt(i * i + q * q)
        floor.append(float(np.median(m)))
        n = m.size - 520
        if n <= 0:
            break
        pair = m[:-1] + m[1:]
        pulses = pair[0:n] + pair[4:n + 4] + pair[14:n + 14] + pair[18:n + 18]
        csum = np.concatenate(([0.0], np.cumsum(m, dtype=np.float64)))
        window = (csum[32:n + 32] - csum[0:n]).astype(np.float32)
        quiet = window - pulses
        hit = (pulses / 8 > a.ratio * quiet / 24) & (pulses >= np.roll(pulses, 1)) & (pulses > np.roll(pulses, -1))
        hit[0] = hit[-1] = False
        at = np.flatnonzero(hit)
        at = at[at < min(chunk, n)]
        tried += at.size
        if not at.size:
            continue
        # Each bit: its first half against its second.
        k = np.arange(112) * 4 + 32
        first = m[at[:, None] + k] + m[at[:, None] + k + 1]
        second = m[at[:, None] + k + 2] + m[at[:, None] + k + 3]
        bits = (first > second).astype(np.uint8)
        data = np.packbits(bits, axis=1)
        df = data[:, 0] >> 3
        strength = (pulses[at] / 8) / np.maximum(quiet[at] / 24, 1e-6)
        whole = (parity(data[:, :11]) == ((data[:, 11].astype(np.uint32) << 16) | (data[:, 12].astype(np.uint32) << 8) | data[:, 13])) & ((df == 17) | (df == 18))
        for row in np.flatnonzero(whole):
            long_msgs.append(((start + int(at[row])) / a.rate, bytes(data[row, :14]), float(strength[row]), float(pulses[at[row]] / 8)))
        short = (parity(data[:, :4]) == ((data[:, 4].astype(np.uint32) << 16) | (data[:, 5].astype(np.uint32) << 8) | data[:, 6])) & (df == 11)
        for row in np.flatnonzero(short):
            short_msgs.append(((start + int(at[row])) / a.rate, bytes(data[row, :7])))

    # The same message is found at neighbouring samples: one a time.
    def single(msgs):
        out = []
        for msg in sorted(msgs):
            if not out or msg[1] != out[-1][1] or msg[0] - out[-1][0] > 50e-6:
                out.append(msg)
        return out

    long_msgs, short_msgs = single(long_msgs), single(short_msgs)
    seconds = total / a.rate
    noise = float(np.median(floor))
    print(f"{a.file.replace(chr(92), '/').rsplit('/', 1)[-1]}: {seconds:.1f} s at {a.rate / 1e6:g} MSPS; the largest sample {peak} of 2047, {clipped} at full scale; median amplitude {noise:.1f}")
    print(f"  preambles tried {tried:,}; extended squitters with a good parity {len(long_msgs)} ({len(long_msgs) / seconds:.1f} a second); all-call replies with a good parity {len(short_msgs)}")
    if not long_msgs:
        return 0
    amp = np.array([m[3] for m in long_msgs])
    print(f"  their pulses over the median amplitude: weakest {20 * np.log10(amp.min() / noise):.1f} dB, median {20 * np.log10(np.median(amp) / noise):.1f} dB, strongest {20 * np.log10(amp.max() / noise):.1f} dB")
    by = defaultdict(lambda: {"n": 0, "types": Counter(), "callsign": None, "alt": [], "even": None, "odd": None, "pos": None, "speed": None})
    for t, msg, _, _ in long_msgs:
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
    print(f"  aircraft {len(by)}:")
    for icao, p in sorted(by.items(), key=lambda kv: -kv[1]["n"]):
        alt = f"{min(p['alt']):,} to {max(p['alt']):,} ft" if p["alt"] else "-"
        pos = f"{p['pos'][0]:.2f}, {p['pos'][1]:.2f}" if p["pos"] else "-"
        print(f"    {icao}  {p['n']:4d} messages  callsign {p['callsign'] or '-':8s}  altitude {alt:22s}  position {pos:16s}  speed {p['speed'] or '-'} kt  types {dict(sorted(p['types'].items()))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
