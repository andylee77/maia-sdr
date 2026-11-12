#!/usr/bin/env python3
"""Data mode's listening and its captures, through a scanner's API (`/api/v1/data`).

    data_captures.py --host 192.168.120.50 status
    data_captures.py --host 192.168.120.50 keep --count 2 [--freq HZ] [--span HZ] [--wait S] [--take-radio] [--oregon-v1 | --decoded]
    data_captures.py --host 192.168.120.50 list
    data_captures.py --host 192.168.120.50 fetch [ID ...]
    data_captures.py --host 192.168.120.50 delete ID
    data_captures.py look FILE.cs16 [--oregon-v1 | --decode]

`keep` asks for the IQ of the next bursts (any, or those near `--freq`). With `--wait` it waits
that long for them, downloads them under `--out` (default `runs/data/<host>/`), holds each
against the list (its size, its description) and looks at it. With `--take-radio` it puts the
scanner in data mode first and gives the radio back after: no call is followed meanwhile.

`look` says what a capture holds: its burst's pulses and gaps and its carrier's offset. With
`--oregon-v1`, whether it is an Oregon Scientific version 1 message, and what it says; with
`--decode`, what any device this tool knows says in it (Oregon Scientific version 1, AcuRite
606TX, a Prologue sensor). That is a reading of the capture on the PC, apart from the scanner's
own decoders and from the same facts: `keep --oregon-v1` and `keep --decoded` hold what the
scanner read of each burst against it.

Prints one JSON object; exits 0 when all is as asked, 1 when not (a capture missing or not what
the list says, `--oregon-v1`, `--decode` or `--decoded` finding no message or another than the
scanner read), 3 when the scanner refused.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

import scanner_token

REPO = Path(__file__).resolve().parent.parent

# Oregon Scientific version 1, the protocol's facts: Manchester at 342 bits a second; a preamble
# of ones; a sync of a long pulse (about 5.7 ms) between two long gaps; 32 bits, each byte's
# least significant bit first; the fourth byte is the sum of the first three, the carry added.
OREGON_V1_BIT_US = 1e6 / 342.0
OREGON_V1_SYNC_US = (5000, 6500)
OREGON_V1_AFTER_SYNC_US = 5200

# The readers take a burst through a filter of 4 kHz either side, its envelope smoothed over
# 120 us: every keying read here has pulses of half a millisecond or longer, and the band's
# clicks, as wide as the channel, lose more to a narrow filter than a burst does.
READ_HALF_HZ = 4000.0
READ_SMOOTH_US = 120

# Devices keyed by pulse distance: pulses of half a millisecond, the gap after each a bit (about
# 2 ms a zero, 4 ms a one), about 9 ms between the repeats of the message. Each device's facts:
# how many bits, how many times a burst must hold the same message, what guards it, and where its
# fields are (first bit, bits). Temperatures are signed, in tenths of a degree C.
#   AcuRite 606TX: 32 bits, six times. The fourth byte is a digest of the first three: each bit
#   that is set adds in a key that is shifted right at every bit, 0x98 put in when a one falls out
#   (the key begins as 0xF1).
#   A Prologue sensor: 36 bits and a spare one, six or seven times; no check of its own but its
#   first four bits, 1001.
# What is shorter than 160 us in their envelope is a glitch, not an edge.
DISTANCE_ZERO_US = (1400, 2800)
DISTANCE_ONE_US = (3200, 5000)
DISTANCE_REPEAT_US = 7000
DISTANCE_PULSE_US = (250, 900)
DISTANCE_GLITCH_US = 160
DISTANCE_DEVICES = [
    {"model": "AcuRite 606TX", "bits": 32, "spare": 0, "needs": 2, "digest": (0x98, 0xF1), "leads": None,
     "id": (0, 8), "channel": None, "battery_good": 8, "temperature": (12, 12), "humidity": None},
    {"model": "Prologue TH", "bits": 36, "spare": 1, "needs": 3, "digest": None, "leads": "1001",
     "id": (4, 8), "channel": (14, 2), "battery_good": 12, "temperature": (16, 12), "humidity": (28, 8)},
]


def call(base, path, method="GET", body=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"} if data else {})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def refused(e):
    try:
        return json.load(e).get("error") or str(e)
    except (ValueError, AttributeError):
        return str(e)


def summary(view):
    return {
        "state": view["state"],
        "error": view["error"],
        "window": view["window"],
        "floor_dbfs": view["floor_dbfs"],
        "counts": view["counts"],
        "arm": view["arm"],
        "emitters": [{k: e[k] for k in ("freq_hz", "bursts", "every_ms", "length_ms", "over_db", "level_dbfs", "device")} for e in view["emitters"]],
        "devices": [{k: d[k] for k in ("key", "model", "id", "channel", "freq_hz", "messages", "every_ms", "latest")} for d in view["devices"]],
    }


def envelope(path, freq_hz=None, half_hz=15_000.0):
    """The burst of a capture: its envelope at the capture's rate, with what the description says."""
    import numpy as np

    meta_path = path.with_suffix(".sigmf-meta")
    meta = json.loads(meta_path.read_text(encoding="utf-8")) if meta_path.is_file() else None
    if meta:
        rate = float(meta["global"]["core:sample_rate"])
        centre = float(meta["captures"][0]["core:frequency"])
        note = meta["annotations"][0]
        burst_hz = (note["core:freq_lower_edge"] + note["core:freq_upper_edge"]) / 2
    else:
        tokens = path.stem.split("_")
        number = lambda unit: next((float(t[:-1]) for t in tokens if t.endswith(unit) and t[:-1].replace(".", "", 1).isdigit()), None)
        if number("M") is None or number("k") is None:
            raise SystemExit(f"{path.name}: no description beside it, and its name gives no centre and rate")
        rate, centre = number("k") * 1e3, number("M") * 1e6
        burst_hz = centre
    freq_hz = burst_hz if freq_hz is None else freq_hz
    raw = np.fromfile(path, dtype="<i2").reshape(-1, 2).astype(np.float32)
    x = raw[:, 0] + 1j * raw[:, 1]
    x = x * np.exp(-2j * np.pi * (freq_hz - centre) / rate * np.arange(x.size)).astype(np.complex64)
    taps = 121
    k = np.arange(taps) - (taps - 1) / 2
    h = np.sinc(2 * half_hz / rate * k) * np.hamming(taps)
    y = np.convolve(x, (h / h.sum()).astype(np.float32), mode="same")
    env = np.convolve(np.abs(y), np.ones(12) / 12, mode="same")
    return {"rate": rate, "centre_hz": centre, "freq_hz": freq_hz, "samples": int(x.size), "meta": meta, "y": y, "env": env}


