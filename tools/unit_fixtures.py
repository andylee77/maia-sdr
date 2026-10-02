#!/usr/bin/env python3
"""Committed replay fixtures from a unit's trunking trace, with radio IDs replaced.

Radio IDs are mapped to stand-ins of the same digit count (consoles keep 4 digits, field radios
7), in sorted order, so every event stays consistent. Talkgroups, sites and frequencies are kept.

Usage:
  python tools/unit_fixtures.py trace <trace.jsonl> <out.jsonl>

Exit codes: 0 written, 2 bad input.
"""

import json
import sys
from pathlib import Path


def stand_ins(ids):
    """Sorted IDs to stand-ins with the same number of digits."""
    out, used = {}, {}
    for uid in sorted(i for i in ids if i):
        digits = len(str(uid))
        n = used.get(digits, 0) + 1
        used[digits] = n
        out[uid] = 10 ** (digits - 1) + n if digits > 1 else uid
    return out


def trace(src, dst):
    lines = [json.loads(l) for l in src.read_text().splitlines() if l.strip()]
    ids = set()
    for e in lines:
        if isinstance(e.get("src"), int):
            ids.add(e["src"])
        ids |= {s for s in e.get("sources") or [] if isinstance(s, int)}
    m = stand_ins(ids)
    with open(dst, "w", newline="\n") as out:
        for e in lines:
            if isinstance(e.get("src"), int):
                e["src"] = m.get(e["src"], e["src"])
            if e.get("sources"):
                e["sources"] = [m.get(s, s) for s in e["sources"]]
            out.write(json.dumps(e, separators=(",", ":")) + "\n")
    return {"out": str(dst), "events": len(lines), "radios": len(m)}


def main(argv):
    if len(argv) != 4 or argv[1] != "trace":
        print(__doc__)
        return 2
    src, dst = Path(argv[2]), Path(argv[3])
    if not src.exists():
        print(json.dumps({"error": f"{src} not found"}))
        return 2
    print(json.dumps(trace(src, dst)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
