#!/usr/bin/env python3
"""p25_call_monitor.py -- fast poll loop for catching short P25 calls.

Polls /api/traffic every 250 ms on the running p25-httpd target and
logs:
  - every traffic-state transition with a timestamp
  - per-call summary (LDU1/LDU2/TDU/TDU_LC/IMBE/vocoder/retune
    counters, current talkgroup, channel, frequency) when the state
    returns to Idle
  - running total of unencrypted grants seen vs encrypted rejected

P25 LSM calls on this site are often <5 seconds, so the 250 ms poll
cadence matters. A single-shot `curl /api/traffic` after the fact
sees nothing because the traffic chain returns to Idle in between
retunes.

Usage:
    python tools/p25_call_monitor.py [TARGET]

Ctrl-C to stop. TARGET defaults to 192.168.2.1:8080.

SPDX-License-Identifier: MIT
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import sys
import time
import urllib.request


def now_hms() -> str:
    return _dt.datetime.now().strftime('%H:%M:%S.%f')[:-3]


def fetch(target: str) -> dict | None:
    try:
        with urllib.request.urlopen(
            f'http://{target}/api/traffic', timeout=2.0
        ) as r:
            return json.loads(r.read().decode('utf-8'))
    except Exception:
        return None


def extract(d: dict) -> dict:
    """Pull out the fields we care about for transition + summary
    comparison."""
    return {
        'state': d.get('state'),
        'tg': d.get('current_talkgroup'),
        'chan': d.get('current_channel'),
        'freq': d.get('current_frequency_hz'),
        'enc': d.get('current_call_encrypted'),
        'retunes': d.get('retunes', 0),
        'hdus': d.get('hdus_seen', 0),
        'ldu1': d['imbe']['ldu1_count'],
        'ldu2': d['imbe']['ldu2_count'],
        'tdu': d['imbe']['tdu_count'],
        'tdu_lc': d['imbe']['tdu_lc_count'],
        'imbe': d['imbe']['imbe_frames_extracted'],
        'imbe_drop': d['imbe']['imbe_frames_dropped'],
        'vocoder_err': d['imbe']['vocoder_errors'],
        'pcm': d['imbe']['vocoder_pcm_produced'],
        'grants_seen': d.get('grants_seen', 0),
        'grants_enc': d.get('grants_rejected_encrypted', 0),
        'traffic_irq': d['irq']['traffic_dma_total'],
        'traffic_lsm_irq': d['irq']['traffic_lsm_dibit_total'],
    }


def diff(before: dict, after: dict, keys: list[str]) -> str:
    parts = []
    for k in keys:
        d = after[k] - before[k]
        if d != 0:
            parts.append(f'{k}+{d}')
    return ' '.join(parts)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        'target', nargs='?', default='192.168.2.1:8080',
        help='p25-httpd target [default %(default)s]')
    ap.add_argument(
        '--interval', type=float, default=0.25,
        help='poll interval in seconds [default %(default)s]')
    args = ap.parse_args()

    print(f'# P25 call monitor @ {args.target}  '
          f'({args.interval*1000:.0f} ms poll)', flush=True)
    print('# ts           event                 details', flush=True)

    prev: dict | None = None
    call_start: dict | None = None
    call_start_ts: str | None = None
    # Counts of each state entry for sanity.
    last_state_change_ts: str | None = None
    delta_keys = [
        'retunes', 'hdus', 'ldu1', 'ldu2', 'tdu', 'tdu_lc',
        'imbe', 'imbe_drop', 'vocoder_err', 'pcm',
        'grants_seen', 'grants_enc',
        'traffic_irq', 'traffic_lsm_irq',
    ]

    try:
        while True:
            raw = fetch(args.target)
            if raw is None:
                time.sleep(args.interval)
                continue
            cur = extract(raw)
            ts = now_hms()
            if prev is None:
                print(f'{ts}  init                  state={cur["state"]} '
                      f'grants_seen={cur["grants_seen"]} '
                      f'grants_enc={cur["grants_enc"]} '
                      f'retunes={cur["retunes"]}', flush=True)
                prev = cur
                if cur['state'] != 'Idle':
                    call_start = cur
                    call_start_ts = ts
                time.sleep(args.interval)
                continue

            # State transition?
            if cur['state'] != prev['state']:
                delta = diff(prev, cur, delta_keys)
                extra = f' [{delta}]' if delta else ''
                tg_info = ''
                if cur['tg'] is not None:
                    tg_info = (f' tg={cur["tg"]} chan={cur["chan"]} '
                               f'freq={cur["freq"]} enc={cur["enc"]}')
                print(
                    f'{ts}  {prev["state"]:>12} -> {cur["state"]:<12}'
                    f'{tg_info}{extra}',
                    flush=True)
                last_state_change_ts = ts

                # Idle -> non-Idle = call start
                if prev['state'] == 'Idle' and cur['state'] != 'Idle':
                    call_start = dict(prev)
                    call_start_ts = ts

                # non-Idle -> Idle = call end, summarize
                if prev['state'] != 'Idle' and cur['state'] == 'Idle':
                    if call_start is not None:
                        summary = diff(call_start, cur, delta_keys)
                        dur = 'unk'
                        if call_start_ts is not None:
                            try:
                                t0 = _dt.datetime.strptime(
                                    call_start_ts, '%H:%M:%S.%f')
                                t1 = _dt.datetime.strptime(
                                    ts, '%H:%M:%S.%f')
                                dur = f'{(t1 - t0).total_seconds():.2f}s'
                            except Exception:
                                pass
                        print(f'{ts}  CALL-SUMMARY          dur={dur} '
                              f'{summary}', flush=True)
                        call_start = None
                        call_start_ts = None

            # Big counter deltas while in a call (IMBE count changing)
            elif cur['state'] != 'Idle':
                if cur['imbe'] != prev['imbe']:
                    print(
                        f'{ts}  {cur["state"]:>12}  IMBE+'
                        f'{cur["imbe"] - prev["imbe"]} '
                        f'(total {cur["imbe"]}) '
                        f'ldu1+{cur["ldu1"] - prev["ldu1"]} '
                        f'ldu2+{cur["ldu2"] - prev["ldu2"]}',
                        flush=True)

            prev = cur
            time.sleep(args.interval)
    except KeyboardInterrupt:
        print(f'\n{now_hms()}  stopped', flush=True)
        return 0


if __name__ == '__main__':
    sys.exit(main() or 0)
