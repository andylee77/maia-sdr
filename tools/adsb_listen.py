#!/usr/bin/env python3
"""The aircraft module through a scanner's API (`/api/v1/adsb`): what it hears on its two links,
1090 MHz (Mode S) and 978 MHz (UAT).

    adsb_listen.py --host 192.168.120.50 status
    adsb_listen.py --host 192.168.120.50 link 1090|978|both
    adsb_listen.py --host 192.168.120.50 listen --seconds 30 [--link 978] [--take-radio] [--aircraft]

`status` prints the module's state, the link it listens to, its window, its counts and how many
aircraft it lists. `link` sets the link: one of the two, or both in turn.

`listen` watches it for `--seconds` and says what came in that time: Mode S messages whose
parity was right (extended squitters and all-call replies) of the preambles tried, UAT messages
their code put right of the places its synchronisation fitted, how many a second, the aircraft
listed at the end by link and how many of them said a callsign, an altitude and a place, whether
samples were lost (`gaps`) or blocks dropped, and the scanner's threads' share of the processor.
With `--link` it listens to that link for the stay and puts the link back as it was. With
`--take-radio` it puts the scanner in ADS-B mode first and gives the radio back after: no call
is followed meanwhile. `--aircraft` adds the list itself.

Prints one JSON object; exits 0 when nothing was dropped and, on 1090 MHz or both, extended
squitters came (on 978 MHz alone there may be no aircraft in the air to hear), 1 when not, 3
when the scanner refused.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

import scanner_token


def call(base, path, method="GET", body=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"} if data else {})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def refused(e):
    try:
        return json.load(e).get("error") or str(e)
    except (ValueError, AttributeError):
        return str(e)


def summary(view):
    out = {k: view[k] for k in ("state", "error", "link", "on", "window", "messages_per_s", "counts")}
    aircraft = view["aircraft"]
    out["aircraft"] = {
        "listed": len(aircraft),
        "on_1090": sum(1 for a in aircraft if a["link"] == "1090"),
        "on_978": sum(1 for a in aircraft if a["link"] == "978"),
        "with_a_callsign": sum(1 for a in aircraft if a["callsign"]),
        "with_an_altitude": sum(1 for a in aircraft if a["altitude_ft"] is not None),
        "with_a_speed": sum(1 for a in aircraft if a["speed_kt"] is not None),
        "with_a_place": sum(1 for a in aircraft if a["lat"] is not None),
    }
    return out


def threads(base):
    """The scanner's busiest threads, per cent of one core each (a unit only)."""
    try:
        system = call(base, "/api/v1/system")
    except (urllib.error.URLError, OSError, ValueError):
        return None
    top = sorted(system.get("thread_cpu") or [], key=lambda t: -t["cpu_pct"])[:4]
    return {"process_pct": system.get("cpu_pct"), "cores_pct": system.get("core_cpu_pct"), "threads": {t["name"]: t["cpu_pct"] for t in top}}


def listen(base, args):
    """Watch the module. The result and the exit code; the radio is given back whatever
    happened."""
    result = {}
    took = False
    link_was = None
    try:
        if args.link:
            link_was = call(base, "/api/v1/adsb")["link"]
            call(base, "/api/v1/adsb/link", "PUT", {"link": args.link})
        if args.take_radio and call(base, "/api/v1/mode")["mode"] != "adsb":
            call(base, "/api/v1/mode", "PUT", {"mode": "adsb"}, timeout=120)
            took = True
        view = call(base, "/api/v1/adsb")
        for _ in range(40):
            if view["state"] in ("listening", "error"):
                break
            time.sleep(0.5)
            view = call(base, "/api/v1/adsb")
        if view["state"] != "listening":
            result["error"] = f"the aircraft module does not listen: {view['error'] or view['state']}"
            return result, 1
        first, began = view["counts"], time.time()
        cpu = []
        while time.time() - began < args.seconds:
            time.sleep(min(5.0, max(0.2, args.seconds - (time.time() - began))))
            cpu.append(threads(base))
        view = call(base, "/api/v1/adsb")
        seconds = time.time() - began
        heard = {k: view["counts"][k] - first[k] for k in first}
        result.update(summary(view))
        result["in_the_stay"] = {"seconds": round(seconds, 1), **heard, "messages_per_s": round((heard["squitters"] + heard["all_call"] + heard["uat"]) / seconds, 1),
                                 "tried_per_s": round(heard["tried"] / seconds), "fitted_per_s": round(heard["fitted"] / seconds, 1)}
        result["processor"] = cpu[-1] if cpu else None
        if args.aircraft:
            result["list"] = sorted(view["aircraft"], key=lambda a: -a["messages"])
        silent = view["link"] != "978" and heard["squitters"] == 0
        ok = not silent and heard["dropped"] == 0
        if not ok:
            result["error"] = "no extended squitter came" if silent else f"{heard['dropped']} blocks of the window were dropped"
        return result, 0 if ok else 1
    except urllib.error.HTTPError as e:
        result["error"] = refused(e)
        return result, 3
    finally:
        if link_was is not None:
            try:
                call(base, "/api/v1/adsb/link", "PUT", {"link": link_was})
            except (urllib.error.URLError, OSError) as e:
                result["link"] = f"NOT put back to {link_was}: {e}"
        if took:
            try:
                call(base, "/api/v1/mode", "PUT", {"mode": "scanner"}, timeout=180)
                result["radio"] = "given back to the scanner"
            except (urllib.error.URLError, OSError) as e:
                result["radio"] = f"NOT given back to the scanner: {e}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", required=True, help="the scanner: a host, host:port or URL (port 8080; with SCANNER_TOKEN set, HTTPS on 8443)")
    sub = ap.add_subparsers(dest="what", required=True)
    sub.add_parser("status", help="what the module hears")
    lk = sub.add_parser("link", help="the link it listens to")
    lk.add_argument("to", choices=["1090", "978", "both"])
    ls = sub.add_parser("listen", help="watch it for a while")
    ls.add_argument("--seconds", type=float, default=30.0)
    ls.add_argument("--link", choices=["1090", "978", "both"], help="listen to this link for the stay, and put the link back after")
    ls.add_argument("--take-radio", action="store_true", help="put the scanner in ADS-B mode first, and give the radio back after")
    ls.add_argument("--aircraft", action="store_true", help="the aircraft listed at the end too")
    args = ap.parse_args()

    scanner_token.install(args.host)
    base = scanner_token.base(args.host)
    try:
        if args.what == "status":
            result, code = summary(call(base, "/api/v1/adsb")), 0
        elif args.what == "link":
            result, code = summary(call(base, "/api/v1/adsb/link", "PUT", {"link": args.to})), 0
        else:
            result, code = listen(base, args)
    except urllib.error.HTTPError as e:
        result, code = {"error": refused(e)}, 3
    print(json.dumps({"host": base, "what": args.what, **result}, indent=1))
    return code


if __name__ == "__main__":
    sys.exit(main())
