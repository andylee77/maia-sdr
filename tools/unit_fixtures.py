#!/usr/bin/env python3
"""Committed test fixtures from a unit's state, with radio IDs replaced.

A unit's state is copied read-only into runs/076/units/<unit>/ (jffs2/ files, sd/history.sqlite,
sd/recordings.txt). This tool writes the committed copy the 076 migration and Activity tests
run on. Radio IDs are mapped to stand-ins of the same digit count (consoles keep 4 digits,
field radios 7), in sorted order, so every table and file name stays consistent. Talkgroups,
sites and frequencies are kept.

Usage:
  python tools/unit_fixtures.py unit runs/076/units/A scanner/tests/fixtures/unit_a
  python tools/unit_fixtures.py trace <trace.jsonl> <out.jsonl>

Exit codes: 0 written, 2 bad input.
"""

import json
import re
import shutil
import sqlite3
import sys
from pathlib import Path

UNIT_COLUMNS = {"calls": ["source"], "call_units": ["unit"], "hour_unit": ["unit"],
                "unit_events": ["unit"]}
REC_RE = re.compile(r"(rec_\d+_\d+_tg\d+_from)(\d+)(\..*\.wav)$")


def stand_ins(ids):
    """Sorted IDs to stand-ins with the same number of digits."""
    out, used = {}, {}
    for uid in sorted(i for i in ids if i):
        digits = len(str(uid))
        n = used.get(digits, 0) + 1
        used[digits] = n
        out[uid] = 10 ** (digits - 1) + n if digits > 1 else uid
    return out


def history_units(db):
    ids = set()
    for table, cols in UNIT_COLUMNS.items():
        for col in cols:
            ids |= {r[0] for r in db.execute(f"select distinct {col} from {table}") if r[0]}
    return ids


def recording_names(listing):
    for line in listing.read_text().splitlines():
        parts = line.split()
        if parts and parts[-1].endswith(".wav"):
            yield int(parts[4]), parts[-1]


def unit(src, dst):
    db = sqlite3.connect(":memory:")
    sqlite3.connect(src / "sd" / "history.sqlite").backup(db)
    recs = list(recording_names(src / "sd" / "recordings.txt"))
    ids = history_units(db) | {int(r.group(2)) for _, n in recs if (r := REC_RE.match(n))}
    m = stand_ins(ids)
    for table, cols in UNIT_COLUMNS.items():
        for col in cols:
            for old, new in m.items():
                db.execute(f"update {table} set {col} = ? where {col} = ?", (-new, old))
            db.execute(f"update {table} set {col} = -{col} where {col} < 0")
    db.commit()

    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src / "jffs2", dst / "jffs2")
    settings = dst / "jffs2" / "p25-ui-settings.json"
    s = json.loads(settings.read_text())
    for scope in [s, *s.get("sites", {}).values()]:
        aliases = scope.get("unit_aliases") or {}
        scope["unit_aliases"] = {str(m.get(int(k), int(k))): v for k, v in aliases.items()}
    settings.write_text(json.dumps(s, indent=2) + "\n")
    (dst / "history.sql").write_text("\n".join(db.iterdump()) + "\n")
    names = []
    for size, name in recs:
        r = REC_RE.match(name)
        names.append({"name": f"{r.group(1)}{m.get(int(r.group(2)), int(r.group(2)))}{r.group(3)}"
                      if r else name, "size": size})
    (dst / "recordings.json").write_text(json.dumps(names, indent=1) + "\n")
    return {"out": str(dst), "radios": len(m), "calls": db.execute("select count(*) from calls").fetchone()[0],
            "recordings": len(names)}


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
    if len(argv) != 4 or argv[1] not in ("unit", "trace"):
        print(__doc__)
        return 2
    src, dst = Path(argv[2]), Path(argv[3])
    if not src.exists():
        print(json.dumps({"error": f"{src} not found"}))
        return 2
    result = unit(src, dst) if argv[1] == "unit" else trace(src, dst)
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
