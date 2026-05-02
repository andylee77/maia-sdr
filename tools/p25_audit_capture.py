#!/usr/bin/env python3
"""Pull all inputs for `doc/diagnostics/2026-04-30/audit/audit.py`-style
analysis from a running p25-httpd board into a fresh diagnostics folder.

Usage:
    python tools/p25_audit_capture.py \\
        --host http://192.168.2.1:8080 \\
        --out doc/diagnostics/<date>/audit_m2b

Pulls (best-effort; missing endpoints are skipped):

  - /api/system, /api/sys_health, /api/ppm        (snapshot context)
  - /api/grant_decode_stats?limit=1000            (per-call lifecycle)
  - /api/log?category=grant&limit=2000            (raw GVCG/UPD events)
  - /api/log?category=recorder&limit=1000         (lifecycle boundaries)
  - /api/log?category=traffic&limit=1000          (retune events)
  - /api/log?category=duid&limit=2000             (per-NID events)
  - /api/log?category=system&limit=200            (PPM, errors)
  - /api/log?category=vocoder&limit=500           (vocoder/IMBE)
  - /api/recordings?limit=200                     (recorder slots)
  - /api/grants_active                            (live grant map)
  - /api/decoder_compare                          (PS vs HDL framer)
  - /api/hdl_lsm                                  (control LSM health)
  - /api/traffic                                  (traffic LSM health + bins)
  - /api/traffic_bins                             (M2B channelizer bins)
  - /api/spectrum_wide?fft_size=4096              (one wideband shot)
  - /api/bands                                    (channel raster)

Then writes a tiny `meta.json` with capture wall-clock + build_tag for
correlation against future captures.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import requests


ENDPOINTS: list[tuple[str, str]] = [
    # (output_filename, api_path)
    ("system.json",             "/api/system"),
    ("sys_health.json",         "/api/sys_health"),
    ("ppm.json",                "/api/ppm"),
    ("grant_decode_stats.json", "/api/grant_decode_stats?limit=1000"),
    ("log_grant.json",          "/api/log?category=grant&limit=2000"),
    ("log_recorder.json",       "/api/log?category=recorder&limit=1000"),
    ("log_traffic.json",        "/api/log?category=traffic&limit=1000"),
    ("log_duid.json",           "/api/log?category=duid&limit=2000"),
    ("log_system.json",         "/api/log?category=system&limit=200"),
    ("log_vocoder.json",        "/api/log?category=vocoder&limit=500"),
    ("log_all.json",            "/api/log?limit=3000"),
    ("recordings.json",         "/api/recordings?limit=200"),
    ("grants_active.json",      "/api/grants_active"),
    ("decoder_compare.json",    "/api/decoder_compare"),
    ("hdl_lsm.json",            "/api/hdl_lsm"),
    ("traffic.json",            "/api/traffic"),
    ("traffic_bins.json",       "/api/traffic_bins"),
    ("spectrum_wide.json",      "/api/spectrum_wide?fft_size=4096"),
    ("bands.json",              "/api/bands"),
    ("stats.json",              "/api/stats"),
    ("irq_stats.json",          "/api/irq_stats"),
]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="http://192.168.2.1:8080")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--timeout", type=float, default=8.0)
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    pulled, missed = [], []
    t0 = time.time()
    for fname, path in ENDPOINTS:
        url = args.host.rstrip("/") + path
        try:
            r = requests.get(url, timeout=args.timeout)
            if r.status_code != 200:
                missed.append((fname, path, f"http {r.status_code}"))
                continue
            try:
                body = r.json()
            except ValueError:
                # Not JSON; save raw
                (args.out / fname).write_bytes(r.content)
                pulled.append(fname)
                continue
            (args.out / fname).write_text(json.dumps(body, indent=2))
            pulled.append(fname)
        except requests.exceptions.RequestException as e:
            missed.append((fname, path, str(e)))

    meta = {
        "host":       args.host,
        "captured_unix_secs": int(t0),
        "captured_iso":       time.strftime(
            "%Y-%m-%dT%H:%M:%SZ", time.gmtime(t0)),
        "pulled":     pulled,
        "missed":     [{"file": f, "path": p, "err": e}
                       for f, p, e in missed],
    }
    (args.out / "meta.json").write_text(json.dumps(meta, indent=2))

    print(f"Captured {len(pulled)} endpoints into {args.out}")
    if missed:
        print(f"  ({len(missed)} missed: {', '.join(m[0] for m in missed)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
