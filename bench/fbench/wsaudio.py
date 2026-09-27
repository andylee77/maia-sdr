"""Minimal ``/ws/audio`` recorder (RFC 6455 client on the standard library).

p25-httpd sends 320-byte binary frames (160 int16 samples = 20 ms at 8 kHz) and
text control frames such as ``{"type":"lag","skipped":N}``. The recorder runs in
a thread and keeps every frame with its host arrival time.
"""

from __future__ import annotations

import base64
import json
import os
import socket
import struct
import threading
import time
from typing import Any


class WsAudioRecorder:
    def __init__(self, host: str, port: int = 8080, path: str = "/ws/audio",
                 connect_timeout: float = 5.0) -> None:
        self.host, self.port, self.path = host, port, path
        self.connect_timeout = connect_timeout
        self.chunks: list[tuple[float, bytes]] = []
        self.texts: list[tuple[float, str]] = []
        self.error: str | None = None
        self._stop = threading.Event()
        self._sock: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()

    # -- lifecycle --------------------------------------------------------------
    def start(self) -> "WsAudioRecorder":
        s = socket.create_connection((self.host, self.port), timeout=self.connect_timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (f"GET {self.path} HTTP/1.1\r\nHost: {self.host}:{self.port}\r\n"
               "Upgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        s.sendall(req.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            part = s.recv(1024)
            if not part:
                raise OSError("websocket handshake: connection closed")
            head += part
        status = head.split(b"\r\n", 1)[0]
        if b" 101" not in status:
            raise OSError(f"websocket handshake refused: {status.decode(errors='replace')}")
        self._buf = head.split(b"\r\n\r\n", 1)[1]
        s.settimeout(0.5)
        self._sock = s
        self._thread = threading.Thread(target=self._run, name="ws-audio", daemon=True)
        self._thread.start()
        return self

    def stop(self) -> dict[str, Any]:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=3.0)
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
        return self.snapshot()

    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            return {"chunks": list(self.chunks), "texts": list(self.texts), "error": self.error}

    # -- frames -------------------------------------------------------------------
    def _recv_exact(self, n: int) -> bytes:
        assert self._sock is not None
        while len(self._buf) < n:
            if self._stop.is_set():
                raise EOFError
            try:
                part = self._sock.recv(65536)
            except socket.timeout:
                continue
            if not part:
                raise EOFError
            self._buf += part
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _send(self, opcode: int, payload: bytes = b"") -> None:
        assert self._sock is not None
        mask = os.urandom(4)
        n = len(payload)
        hdr = bytes([0x80 | opcode])
        if n < 126:
            hdr += bytes([0x80 | n])
        elif n < 65536:
            hdr += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            hdr += bytes([0x80 | 127]) + struct.pack(">Q", n)
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self._sock.sendall(hdr + mask + body)

    def _run(self) -> None:
        try:
            while not self._stop.is_set():
                b0, b1 = self._recv_exact(2)
                op, n = b0 & 0x0F, b1 & 0x7F
                if n == 126:
                    n = struct.unpack(">H", self._recv_exact(2))[0]
                elif n == 127:
                    n = struct.unpack(">Q", self._recv_exact(8))[0]
                mask = self._recv_exact(4) if b1 & 0x80 else b""
                data = self._recv_exact(n)
                if mask:
                    data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
                t = time.time()
                if op == 0x2:
                    with self._lock:
                        self.chunks.append((t, data))
                elif op == 0x1:
                    with self._lock:
                        self.texts.append((t, data.decode("utf-8", "replace")))
                elif op == 0x9:
                    self._send(0xA, data)
                elif op == 0x8:
                    break
        except EOFError:
            pass
        except OSError as exc:
            if not self._stop.is_set():
                self.error = str(exc)
        try:
            if self._sock is not None and not self._stop.is_set():
                self._send(0x8)
        except OSError:
            pass


def lag_events(texts: list[tuple[float, str]]) -> list[dict[str, Any]]:
    out = []
    for t, s in texts:
        try:
            d = json.loads(s)
        except ValueError:
            continue
        if isinstance(d, dict) and d.get("type") == "lag":
            out.append({"t": t, "skipped": d.get("skipped")})
    return out
