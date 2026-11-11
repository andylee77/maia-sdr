"""Hold the scanner's reading of SDRTrunk playlists against SDRTrunk's own (change 098).

Each playlist is read twice: by SDRTrunk's reader (tools/sdrtrunk_playlist_harness/PlaylistDump.java,
set up as its PlaylistManager sets it up) and by the scanner's (`cargo test playlist_reference`).
Both write the playlist as lines of text, one a channel and one an alias, and the lines are
compared. With `--unit`, the playlist a scanner exports (`GET /api/v1/playlist`) is fetched and
held the same way: that SDRTrunk reads it, and reads what the scanner wrote.

Needs a built SDRTrunk checkout (build/classes) and a JDK matching its toolchain; the classpath
comes from Gradle as for tools/sdrtrunk_dmr_reference.py. Build products and the lines go to the
system temp dir. Exit code 1 when a playlist is read differently.

    python tools/sdrtrunk_playlist_reference.py C:/Users/Andy/SDRTrunk/playlist/default.xml
    python tools/sdrtrunk_playlist_reference.py --unit http://192.168.120.50:8080
"""
import argparse
import os
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

from sdrtrunk_dmr_reference import classpath

import scanner_token

HERE = Path(__file__).resolve().parent
HARNESS = HERE / "sdrtrunk_playlist_harness" / "PlaylistDump.java"
SCANNER = HERE.parent / "scanner"
# What SDRTrunk takes a P25 channel to have when its file does not say.
DEFAULTS = {"mod": "C4FM", "ium": "false"}


def same(ours: str, theirs: str) -> bool:
    """One line of each. The scanner writes `-` for what a channel's file leaves out, where
    SDRTrunk has its default."""
    if ours == theirs:
        return True
    a, b = ours.split("|"), theirs.split("|")
    if len(a) != len(b) or a[0] != "channel":
        return False
    for x, y in zip(a, b):
        key = x.split("=")[0]
        if x != y and not (x == f"{key}=-" and y == f"{key}={DEFAULTS.get(key)}"):
            return False
    return True


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("playlists", nargs="*", type=Path, help="playlist files")
    ap.add_argument("--unit", action="append", default=[], help="a scanner's origin (http://host:8080): its exported playlist is held too")
    ap.add_argument("--sdrtrunk", default="C:/Users/Andy/Projects/SDRTrunk/sdrtrunk", type=Path)
    ap.add_argument("--refresh-classpath", action="store_true")
    args = ap.parse_args()
    scanner_token.install(*args.unit)

    work = Path(tempfile.gettempdir()) / "sdrtrunk_playlist_harness"
    work.mkdir(exist_ok=True)
    files = [p.resolve() for p in args.playlists]
    for n, unit in enumerate(args.unit):
        exported = work / f"exported_{n}.xml"
        with urllib.request.urlopen(f"{unit.rstrip('/')}/api/v1/playlist", timeout=20) as r:
            exported.write_bytes(r.read())
        print(f"{unit}: its playlist is {exported} ({exported.stat().st_size} bytes)")
        files.append(exported)
    if not files:
        ap.error("no playlist: name a file or a --unit")

    ours = [work / f"{n}_scanner.txt" for n in range(len(files))]
    theirs = [work / f"{n}_sdrtrunk.txt" for n in range(len(files))]
    cp = classpath(args.sdrtrunk, work, args.refresh_classpath)
    sep = ";" if sys.platform == "win32" else ":"
    subprocess.run(["javac", "-nowarn", "-cp", cp, "-d", str(work), str(HARNESS)], check=True)
    pairs = [str(x) for pair in zip(files, theirs) for x in pair]
    subprocess.run(["java", "-cp", f"{work}{sep}{cp}", "PlaylistDump", *pairs], stderr=subprocess.DEVNULL, check=True)
    env = dict(os.environ, PLAYLIST_FILES=";".join(map(str, files)), PLAYLIST_OUT=";".join(map(str, ours)))
    subprocess.run(["cargo", "test", "--quiet", "playlist_reference", "--", "--ignored"], cwd=SCANNER, env=env, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    wrong = 0
    for file, mine, ref in zip(files, ours, theirs):
        a = mine.read_text(encoding="utf-8").splitlines()
        b = ref.read_text(encoding="utf-8").splitlines()
        differing = [(x, y) for x, y in zip(a, b) if not same(x, y)]
        channels = sum(1 for x in b if x.startswith("channel|"))
        print(f"{file}: SDRTrunk reads {channels} channels and {len(b) - channels} aliases; "
              f"the scanner reads {len(a)} lines, {len(a) - len(differing) if len(a) == len(b) else 0} of {len(b)} the same")
        if len(a) != len(b):
            wrong += 1
            print(f"  the scanner has {len(a)} lines, SDRTrunk {len(b)}")
        for x, y in differing[:10]:
            print(f"  scanner : {x}\n  sdrtrunk: {y}")
        wrong += bool(differing)
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
