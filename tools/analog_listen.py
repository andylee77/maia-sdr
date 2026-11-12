#!/usr/bin/env python3
"""The analog receiver through a scanner's API (`/api/v1/analog`) and its audio (`/ws/analog`).

    analog_listen.py --host 192.168.120.50 status
    analog_listen.py --host 192.168.120.50 tune --mhz 102.9 --demod wfm
    analog_listen.py --host 192.168.120.50 tune --mhz 162.55 --demod nfm --squelch 8
    analog_listen.py --host 192.168.120.50 listen --seconds 15 [--mhz 162.55 --demod nfm] [--take-radio] [--silent]

`tune` sets what the receiver is tuned to: the frequency in MHz, how it is demodulated (`wfm`
broadcast FM, `nfm` narrow FM, `am`) and, for the narrow two, the squelch in dB over the floor
(`off` for none).

`listen` records the receiver's audio for `--seconds` as a WAV file under `--out` (default
`runs/analog/<host>/`), after tuning when a frequency is given, and says what came: how much
audio, its level, what the meter read. With `--take-radio` it puts the scanner in analog mode
first and gives the radio back after: no call is followed meanwhile. It expects audio for most of
the time; with `--silent` it expects none (a frequency with nothing on it, the squelch shut).

Prints one JSON object; exits 0 when all is as asked, 1 when not, 3 when the scanner refused.
"""

import argparse
import base64
import json
import math
import os
import socket
import ssl
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import wave
from pathlib import Path

import scanner_token

REPO = Path(__file__).resolve().parent.parent


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
    return {k: view[k] for k in ("state", "error", "tuned", "window", "level", "audio_hz", "listeners", "counts")}


