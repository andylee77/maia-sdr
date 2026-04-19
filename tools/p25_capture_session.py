#!/usr/bin/env python3
"""Full-session Fishball capture for SDRTrunk cross-validation.

Captures everything Fishball observes during a window, in a form
designed to be diffed against an SDRTrunk session recorded on the
same RF signal over the same window:

  - NID ring (every unique NID event: seq, duid, n_errors, pll_dbg,
    sp_dbg, sync_distance, valid, t_ms_since_boot) — the primary
    evidence for the TDU_LC phase-slip-vs-noise question.
  - TSBK opcode counters at start + end (for message-volume diff).
  - Grants + grant_map snapshots at 1 Hz (call-event timeline).
  - Traffic counters (HDU/LDU/TDU/TDU_LC/extracted/dropped).
  - Boot-time anchor so board `t_ms_since_boot` can be converted to
    wall-clock ms matching SDRTrunk's timestamps.

Usage:
    python tools/p25_capture_session.py \\
        --host 192.168.2.1:8080 \\
        --secs 900 \\
        --out doc/diagnostics/2026-04-19/sdrtrunk_xcheck
"""
from __future__ import annotations

import argparse
import json
import os
import time
import urllib.request


def fetch(host: str, path: str, timeout: float = 3.0):
    url = f"http://{host}{path}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except Exception as e:
        return {"_err": str(e)}


def compute_boot_unix_ms(host: str) -> int | None:
    """Anchor board t_ms_since_boot → wall-clock unix ms.

    The board clock is typically UNLOCKED (no NTP), so its own
    system_clock field is unusable. Instead: read /api/sys_health
    uptime_secs, subtract from this host's current wall-clock ms,
    get the unix-ms at which the board booted. Add that to any
    NID ring `t_ms_since_boot` to get wall-time matching SDRTrunk.
    """
    sh = fetch(host, "/api/sys_health")
    if "_err" in sh:
        return None
    uptime_s = sh.get("uptime_secs")
    if uptime_s is None:
        return None
    now_ms = int(time.time() * 1000)
    return now_ms - int(uptime_s * 1000)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.2.1:8080")
    ap.add_argument("--secs", type=float, default=900.0)
    ap.add_argument("--out", required=True)
    ap.add_argument("--nid-poll-hz", type=float, default=5.0,
                    help="NID-ring poll rate; must be high enough to not miss seqs")
    ap.add_argument("--grants-poll-hz", type=float, default=1.0)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)

    session_start_unix_ms = int(time.time() * 1000)
    boot_unix_ms = compute_boot_unix_ms(args.host)
    anchor = {
        "host": args.host,
        "session_start_unix_ms": session_start_unix_ms,
        "session_start_iso": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime()),
        "board_boot_unix_ms": boot_unix_ms,
        "note": (
            "Add board_boot_unix_ms to any NID ring t_ms_since_boot "
            "to get wall-time unix-ms matching SDRTrunk log entries."
        ),
    }
    with open(os.path.join(args.out, "anchor.json"), "w") as f:
        json.dump(anchor, f, indent=2)
    print(f"session start {anchor['session_start_iso']}  board_boot={boot_unix_ms}")

    # Snapshot endpoints at start (for before-snapshot).
    for name, path in [
        ("system", "/api/system"),
        ("sys_health", "/api/sys_health"),
        ("tsbk_opcodes_start", "/api/tsbk_opcodes"),
        ("grant_map_start", "/api/grant_map"),
        ("encrypted_tgs_start", "/api/encrypted_tgs"),
        ("hdl_lsm_start", "/api/hdl_lsm"),
    ]:
        d = fetch(args.host, path)
        with open(os.path.join(args.out, f"ep_{name}.json"), "w") as f:
            json.dump(d, f, indent=2)

    # Open the continuous-capture files.
    nid_log = open(os.path.join(args.out, "nid_events.jsonl"), "w")
    grants_log = open(os.path.join(args.out, "grants_timeline.jsonl"), "w")
    traffic_log = open(os.path.join(args.out, "traffic_counters.jsonl"), "w")

    deadline = time.monotonic() + args.secs
    next_nid = time.monotonic()
    next_grants = time.monotonic()
    seen_seqs: set[int] = set()
    n_nids = 0
    n_grant_snaps = 0
    nid_interval = 1.0 / args.nid_poll_hz
    grants_interval = 1.0 / args.grants_poll_hz

    try:
        while time.monotonic() < deadline:
            t_now_ms = int(time.time() * 1000)

            if time.monotonic() >= next_nid:
                next_nid += nid_interval
                d = fetch(args.host, "/api/hdl_lsm")
                if "_err" not in d:
                    for ev in d.get("nid_ring") or []:
                        seq = ev.get("seq")
                        if seq is None or seq in seen_seqs:
                            continue
                        seen_seqs.add(seq)
                        ev["_poll_unix_ms"] = t_now_ms
                        if boot_unix_ms is not None:
                            ev["_event_unix_ms"] = (
                                boot_unix_ms + int(ev.get("t_ms_since_boot", 0))
                            )
                        nid_log.write(json.dumps(ev) + "\n")
                        n_nids += 1

            if time.monotonic() >= next_grants:
                next_grants += grants_interval
                g = fetch(args.host, "/api/grants")
                t = fetch(args.host, "/api/traffic")
                if "_err" not in g:
                    rec = {"unix_ms": t_now_ms, "grants": g}
                    grants_log.write(json.dumps(rec) + "\n")
                if "_err" not in t:
                    # Keep only the counter subset that matters for diff.
                    keep = {
                        k: t.get(k)
                        for k in (
                            "hdu_received", "ldu1_received", "ldu2_received",
                            "tdu_received", "tdu_lc_received",
                            "imbe_frames_extracted", "imbe_frames_dropped",
                            "vocoder_frames_silent_suppressed",
                            "vocoder_errors", "vocoder_frames_encrypted",
                            "grants_seen", "grants_rejected_encrypted", "retunes",
                        )
                    }
                    keep["unix_ms"] = t_now_ms
                    traffic_log.write(json.dumps(keep) + "\n")
                    n_grant_snaps += 1

            # Yield to stdout periodically so operator sees progress.
            if n_grant_snaps % 30 == 0 and n_grant_snaps > 0:
                print(f"  t+{int(time.monotonic() - (deadline - args.secs))}s  "
                      f"nids={n_nids}  grants_snaps={n_grant_snaps}")

            time.sleep(0.05)
    finally:
        nid_log.close()
        grants_log.close()
        traffic_log.close()

    # End-of-session snapshot for diff against start.
    for name, path in [
        ("tsbk_opcodes_end", "/api/tsbk_opcodes"),
        ("grant_map_end", "/api/grant_map"),
        ("recordings_end", "/api/recordings"),
        ("hdl_lsm_end", "/api/hdl_lsm"),
    ]:
        d = fetch(args.host, path)
        with open(os.path.join(args.out, f"ep_{name}.json"), "w") as f:
            json.dump(d, f, indent=2)

    print(f"\nsession complete")
    print(f"  unique NIDs: {n_nids}")
    print(f"  grant snapshots: {n_grant_snaps}")
    print(f"  output: {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
