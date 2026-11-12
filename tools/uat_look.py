#!/usr/bin/env python3
"""What a raw capture around 978 MHz holds of UAT, the second ADS-B link (aircraft below 18,000 ft
in the United States, and the ground stations' weather and traffic uplinks): the messages its
Reed-Solomon codes can put right, and what they say (no scanner needed).

    uat_look.py FILE.cs16 [--messages [LIST]]

The capture is 16-bit little-endian I and Q at 4 MSPS with its LO at 979 MHz, a megahertz above
the link, as `iio_readdev` takes it on a unit with the scanner stopped (doc/changes/113 has the
commands).

It finds an aircraft's messages as the scanner does (scanner/src/protocol/adsb/uat/), sample for
sample and in whole numbers, so the two can be held against each other: `--messages` writes each
message with the sample its synchronisation begins at and the bytes its code put right (to
LIST, or beside the capture as FILE.messages), which is what the scanner's test reads
(UAT_CAPTURE_DIR). A ground station's uplinks are looked for too, which the scanner does not do.

The facts it is written from: continuous-phase frequency-shift keying at 1.041667 Mbit/s, a one
312.5 kHz above the carrier and a zero as far below; a message begins with 36 bits of
synchronisation, 0xEACDDA4E2 from an aircraft and its inverse from a ground station; an
aircraft's message is 144 bits and 96 of parity (Reed-Solomon RS(30,18)) or 272 and 112
(RS(48,34)), its first five bits saying which; a ground station's is 4,416 bits, six blocks of
RS(92,72) taken a byte from each in turn; the codes are over GF(256) with x^8+x^7+x^2+x+1, their
generators' roots the powers of alpha from 120 on. An aircraft's message: 24 bits of address
after its first byte; a latitude of 23 bits and a longitude of 24 in steps of 360/2^24 degrees;
an altitude of 12 bits in steps of 25 ft from 1,000 ft under zero; its velocity north and east,
each a sign and ten bits one more than the knots; its rate of climb, a sign and nine bits one
more than its steps of 64 ft a minute; in a long message of type 1 or 3, its category and eight
characters in base 40 that are its callsign when a flag says so and its squawk otherwise.
dump978 is the reference such a reader is to be held against; none of its code is here.

The method: each sample turned a quarter of a turn more than the one before (978 MHz to the
middle); four summed (nothing at the radio's own DC, now a megahertz off); the way the phase
turns from one sum to the next, over three samples together, as the frequency's sign; a message
where the synchronisation's 36 bits, read at the middle of each (3.84 samples a bit), fit with
at most four wrong; of the four samples from the first that fits, the middle one of those that
fit best; a message taken when its code can put it right (6 wrong bytes of a short one's 30, 7
of a long one's 48).
"""

import argparse
import math
import sys
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

