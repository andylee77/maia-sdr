#!/usr/bin/env python3
"""The JSON shapes of the routes the tools and the bench read.

`capture` reads each route from a unit (plain GET, no parameters, so nothing is written) and
merges its shape into scanner/tests/fixtures/routes/shapes.json; capturing on a P25 site and on a
DMR site fills in fields that are null on one of them. `check` compares a unit's answers with
the stored shapes and lists every key that went missing or changed type: the 076 route contract.

A shape keeps keys and value types, not values. Objects whose keys are data (frequencies,
message classes) are stored as maps of one value shape.

Usage:
  python tools/route_shapes.py capture [--host 192.168.120.50]
  python tools/route_shapes.py check   [--host ...]

Exit codes: 0 done (check: no differences), 1 check found differences, 2 a route failed.
"""

import argparse
import json
import sys
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
STORE = REPO / "scanner" / "tests" / "fixtures" / "routes" / "shapes.json"

# GET routes read by tools/ and bench/ that exist today.
ROUTES = [
    "/api/activity/sites", "/api/aliases", "/api/bands", "/api/bch_t", "/api/decoder_compare",
    "/api/dibit_delivery", "/api/dmr", "/api/forensics_status", "/api/grant_decode_stats",
    "/api/grant_map", "/api/grants", "/api/hdl_lsm", "/api/irq_stats", "/api/log",
    "/api/modulation", "/api/monitor", "/api/ppm", "/api/ps_cores", "/api/recent_tsbks",
    "/api/recordings", "/api/spectrum_wide", "/api/stats", "/api/sys_health", "/api/system",
    "/api/traffic", "/api/traffic_bins", "/api/tsbk_opcodes", "/api/ui/calls",
    "/api/ui/settings", "/api/ui/state", "/api/control_lsm_control",
]


def scalar(v):
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, int):
        return "integer"
    if isinstance(v, float):
        return "number"
    return "string"


def is_map(d):
    if not d:
        return False
    if all(k.lstrip("-").isdigit() for k in d):
        return True
    shapes = [json.dumps(shape(v), sort_keys=True) for v in d.values()]
    return len(d) >= 8 and len(set(shapes)) == 1 and isinstance(next(iter(d.values())), dict)


def shape(v):
    if isinstance(v, dict):
        if is_map(v):
            s = None
            for x in v.values():
                s = merge(s, shape(x))
            return {"map": s}
        return {"object": {k: shape(x) for k, x in v.items()}}
    if isinstance(v, list):
        s = None
        for x in v[:200]:
            s = merge(s, shape(x))
        return {"array": s}
    return {"scalar": [scalar(v)]}


def merge(a, b):
    """The shape that accepts both. `null` merges with any type."""
    if a is None:
        return b
    if b is None:
        return a
    if "scalar" in a and "scalar" in b:
        return {"scalar": sorted(set(a["scalar"]) | set(b["scalar"]))}
    if a == {"scalar": ["null"]}:
        return merge(b, {"scalar": ["null"]}) if "scalar" in b else {**b, "nullable": True}
    if b == {"scalar": ["null"]}:
        return {**a, "nullable": True} if "scalar" not in a else merge(a, b)
    nullable = a.get("nullable") or b.get("nullable")
    out = None
    if "object" in a and "object" in b:
        keys = {**a["object"], **b["object"]}
        out = {"object": {k: merge(a["object"].get(k), b["object"].get(k)) for k in sorted(keys)}}
    elif "map" in a and "map" in b:
        out = {"map": merge(a["map"], b["map"])}
    elif "array" in a and "array" in b:
        out = {"array": merge(a["array"], b["array"])}
    elif ("map" in a and "object" in b) or ("object" in a and "map" in b):
        m = a if "map" in a else b
        o = b if "map" in a else a
        v = m["map"]
        for x in o["object"].values():
            v = merge(v, x)
        out = {"map": v}
    else:
        out = {"any": True}
    if nullable:
        out["nullable"] = True
    return out


def differences(want, got, path=""):
    """Keys of `want` that `got` lacks or types it does not accept."""
    if want is None or got is None or "any" in want:
        return []
    if got == {"scalar": ["null"]}:
        return [] if want.get("nullable") or "null" in want.get("scalar", []) else [f"{path}: null"]
    if "scalar" in want:
        extra = set(got.get("scalar", ["<structure>"])) - set(want["scalar"]) - {"null"}
        if "integer" in want["scalar"] and extra == {"number"}:
            extra = set()
        if "number" in want["scalar"]:
            extra -= {"integer"}
        return [f"{path}: {sorted(extra)} not {want['scalar']}"] if extra else []
    for kind in ("object", "map", "array"):
        if kind in want:
            if kind not in got:
                if kind == "map" and "object" in got:
                    return [d for x in got["object"].values() for d in differences(want["map"], x, path + ".*")]
                return [f"{path}: not an {kind}"]
            if kind == "object":
                out = []
                for k, w in want["object"].items():
                    if k not in got["object"]:
                        out.append(f"{path}.{k}: missing")
                    else:
                        out += differences(w, got["object"][k], f"{path}.{k}")
                return out
            sub = "[]" if kind == "array" else ".*"
            return differences(want[kind], got[kind], path + sub)
    return []


def fetch(base, route):
    with urllib.request.urlopen(base + route, timeout=20) as r:
        return json.load(r)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["capture", "check"])
    ap.add_argument("--host", default="192.168.120.50")
    ap.add_argument("--port", type=int, default=8080)
    args = ap.parse_args()
    base = f"http://{args.host}:{args.port}"
    stored = json.loads(STORE.read_text()) if STORE.exists() else {"routes": {}}
    failed, report = [], {}
    for route in ROUTES:
        try:
            got = shape(fetch(base, route))
        except Exception as e:
            failed.append(f"{route}: {e}")
            continue
        if args.mode == "capture":
            stored["routes"][route] = merge(stored["routes"].get(route), got)
        else:
            diff = differences(stored["routes"].get(route), got, "")
            if diff:
                report[route] = diff
    if args.mode == "capture":
        STORE.parent.mkdir(parents=True, exist_ok=True)
        stored["routes"] = dict(sorted(stored["routes"].items()))
        STORE.write_text(json.dumps(stored, indent=1, sort_keys=True) + "\n")
    print(json.dumps({"mode": args.mode, "routes": len(ROUTES), "failed": failed, "differences": report},
                     indent=1))
    if failed:
        return 2
    return 1 if report else 0


if __name__ == "__main__":
    sys.exit(main())