def runs(on):
    """(state, start, length) of each run of `on`."""
    import numpy as np

    edges = np.flatnonzero(np.diff(on.astype(np.int8))) + 1
    starts = np.concatenate(([0], edges))
    ends = np.concatenate((edges, [on.size]))
    return [(bool(on[s]), int(s), int(e - s)) for s, e in zip(starts, ends)]


def look(path, freq_hz=None, oregon_v1=False, decode=False):
    """What a capture holds."""
    import numpy as np

    c = envelope(path, freq_hz)
    env, us = c["env"], 1e6 / c["rate"]
    noise = float(np.median(env))
    peak = float(np.percentile(env, 99.8))
    on = env > noise + 0.35 * (peak - noise)
    rr = [(s, st, ln) for s, st, ln in runs(on) if ln * us >= 50]
    pulses = [ln * us for s, _, ln in rr if s]
    gaps = [ln * us for s, _, ln in rr[1:-1] if not s]

    def spread(values):
        hist = {}
        for v in values:
            key = int(v // 100 * 100) if v < 3000 else int(round(v / 1000) * 1000)
            hist[key] = hist.get(key, 0) + 1
        return {str(k): hist[k] for k in sorted(hist)}

    step = np.angle(c["y"][1:] * np.conj(c["y"][:-1])) * c["rate"] / (2 * np.pi)
    while_on = step[on[1:] & on[:-1]]
    out = {
        "file": path.name,
        "sample_rate_hz": c["rate"],
        "centre_hz": c["centre_hz"],
        "looked_at_hz": round(c["freq_hz"]),
        "seconds": round(c["samples"] / c["rate"], 4),
        "over_noise_db": round(float(20 * np.log10(peak / max(noise, 1e-9))), 1),
        "pulses": len(pulses),
        "pulse_us": spread(pulses),
        "gap_us": spread(gaps),
        "carrier_offset_hz": round(float(np.median(while_on))) if while_on.size else None,
    }
    if c["meta"]:
        out["burst"] = c["meta"]["annotations"][0].get("fishball:burst")
        out["taken_utc"] = c["meta"]["captures"][0]["core:datetime"]
    if oregon_v1 or decode:
        keyed, keyed_us = keying(path, freq_hz)
        out["oregon_v1"] = oregon_v1_messages(keyed, keyed_us)
    if decode:
        out["decoded"] = oregon_v1_reading(out["oregon_v1"]) or distance_reading(keyed, keyed_us)
    return out


def keying(path, freq_hz=None):
    """A capture's burst as the readers take it: on or off at each sample, and a sample's
    microseconds."""
    import numpy as np

    c = envelope(path, freq_hz, READ_HALF_HZ)
    us = 1e6 / c["rate"]
    width = max(1, round(READ_SMOOTH_US / us))
    env = np.convolve(np.abs(c["y"]), np.ones(width) / width, mode="same")
    noise = float(np.median(env))
    return env > noise + 0.35 * (float(np.percentile(env, 99.8)) - noise), us


def oregon_v1_reading(messages):
    """The look's Oregon Scientific messages as the scanner gives a reading."""
    if not messages:
        return None
    first = messages[0]
    return {"model": "Oregon Scientific v1", "kind": "weather", "id": first["id"], "channel": first["channel"],
            "temperature_c": first["temperature_c"], "humidity_pct": None, "battery_low": first["battery_low"],
            "repeats": sum(m["bytes"] == first["bytes"] for m in messages)}


def digest8(data, generator, key):
    """Each set bit of `data` adds in the key, which is shifted right at every bit, the generator
    put in when a one falls out."""
    total = 0
    for byte in data:
        for bit in range(7, -1, -1):
            if byte >> bit & 1:
                total ^= key
            key = key >> 1 ^ generator if key & 1 else key >> 1
    return total


def distance_words(on, us):
    """The words of a burst keyed by pulse distance: the gaps after its pulses, as bits, between
    the longer gaps."""
    # The runs in microseconds, a glitch given to the run before it.
    clean = []
    for state, _, length in runs(on):
        if clean and (length * us < DISTANCE_GLITCH_US or clean[-1][0] == state):
            clean[-1][1] += length * us
        else:
            clean.append([state, length * us])
    words, word = [], ""
    after_pulse = False
    for state, length in clean:
        if state:
            # A pulse of another length is not this keying: what was read of the word is dropped.
            after_pulse = DISTANCE_PULSE_US[0] <= length <= DISTANCE_PULSE_US[1]
            if not after_pulse:
                word = ""
        elif not after_pulse:
            word = ""
        elif DISTANCE_ZERO_US[0] <= length <= DISTANCE_ZERO_US[1]:
            word += "0"
        elif DISTANCE_ONE_US[0] <= length <= DISTANCE_ONE_US[1]:
            word += "1"
        else:
            if length >= DISTANCE_REPEAT_US and word:
                words.append(word)
            word = ""
    return words


def distance_reading(on, us):
    """What a device keyed by pulse distance says in a burst, as the scanner gives a reading."""
    words = distance_words(on, us)
    for device in DISTANCE_DEVICES:
        seen = {}
        for word in words:
            if not device["bits"] <= len(word) <= device["bits"] + device["spare"]:
                continue
            word = word[:device["bits"]]
            if device["leads"] and not word.startswith(device["leads"]):
                continue
            if device["digest"]:
                data = [int(word[i:i + 8], 2) for i in range(0, len(word), 8)]
                if digest8(data[:-1], *device["digest"]) != data[-1]:
                    continue
            seen[word] = seen.get(word, 0) + 1
        if not seen:
            continue
        word, repeats = max(seen.items(), key=lambda kv: kv[1])
        if repeats < device["needs"]:
            continue

        def field(at, word=word):
            return int(word[at[0]:at[0] + at[1]], 2)

        tenths = field(device["temperature"])
        tenths -= 1 << device["temperature"][1] if tenths >> device["temperature"][1] - 1 else 0
        humidity = field(device["humidity"]) if device["humidity"] else None
        return {"model": device["model"], "kind": "weather", "id": field(device["id"]),
                "channel": field(device["channel"]) + 1 if device["channel"] else None,
                "temperature_c": tenths / 10, "temperature_f": round(tenths / 10 * 9 / 5 + 32, 1),
                "humidity_pct": humidity if humidity is not None and humidity <= 100 else None,
                "battery_low": word[device["battery_good"]] == "0", "repeats": repeats, "bits": word}
    return None


def oregon_v1_messages(on, us):
    """The Oregon Scientific version 1 messages in an envelope's on and off: each with a good
    checksum."""
    found = []
    for state, start, length in runs(on):
        if not state or not OREGON_V1_SYNC_US[0] <= length * us <= OREGON_V1_SYNC_US[1]:
            continue
        sync_end = (start + length) * us
        best = None
        for delta in range(-800, 1600, 50):
            for flip in (False, True):
                bits, margin = [], 0.0
                for k in range(32):
                    cell = sync_end + OREGON_V1_AFTER_SYNC_US + delta + k * OREGON_V1_BIT_US

                    def level(f0, f1, cell=cell):
                        i0, i1 = int((cell + f0 * OREGON_V1_BIT_US) / us), int((cell + f1 * OREGON_V1_BIT_US) / us)
                        return float(on[i0:i1].mean()) if 0 <= i0 < i1 <= on.size else 0.5

                    first, second = level(0.12, 0.38), level(0.62, 0.88)
                    if (first > 0.5) == (second > 0.5):
                        bits = None
                        break
                    margin += abs(first - second)
                    bits.append(int((first > 0.5) != flip))
                if bits is None:
                    continue
                b = [sum(bits[8 * i + j] << j for j in range(8)) for i in range(4)]
                total = b[0] + b[1] + b[2]
                if ((total & 0xFF) + (total >> 8)) & 0xFF == b[3] and (best is None or margin > best[0]):
                    best = (margin, b)
        if best:
            b = best[1]
            temp = (b[2] & 0xF) * 10 + (b[1] >> 4) + (b[1] & 0xF) / 10
            temp = -temp if b[2] & 0x20 else temp
            found.append({
                "at_ms": round(start * us / 1e3, 1),
                "bytes": " ".join(f"{x:02x}" for x in b),
                "id": b[0] & 0xF,
                "channel": (b[0] >> 6) + 1,
                "temperature_c": temp,
                "temperature_f": round(temp * 9 / 5 + 32, 1),
                "battery_low": bool(b[2] & 0x80),
            })
    return found


def fetch(base, kept, out_dir):
    """Download a capture's two files and hold them against the list."""
    out_dir.mkdir(parents=True, exist_ok=True)
    got = {"id": kept["id"], "ok": True}
    for name in (kept["file"], kept["meta"]):
        with urllib.request.urlopen(f"{base}/api/v1/data/captures/{urllib.parse.quote(name)}", timeout=120) as r:
            (out_dir / name).write_bytes(r.read())
    data = out_dir / kept["file"]
    meta = json.loads((out_dir / kept["meta"]).read_text(encoding="utf-8"))
    ours = meta["annotations"][0]["fishball:burst"]
    problems = []
    if data.stat().st_size != kept["bytes"] or data.stat().st_size != 4 * kept["samples"]:
        problems.append(f"{data.name} is {data.stat().st_size} bytes, the list says {kept['bytes']} ({kept['samples']} samples)")
    if meta["global"]["core:dataset"] != kept["file"] or meta["global"]["core:datatype"] != "ci16_le" or meta["global"]["core:sample_rate"] != kept["sample_rate_hz"]:
        problems.append("the description does not describe the samples")
    if ours["id"] != kept["id"] or ours["freq_hz"] != kept["freq_hz"]:
        problems.append("the description's burst is not the list's")
    got.update(file=str(data), bytes=data.stat().st_size, problems=problems, ok=not problems, reading=ours.get("reading"))
    return got


def keep(base, args, out_dir):
    """Ask for the next bursts' IQ and, with --wait, fetch and look at them. The result and the exit
    code; the radio is given back whatever happened."""
    result = {}
    took = False
    try:
        if args.take_radio:
            was = call(base, "/api/v1/mode")["mode"]
            if was != "data":
                call(base, "/api/v1/mode", "PUT", {"mode": "data"}, timeout=120)
                took = True
            state = None
            for _ in range(40):
                state = call(base, "/api/v1/data")
                if state["state"] in ("listening", "error"):
                    break
                time.sleep(0.5)
            if state["state"] != "listening":
                result["error"] = f"data mode does not listen: {state['error'] or state['state']}"
                return result, 1
        body = {"count": args.count}
        if args.freq:
            body["freq_hz"] = args.freq
        if args.span:
            body["span_hz"] = args.span
        before = {k["id"] for k in call(base, "/api/v1/data/captures")["items"]}
        view = call(base, "/api/v1/data/arm", "PUT", body)
        result["armed"] = view["arm"]
        result["window"] = view["window"]
        if not args.wait:
            return result, 0
        deadline = time.time() + args.wait
        new = []
        while time.time() < deadline:
            time.sleep(1)
            view = call(base, "/api/v1/data")
            new = [k for k in call(base, "/api/v1/data/captures")["items"] if k["id"] not in before]
            if len(new) >= args.count and view["arm"] is None:
                break
        result["listening"] = summary(view)
        result["kept"] = len(new)
        result["captures"] = []
        ok = len(new) >= args.count
        for kept in reversed(new):
            got = fetch(base, kept, out_dir)
            if got["ok"]:
                got["look"] = look(Path(got["file"]), oregon_v1=args.oregon_v1, decode=args.decoded)
                if args.decoded:
                    # What the scanner read of the burst is what the look reads of its capture. How
                    # many times a weak burst held its message is each reader's own count.
                    seen, read = got["look"]["decoded"], got["reading"]
                    same = ("model", "id", "channel", "humidity_pct", "battery_low")
                    if not seen:
                        got["ok"] = False
                        got["problems"].append("the look reads no device's message in it")
                    elif not read:
                        got["ok"] = False
                        got["problems"].append("the scanner did not read the burst; the look does")
                    elif any(read.get(k) != seen[k] for k in same) or round(read["temperature_c"], 1) != seen["temperature_c"]:
                        got["ok"] = False
                        got["problems"].append(f"the scanner read {read}, the look {seen}")
                elif args.oregon_v1 and not got["look"]["oregon_v1"]:
                    got["ok"] = False
                    got["problems"].append("no Oregon Scientific version 1 message in it")
                elif args.oregon_v1:
                    # What the scanner read of the burst is what the look reads of its capture.
                    seen, read = got["look"]["oregon_v1"][0], got["reading"]
                    if not read:
                        got["ok"] = False
                        got["problems"].append("the scanner did not read the burst; the look does")
                    elif (read["id"], read["channel"], round(read["temperature_c"], 1), read["battery_low"]) != (seen["id"], seen["channel"], seen["temperature_c"], seen["battery_low"]) \
                            or read["repeats"] != len(got["look"]["oregon_v1"]):
                        got["ok"] = False
                        got["problems"].append(f"the scanner read {read}, the look {got['look']['oregon_v1']}")
            ok = ok and got["ok"]
            result["captures"].append(got)
        if not ok and len(new) < args.count:
            result["error"] = f"{len(new)} of {args.count} bursts were kept in {args.wait} s"
        return result, 0 if ok else 1
    except urllib.error.HTTPError as e:
        result["error"] = refused(e)
        return result, 3
    finally:
        if took:
            try:
                call(base, "/api/v1/data/arm", "DELETE")
                call(base, "/api/v1/mode", "PUT", {"mode": "scanner"}, timeout=180)
                result["radio"] = "given back to the scanner"
            except (urllib.error.URLError, OSError) as e:
                result["radio"] = f"NOT given back to the scanner: {e}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", help="the scanner: a host, host:port or URL (port 8080; with SCANNER_TOKEN set, HTTPS on 8443)")
    ap.add_argument("--out", type=Path, help="where to save (default runs/data/<host>/)")
    sub = ap.add_subparsers(dest="what", required=True)
    sub.add_parser("status", help="what data mode hears")
    k = sub.add_parser("keep", help="keep the IQ of the next bursts")
    k.add_argument("--count", type=int, default=1)
    k.add_argument("--freq", type=int, help="only bursts near this frequency, Hz")
    k.add_argument("--span", type=int, help="how wide 'near' is, edge to edge, Hz (20,000 when not given)")
    k.add_argument("--wait", type=float, help="wait this many seconds for them, then download and look at each")
    k.add_argument("--take-radio", action="store_true", help="put the scanner in data mode first, and give the radio back after")
    k.add_argument("--oregon-v1", action="store_true", help="each capture must hold an Oregon Scientific version 1 message")
    k.add_argument("--decoded", action="store_true", help="each capture must hold a message this tool reads, the one the scanner read")
    sub.add_parser("list", help="the captures on the card")
    f = sub.add_parser("fetch", help="download captures (every one, or those named)")
    f.add_argument("ids", nargs="*")
    d = sub.add_parser("delete", help="delete a capture")
    d.add_argument("id")
    lk = sub.add_parser("look", help="what a capture's file holds (no scanner needed)")
    lk.add_argument("file", type=Path)
    lk.add_argument("--freq", type=float, help="look at this frequency, Hz (default: the burst's, from the description)")
    lk.add_argument("--oregon-v1", action="store_true", help="it must hold an Oregon Scientific version 1 message")
    lk.add_argument("--decode", action="store_true", help="it must hold a message of a device this tool reads")
    args = ap.parse_args()

    if args.what == "look":
        result = look(args.file, args.freq, args.oregon_v1, args.decode)
        print(json.dumps(result, indent=1))
        return 1 if args.oregon_v1 and not result["oregon_v1"] or args.decode and not result["decoded"] else 0
    if not args.host:
        ap.error("--host is needed")
    scanner_token.install(args.host)
    base = scanner_token.base(args.host)
    out_dir = args.out or REPO / "runs" / "data" / urllib.parse.urlparse(base).hostname
    result = {"host": base, "what": args.what}
    code = 0
    try:
        if args.what == "status":
            result.update(summary(call(base, "/api/v1/data")))
        elif args.what == "list":
            result.update(call(base, "/api/v1/data/captures"))
        elif args.what == "delete":
            result.update(call(base, f"/api/v1/data/captures/{urllib.parse.quote(args.id)}", "DELETE"))
        elif args.what == "fetch":
            items = call(base, "/api/v1/data/captures")["items"]
            wanted = [k for k in items if not args.ids or k["id"] in args.ids]
            missing = [i for i in args.ids if i not in {k["id"] for k in items}]
            result["captures"] = [fetch(base, k, out_dir) for k in wanted]
            result["missing"] = missing
            code = 0 if not missing and all(c["ok"] for c in result["captures"]) else 1
        else:
            more, code = keep(base, args, out_dir)
            result.update(more)
    except urllib.error.HTTPError as e:
        result["error"] = refused(e)
        code = 3
    print(json.dumps(result, indent=1))
    return code


if __name__ == "__main__":
    sys.exit(main())
