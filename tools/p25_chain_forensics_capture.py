#!/usr/bin/env python3
"""p25_chain_forensics_capture.py -- Track 2 of the 2026-05-03 three-track plan.

Captures a synchronised window of everything we need to compare the HDL
traffic chain to the SW reference decoder on the same RF:

  * wideband pre-DDC IQ (.cs16) -- input to both HDL and SW DDC.
    Rate defaults to 4 MSPS (current dual-DDC bake); pass
    --wideband-rate to override if the bake's wideband_iq DMA changes.
  * HDL traffic-chain hard dibits (.bits, SDRTrunk MSB-first 4-per-byte
    packing) -- the chain's per-symbol output for the same window
  * NID ring snapshots (.jsonl) -- HDL framer's per-NID PLL/sp/sync_dist
  * log tail (.jsonl) -- events during the window

Pair with `tools/p25_chain_compare.py` to run `software_decode` against
the captured wideband IQ, slide-align SW vs HDL dibits, and pinpoint
divergence (the diagnostic we have been skipping for 5+ flash cycles).

Default workflow:
  1. Snapshot /api/system + /api/stats for build + RX LO.
  2. Watch /api/grants every 200 ms; on first non-encrypted grant:
  3. POST /api/wideband_iq_capture?seconds=N (auto-enables wideband DMA).
  4. While capture runs (poll at 4 Hz):
       * /api/traffic_dibit_capture -> reconstruct full dibit stream by
         tracking total_dibits cursor; flag overflow if we miss
       * /api/hdl_lsm.nid_ring -> jsonl, deduped by seq
       * /api/log -> jsonl, deduped by seq
  5. Wait for the wideband capture to finish, scp the .cs16 file down.
  6. POST /api/sw_demod?enabled=0 so the wideband DMA goes idle again.
  7. Write meta.json + hdl_dibits.bits + nid_ring.jsonl + log.jsonl,
     plus a FINDINGS.md template, and print the cargo recipe to feed
     the capture into `software_decode`.

Usage:
  python tools/p25_chain_forensics_capture.py \\
    [--out doc/diagnostics/2026-05-03/forensics] \\
    [--seconds 3] \\
    [--host 192.168.2.1:8080] \\
    [--target-tg <id>] \\
    [--allow-encrypted] \\
    [--watch-timeout 60] \\
    [--no-wideband] \\
    [--clear-only]
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
import urllib.request
from datetime import datetime
from pathlib import Path

DEFAULT_HOST = "192.168.2.1:8080"
DEFAULT_WIDEBAND_RATE_HZ = 4_000_000  # post-2026-05-03 dual-DDC pivot
# 2026-05-03: bumped from 4 Hz to 10 Hz after observing a 3-sec poll gap
# during a 4-sec capture run that dropped 14336 dibits to a single ring
# overflow. At 100 ms cadence, a transient HTTP latency would have to
# exceed 426 ms (ring depth) to lose data; 10 Hz survives most jitter.
DIBIT_POLL_HZ = 10.0
GRANT_POLL_INTERVAL_S = 0.2
WIDEBAND_POLL_INTERVAL_S = 0.5
DIBIT_RING_DEPTH = 2048      # /api/traffic_dibit_capture buffer size
DIBIT_RATE_PER_SEC = 4800    # P25 symbol rate


def http_get(host: str, path: str, timeout: float = 5.0):
    url = f"http://{host}{path}"
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.loads(r.read())


def http_post(host: str, path: str, timeout: float = 10.0):
    url = f"http://{host}{path}"
    req = urllib.request.Request(url, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
        try:
            return json.loads(body)
        except ValueError:
            return {"_raw": body.decode("utf-8", errors="replace")}


def watch_for_grant(host, allow_encrypted, target_tg, timeout):
    """Poll /api/grants until a matching non-stale grant appears.
    Returns the grant dict or None on timeout."""
    seen_call_ids = set()
    # Prime the seen-set with whatever's already there so we only trigger
    # on a freshly-arrived grant.
    try:
        for g in http_get(host, "/api/grants") or []:
            cid = grant_id(g)
            seen_call_ids.add(cid)
    except Exception as e:
        print(f"  initial grant snapshot failed: {e}", file=sys.stderr)

    t_end = time.monotonic() + timeout
    poll_n = 0
    while time.monotonic() < t_end:
        try:
            grants = http_get(host, "/api/grants") or []
        except Exception as e:
            print(f"  poll {poll_n}: {e}", file=sys.stderr)
            time.sleep(GRANT_POLL_INTERVAL_S)
            continue
        for g in grants:
            cid = grant_id(g)
            if cid in seen_call_ids:
                continue
            seen_call_ids.add(cid)
            if g.get("encrypted") and not allow_encrypted:
                print(f"  poll {poll_n}: skip encrypted TG "
                      f"{g.get('talkgroup')}", file=sys.stderr)
                continue
            if target_tg is not None and g.get("talkgroup") != target_tg:
                continue
            return g
        poll_n += 1
        if poll_n % 25 == 0:
            print(f"  poll {poll_n}: no matching grant yet "
                  f"({GRANT_POLL_INTERVAL_S * poll_n:.1f}s elapsed)",
                  file=sys.stderr)
        time.sleep(GRANT_POLL_INTERVAL_S)
    return None


def grant_id(g):
    return g.get("call_id") or (
        g.get("talkgroup"), g.get("frequency_hz"), g.get("started_unix_ms"),
    )


def grant_freq_hz(g):
    fhz = g.get("frequency_hz") or g.get("freq_hz")
    if fhz is not None:
        return int(fhz)
    fmhz = g.get("frequency_mhz")
    if fmhz is not None:
        return int(round(fmhz * 1_000_000))
    return None


def pack_dibits_msb(dibits: list[int]) -> bytes:
    """Pack to SDRTrunk-style 4-dibits-per-byte MSB-first.

    Matches `SOFTDEC_DIBITS_OUT` in software_decode_tests.rs and the
    unpack convention in `tools/p25_dibit_diff.py`. First dibit lands
    in the top 2 bits of the byte; residual dibits are left-justified.
    """
    out = bytearray()
    acc = 0
    count = 0
    for d in dibits:
        acc = ((acc << 2) | (d & 0x3)) & 0xFF
        count += 1
        if count == 4:
            out.append(acc)
            acc = 0
            count = 0
    if count != 0:
        acc = (acc << ((4 - count) * 2)) & 0xFF
        out.append(acc)
    return bytes(out)


class DibitStream:
    """Reconstruct the full dibit stream by polling the rolling buffer.

    /api/traffic_dibit_capture exposes the last <=2048 dibits and a
    cumulative `total_dibits` counter. We poll fast enough that
    successive polls always overlap, then take the new tail off each
    response. If we ever fall behind by more than the ring depth we
    record an overflow (data lost) -- the compare tool looks for these.
    """

    def __init__(self):
        self.dibits: list[int] = []
        self.first_total: int | None = None
        self.last_total: int = 0
        self.overflows = 0     # times we lagged > buffer
        self.gap_dibits = 0    # total dibits dropped on the floor

    def consume(self, snapshot):
        captured = int(snapshot.get("captured", 0))
        total = int(snapshot.get("total_dibits", 0))
        hex_str = snapshot.get("dibits_hex", "") or ""
        if captured == 0 or not hex_str:
            return
        if self.first_total is None:
            # Anchor on first poll: take everything captured. We don't
            # know how far back into history this stretches, but the
            # SW reference decoder doesn't care about pre-grant dibits.
            self.dibits.extend(int(c, 16) & 0x3 for c in hex_str)
            self.first_total = total - captured
            self.last_total = total
            return
        new = total - self.last_total
        if new <= 0:
            return
        if new > captured:
            # Lagged farther than the ring; some dibits were overwritten.
            self.overflows += 1
            self.gap_dibits += new - captured
            new = captured
        tail = hex_str[-new:]
        self.dibits.extend(int(c, 16) & 0x3 for c in tail)
        self.last_total = total

    def summary(self):
        return {
            "n_dibits":     len(self.dibits),
            "first_total":  self.first_total,
            "last_total":   self.last_total,
            "overflows":    self.overflows,
            "gap_dibits":   self.gap_dibits,
            "duration_s":   round(len(self.dibits) / DIBIT_RATE_PER_SEC, 3),
        }


def fetch_seq_dedup(host, path, max_seen):
    """GET a /api endpoint that returns a list of {seq, ...} entries;
    return only entries with seq > max_seen. Returns (new_entries,
    new_max_seen)."""
    try:
        body = http_get(host, path)
    except Exception as e:
        return [], max_seen
    entries = body if isinstance(body, list) else body.get("entries", [])
    new = []
    new_max = max_seen
    for e in entries:
        s = e.get("seq")
        if s is None:
            continue
        if s > max_seen:
            new.append(e)
            if s > new_max:
                new_max = s
    return new, new_max


def scp_pull(host, remote_path, local_path):
    """Try scp -O first (modern OpenSSH on Windows + dropbear on the
    board need this), fall back to plain scp. Returns True on success.
    """
    host_ip = host.split(":")[0]
    if not shutil.which("scp"):
        print("  scp not on PATH; skipping wideband pull "
              f"(remote: {remote_path})", file=sys.stderr)
        return False
    src = f"root@{host_ip}:{remote_path}"
    for args in (["scp", "-O", src, str(local_path)],
                 ["scp", src, str(local_path)]):
        try:
            r = subprocess.run(args, capture_output=True, text=True, timeout=120)
            if r.returncode == 0:
                return True
            print(f"  scp ({args[1]}) failed: {r.stderr.strip()}",
                  file=sys.stderr)
        except Exception as e:
            print(f"  scp exception: {e}", file=sys.stderr)
    return False


def write_findings_template(path: Path, meta: dict, dibit_summary: dict):
    body = f"""# Forensics run {meta['run_id']}