class Socket:
    """A WebSocket read with the standard library: enough for a server's text and binary frames."""

    def __init__(self, base, path):
        url = urllib.parse.urlparse(base)
        secure = url.scheme == "https"
        port = url.port or (443 if secure else 80)
        raw = socket.create_connection((url.hostname, port), timeout=15)
        if secure:
            context = ssl.create_default_context(cafile=os.environ.get("SCANNER_CA"))
            raw = context.wrap_socket(raw, server_hostname=url.hostname)
        self.sock = raw
        key = base64.b64encode(os.urandom(16)).decode()
        lines = [f"GET {path} HTTP/1.1", f"Host: {url.netloc}", "Upgrade: websocket", "Connection: Upgrade", f"Sec-WebSocket-Key: {key}", "Sec-WebSocket-Version: 13"]
        token = os.environ.get("SCANNER_TOKEN")
        if token and secure:
            lines.append(f"Authorization: Bearer {token}")
        raw.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
        self.held = b""
        head = b""
        while b"\r\n\r\n" not in head:
            more = raw.recv(4096)
            if not more:
                raise OSError("the socket closed before it opened")
            head += more
        head, self.held = head.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n", 1)[0].decode(errors="replace")
        if " 101 " not in status:
            raise OSError(f"no socket: {status}")

    def take(self, n):
        while len(self.held) < n:
            more = self.sock.recv(65536)
            if not more:
                raise OSError("the socket closed")
            self.held += more
        out, self.held = self.held[:n], self.held[n:]
        return out

    def frame(self):
        """The next frame: (`text` or `binary`, its payload)."""
        while True:
            first, second = self.take(2)
            length = second & 0x7F
            if length == 126:
                (length,) = struct.unpack(">H", self.take(2))
            elif length == 127:
                (length,) = struct.unpack(">Q", self.take(8))
            payload = self.take(length)
            opcode = first & 0x0F
            if opcode == 0x1:
                return "text", payload.decode()
            if opcode == 0x2:
                return "binary", payload
            if opcode == 0x8:
                raise OSError("the scanner closed the socket")
            if opcode == 0x9:
                # A ping: its payload back, masked as a client's frames are.
                mask = os.urandom(4)
                self.sock.sendall(bytes([0x8A, 0x80 | len(payload)]) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def tuned(args, now):
    """What to tune to, from the arguments and what the receiver is tuned to now."""
    to = dict(now)
    if args.mhz is not None:
        to["freq_hz"] = round(args.mhz * 1e6)
    if args.demod:
        to["demod"] = args.demod
    if args.squelch is not None:
        to["squelch_db"] = None if args.squelch == "off" else float(args.squelch)
    if to["demod"] == "wfm":
        to["squelch_db"] = None
    return to


def listen(base, args, out_dir):
    """Record the receiver's audio. The result and the exit code; the radio is given back whatever
    happened."""
    result = {}
    took = False
    sock = None
    try:
        if args.take_radio and call(base, "/api/v1/mode")["mode"] != "analog":
            call(base, "/api/v1/mode", "PUT", {"mode": "analog"}, timeout=120)
            took = True
        view = call(base, "/api/v1/analog")
        if args.mhz is not None or args.demod or args.squelch is not None:
            view = call(base, "/api/v1/analog/tune", "PUT", tuned(args, view["tuned"]))
        for _ in range(40):
            if view["state"] in ("listening", "error"):
                break
            time.sleep(0.5)
            view = call(base, "/api/v1/analog")
        if view["state"] != "listening":
            result["error"] = f"the analog receiver does not listen: {view['error'] or view['state']}"
            return result, 1
        # The radio has moved and the meter has read before the recording begins.
        time.sleep(1.5)
        sock = Socket(base, "/ws/analog")
        rate, samples, lag = None, bytearray(), 0
        levels = []
        began = time.time()
        sock.sock.settimeout(1.0)
        next_look = began
        while time.time() - began < args.seconds:
            if time.time() >= next_look:
                level = call(base, "/api/v1/analog")["level"]
                if level:
                    levels.append(level)
                next_look += 1.0
            try:
                kind, payload = sock.frame()
            except (socket.timeout, TimeoutError):
                continue
            if kind == "text":
                message = json.loads(payload)
                if message.get("type") == "format":
                    rate = message["rate_hz"]
                elif message.get("type") == "lag":
                    lag += message.get("skipped", 0)
            else:
                samples += payload
        view = call(base, "/api/v1/analog")
        result["listening"] = summary(view)
        count = len(samples) // 2
        values = struct.unpack(f"<{count}h", bytes(samples[:2 * count]))
        seconds = count / rate if rate else 0.0
        rms = math.sqrt(sum(v * v for v in values) / count) / 32767 if count else 0.0
        result.update(audio_hz=rate, seconds_asked=args.seconds, seconds_of_audio=round(seconds, 2), samples=count, lagged_frames=lag,
                      rms=round(rms, 4), peak=round(max((abs(v) for v in values), default=0) / 32767, 4))
        if levels:
            over = [x["over_db"] for x in levels if x["over_db"] is not None]
            result["meter"] = {"readings": len(levels), "open": sum(1 for x in levels if x["open"]),
                               "level_dbfs": [round(min(x["level_dbfs"] for x in levels), 1), round(max(x["level_dbfs"] for x in levels), 1)],
                               "over_db": [round(min(over), 1), round(max(over), 1)] if over else None}
        if count:
            t = view["tuned"]
            out_dir.mkdir(parents=True, exist_ok=True)
            path = out_dir / f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime(began))}_{t['freq_hz'] / 1e6:.4f}M_{t['demod']}.wav"
            with wave.open(str(path), "wb") as w:
                w.setnchannels(1)
                w.setsampwidth(2)
                w.setframerate(rate)
                w.writeframes(bytes(samples[:2 * count]))
            result["file"] = str(path)
        if args.silent:
            ok = seconds < 0.1 * args.seconds
            if not ok:
                result["error"] = f"{seconds:.1f} s of audio where none was expected"
        else:
            ok = seconds > 0.8 * args.seconds and rms > 0.005 and lag == 0
            if not ok:
                result["error"] = f"{seconds:.1f} s of audio in {args.seconds} s, rms {rms:.4f}, {lag} frames lagged"
        return result, 0 if ok else 1
    except urllib.error.HTTPError as e:
        result["error"] = refused(e)
        return result, 3
    except OSError as e:
        result["error"] = f"the audio socket: {e}"
        return result, 1
    finally:
        if sock:
            sock.close()
        if took:
            try:
                call(base, "/api/v1/mode", "PUT", {"mode": "scanner"}, timeout=180)
                result["radio"] = "given back to the scanner"
            except (urllib.error.URLError, OSError) as e:
                result["radio"] = f"NOT given back to the scanner: {e}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", required=True, help="the scanner: a host, host:port or URL (port 8080; with SCANNER_TOKEN set, HTTPS on 8443)")
    ap.add_argument("--out", type=Path, help="where to save audio (default runs/analog/<host>/)")
    sub = ap.add_subparsers(dest="what", required=True)
    sub.add_parser("status", help="what the receiver is tuned to and hears")

    def tuning(p):
        p.add_argument("--mhz", type=float, help="the frequency, MHz")
        p.add_argument("--demod", choices=["wfm", "nfm", "am"], help="how it is demodulated")
        p.add_argument("--squelch", help="dB over the floor for audio to pass, or `off`")

    tuning(sub.add_parser("tune", help="tune the receiver"))
    ls = sub.add_parser("listen", help="record its audio")
    tuning(ls)
    ls.add_argument("--seconds", type=float, default=10.0)
    ls.add_argument("--take-radio", action="store_true", help="put the scanner in analog mode first, and give the radio back after")
    ls.add_argument("--silent", action="store_true", help="no audio is expected: the squelch stays shut")
    args = ap.parse_args()

    scanner_token.install(args.host)
    base = scanner_token.base(args.host)
    out_dir = args.out or REPO / "runs" / "analog" / urllib.parse.urlparse(base).hostname
    try:
        if args.what == "status":
            result, code = summary(call(base, "/api/v1/analog")), 0
        elif args.what == "tune":
            now = call(base, "/api/v1/analog")["tuned"]
            result, code = summary(call(base, "/api/v1/analog/tune", "PUT", tuned(args, now))), 0
        else:
            result, code = listen(base, args, out_dir)
    except urllib.error.HTTPError as e:
        result, code = {"error": refused(e)}, 3
    print(json.dumps({"host": base, "what": args.what, **result}, indent=1))
    return code


if __name__ == "__main__":
    sys.exit(main())
