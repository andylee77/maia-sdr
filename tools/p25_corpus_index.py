#!/usr/bin/env python3
"""Inventory the SDRTrunk recordings + wideband captures and write the P25 replay manifest.

Maps every wideband capture's time span to the ``.mbe`` transmissions inside it
(counts, TGs, clear/encrypted, in band or not), aligns every channel recording
to its ``decoded_messages`` log clock (CC and traffic alike start at the log's
bit 0 + ~12 ms), maps CC recordings to the traffic recordings that overlap them
and traffic recordings to their ``.mbe`` / ``.bits`` / ``.mp3``, and plans the
three replay modes of ``rf.p25_corpus`` (A real wideband air, B synthetic full
system, C traffic only). Prints how many distinct transmissions and calls each
mode covers. Read-only on the SDRTrunk directories.

Usage:
  python tools/p25_corpus_index.py                       # default dirs -> bench/.state/corpus/manifest.json
  python tools/p25_corpus_index.py --report corpus.md --json-summary
  python tools/p25_corpus_index.py --focus 20260503_091125_858437500_1_300_1014 --primer-tg 300

The focus calls (default: the 2026-05-03 09:11 TG 300 / 1014 two-tone alert) are
ordered first in modes B and C; with ffmpeg on PATH their SDRTrunk MP3s are decoded
once to record the reference tone frequencies for the audio-continuity check.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "bench"))

from fbench.analysis import p25_corpus as pc  # noqa: E402
from fbench.analysis.p25_score import tone_reference  # noqa: E402
from fbench.analysis.sdrtrunk import local_stamp  # noqa: E402
from fbench.util import dump_json  # noqa: E402

SDRT = Path("C:/Users/Andy/SDRTrunk")


def mp3_pcm(path: Path) -> "object | None":
    ff = shutil.which("ffmpeg")
    if not ff:
        return None
    import numpy as np

    res = subprocess.run([ff, "-v", "error", "-i", str(path), "-f", "s16le", "-ac", "1",
                          "-ar", "8000", "-"], capture_output=True, timeout=60)
    if res.returncode != 0:
        return None
    return np.frombuffer(res.stdout, dtype="<i2").astype(float)


def focus_references(man: dict, recordings: Path) -> None:
    """Reference tone stats of the focus calls' MP3s (SDRTrunk's own decode)."""
    refs = {}
    recs = [r for r in man["recordings"] if r["kind"] == "traffic"]
    txs = pc.tx_index(man)
    for call in man["focus"]:
        rec = next((r for r in recs if any(txs[i]["call"] == call for i in r["transmissions"])),
                   None)
        if rec is None:
            continue
        out = []
        for mp3 in rec.get("mp3", []):
            pcm = mp3_pcm(recordings / mp3)
            if pcm is None:
                continue
            ref = tone_reference(pcm)
            ref["mp3"] = mp3
            out.append(ref)
        refs[call] = {"recording": rec["id"], "mp3": out}
    man["focus_reference"] = refs


def report(man: dict) -> str:
    s = man["summary"]
    m = s["modes"]
    L = [f"# P25 replay corpus inventory ({man['generated']})", "",
         f"Sources: `{man['sources']['captures']}`, `{man['sources']['recordings']}`, "
         f"`{man['sources']['event_logs']}`.", "",
         "## Inventory", "",
         f"- `.mbe` calls: {s['mbe_calls']}; transmissions (frame runs split at > 0.5 s): "
         f"{s['transmissions']} ({s['transmissions_clear']} clear), {s['frames']} IMBE frames",
         f"- Wideband captures: {s['captures']}, {s['capture_seconds'] / 60:.1f} min, "
         f"{s['capture_bytes'] / 1e9:.1f} GB",
         f"- Channel recordings: {s['channel_recordings']} ({s['cc_recordings']} CC, "
         f"{s['traffic_recordings']} traffic, {s['traffic_with_truth']} traffic with `.mbe` "
         f"truth); {s['log_aligned']} aligned to a decoded_messages log clock, "
         f"{s['bits_files']} with `.bits`",
         f"- Transmissions inside a channel recording: {s['transmissions_in_channel_recordings']}",
         f"- Carrier offsets as recorded (Hz, p10 / median / p90): CC "
         f"{'/'.join(str(v) for v in (s['offset_hz_cc'] or {}).values())}, traffic "
         f"{'/'.join(str(v) for v in (s['offset_hz_traffic'] or {}).values())} "
         "(removed per recording in modes B and C)",
         f"- MP3 files: {s['mp3_files']}; decoded_messages logs: {s['decoded_messages_logs']}",
         "", "## Coverage per replay mode", "",
         "| Mode | Transmissions | Clear | Encrypted | Calls (clear) | Clear IMBE | Stream | "
         "Data | Upload |", "|---|---|---|---|---|---|---|---|---|"]
    for name, c in (("A whole captures (SD relay)", m["A_whole"]),
                    ("A windows (RAM)", m["A_windows"]), ("B synthetic full system", m["B"]),
                    ("C traffic only", m["C"])):
        L.append(f"| {name} | {c['transmissions']} | {c['clear']} | {c['encrypted']} | "
                 f"{c['calls']} ({c['calls_clear']}) | {c['frames_clear']} | "
                 f"{c['stream_s'] / 60:.1f} min | {c['bytes'] / 1e9:.2f} GB {c['format']} | "
                 f"{c['upload_min']:.0f} min |")
    L += ["", "## Mode A: captures", "",
          "| Capture | Start | Length | In band | Clear | TGs | Partial | Out of band |",
          "|---|---|---|---|---|---|---|---|"]
    for c in man["plans"]["A"]["captures"]:
        tgs = ", ".join(f"{k}x{v}" for k, v in c["tgs"].items()) or "-"
        L.append(f"| `{c['id']}` | {local_stamp(c['start_unix'])} | {c['seconds']:.0f} s | "
                 f"{len(c['transmissions'])} | {c['clear']} | {tgs} | {len(c['partial'])} | "
                 f"{len(c['out_of_band'])} |")
    aw = m["A_whole"]
    L += ["", f"`items=all` plays the {aw['captures']} captures with `.mbe` truth "
          f"({aw['bytes'] / 1e9:.1f} GB {aw['format']}); all {aw['captures_all']} captures would "
          f"be {aw['bytes_all'] / 1e9:.1f} GB."]
    b = man["plans"]["B"]
    rates = sorted({s["rate_hz"] for s in b["scenes"]})
    L += ["", f"## Mode B: {len(b['scenes'])} scenes ({', '.join(f'{r / 1e6:g}' for r in rates)} "
          "MSPS)", "",
          f"Alignment: {b['alignment']['method']}, residual about "
          f"+-{b['alignment']['residual_s'] * 1000:.0f} ms; "
          f"{b['alignment']['name_only_sources']} sources fall back to the 1 s file name; "
          f"{len(b['dropped_out_of_band'])} traffic recordings too far from the CC for one "
          f"{b['max_rate_hz'] / 1e6:g} MSPS band. TX centres keep every channel within "
          f"+-{pc.USABLE:.2f} fs and its TX IQ image (and the LO) >= "
          f"{pc.IMAGE_CLEAR_HZ / 1e3:.0f} kHz from every channel.", "",
          "| Scene | Length | Rate | Sources | Transmissions | Clear | Focus |",
          "|---|---|---|---|---|---|---|"]
    for sc in b["scenes"]:
        L.append(f"| `{sc['id']}` | {sc['seconds']:.0f} s | {sc['rate_hz'] / 1e6:g} | "
                 f"{len(sc['sources'])} | {len(sc['transmissions'])} | {sc['clear']} | "
                 f"{'yes' if sc['focus'] else ''} |")
    c = man["plans"]["C"]
    pr = c["primer"]
    L += ["", f"## Mode C: {len(c['items'])} items in {len(c['batches'])} batches on "
          f"{c['channel_hz'] / 1e6:.4f} MHz ({c['rate_hz'] / 1e6:g} MSPS, TX centre "
          f"{c['centre_hz'] / 1e6:.4f} MHz)", ""]
    if pr:
        g = pr["grant"]
        L.append(f"Primer: CC recording `{pr['cc']}` at +{pr['offset_s']:.1f} s, grant TG {g['tg']} "
                 f"to {g['freq_hz'] / 1e6:.4f} MHz.")
    else:
        L.append("No primer grant found: mode C cannot run.")
    if man.get("focus_reference"):
        L += ["", "## Focus calls", ""]
        for call, ref in man["focus_reference"].items():
            for r in ref["mp3"]:
                L.append(f"- `{call}` via `{r['mp3']}`: {r['seconds']:.2f} s, tones "
                         f"{r['tones_hz']} Hz, tonal frames {r['tonal_frames']}")
    return "\n".join(L) + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--captures", default=str(SDRT / "my_captures"))
    ap.add_argument("--recordings", default=str(SDRT / "recordings"))
    ap.add_argument("--event-logs", default=str(SDRT / "event_logs"))
    ap.add_argument("--out", default=str(REPO / "bench" / ".state" / "corpus" / "manifest.json"))
    ap.add_argument("--report", default="", help="write the markdown report here (default stdout)")
    ap.add_argument("--cc-freq", type=int, default=860962500)
    ap.add_argument("--nac", default="8A1")
    ap.add_argument("--focus", nargs="*", default=list(pc.DEFAULT_FOCUS),
                    help=".mbe call ids (file stems) to put first in modes B and C")
    ap.add_argument("--primer-tg", type=int, default=300)
    ap.add_argument("--no-mp3", action="store_true", help="skip the MP3 tone references")
    ap.add_argument("--json-summary", action="store_true", help="print the summary as JSON")
    a = ap.parse_args(argv)
    man = pc.build_manifest(a.captures, a.recordings, a.event_logs, cc_freq=a.cc_freq,
                            nac=a.nac, focus=tuple(a.focus), primer_tg=a.primer_tg)
    if not a.no_mp3:
        focus_references(man, Path(a.recordings))
    dump_json(man, Path(a.out))
    text = report(man)
    if a.report:
        Path(a.report).write_text(text, encoding="utf-8")
    if a.json_summary:
        print(json.dumps({"manifest": a.out, **man["summary"]}, indent=1))
    else:
        print(text)
        print(f"manifest: {a.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