Captured {meta['captured_iso']} on board {meta['host']} (build
`{meta.get('build_tag', '?')}`).

## RF context

- Talkgroup: {meta.get('target_tg')}
- Frequency: {meta.get('target_freq_hz')} Hz
- RX LO: {meta.get('rx_lo_hz')} Hz
- NCO offset (target − center): {meta.get('nco_offset_hz')} Hz
- Encrypted: {meta.get('encrypted')}

## Capture inventory

| File | Size | Notes |
|---|---|---|
| `wideband.cs16` | {meta.get('wideband_bytes', 0)} B | 8 MSPS pre-DDC, raw i16 LE I/Q |
| `hdl_dibits.bits` | {meta.get('dibits_bytes', 0)} B | {dibit_summary.get('n_dibits', 0)} dibits, SDRTrunk MSB-first packing |
| `nid_ring.jsonl` | -- | control-side NID events |
| `log.jsonl` | -- | log entries during the window |

Dibit-stream health: overflows={dibit_summary.get('overflows', 0)}
gap_dibits={dibit_summary.get('gap_dibits', 0)}
duration_s={dibit_summary.get('duration_s', 0)}

## Next step

```sh
python tools/p25_chain_compare.py {meta['run_id']}
```

The compare script will run `cargo test --release software_decode`
against `wideband.cs16`, dump SW dibits to `sw_dibits.bits`, then
slide-align SW vs HDL and report per-window agreement.

