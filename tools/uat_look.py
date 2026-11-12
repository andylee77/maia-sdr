#!/usr/bin/env python3
"""What a raw capture around 978 MHz holds of UAT, the second ADS-B link (aircraft below 18,000 ft
in the United States, and the ground stations' weather and traffic uplinks): the messages whose
Reed-Solomon check leaves nothing, and what they say (no scanner needed).

    uat_look.py FILE.cs16 [--offset-hz -1000000]

The capture is 16-bit little-endian I and Q at 4 MSPS, as `iio_readdev` takes it on a unit with
the scanner stopped (doc/changes/113 has the commands), with its LO a megahertz above 978 MHz
(the name's `979M`), so the radio's own DC is outside the signal; `--offset-hz` says where
978 MHz is from the LO.

The facts it is written from: continuous-phase frequency-shift keying at 1.041667 Mbit/s, a one
312.5 kHz above the carrier and a zero as far below; a message begins with 36 bits of
synchronisation, 0xEACDDA4E2 from an aircraft and its inverse from a ground station; an
aircraft's message is 144 bits and 96 of parity (Reed-Solomon RS(30,18)) or 272 and 112
(RS(48,34)), its first five bits saying which; a ground station's is 4,416 bits, six blocks of
RS(92,72) taken a byte from each in turn; the codes are over GF(256) with x^8+x^7+x^2+x+1, their
generators' roots the powers of alpha from 120 on. An aircraft's message: 24 bits of address
after its first byte, then a latitude of 23 bits and a longitude of 24 in steps of 360/2^24
degrees, and an altitude of 12 bits in steps of 25 ft from 1,000 ft under zero. dump978 is the
reference such a reader is to be held against; none of its code is here.

The method: the capture moved to 978 MHz and low-passed, each sample's turn of phase from the
one before as its frequency, the bits taken 3.84 samples apart from where the synchronisation
fits (three of its bits may be wrong). No error is corrected: a message counted is one received
whole, so weak ones that a decoder with the correction would take are missing here.
"""

import argparse
import sys
from collections import Counter, defaultdict

import numpy as np

RATE = 4_000_000
BIT = RATE / (25e6 / 24)
SYNC_AIR = 0xEACDDA4E2
SYNC_GROUND = 0x153225B1D
SYNC_BITS = 36
ROOT = 120
BASE40 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ  .."

# GF(256) by x^8 + x^7 + x^2 + x + 1.
EXP = [0] * 510
LOG = [0] * 256
_x = 1
for _i in range(255):
    EXP[_i] = EXP[_i + 255] = _x
    LOG[_x] = _i
    _x <<= 1
    if _x & 0x100:
        _x ^= 0x187


def syndromes(block, roots):
    """What a block leaves at each root of its code's generator: all zero when it is whole."""
    out = []
    for i in range(roots):
        step = (ROOT + i) % 255
        s = 0
        for byte in block:
            s = (EXP[LOG[s] + step] if s else 0) ^ int(byte)
        out.append(s)
    return out


def whole(block, roots):
    return not any(syndromes(block, roots))


def baseband(raw, offset_hz):
    """The capture at 978 MHz: turned by the offset (a quarter of the rate is a turn of 90
    degrees a sample) and low-passed to 0.8 MHz either side."""
    x = raw[:, 0].astype(np.float32) + 1j * raw[:, 1].astype(np.float32)
    n = np.arange(x.size)
    x = x * np.exp(-2j * np.pi * offset_hz / RATE * n).astype(np.complex64)
    k = np.arange(-12, 13)
    taps = np.sinc(2 * 0.8e6 / RATE * k) * np.hamming(k.size)
    return np.convolve(x, (taps / taps.sum()).astype(np.float32), mode="same")


def messages(raw, offset_hz):
    """Every message found: (sample, kind, bytes, mismatches in its synchronisation)."""
    offsets = np.round((np.arange(SYNC_BITS) + 0.5) * BIT).astype(np.int64)
    want = {name: np.array([(word >> (SYNC_BITS - 1 - k)) & 1 for k in range(SYNC_BITS)], dtype=np.int8) for name, word in (("air", SYNC_AIR), ("ground", SYNC_GROUND))}
    found, tried = [], Counter()
    chunk, lap = RATE, int(4500 * BIT) + 64
    busy_until = -1
    for start in range(0, raw.shape[0], chunk):
        x = baseband(np.asarray(raw[start:start + chunk + lap]), offset_hz)
        d = np.angle(x[1:] * np.conj(x[:-1]))
        # Three samples together are most of a bit.
        d = np.convolve(d, np.ones(3, dtype=np.float32) / 3, mode="same")
        level = np.abs(x)
        n = min(chunk, d.size - lap)
        if n <= 0:
            break
        ones = (d > 0).astype(np.int8)
        sampled = np.stack([ones[o:o + n] for o in offsets])
        for name, bits in want.items():
            wrong = (sampled != bits[:, None]).sum(axis=0)
            at = np.flatnonzero(wrong <= 3)
            tried[name] += at.size
            # Neighbouring samples fit the same message: the one with the fewest wrong, then
            # the widest eye.
            group = []
            for a in list(at) + [None]:
                if group and (a is None or a - group[-1] > 2):
                    best = min(group, key=lambda g: (wrong[g], -np.abs(d[g + offsets]).sum()))
                    if start + best > busy_until:
                        got = read(d, best, name)
                        if got:
                            kind, data, length = got
                            found.append((start + best, kind, data, int(wrong[best]), float(np.median(level[best:best + length]))))
                            busy_until = start + best + length
                    group = []
                if a is not None:
                    group.append(int(a))
    return found, tried


