"""Decode DMR IQ WAVs with SDRTrunk's own DMRDecoder: the reference for protocol::dmr (change 075).

Input: 50 kSPS stereo int16 WAVs as p25-httpd's /api/control_iq_dump writes them.
Output: one line per message, `file|timestamp_ms|timeslot|valid|class|text`, where text is
SDRTrunk's toString() (protocol::dmr prints the same text, so the two can be diffed).

Needs a built SDRTrunk checkout (build/classes) and a JDK matching its toolchain. The runtime
classpath comes from Gradle --offline through an init script; nothing in the SDRTrunk repo
changes. Build products go to the system temp dir.

    python tools/sdrtrunk_dmr_reference.py --out ref.txt runs/dmr/cc_*.wav
"""
import argparse
import glob
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
HARNESS = HERE / "sdrtrunk_dmr_harness" / "DmrWavHarness.java"
INIT_SCRIPT = """allprojects {
    afterEvaluate { p ->
        if (p.plugins.hasPlugin('java')) {
            p.tasks.register('printRuntimeClasspath') {
                doLast { new File(System.getProperty('cpOut')).text = p.sourceSets.main.runtimeClasspath.asPath }
            }
        }
    }
}
"""


def classpath(sdrtrunk: Path, work: Path, refresh: bool) -> str:
    cp_file = work / "cp.txt"
    if refresh or not cp_file.exists():
        init = work / "printcp.gradle"
        init.write_text(INIT_SCRIPT)
        gradlew = sdrtrunk / ("gradlew.bat" if sys.platform == "win32" else "gradlew")
        subprocess.run([str(gradlew), "--offline", "-q", "--init-script", str(init), f"-DcpOut={cp_file}",
                        "printRuntimeClasspath"], cwd=sdrtrunk, check=True)
    return cp_file.read_text().strip()


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("wavs", nargs="+", help="WAV files or globs")
    ap.add_argument("--sdrtrunk", default="C:/Users/Andy/Projects/SDRTrunk/sdrtrunk", type=Path)
    ap.add_argument("--out", type=Path, help="output file (default stdout)")
    ap.add_argument("--refresh-classpath", action="store_true")
    args = ap.parse_args()

    work = Path(tempfile.gettempdir()) / "sdrtrunk_dmr_harness"
    work.mkdir(exist_ok=True)
    cp = classpath(args.sdrtrunk, work, args.refresh_classpath)
    sep = ";" if sys.platform == "win32" else ":"
    subprocess.run(["javac", "--add-modules", "jdk.incubator.vector", "-nowarn", "-cp", cp, "-d", str(work),
                    str(HARNESS)], check=True)
    files = sorted(f for pattern in args.wavs for f in (glob.glob(pattern) or [pattern]))
    out = open(args.out, "w", encoding="utf-8") if args.out else sys.stdout
    subprocess.run(["java", "--add-modules", "jdk.incubator.vector", "-cp", f"{work}{sep}{cp}", "DmrWavHarness",
                    *files], stdout=out, stderr=subprocess.DEVNULL, check=True)


if __name__ == "__main__":
    main()