## Findings

(Fill in after running compare.)
"""
    path.write_text(body, encoding="utf-8")


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default=DEFAULT_HOST,
                    help=f"board host:port (default {DEFAULT_HOST})")
    ap.add_argument("--out", default=None,
                    help="output base dir; default doc/diagnostics/<date>/forensics")
    ap.add_argument("--seconds", type=float, default=3.0,
                    help="capture duration (s); default 3.0")
    ap.add_argument("--target-tg", type=int, default=None,
                    help="only trigger for this talkgroup id")
    ap.add_argument("--allow-encrypted", action="store_true",
                    help="trigger on encrypted grants too")
    ap.add_argument("--watch-timeout", type=float, default=60.0,
                    help="seconds to wait for a grant; default 60")
    ap.add_argument("--no-wideband", action="store_true",
                    help="skip wideband cs16 capture (only collect dibits)")
    ap.add_argument("--wideband-rate", type=int,
                    default=DEFAULT_WIDEBAND_RATE_HZ,
                    help=f"wideband_iq DMA sample rate "
                         f"(default {DEFAULT_WIDEBAND_RATE_HZ})")
    ap.add_argument("--clear-only", action="store_true",
                    help="dump current grants + system snapshot and exit")
    args = ap.parse_args()

    host = args.host
    today = datetime.now().strftime("%Y-%m-%d")
    base = Path(args.out) if args.out else Path(
        "doc/diagnostics") / today / "forensics"
    base.mkdir(parents=True, exist_ok=True)

    print(f"# host: {host}")
    sys_info = http_get(host, "/api/system")
    stats = http_get(host, "/api/stats")
    build_tag = sys_info.get("build") or sys_info.get("build_tag")
    rx_lo_hz = stats.get("rx_lo_hz")
    # `sampling_frequency_hz` is the AD9361 ADC rate the wideband_iq DMA
    # taps (4 MSPS post-2026-05-04 dual-DDC pivot, 8 MSPS before). The
    # capture POST returns an `approx_bytes` field but it's hardcoded
    # to 8 MSPS in the daemon (debug.rs:648 stale constant) so we don't
    # trust it. CLI --wideband-rate overrides this.
    sampling_freq_hz = stats.get("sampling_frequency_hz")
    print(f"# build: {build_tag}  rx_lo: {rx_lo_hz} Hz  "
          f"sample_rate: {sampling_freq_hz} Hz")

    if args.clear_only:
        try:
            grants = http_get(host, "/api/grants")
            print(json.dumps(grants, indent=2))
        except Exception as e:
            print(f"  grants fetch: {e}", file=sys.stderr)
        return 0

    print(f"# watching for non-stale "
          f"{'(any)' if args.allow_encrypted else 'unencrypted'} "
          f"grant; timeout={args.watch_timeout}s")
    grant = watch_for_grant(
        host, args.allow_encrypted, args.target_tg, args.watch_timeout)
    if grant is None:
        print("# no grant within watch timeout -- aborting", file=sys.stderr)
        return 2

    target_hz = grant_freq_hz(grant)
    target_tg = grant.get("talkgroup")
    encrypted = bool(grant.get("encrypted"))
    print(f"# GRANT TG={target_tg} freq={target_hz} encrypted={encrypted}")

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    run_id = f"run_{ts}"
    run_dir = base / run_id
    run_dir.mkdir(parents=True, exist_ok=False)
    print(f"# run dir: {run_dir}")

    # Trigger wideband capture (POST returns immediately; the reader
    # task tees the next N seconds of samples into the file).
    wb_remote_path = None
    # Prefer the live `sampling_frequency_hz` from /api/stats over the
    # CLI default; the CLI arg only kicks in if /api/stats didn't carry
    # the field for some reason.
    wideband_rate_hz = sampling_freq_hz or args.wideband_rate
    if not args.no_wideband:
        try:
            cap = http_post(
                host, f"/api/wideband_iq_capture?seconds={args.seconds:.2f}")
            wb_remote_path = cap.get("path")
            print(f"# wideband capture: {wb_remote_path} "
                  f"({args.seconds:.2f}s @ {wideband_rate_hz} Hz)")
        except Exception as e:
            print(f"# wideband capture POST failed: {e}", file=sys.stderr)

    # Snapshot the log + nid_ring high-water marks so we only persist
    # entries from THIS window.
    try:
        hdl_lsm = http_get(host, "/api/hdl_lsm")
        nid_seen = max(
            (int(e.get("seq", 0)) for e in (hdl_lsm.get("nid_ring") or [])),
            default=0)
    except Exception:
        nid_seen = 0
    log_seen, _ = fetch_seq_dedup(host, "/api/log?limit=200", -1)
    log_seen_max = max((int(e.get("seq", -1)) for e in log_seen), default=-1)

    # Drain loop: poll dibit ring + nid_ring + log at DIBIT_POLL_HZ.
    stream = DibitStream()
    nid_events: list[dict] = []
    log_events: list[dict] = []
    nid_max_seen = nid_seen
    log_max_seen = log_seen_max
    drain_until = time.monotonic() + args.seconds + 0.5
    poll_dt = 1.0 / DIBIT_POLL_HZ
    n_polls = 0
    while time.monotonic() < drain_until:
        t_poll = time.monotonic()
        try:
            snap = http_get(host, "/api/traffic_dibit_capture", timeout=2.0)
            stream.consume(snap)
        except Exception as e:
            print(f"  dibit poll error: {e}", file=sys.stderr)
        try:
            hdl = http_get(host, "/api/hdl_lsm", timeout=2.0)
            for e in (hdl.get("nid_ring") or []):
                s = int(e.get("seq", 0))
                if s > nid_max_seen:
                    nid_events.append(e)
                    nid_max_seen = s
        except Exception as e:
            print(f"  nid poll error: {e}", file=sys.stderr)
        new_log, log_max_seen = fetch_seq_dedup(
            host, "/api/log?limit=200", log_max_seen)
        log_events.extend(new_log)
        n_polls += 1
        sleep_left = poll_dt - (time.monotonic() - t_poll)
        if sleep_left > 0:
            time.sleep(sleep_left)
    print(f"# drained {n_polls} polls; "
          f"dibits={len(stream.dibits)} nid={len(nid_events)} "
          f"log={len(log_events)}")
    if stream.overflows:
        print(f"  WARNING: {stream.overflows} dibit ring overflows "
              f"({stream.gap_dibits} dibits dropped)", file=sys.stderr)

    # Wait for the wideband capture to finish, then scp the file down.
    wideband_local = None
    wideband_bytes = 0
    if wb_remote_path:
        deadline = time.monotonic() + args.seconds + 8.0
        while time.monotonic() < deadline:
            try:
                snap = http_get(host, "/api/wideband_iq_capture", timeout=3.0)
                cap = (snap.get("capture") or {})
                if cap.get("active") is None:
                    break
            except Exception:
                pass
            time.sleep(WIDEBAND_POLL_INTERVAL_S)
        wideband_local = run_dir / "wideband.cs16"
        ok = scp_pull(host, wb_remote_path, wideband_local)
        if ok and wideband_local.exists():
            wideband_bytes = wideband_local.stat().st_size
            print(f"# scp'd wideband {wideband_bytes} B -> {wideband_local}")
        else:
            print(f"# WARNING: scp failed; raw file remains at "
                  f"{host}:{wb_remote_path}")

        # Turn the wideband DMA back off so we don't burn power /
        # tmpfs once the user is done.
        try:
            http_post(host, "/api/sw_demod?enabled=0")
        except Exception as e:
            print(f"  sw_demod disable failed: {e}", file=sys.stderr)

    # Persist outputs.
    dibits_path = run_dir / "hdl_dibits.bits"
    packed = pack_dibits_msb(stream.dibits)
    dibits_path.write_bytes(packed)
    (run_dir / "hdl_dibits_meta.json").write_text(
        json.dumps(stream.summary(), indent=2))
    with (run_dir / "nid_ring.jsonl").open("w") as f:
        for e in nid_events:
            f.write(json.dumps(e) + "\n")
    with (run_dir / "log.jsonl").open("w") as f:
        for e in log_events:
            f.write(json.dumps(e) + "\n")

    nco_offset = (target_hz - rx_lo_hz) if (target_hz and rx_lo_hz) else None
    meta = {
        "run_id":           run_id,
        "host":             host,
        "build_tag":        build_tag,
        "captured_unix":    int(time.time()),
        "captured_iso":     datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ"),
        "rx_lo_hz":         rx_lo_hz,
        "target_freq_hz":   target_hz,
        "target_tg":        target_tg,
        "encrypted":        encrypted,
        "nco_offset_hz":    nco_offset,
        "capture_seconds":  args.seconds,
        "wideband_path":    str(wideband_local) if wideband_local else None,
        "wideband_bytes":   wideband_bytes,
        "wideband_remote":  wb_remote_path,
        "wideband_rate_hz": wideband_rate_hz,
        "dibits_bytes":     len(packed),
        "dibits_summary":   stream.summary(),
        "n_polls":          n_polls,
        "n_nid_events":     len(nid_events),
        "n_log_events":     len(log_events),
        "grant":            grant,
    }
    (run_dir / "meta.json").write_text(json.dumps(meta, indent=2))
    write_findings_template(
        run_dir / "FINDINGS.md", meta, stream.summary())

    # Print the cargo recipe (compare tool also emits this; convenience
    # for one-step copy/paste from the capture invocation).
    print()
    print("=== ready-to-run software_decode recipe ===")
    if wideband_local:
        print(f"SOFTDEC_INPUT={wideband_local} \\")
        print(f"  SOFTDEC_INPUT_RATE={wideband_rate_hz} \\")
        print(f"  SOFTDEC_CENTER_HZ={rx_lo_hz} \\")
        print(f"  SOFTDEC_TARGET_HZ={target_hz} \\")
        print(f"  SOFTDEC_OUT_WAV={run_dir / 'sw_decoded.wav'} \\")
        print(f"  SOFTDEC_DIBITS_OUT={run_dir / 'sw_dibits.bits'} \\")
        print(f"  SOFTDEC_METRICS_JSON={run_dir / 'sw_metrics.json'} \\")
        print(f"  cargo test --release software_decode -- "
              f"--ignored --nocapture")
        print()
        print(f"# or run the compare tool to do all of the above + diff:")
        print(f"python tools/p25_chain_compare.py {run_dir}")
    else:
        print("(no wideband file -- re-run without --no-wideband to "
              "feed the SW decoder)")
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
