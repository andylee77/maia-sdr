#!/usr/bin/env python3
"""Data mode's listening and its captures, through a scanner's API (`/api/v1/data`).

    data_captures.py --host 192.168.120.50 status
    data_captures.py --host 192.168.120.50 keep --count 2 [--freq HZ] [--span HZ] [--wait S] [--take-radio] [--oregon-v1]
    data_captures.py --host 192.168.120.50 list
    data_captures.py --host 192.168.120.50 fetch [ID ...]
    data_captures.py --host 192.168.120.50 delete ID
    data_captures.py look FILE.cs16 [--oregon-v1]

`keep` asks for the IQ of the next bursts (any, or those near `--freq`). With `--wait` it waits
that long for them, downloads them under `--out` (default `runs/data/<host>/`), holds each
against the list (its size, its description) and looks at it. With `--take-radio` it puts the
scanner in data mode first and gives the radio back after: no call is followed meanwhile.

`look` says what a capture holds: its burst's pulses and gaps and its carrier's offset; with
`--oregon-v1`, whether it is an Oregon Scientific version 1 message, and what it says. That is a
check of the capture on the PC, not the scanner's decoder (which is not built).

Prints one JSON object; exits 0 when all is as asked, 1 when not (a capture missing or not what
the list says, `--oregon-v1` finding no message), 3 when the scanner refused.
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
        "emitters": [{k: e[k] for k in ("freq_hz", "bursts", "every_ms", "length_ms", "over_db", "level_dbfs")} for e in view["emitters"]],
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


def look(path, freq_hz=None, oregon_v1=False):
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
    if oregon_v1:
        out["oregon_v1"] = oregon_v1_messages(on, us)
    return out


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
    got.update(file=str(data), bytes=data.stat().st_size, problems=problems, ok=not problems)
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
                got["look"] = look(Path(got["file"]), oregon_v1=args.oregon_v1)
                if args.oregon_v1 and not got["look"]["oregon_v1"]:
                    got["ok"] = False
                    got["problems"].append("no Oregon Scientific version 1 message in it")
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
    sub.add_parser("list", help="the captures on the card")
    f = sub.add_parser("fetch", help="download captures (every one, or those named)")
    f.add_argument("ids", nargs="*")
    d = sub.add_parser("delete", help="delete a capture")
    d.add_argument("id")
    lk = sub.add_parser("look", help="what a capture's file holds (no scanner needed)")
    lk.add_argument("file", type=Path)
    lk.add_argument("--freq", type=float, help="look at this frequency, Hz (default: the burst's, from the description)")
    lk.add_argument("--oregon-v1", action="store_true", help="it must hold an Oregon Scientific version 1 message")
    args = ap.parse_args()

    if args.what == "look":
        result = look(args.file, args.freq, args.oregon_v1)
        print(json.dumps(result, indent=1))
        return 1 if args.oregon_v1 and not result["oregon_v1"] else 0
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
