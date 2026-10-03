"""Run SDRTrunk's own P25 LSM decoder (P25P1DecoderLSM): the reference for the scanner's LSM port (change 079).

`taps` prints the decoder's filters at 25 kSPS (name|count|values). `decode` takes 50 kSPS stereo
int16 WAVs (SDRTrunk `_baseband.wav` recordings, /api/v1/iq/control.wav captures), writes each
one's dibits to OUT_DIR/<name>.bits in SDRTrunk's `.bits` packing, and prints one line per message,
`file|timestamp_ms|valid|class|text`. Each file gets a fresh decoder.

Needs a built SDRTrunk checkout and a JDK matching its toolchain; the classpath and the build
directory are shared with tools/sdrtrunk_dmr_reference.py.

    python tools/sdrtrunk_lsm_reference.py taps
    python tools/sdrtrunk_lsm_reference.py decode --out-dir runs/079/sdrtrunk "C:/Users/Andy/SDRTrunk/recordings/*_baseband.wav"
"""
import argparse
import glob
import subprocess
import sys
import tempfile
from pathlib import Path

from sdrtrunk_dmr_reference import classpath

HERE = Path(__file__).resolve().parent
HARNESS = HERE / "sdrtrunk_lsm_harness" / "LsmWavHarness.java"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("mode", choices=["taps", "decode"])
    ap.add_argument("wavs", nargs="*", help="WAV files or globs (decode)")
    ap.add_argument("--out-dir", type=Path, help="where the .bits files go (decode)")
    ap.add_argument("--out", type=Path, help="output file (default stdout)")
    ap.add_argument("--sdrtrunk", default="C:/Users/Andy/Projects/SDRTrunk/sdrtrunk", type=Path)
    ap.add_argument("--refresh-classpath", action="store_true")
    args = ap.parse_intermixed_args()
    if args.mode == "decode" and (not args.wavs or not args.out_dir):
        ap.error("decode needs --out-dir and at least one WAV")

    work = Path(tempfile.gettempdir()) / "sdrtrunk_dmr_harness"
    work.mkdir(exist_ok=True)
    cp = classpath(args.sdrtrunk, work, args.refresh_classpath)
    sep = ";" if sys.platform == "win32" else ":"
    subprocess.run(["javac", "--add-modules", "jdk.incubator.vector", "-nowarn", "-cp", cp, "-d", str(work),
                    str(HARNESS)], check=True)
    runs = [["taps"]]
    if args.mode == "decode":
        files = sorted(f for pattern in args.wavs for f in (glob.glob(pattern) or [pattern]))
        # Batches keep each command line under Windows' length limit.
        runs = [["decode", str(args.out_dir), *files[k:k + 40]] for k in range(0, len(files), 40)]
    out = open(args.out, "w", encoding="utf-8") if args.out else sys.stdout
    for harness_args in runs:
        subprocess.run(["java", "--add-modules", "jdk.incubator.vector", "-cp", f"{work}{sep}{cp}", "LsmWavHarness",
                        *harness_args], stdout=out, stderr=subprocess.DEVNULL, check=True)
        out.flush()


if __name__ == "__main__":
    main()