RATE = 4_000_000
SYNC_AIR = 0xEACDDA4E2
SYNC_GROUND = 0x153225B1D
SYNC_BITS = 36
ROOT = 120
BASE40 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ  ??"
# Where each bit is read, in samples from the first of the synchronisation: its middle.
OFFSETS = np.array([((2 * k + 1) * 192 + 50) // 100 for k in range(SYNC_BITS + 4416)], dtype=np.int64)
LAST = int(OFFSETS[SYNC_BITS + 384 - 1])

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


def mul(a, b):
    return EXP[LOG[a] + LOG[b]] if a and b else 0


def inv(a):
    return EXP[255 - LOG[a]]


def at_point(poly, x):
    """poly[0] + poly[1] x + ..."""
    y = 0
    for c in reversed(poly):
        y = mul(y, x) ^ c
    return y


def left(block, roots):
    out = []
    for i in range(roots):
        a = EXP[(ROOT + i) % 255]
        s = 0
        for byte in block:
            s = mul(s, a) ^ int(byte)
        out.append(s)
    return out


def correct(block, roots):
    """The block with up to roots/2 wrong bytes put right, and how many were; None when it is
    further than that from any word of the code."""
    n = len(block)
    synd = left(block, roots)
    if not any(synd):
        return bytes(block), 0
    # The shortest locator that makes the syndromes (Berlekamp and Massey).
    sigma, old, length, shift, last = [1], [1], 0, 1, 1
    for r in range(roots):
        delta = synd[r]
        for i in range(1, length + 1):
            if i < len(sigma):
                delta ^= mul(sigma[i], synd[r - i])
        if delta == 0:
            shift += 1
            continue
        scaled = [0] * shift + [mul(c, mul(delta, inv(last))) for c in old]
        size = max(len(sigma), len(scaled))
        new = [a ^ b for a, b in zip(sigma + [0] * (size - len(sigma)), scaled + [0] * (size - len(scaled)))]
        if 2 * length <= r:
            old, last, length, shift = sigma, delta, r + 1 - length, 1
        else:
            shift += 1
        sigma = new
    while len(sigma) > 1 and sigma[-1] == 0:
        sigma.pop()
    if len(sigma) - 1 != length or length > roots // 2:
        return None
    # The byte at j is the coefficient of x^(n-1-j).
    places = [j for j in range(n) if at_point(sigma, EXP[(255 - (n - 1 - j)) % 255]) == 0]
    if len(places) != length:
        return None
    omega = [0] * roots
    for i in range(roots):
        for k in range(min(i, len(sigma) - 1) + 1):
            omega[i] ^= mul(sigma[k], synd[i - k])
    slope = [sigma[i] if i % 2 == 1 else 0 for i in range(1, len(sigma))]
    fixed = bytearray(block)
    for j in places:
        power = n - 1 - j
        x_inv = EXP[(255 - power) % 255]
        below = at_point(slope, x_inv)
        if below == 0:
            return None
        fixed[j] ^= mul(EXP[(power * (256 - ROOT)) % 255], mul(at_point(omega, x_inv), inv(below)))
    return (bytes(fixed), length) if not any(left(fixed, roots)) else None


def signs(raw, start):
    """The frequency's sign at each sample of `raw`, which begins at the capture's sample
    `start`, and each sample's power."""
    i = np.clip(raw[:, 0].astype(np.int64), -2048, 2048)
    q = np.clip(raw[:, 1].astype(np.int64), -2048, 2048)
    phase = (start + np.arange(i.size)) % 4
    ti = np.select([phase == 0, phase == 1, phase == 2], [i, -q, -i], q)
    tq = np.select([phase == 0, phase == 1, phase == 2], [q, i, -q], -i)
    ci = np.concatenate(([0], np.cumsum(ti)))
    cq = np.concatenate(([0], np.cumsum(tq)))
    k = np.arange(i.size)
    lo = np.maximum(k + 1 - 4, 0)
    si, sq = ci[k + 1] - ci[lo], cq[k + 1] - cq[lo]
    turn = np.zeros(i.size, dtype=np.int64)
    turn[1:] = sq[1:] * si[:-1] - si[1:] * sq[:-1]
    if start == 0:
        turn[0] = 0
    together = turn.copy()
    together[1:] += turn[:-1]
    together[:-1] += turn[1:]
    return (together > 0).astype(np.int8), i * i + q * q


def search(raw, word, read):
    """Every place the synchronisation `word` fits, in order, each group of neighbours given to
    `read(ones, power, place, sample)` at the middle of those that fit best; a message read
    there ends the search until its end. Returns the messages and the places that fitted."""
    want = np.array([(word >> (SYNC_BITS - 1 - k)) & 1 for k in range(SYNC_BITS)], dtype=np.int8)
    total = raw.shape[0]
    found, fitted, past = [], 0, 0
    chunk, lead, tail = RATE, 8, int(OFFSETS[-1]) + 80
    for start in range(0, total, chunk):
        first = max(start - lead, 0)
        ones, power = signs(np.asarray(raw[first:start + chunk + tail]), first)
        shift = start - first
        # The places of this chunk that have all of a long message, and of its group, after.
        count = min(chunk, total - LAST - 16 + 1 - start)
        if count <= 0:
            break
        wrong = np.zeros(count + 4, dtype=np.int16)
        for k in range(SYNC_BITS):
            at = shift + int(OFFSETS[k])
            wrong += ones[at:at + count + 4] != want[k]
        fits = np.flatnonzero(wrong[:count] <= 4)
        fitted += fits.size
        for a in fits:
            a = int(a)
            if start + a < past:
                continue
            group = wrong[a:a + 4]
            best = [m for m in range(4) if group[m] == group.min()]
            middle = a + best[(len(best) - 1) // 2]
            past = start + a + 4
            got = read(ones, power, shift + middle)
            if got:
                message, bits = got
                found.append((start + middle,) + message)
                past = start + middle + int(OFFSETS[SYNC_BITS + bits - 1])
    return found, fitted


def aircraft_message(ones, power, at):
    block = np.packbits(ones[at + OFFSETS[SYNC_BITS:SYNC_BITS + 384]].astype(np.uint8))
    short_said = block[0] >> 3 == 0
    for long in (not short_said, short_said):
        length, roots, size = (48, 14, 34) if long else (30, 12, 18)
        fixed = correct(block[:length], roots)
        if fixed and (fixed[0][0] >> 3 == 0) != long:
            level = 10 * math.log10(max(float(power[at:at + 64].mean()), 1.0) / 2048 ** 2)
            return (fixed[0][:size], fixed[1], level), length * 8
    return None


def uplink(ones, power, at):
    if at + int(OFFSETS[-1]) >= ones.size:
        return None
    data = np.packbits(ones[at + OFFSETS[SYNC_BITS:]].astype(np.uint8))
    blocks = [correct(data[j::6], 20) for j in range(6)]
    if not any(blocks):
        return None
    payload = b"".join(b[0][:72] if b else bytes(72) for b in blocks)
    level = 10 * math.log10(max(float(power[at:at + 64].mean()), 1.0) / 2048 ** 2)
    return (payload, sum(1 for b in blocks if b), level), 4416


def place(b):
    """A latitude of 23 bits and a longitude of 24 from six bytes, degrees."""
    lat = ((b[0] << 15) | (b[1] << 7) | (b[2] >> 1)) * 360 / 2 ** 24
    lon = (((b[2] & 1) << 23) | (b[3] << 15) | (b[4] << 7) | (b[5] >> 1)) * 360 / 2 ** 24
    return (lat - 180 if lat > 90 else lat), (lon - 360 if lon > 180 else lon)


def says(b):
    """What an aircraft's message says: its place, altitude, speed, track, rate of climb,
    callsign and category, each None when it does not."""
    out = {"lat": None, "lon": None, "alt": None, "speed": None, "track": None, "climb": None, "callsign": None, "category": None, "squawk": None}
    lat, lon = place(b[4:10])
    if lat or lon:
        out["lat"], out["lon"] = lat, lon
    alt = (b[10] << 4) | (b[11] >> 4)
    if alt:
        out["alt"] = (alt - 1) * 25 - 1000
    step = {0: 1, 1: 4}.get(b[12] >> 6)
    north = ((b[12] & 0x1F) << 6) | (b[13] >> 2)
    east = ((b[13] & 3) << 9) | (b[14] << 1) | (b[15] >> 7)
    if step and north & 0x3FF and east & 0x3FF:
        n = ((north & 0x3FF) - 1) * step * (-1 if north & 0x400 else 1)
        e = ((east & 0x3FF) - 1) * step * (-1 if east & 0x400 else 1)
        out["speed"], out["track"] = math.hypot(n, e), math.degrees(math.atan2(e, n)) % 360
    climb = ((b[15] & 0x7F) << 4) | (b[16] >> 4)
    if step and climb & 0x1FF:
        out["climb"] = ((climb & 0x1FF) - 1) * 64 * (-1 if climb & 0x200 else 1)
    if len(b) >= 34 and b[0] >> 3 in (1, 3):
        v = [(b[17 + 2 * k] << 8) | b[18 + 2 * k] for k in range(3)]
        which = (v[0] // 1600) % 40
        if 0 < which < 32:
            out["category"] = f"{'ABCD'[which // 8]}{which % 8}"
        chars = [(v[0] // 40) % 40, v[0] % 40] + [c for x in v[1:] for c in ((x // 1600) % 40, (x // 40) % 40, x % 40)]
        text = "".join(BASE40[c] for c in chars).strip()
        out["callsign" if b[26] & 2 else "squawk"] = text or None
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--messages", nargs="?", const=True, help="write each aircraft's message with its sample: to this file, or beside the capture as FILE.messages")
    a = ap.parse_args()
    raw = np.memmap(a.file, dtype="<i2", mode="r").reshape(-1, 2)
    seconds = raw.shape[0] / RATE
    first = np.asarray(raw[:RATE]).astype(np.float32)
    noise = float(np.median(np.hypot(first[:, 0], first[:, 1])))
    found, fitted = search(raw, SYNC_AIR, aircraft_message)
    uplinks, up_fitted = search(raw, SYNC_GROUND, uplink)
    print(f"{a.file.replace(chr(92), '/').rsplit('/', 1)[-1]}: {seconds:.1f} s at {RATE / 1e6:g} MSPS; the largest sample in its first second {int(np.abs(first).max())} of 2047; median amplitude {noise:.1f} ({20 * math.log10(max(noise, 1e-6) / 2048):.1f} dBFS)")
    print(f"  places the synchronisation fitted: {fitted:,} of an aircraft's, {up_fitted:,} of a ground station's")
    mended = Counter(m[2] for m in found)
    print(f"  aircraft's messages: {sum(1 for m in found if len(m[1]) == 18)} short and {sum(1 for m in found if len(m[1]) == 34)} long; bytes put right {dict(sorted(mended.items()))}; uplinks with a block put right: {len(uplinks)}")
    if a.messages:
        listed = Path(a.file).with_suffix(".messages") if a.messages is True else Path(a.messages)
        with open(listed, "w", encoding="ascii", newline="\n") as f:
            for at, payload, fixed, _ in found:
                f.write(f"{at} {payload.hex()} {fixed}\n")
        print(f"  wrote {listed}")
    by = defaultdict(lambda: {"n": 0, "kinds": Counter(), "last": {}, "alts": [], "levels": [], "relayed": False})
    for at, payload, fixed, level in found:
        p = by[payload[1:4].hex().upper()]
        p["n"] += 1
        p["kinds"][f"type {payload[0] >> 3}"] += 1
        p["levels"].append(level)
        p["relayed"] |= payload[0] & 7 > 1
        said = says(payload)
        if said["alt"] is not None:
            p["alts"].append(said["alt"])
        p["last"].update({k: v for k, v in said.items() if v is not None})
    print(f"  aircraft {len(by)}:")
    for icao, p in sorted(by.items(), key=lambda kv: -kv[1]["n"]):
        s = p["last"]
        alt = f"{min(p['alts']):,} to {max(p['alts']):,} ft" if p["alts"] else "-"
        pos = f"{s['lat']:.4f}, {s['lon']:.4f}" if "lat" in s else "-"
        speed = f"{s['speed']:.0f} kt on {s['track']:.0f}" if "speed" in s else "-"
        print(f"    {icao}{' (relayed)' if p['relayed'] else ''}  {p['n']:3d} messages  callsign {s.get('callsign', '-'):8s} squawk {s.get('squawk', '-'):5s} category {s.get('category', '-'):3s} altitude {alt:20s} position {pos:20s} {speed:16s} climb {s.get('climb', '-')} ft/min  level {np.median(p['levels']):.1f} dBFS  {dict(p['kinds'])}")
    if uplinks:
        stations = Counter()
        for at, payload, blocks, level in uplinks:
            if any(payload[:72]):
                lat, lon = place(payload[0:6])
                stations[(round(lat, 2), round(lon, 2), payload[6] & 0x1F)] += 1
        print(f"  ground stations (where an uplink's first block says it is, and its slot): {dict(stations)}; blocks put right of six: {dict(Counter(u[2] for u in uplinks))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
