#!/usr/bin/env python3
"""Take the files on a scanner's card away, through its API (`/api/v1/files`).

    scanner_files.py --host 192.168.120.50 list
    scanner_files.py --host 192.168.120.50 recordings [--from DAY] [--to DAY] [--site ID] [--tg N] [--unit N] [--locked]
    scanner_files.py --host 192.168.120.50 history
    scanner_files.py --host 192.168.120.50 tv RF

`--from` and `--to` are local days (`YYYY-MM-DD`, `--to` included) or unix milliseconds. A
download is saved under `--out` (default `runs/files/<host>/`) and checked: an archive's files
against their CRCs and against its `recordings.csv`, the history's database with SQLite's own
check, a TV recording by its length and sync bytes. With `--health` the scanner's receivers are
read before, during and after, to see what the download cost them.

Prints one JSON object; exits 0 when the download is whole and checks, 1 when not, 3 when the
scanner refused it (another download is running: one at a time).
"""

import argparse
import datetime
import json
import sqlite3
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CHUNK = 1 << 20


def get_json(base, path, timeout=30):
    with urllib.request.urlopen(base + path, timeout=timeout) as r:
        return json.load(r)


def day_ms(text, end):
    """A local day's midnight (or the next day's, for `end`) as unix ms; a number is taken as it is."""
    if text.isdigit():
        return int(text)
    day = datetime.datetime.strptime(text, "%Y-%m-%d")
    if end:
        day += datetime.timedelta(days=1)
    return int(day.timestamp() * 1000)


def selection(args):
    q = {}
    if args.since:
        q["from"] = day_ms(args.since, False)
    if args.to:
        q["to"] = day_ms(args.to, True)
    for key in ("site", "tg", "unit"):
        if getattr(args, key) is not None:
            q[key] = getattr(args, key)
    if args.locked:
        q["locked"] = "true"
    return urllib.parse.urlencode(q)


class Health:
    """The receivers' counters before and after a download, and their state while it runs."""

    def __init__(self, base):
        self.base = base
        self.samples = []
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.watch, daemon=True)

    def counters(self):
        control = get_json(self.base, "/api/v1/receivers")["control"]
        lsm = (control.get("counters") or {}).get("lsm") or {}
        readback = get_json(self.base, "/api/v1/radio")["readback"]
        storage = get_json(self.base, "/api/v1/recordings?limit=0")["storage"]
        lanes = readback.get("lane_counters") or []
        return {
            "control_iq_dropped": control["input"]["dropped"],
            "control_iq_gaps": control["input"]["gaps"],
            "tsbk_crc_failures": lsm.get("tsbk_crc_failures"),
            "grants_dropped": control.get("grants_dropped"),
            "lane_packets_lost": sum(c["lost"] for c in lanes),
            "lane_packets_missed": sum(c["missed"] for c in lanes),
            "packet_faults": readback.get("packet_faults"),
            "recordings_saved": storage["counters"]["saved"],
            "recordings_failed": storage["counters"]["failed"],
            "audio_chunks_missed": storage["counters"]["missed_chunks"],
        }

    def watch(self):
        while not self.stop.wait(2.0):
            try:
                status = get_json(self.base, "/api/v1/status", timeout=5)
                system = get_json(self.base, "/api/v1/system", timeout=5)
            except OSError:
                continue
            threads = {t["name"]: t["cpu_pct"] for t in system.get("thread_cpu", [])}
            self.samples.append({
                "ok_pct": status["control"].get("ok_pct"),
                "msgs_per_s": status["control"].get("msgs_per_s"),
                "live": status["live"]["state"],
                "cores": system.get("core_cpu_pct"),
                "files_cpu": threads.get("files"),
                "workers_cpu": threads.get("tokio-rt-worker"),
            })

    def __enter__(self):
        self.before = self.counters()
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stop.set()
        self.thread.join()
        after = self.counters()
        self.delta = {k: (after[k] - v if isinstance(v, (int, float)) and isinstance(after[k], (int, float)) else None) for k, v in self.before.items()}

    def summary(self):
        s = self.samples
        nums = lambda key: [x[key] for x in s if isinstance(x[key], (int, float))]
        low = lambda key: min(nums(key), default=None)
        high = lambda key: max(nums(key), default=None)
        cores = [max(x["cores"]) for x in s if x["cores"]]
        return {
            "samples": len(s),
            "control_ok_pct_min": low("ok_pct"),
            "control_msgs_per_s_min": low("msgs_per_s"),
            "not_live_samples": sum(1 for x in s if x["live"] != "live"),
            "busiest_core_pct_max": max(cores, default=None),
            "files_thread_cpu_pct_max": high("files_cpu"),
            "workers_cpu_pct_max": high("workers_cpu"),
            "counters_changed_by": self.delta,
        }