def bits_at(d, first, count):
    """`count` bits from the sample the first one begins at."""
    at = np.round(first + (np.arange(count) + 0.5) * BIT).astype(np.int64)
    return np.packbits((d[at] > 0).astype(np.uint8))


def read(d, at, name):
    """The message whose synchronisation begins at `at`, if its check leaves nothing: its kind,
    its bytes and its length in samples."""
    first = at + SYNC_BITS * BIT
    if name == "air":
        data = bits_at(d, first, 384)
        if data[0] >> 3 == 0 and whole(data[:30], 12):
            return "short", bytes(data[:30]), int((SYNC_BITS + 240) * BIT)
        if data[0] >> 3 != 0 and whole(data[:48], 14):
            return "long", bytes(data[:48]), int((SYNC_BITS + 384) * BIT)
        return None
    data = bits_at(d, first, 4416)
    blocks = [data[j::6] for j in range(6)]
    good = [whole(b, 20) for b in blocks]
    if not any(good):
        return None
    # The blocks' data one after another; a block that is not whole is left as zeros.
    payload = b"".join(bytes(b[:72]) if ok else bytes(72) for b, ok in zip(blocks, good))
    return f"uplink {sum(good)}/6", payload, int((SYNC_BITS + 4416) * BIT)


def place(b):
    """A latitude of 23 bits and a longitude of 24 from seven bytes, degrees."""
    lat = ((b[0] << 15) | (b[1] << 7) | (b[2] >> 1)) * 360 / 2 ** 24
    lon = (((b[2] & 1) << 23) | (b[3] << 15) | (b[4] << 7) | (b[5] >> 1)) * 360 / 2 ** 24
    return (lat - 180 if lat > 90 else lat), (lon - 360 if lon > 180 else lon)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--offset-hz", type=float, default=-1_000_000, help="where 978 MHz is from the capture's LO")
    a = ap.parse_args()
    raw = np.memmap(a.file, dtype="<i2", mode="r").reshape(-1, 2)
    seconds = raw.shape[0] / RATE
    noise = float(np.median(np.abs(raw[:RATE, 0].astype(np.float32) + 1j * raw[:RATE, 1].astype(np.float32))))
    peak = int(np.abs(raw[:RATE]).max())
    found, tried = messages(raw, a.offset_hz)
    kinds = Counter("uplink" if k.startswith("uplink") else k for _, k, _, _, _ in found)
    print(f"{a.file.replace(chr(92), '/').rsplit('/', 1)[-1]}: {seconds:.1f} s at {RATE / 1e6:g} MSPS; the largest sample in its first second {peak} of 2047; median amplitude {noise:.1f}")
    print(f"  synchronisations fitted: {tried['air']:,} of aircraft, {tried['ground']:,} of ground stations")
    print(f"  messages whole: {kinds['short']} short and {kinds['long']} long from aircraft, {kinds['uplink']} uplinks with a whole block")
    by = defaultdict(lambda: {"n": 0, "kinds": Counter(), "places": [], "alts": [], "ids": [], "levels": []})
    for at, kind, data, wrong, level in found:
        if kind.startswith("uplink"):
            continue
        p = by[data[1:4].hex().upper()]
        p["n"] += 1
        p["kinds"][f"type {data[0] >> 3}"] += 1
        p["levels"].append(level)
        lat, lon = place(data[4:10])
        if lat or lon:
            p["places"].append((lat, lon))
        alt = (data[10] << 4) | (data[11] >> 4)
        if alt:
            p["alts"].append((alt - 1) * 25 - 1000)
        if kind == "long" and data[0] >> 3 in (1, 3):
            # Three pairs of bytes, each three characters in base 40; the first is the category.
            # The eight characters are the callsign in some messages and the squawk in others.
            v = [(data[17 + 2 * k] << 8) | data[18 + 2 * k] for k in range(3)]
            chars = [(v[0] // 40) % 40, v[0] % 40] + [c for x in v[1:] for c in ((x // 1600) % 40, (x // 40) % 40, x % 40)]
            said = "".join(BASE40[c] for c in chars).strip()
            if said and said not in p["ids"]:
                p["ids"].append(said)
    print(f"  aircraft {len(by)}:")
    for icao, p in sorted(by.items(), key=lambda kv: -kv[1]["n"]):
        alt = f"{min(p['alts']):,} to {max(p['alts']):,} ft" if p["alts"] else "-"
        pos = f"{p['places'][-1][0]:.2f}, {p['places'][-1][1]:.2f}" if p["places"] else "-"
        print(f"    {icao}  {p['n']:3d} messages  callsign or squawk {', '.join(p['ids']) or '-':14s}  altitude {alt:20s}  position {pos:16s}  level {20 * np.log10(np.median(p['levels']) / max(noise, 1e-6)):5.1f} dB over the median  {dict(p['kinds'])}")
    uplinks = [(at, kind, data, level) for at, kind, data, _, level in found if kind.startswith("uplink")]
    if uplinks:
        stations = Counter()
        for at, kind, data, level in uplinks:
            if any(data[:72]):
                lat, lon = place(data[0:6])
                stations[(round(lat, 2), round(lon, 2), data[6] & 0x1F)] += 1
        print(f"  ground stations (where the uplink's first block says it is, and its slot): {dict(stations)}")
        print(f"  uplinks' blocks whole: {dict(Counter(kind for _, kind, _, _ in uplinks))}; level {20 * np.log10(np.median([u[3] for u in uplinks]) / max(noise, 1e-6)):.1f} dB over the median")
    return 0


if __name__ == "__main__":
    sys.exit(main())
