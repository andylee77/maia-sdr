#!/usr/bin/env python3
"""The scanner's API, field by field: every GET route's response fields with their types and
meanings (tools/api_fields_meanings.py), each with an example from a unit's own response, as
markdown.

The unit is asked for every GET route it lists in /api/v1/routes, with path parameters taken
from its own data (a call with voice, the live site, the busiest radio and talkgroup), so run it
on a unit with a live site that has heard calls. A field in a response the meanings do not
describe fails the run, so the document cannot fall behind the API.

  python tools/api_fields.py --host 10.25.0.2 --out scanner/doc/API_FIELDS.md

Exit codes: 0 written, 2 the unit did not answer, 3 a field has no meaning (listed on stderr).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from api_fields_meanings import NOT_JSON, ROUTES, STRUCTS  # noqa: E402

ID_KEY = re.compile(r"^-?\d+$")


def get(host: str, path: str):
    with urllib.request.urlopen(f"http://{host}:8080{path}", timeout=30) as r:
        return json.load(r)


def maybe(host: str, path: str):
    """The answer, or None when the unit refuses."""
    try:
        return get(host, path)
    except OSError:
        return None


def join(prefix: str, path: str) -> str:
    if not prefix or not path:
        return prefix or path
    return prefix + path if path[0] in "[{" else f"{prefix}.{path}"


# What one element is, for a structure included as an array's elements.
NOUNS = {"call": "call", "site": "site", "system": "system", "alias": "alias", "recording": "recording",
         "lsm_control": "lane's LSM settings", "carrier": "carrier"}


def expand(entries: list, prefix: str = "") -> list[tuple[str, str, str]]:
    """A route's or structure's fields with the structures it includes, in order."""
    out = []
    for e in entries:
        if e[0].startswith("@"):
            at = join(prefix, e[0][1:])
            if at.endswith("[]") and not any(p == at for p, _, _ in out):
                out.append((at, "object", f"One {NOUNS.get(e[1], 'element')}."))
            out += expand(STRUCTS[e[1]], at)
        else:
            out.append((join(prefix, e[0]), e[1], e[2]))
    return out


def kind(v) -> str:
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "bool"
    if isinstance(v, (int, float)):
        return "number"
    return {str: "string", list: "array"}.get(type(v), "object")


def example(v) -> str:
    s = json.dumps(v, ensure_ascii=False)
    return s if len(s) <= 40 else s[:37] + "..."


def walk(v, path: str, seen: dict[str, str | None], maps: set[str]) -> None:
    """Every field path in `v` under `path`, with an example (the first value not null)."""
    k = kind(v)
    if path:
        if seen.get(path) is None:
            seen[path] = example(v) if k not in ("object", "null") and not (k == "array" and v and kind(v[0]) == "object") else None
    if k == "object":
        if path in maps or (v and all(ID_KEY.match(x) for x in v)):
            if v and seen.get(path) is None:
                key, val = next(iter(v.items()))
                seen[path] = example({key: val})
            for x in list(v.values())[:20]:
                walk(x, f"{path}{{key}}", seen, maps)
            return
        for key, x in v.items():
            walk(x, join(path, key), seen, maps)
    elif k == "array" and not all(kind(x) in ("number", "null") for x in v):
        for x in v[:50]:
            walk(x, f"{path}[]", seen, maps)


def fill(path: str, ctx: dict) -> str | None:
    """The route with its parameters from the unit's data; None if one is unknown."""
    missing = False

    def sub(m):
        nonlocal missing
        value = ctx.get(m.group(1).lstrip("*"))
        missing |= value is None
        return str(value)
    out = re.sub(r"\{([^}]+)\}", sub, path)
    return None if missing else out


def cell(s: str) -> str:
    return s.replace("|", "\\|")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", required=True)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()
    try:
        routes = get(args.host, "/api/v1/routes")
        status = get(args.host, "/api/v1/status")
    except OSError as e:
        print(f"{args.host}: {e}", file=sys.stderr)
        return 2
    live = status["live"]
    site = live["site"]["id"] if live.get("state") == "live" else None
    calls = get(args.host, "/api/v1/calls")
    voiced = next((c for c in calls["recent"] if c.get("voice_frames")), None)
    # With no live site the activity routes refuse; their paths then go unsampled.
    radios = (maybe(args.host, "/api/v1/activity/radios?limit=1") or {}).get("items", [])
    tgs = (maybe(args.host, "/api/v1/activity/talkgroups?limit=1") or {}).get("items", [])
    ctx = {"site": site, "unit": radios[0]["unit"] if radios else None, "tg": tgs[0]["tg"] if tgs else None}

    out = ["# API fields", "",
           "Every GET route's response, field by field, with an example from a unit's own answer.",
           "Generated by `tools/api_fields.py` from the meanings in `tools/api_fields_meanings.py`;",
           "`API.md` lists every route (writes too) and `API_INVENTORY.md` who uses each and what is",
           "missing.", "",
           "- **Paths:** `a.b` is a field of an object, `a[]` an array's elements, `a{key}` the values",
           "  of a map keyed by an id (a talkgroup, an LCN, a site).",
           "- **Types:** \"absent when ...\" means the key is left out; \"or null\" means it is sent as",
           "  null. `number` covers integers and decimals.",
           "- **Times:** fields ending `_unix_ms` and the `first_ms`/`last_ms`/`at_ms` stamps are",
           "  milliseconds since 1970 (UTC); other `_ms` fields are durations.",
           "- **Examples** come from the unit the document was made on; an empty one was not in its",
           "  answer.", ""]
    unknown: list[str] = []
    for r in routes:
        method, path, summary = r["method"], r["path"], r["summary"]
        if method != "get":
            continue
        out += [f"## `GET {path}`", "", summary[0].upper() + summary[1:] + ".", ""]
        if path in NOT_JSON:
            out += [NOT_JSON[path], ""]
            continue
        if path not in ROUTES:
            unknown.append(f"{path}: the route has no meanings")
            continue
        fields = expand(ROUTES[path])
        maps = {p for p, t, _ in fields if t.startswith("map")}
        ctx_id = (voiced["call"] if path.startswith("/api/v1/calls/") and voiced else
                  live["system"]["id"] if path.startswith("/api/v1/systems/") and site else
                  site if path.startswith("/api/v1/sites/") else None)
        url = fill(path, dict(ctx, id=ctx_id))
        seen: dict[str, str | None] = {}
        if url is None:
            out += ["_Not sampled: the unit had nothing to fill the path with._", ""]
        else:
            try:
                walk(get(args.host, url), "", seen, maps)
            except Exception as e:  # noqa: BLE001
                out += [f"_Not sampled: {e}._", ""]
        declared = {p for p, _, _ in fields}
        unknown += [f"{path}: {p}" for p in seen if p not in declared]
        out += ["| Field | Type | Example | Meaning |", "|-------|------|---------|---------|"]
        for p, t, meaning in fields:
            ex = seen.get(p)
            out.append(f"| `{p}` | {cell(t)} | {f'`{cell(ex)}`' if ex else ''} | {cell(meaning)} |")
        out.append("")
    args.out.write_text("\n".join(out).rstrip("\n") + "\n", encoding="utf-8", newline="\n")
    if unknown:
        print(f"{len(unknown)} fields have no meaning:", file=sys.stderr)
        for u in unknown:
            print("  " + u, file=sys.stderr)
        return 3
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