def fetch(base, path, out_dir, fallback_name):
    """Save a download; returns (file, bytes, the length the scanner promised or None, seconds)."""
    started = time.monotonic()
    with urllib.request.urlopen(base + path, timeout=600) as r:
        name = fallback_name
        disposition = r.headers.get("Content-Disposition", "")
        if 'filename="' in disposition:
            name = Path(disposition.split('filename="', 1)[1].split('"', 1)[0]).name
        promised = r.headers.get("Content-Length")
        out_dir.mkdir(parents=True, exist_ok=True)
        file = out_dir / name
        size = 0
        with open(file, "wb") as f:
            while True:
                chunk = r.read(CHUNK)
                if not chunk:
                    break
                f.write(chunk)
                size += len(chunk)
    return file, size, int(promised) if promised else None, time.monotonic() - started


def check_archive(file, expected):
    with zipfile.ZipFile(file) as z:
        bad = z.testzip()
        names = z.namelist()
        wavs = [n for n in names if n.endswith(".wav")]
        table = z.read("recordings.csv").decode("utf-8").splitlines() if "recordings.csv" in names else []
        listed = {line.split(",", 1)[0] for line in table[1:]}
        riff = sum(1 for n in wavs[:50] if z.read(n)[:4] == b"RIFF")
    ok = bad is None and len(wavs) <= expected and listed == set(wavs) and riff == min(50, len(wavs))
    return ok, {
        "crc_failure": bad,
        "recordings": len(wavs),
        "selected": expected,
        "left_out": expected - len(wavs),
        "table_rows": len(table) - 1 if table else None,
        "table_matches": listed == set(wavs),
        "first_50_are_wav": riff == min(50, len(wavs)),
        "zip64": any(i.header_offset > 0xFFFFFFFF for i in zipfile.ZipFile(file).infolist()) or len(names) > 0xFFFF,
    }


def check_history(file, size, promised):
    conn = sqlite3.connect(f"file:{file.as_posix()}?mode=ro&immutable=1", uri=True)
    try:
        check = conn.execute("PRAGMA quick_check").fetchone()[0]
        calls = conn.execute("SELECT COUNT(*) FROM calls").fetchone()[0]
        newest = conn.execute("SELECT MAX(started_ms) FROM calls").fetchone()[0]
    finally:
        conn.close()
    return check == "ok" and size == promised, {"quick_check": check, "calls": calls, "newest_call_unix_ms": newest, "length_as_promised": size == promised}


