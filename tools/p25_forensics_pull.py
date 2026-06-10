#!/usr/bin/env python3
"""p25_forensics_pull.py -- host companion to the on-device forensics
ring (build `2026-05-03-on-device-forensics+`).

Replaces the host-polling design in `p25_chain_forensics_capture.py`:
the device now buffers dibits to RAM, auto-triggers a wideband IQ
capture on every CallOpen, finalises on CallClose. This script just:

  1. POSTs /api/forensics_arm with the chosen options
  2. Polls /api/forensics_status, watching `last_run_dir` for new runs
  3. SCPs each new run dir + matching wideband.cs16 down
  4. Optionally repeats until Ctrl-C

Each pulled run dir lands under
`doc/diagnostics/<date>/forensics/run_<unix>_tg<TG>_<freq>/`
with `meta.json`, `hdl_dibits.bits`, `FINDINGS.md`, and the matching
`wideband.cs16` (scp'd from /mnt/sd/p25_iq_captures/ on the board;
moved off /tmp 2026-05-03 since 30 s of 4 MSPS IQ = 480 MB > tmpfs).
Remote paths come from the device (`last_run_dir` + `meta.wideband_remote`)
so this script doesn't hardcode them.

Usage:
  python tools/p25_forensics_pull.py [--host 192.168.2.1:8080]
      [--dibit-max-mb 8] [--wideband-seconds 30]
      [--follow-encrypted] [--no-auto-rearm]
      [--max-runs N]                      # default: until Ctrl-C
      [--out doc/diagnostics/<date>/forensics]
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
import urllib.request
import urllib.parse
from datetime import datetime
from pathlib import Path

DEFAULT_HOST = "192.168.2.1:8080"
POLL_INTERVAL_S = 1.0


def http_get(host: str, path: str, timeout: float = 5.0):
    url = f"http://{host}{path}"
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.loads(r.read())


def http_post(host: str, path: str, params=None, timeout: float = 10.0):
    url = f"http://{host}{path}"
    if params:
        qs = urllib.parse.urlencode(params)
        url = f"{url}?{qs}"
    req = urllib.request.Request(url, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
        try:
            return json.loads(body)
        except ValueError:
            return {"_raw": body.decode("utf-8", errors="replace")}


def scp_pull(host: str, remote: str, local: Path, timeout: float = 180.0) -> bool:
    if not shutil.which("scp"):
        print(f"  scp not on PATH; cannot pull {remote}", file=sys.stderr)
        return False
    host_ip = host.split(":")[0]
    src = f"root@{host_ip}:{remote}"
    for args in (["scp", "-O", "-r", src, str(local)],
                 ["scp", "-r", src, str(local)]):
        try:
            r = subprocess.run(args, capture_output=True, text=True,
                               timeout=timeout)
            if r.returncode == 0:
                return True
            print(f"  scp ({args[1]}) failed: {r.stderr.strip()}",
                  file=sys.stderr)
        except Exception as e:
            print(f"  scp exception: {e}", file=sys.stderr)
    return False


def arm(host, dibit_max_mb, wideband_seconds, auto_rearm, follow_encrypted):
    params = {
        "dibit_max_mb":     str(dibit_max_mb),
        "wideband_seconds": str(wideband_seconds),
        "auto_rearm":       "1" if auto_rearm else "0",
        "follow_encrypted": "1" if follow_encrypted else "0",
    }
    return http_post(host, "/api/forensics_arm", params=params)


def disarm(host):
    return http_post(host, "/api/forensics_disarm")


def status(host):
    return http_get(host, "/api/forensics_status")


def fetch_meta(host, remote_dir):
    """Run dirs live on /mnt/sd; we don't have NFS. Use scp -O to read
    meta.json (Buildroot ships dropbear with no sftp-server, so legacy
    scp protocol is required). Returns dict or None on failure."""
    host_ip = host.split(":")[0]
    src = f"root@{host_ip}:{remote_dir}/meta.json"
    try:
        r = subprocess.run(
            ["scp", "-O", "-q", src, "/dev/stdout"]
            if sys.platform != "win32"
            else ["scp", "-O", "-q", src, "-"],
            capture_output=True, text=True, timeout=15)
        if r.returncode == 0 and r.stdout:
            return json.loads(r.stdout)
    except Exception as e:
        print(f"  fetch_meta exception: {e}", file=sys.stderr)
    return None


def pull_run(host: str, remote_dir: str, local_base: Path) -> Path | None:
    """Pull a complete run dir (including wideband.cs16 if available)."""
    name = remote_dir.rstrip("/").split("/")[-1]
    local_dir = local_base / name
    print(f"  pulling {remote_dir} -> {local_dir}")
    # SCP the whole run dir
    if not scp_pull(host, remote_dir, local_base):
        print(f"  WARNING: scp of run dir failed; skipping wideband too")
        return None
    # Read meta.json to find the wideband path
    meta_path = local_dir / "meta.json"
    wb_remote = None
    if meta_path.is_file():
        try:
            m = json.loads(meta_path.read_text())
            wb_remote = m.get("wideband_remote")
        except Exception:
            pass
    if wb_remote:
        wb_local = local_dir / "wideband.cs16"
        if scp_pull(host, wb_remote, wb_local):
            print(f"  wideband: {wb_local} ({wb_local.stat().st_size:,} B)")
        else:
            print(f"  WARNING: wideband scp failed (remote: {wb_remote})")
    else:
        print(f"  no wideband_remote in meta.json -- run dir only")
    return local_dir


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--dibit-max-mb", type=int, default=8)
    ap.add_argument("--wideband-seconds", type=float, default=30.0)
    ap.add_argument("--no-auto-rearm", action="store_true",
                    help="single-shot capture (default: keep capturing)")
    ap.add_argument("--follow-encrypted", action="store_true",
                    help="device follows encrypted grants too (testing)")
    ap.add_argument("--max-runs", type=int, default=0,
                    help="stop after N runs; 0 = until Ctrl-C")
    ap.add_argument("--out", default=None,
                    help="output base; default doc/diagnostics/<date>/forensics")
    ap.add_argument("--status-only", action="store_true",
                    help="dump device status and exit (no arming)")
    ap.add_argument("--disarm", action="store_true",
                    help="disarm and exit (no pulling)")
    args = ap.parse_args()

    host = args.host
    today = datetime.now().strftime("%Y-%m-%d")
    base = Path(args.out) if args.out else Path(
        "doc/diagnostics") / today / "forensics"
    base.mkdir(parents=True, exist_ok=True)

    if args.status_only:
        print(json.dumps(status(host), indent=2))
        return 0
    if args.disarm:
        print(json.dumps(disarm(host), indent=2))
        return 0

    auto_rearm = not args.no_auto_rearm
    print(f"# host: {host}")
    print(f"# arming: dibit_max_mb={args.dibit_max_mb} "
          f"wideband_seconds={args.wideband_seconds} "
          f"auto_rearm={auto_rearm} follow_encrypted={args.follow_encrypted}")
    arm_resp = arm(host, args.dibit_max_mb, args.wideband_seconds,
                   auto_rearm, args.follow_encrypted)
    if not arm_resp.get("ok"):
        print(f"  arm failed: {arm_resp}", file=sys.stderr)
        return 1

    print(f"# armed. Waiting for next CallOpen on the board "
          f"(Ctrl-C to disarm + exit)...")

    seen_run_dirs: set[str] = set()
    # Prime with current last_run_dir so we don't re-pull old runs.
    cur = status(host)
    if cur.get("last_run_dir"):
        seen_run_dirs.add(cur["last_run_dir"])
    runs_pulled = 0

    try:
        while True:
            if args.max_runs and runs_pulled >= args.max_runs:
                break
            time.sleep(POLL_INTERVAL_S)
            try:
                s = status(host)
            except Exception as e:
                print(f"  status error: {e}", file=sys.stderr)
                continue
            # Live progress line for the current call
            ac = s.get("active_call")
            if ac:
                trunc = " [TRUNCATED]" if ac.get("dibits_truncated") else ""
                sys.stdout.write(
                    f"\r  active: TG {ac['tg']} freq {ac.get('freq_hz')} "
                    f"dibits={ac['dibit_count']:,}{trunc}      ")
                sys.stdout.flush()
            new_run = s.get("last_run_dir")
            if new_run and new_run not in seen_run_dirs:
                seen_run_dirs.add(new_run)
                if ac:
                    sys.stdout.write("\n")
                print(f"# new run completed: {new_run}")
                local = pull_run(host, new_run, base)
                if local:
                    runs_pulled += 1
                    print(f"# pulled {runs_pulled} run(s) so far")
    except KeyboardInterrupt:
        print("\n# Ctrl-C, disarming...")

    print(json.dumps(disarm(host), indent=2))
    print(f"# done. {runs_pulled} run(s) pulled to {base}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
