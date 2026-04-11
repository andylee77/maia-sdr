#!/usr/bin/env python3
"""
Phase 7A.1 sticky-lock verification test.

Polls /api/traffic until state != Idle (i.e. we caught an active call),
then takes a burst of N rapid samples to see whether `retunes` stays
flat while we're locked on the same TG. The pre-fix binary thrashed
retunes upward at ~15+/sec during multi-TG activity. The post-fix
binary should hold retunes constant for the entire duration of a
single call (with at most one extra retune per TG channel
reassignment, which is rare).

Usage: python tools/p25_sticky_lock_test.py [target]
"""
import json
import sys
import time
import urllib.request

TARGET = sys.argv[1] if len(sys.argv) > 1 else "192.168.2.1:8080"


def fetch(path: str):
    with urllib.request.urlopen(f"http://{TARGET}{path}", timeout=3.0) as r:
        return json.load(r)


def short_grants():
    try:
        return [(g["talkgroup"], g["channel"], g["age_secs"])
                for g in fetch("/api/grants")]
    except Exception as e:
        return f"ERR:{e}"


def short_traffic():
    return fetch("/api/traffic")


def main():
    print(f"Probing {TARGET}/api/traffic until state != Idle (max 60s)...")
    deadline = time.time() + 60.0
    found = None
    while time.time() < deadline:
        t = short_traffic()
        if t["state"] != "Idle":
            found = t
            break
        time.sleep(0.4)
    if not found:
        print("FAIL: no active call seen in 60 s. Try again during peak time.")
        return 2

    initial_retunes = found["retunes"]
    initial_grants_seen = found["grants_seen"]
    initial_tg = found["current_talkgroup"]
    print(f"\nGot active call:")
    print(f"  initial state          = {found['state']}")
    print(f"  initial talkgroup      = {initial_tg}")
    print(f"  initial channel        = {found['current_channel']}")
    print(f"  initial frequency_hz   = {found['current_frequency_hz']}")
    print(f"  initial retunes        = {initial_retunes}")
    print(f"  initial grants_seen    = {initial_grants_seen}")
    print(f"  last_retune_secs_ago   = {found['last_retune_secs_ago']:.2f}")
    print(f"\nSampling 12 times over 12 seconds (1 sec apart)...\n")

    samples = []
    for i in range(12):
        t = short_traffic()
        g = short_grants()
        samples.append({
            "i":          i,
            "wall":       time.time(),
            "state":      t["state"],
            "tg":         t["current_talkgroup"],
            "ch":         t["current_channel"],
            "retunes":    t["retunes"],
            "grants_seen": t["grants_seen"],
            "last_retune_ago": t["last_retune_secs_ago"],
            "wakeups":    t["stats"]["wakeups"],
            "all_grants": g,
        })
        print(f"  i={i:2d} state={t['state']:<10} tg={t['current_talkgroup']} "
              f"retunes={t['retunes']} grants_seen={t['grants_seen']} "
              f"wakeups={t['stats']['wakeups']} all_grants={g}")
        time.sleep(1.0)

    final_retunes = samples[-1]["retunes"]
    delta_retunes = final_retunes - initial_retunes
    delta_grants_seen = samples[-1]["grants_seen"] - initial_grants_seen

    print(f"\n=== verdict ===")
    print(f"  retunes:     {initial_retunes} -> {final_retunes}  (delta {delta_retunes})")
    print(f"  grants_seen: {initial_grants_seen} -> {samples[-1]['grants_seen']}  "
          f"(delta {delta_grants_seen})")

    # Compute "retunes per sec during this 12s window"
    rate = delta_retunes / 12.0
    print(f"  retune rate during the 12s sample window: {rate:.2f} retunes/sec")

    # Check whether the locked TG stayed pinned
    tgs_seen = set(s["tg"] for s in samples if s["tg"] is not None)
    print(f"  unique talkgroups locked across the 12 samples: {sorted(tgs_seen)}")

    if delta_retunes <= 2:
        print(f"\nPASS: retune count stable (<=2 retunes in 12s) -- sticky lock works")
        return 0
    elif delta_retunes <= 6:
        print(f"\nMARGINAL: {delta_retunes} retunes in 12s -- some TG transitions but not thrashing")
        return 1
    else:
        print(f"\nFAIL: {delta_retunes} retunes in 12s -- still thrashing")
        return 3


if __name__ == "__main__":
    sys.exit(main())