def check_tv(file, size, promised):
    with open(file, "rb") as f:
        head = f.read(188 * 64)
    synced = len(head) >= 188 and all(head[i] == 0x47 for i in range(0, len(head) - 187, 188))
    return synced and size == promised and size % 188 == 0, {"sync_bytes": synced, "whole_packets": size % 188 == 0, "length_as_promised": size == promised}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", required=True, help="the scanner: a host, host:port or URL (port 8080 by default)")
    ap.add_argument("--out", type=Path, help="where to save (default runs/files/<host>/)")
    ap.add_argument("--health", action="store_true", help="read the receivers before, during and after the download")
    sub = ap.add_subparsers(dest="what", required=True)
    sub.add_parser("list", help="what is on the card")
    rec = sub.add_parser("recordings", help="recordings as one ZIP archive")
    rec.add_argument("--from", dest="since", help="first day (YYYY-MM-DD, local) or unix ms")
    rec.add_argument("--to", help="last day (YYYY-MM-DD, local, included) or unix ms (excluded)")
    rec.add_argument("--site")
    rec.add_argument("--tg", type=int)
    rec.add_argument("--unit", type=int)
    rec.add_argument("--locked", action="store_true")
    rec.add_argument("--count", action="store_true", help="only say how many the selection takes")
    sub.add_parser("history", help="the history's database")
    tv = sub.add_parser("tv", help="a TV recording's file")
    tv.add_argument("rf", type=int)
    args = ap.parse_args()

    base = args.host if "://" in args.host else f"http://{args.host}"
    if base.count(":") < 2:
        base += ":8080"
    host = urllib.parse.urlparse(base).hostname
    out_dir = args.out or REPO / "runs" / "files" / host
    result = {"host": base, "what": args.what}
    try:
        if args.what == "list":
            listing = get_json(base, "/api/v1/files?span=15")
            # The spans added up into this PC's days.
            days = {}
            for s in listing["recordings"].pop("spans"):
                day = datetime.datetime.fromtimestamp(s["from_ms"] / 1000).strftime("%Y-%m-%d")
                d = days.setdefault(day, {"day": day, "count": 0, "bytes": 0, "locked": 0})
                for key in ("count", "bytes", "locked"):
                    d[key] += s[key]
            listing["recordings"]["days"] = list(days.values())
            result.update(listing)
            print(json.dumps(result, indent=2))
            return 0
        expected = None
        if args.what == "recordings":
            q = selection(args)
            taken = get_json(base, "/api/v1/files/recordings" + (f"?{q}" if q else ""))
            result["selection"] = {"query": q, **taken}
            if args.count or not taken["count"]:
                print(json.dumps(result, indent=2))
                return 0 if args.count else 1
            expected = taken["count"]
            path, fallback = "/api/v1/files/recordings.zip" + (f"?{q}" if q else ""), "recordings.zip"
        elif args.what == "history":
            path, fallback = "/api/v1/files/history.sqlite", "scanner-history.sqlite"
        else:
            path, fallback = f"/api/v1/files/tv/rf{args.rf}.ts", f"rf{args.rf}.ts"
        health = Health(base) if args.health else None
        if health:
            with health:
                file, size, promised, seconds = fetch(base, path, out_dir, fallback)
            result["health"] = health.summary()
        else:
            file, size, promised, seconds = fetch(base, path, out_dir, fallback)
        result.update({"file": str(file), "bytes": size, "seconds": round(seconds, 1), "mb_per_s": round(size / (1 << 20) / max(seconds, 1e-3), 2)})
        if args.what == "recordings":
            ok, result["check"] = check_archive(file, expected)
        elif args.what == "history":
            ok, result["check"] = check_history(file, size, promised)
        else:
            ok, result["check"] = check_tv(file, size, promised)
        result["ok"] = ok
        print(json.dumps(result, indent=2))
        return 0 if ok else 1
    except urllib.error.HTTPError as e:
        try:
            result["error"] = json.load(e).get("error") or e.reason
        except ValueError:
            result["error"] = e.reason
        result["status"] = e.code
        print(json.dumps(result, indent=2))
        return 3 if e.code == 409 else 1
    except (OSError, zipfile.BadZipFile, sqlite3.Error) as e:
        result["error"] = str(e)
        print(json.dumps(result, indent=2))
        return 1


if __name__ == "__main__":
    sys.exit(main())
