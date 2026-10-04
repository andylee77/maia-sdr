"""Transports: SSH/SCP (OpenSSH client), HTTP (the scanner's API) and libiio.

All calls take explicit timeouts and never prompt. ``Ssh`` uses BatchMode, a
per-unit ``HostKeyAlias`` and a bench-local known_hosts file, so units that
share an IP across reboots/images do not trip host-key checks. Dropbear on
the boards needs legacy SCP (``scp -O``).
"""

from __future__ import annotations

import json
import logging
import os
import shlex
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Protocol, Sequence

from .errors import PreconditionError, TransportError, TransportTimeout

log = logging.getLogger("fbench.transport")


@dataclass
class CmdResult:
    rc: int
    stdout: str
    stderr: str
    elapsed_s: float


def run_process(
    argv: Sequence[str],
    timeout: float,
    input_text: str | None = None,
    stdout_path: Path | None = None,
    stdin_path: Path | None = None,
) -> CmdResult:
    """Run ``argv`` with a hard timeout. Raises :class:`TransportTimeout`."""
    t0 = time.monotonic()
    log.debug("exec (%.0fs): %s", timeout, " ".join(shlex.quote(a) for a in argv))
    out_fh = open(stdout_path, "wb") if stdout_path else None
    in_fh = open(stdin_path, "rb") if stdin_path else None
    try:
        proc = subprocess.run(
            list(argv),
            input=None if in_fh else (input_text.encode() if input_text is not None else None),
            stdin=in_fh if in_fh else (None if input_text is not None else subprocess.DEVNULL),
            stdout=out_fh if out_fh else subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raise TransportTimeout(
            f"command timed out after {timeout:.0f} s: {argv[0]}", argv=list(argv)
        ) from exc
    except FileNotFoundError as exc:
        raise PreconditionError(f"executable not found: {argv[0]}") from exc
    finally:
        if out_fh:
            out_fh.close()
        if in_fh:
            in_fh.close()
    out = proc.stdout.decode("utf-8", "replace") if isinstance(proc.stdout, bytes) else ""
    err = proc.stderr.decode("utf-8", "replace") if isinstance(proc.stderr, bytes) else ""
    return CmdResult(proc.returncode, out, err, time.monotonic() - t0)


Runner = Callable[..., CmdResult]


class SshLike(Protocol):
    unit: str
    host: str

    def run(self, cmd: str, timeout: float) -> tuple[int, str, str]: ...

    def spawn(self, cmd: str) -> subprocess.Popen: ...

    def put(self, local: Path, remote: str, timeout: float) -> None: ...

    def get(self, remote: str, local: Path, timeout: float) -> None: ...


class Ssh:
    """OpenSSH client wrapper bound to one unit."""

    #: ssh exits 255 on connection/authentication failure.
    CONNECT_FAIL_RC = 255

    def __init__(
        self,
        unit: str,
        host: str,
        user: str,
        identity: str,
        known_hosts: Path,
        connect_timeout: int = 5,
        runner: Runner = run_process,
        card: str | None = None,
    ) -> None:
        self.unit = unit
        self.host = host
        self.user = user
        self.identity = str(Path(identity).expanduser())
        self.known_hosts = known_hosts
        self.connect_timeout = connect_timeout
        self._runner = runner
        self.card = card

    @property
    def host_key_alias(self) -> str:
        """Known-hosts alias. Each SD card image has its own dropbear host key,
        so the alias carries the card's firmware serial when it is known;
        swapping cards then selects a different known-hosts entry instead of
        tripping the host-key-changed check."""
        if not self.card:
            return f"fbench-{self.unit}"
        safe = "".join(ch for ch in self.card if ch.isalnum())[:40]
        return f"fbench-{self.unit}-{safe}"

    def options(self) -> list[str]:
        return [
            "-o", "BatchMode=yes",
            "-o", f"ConnectTimeout={self.connect_timeout}",
            "-o", f"HostKeyAlias={self.host_key_alias}",
            "-o", f"UserKnownHostsFile={self.known_hosts.as_posix()}",
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "ServerAliveInterval=5",
            "-o", "ServerAliveCountMax=3",
            "-i", self.identity,
        ]

    def ssh_argv(self, cmd: str) -> list[str]:
        return ["ssh", *self.options(), f"{self.user}@{self.host}", cmd]

    def _ensure_state(self) -> None:
        self.known_hosts.parent.mkdir(parents=True, exist_ok=True)

    def run(self, cmd: str, timeout: float) -> tuple[int, str, str]:
        """Run ``cmd`` in the remote shell. Raises on connect failure/timeout."""
        self._ensure_state()
        res = self._runner(self.ssh_argv(cmd), timeout)
        if res.rc == self.CONNECT_FAIL_RC:
            raise TransportError(
                f"unit {self.unit} ({self.host}) unreachable over SSH: {res.stderr.strip()[:300]}",
                unit=self.unit,
            )
        return res.rc, res.stdout, res.stderr

    def spawn(self, cmd: str) -> subprocess.Popen:
        """Start ``cmd`` in the background (caller must wait/kill)."""
        self._ensure_state()
        return subprocess.Popen(
            self.ssh_argv(cmd), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def _scp(self, src: str, dst: str, timeout: float) -> None:
        self._ensure_state()
        argv = ["scp", "-O", "-q", *self.options(), src, dst]
        res = self._runner(argv, timeout)
        if res.rc != 0:
            raise TransportError(
                f"scp failed ({res.rc}) for unit {self.unit}: {res.stderr.strip()[:300]}",
                unit=self.unit,
            )

    @staticmethod
    def _local(path: Path) -> str:
        """Native local path (Win32-OpenSSH scp recognises ``C:\…`` as local)."""
        return os.fspath(Path(path).resolve())

    def put(self, local: Path, remote: str, timeout: float) -> None:
        self._scp(self._local(local), f"{self.user}@{self.host}:{remote}", timeout)

    def put_stream(self, chunks: Any, remote: str, timeout: float) -> int:
        """Stream generated bytes into ``remote`` (``cat >``) without a local file.

        Returns the byte count. The remote file is complete only if this returns;
        callers upload to a temporary name and rename it afterwards.
        """
        self._ensure_state()
        proc = subprocess.Popen(self.ssh_argv(f"cat > {shlex.quote(remote)}"),
                                stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE)
        n = 0
        t0 = time.monotonic()
        try:
            assert proc.stdin is not None
            for block in chunks:
                if time.monotonic() - t0 > timeout:
                    raise TransportTimeout(f"upload to {self.unit}:{remote} timed out after "
                                           f"{timeout:.0f} s ({n >> 20} MiB sent)")
                proc.stdin.write(block)
                n += len(block)
            proc.stdin.close()
            rc = proc.wait(timeout=max(30.0, timeout - (time.monotonic() - t0)))
        except BrokenPipeError as exc:
            err = proc.stderr.read().decode("utf-8", "replace") if proc.stderr else ""
            raise TransportError(f"upload to {self.unit}:{remote} failed: {err.strip()[:300]}",
                                 unit=self.unit) from exc
        finally:
            if proc.poll() is None:
                proc.kill()
        if rc != 0:
            err = proc.stderr.read().decode("utf-8", "replace") if proc.stderr else ""
            raise TransportError(f"upload to {self.unit}:{remote} failed ({rc}): "
                                 f"{err.strip()[:300]}", unit=self.unit)
        return n

    def get(self, remote: str, local: Path, timeout: float) -> None:
        Path(local).parent.mkdir(parents=True, exist_ok=True)
        self._scp(f"{self.user}@{self.host}:{remote}", self._local(local), timeout)

    def reachable(self, timeout: float = 10.0) -> bool:
        try:
            rc, _, _ = self.run("true", timeout)
            return rc == 0
        except (TransportError, TransportTimeout):
            return False


class Http:
    """Minimal JSON-over-HTTP client for the scanner's API (scanner/doc/API.md)."""

    def __init__(self, host: str, port: int = 8080, timeout: float = 5.0) -> None:
        self.base = f"http://{host}:{port}"
        self.timeout = timeout

    def _url(self, path: str, params: dict[str, Any] | None) -> str:
        url = self.base + path
        if params:
            url += "?" + urllib.parse.urlencode(params)
        return url

    def _request(self, req: urllib.request.Request, timeout: float | None) -> Any:
        try:
            with urllib.request.urlopen(req, timeout=timeout or self.timeout) as resp:
                body = resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as exc:
            raise TransportError(f"HTTP {exc.code} for {req.full_url}") from exc
        except (urllib.error.URLError, OSError) as exc:
            raise TransportError(f"HTTP request failed for {req.full_url}: {exc}") from exc
        try:
            return json.loads(body)
        except json.JSONDecodeError as exc:
            raise TransportError(f"non-JSON reply from {req.full_url}") from exc

    def get_json(self, path: str, params: dict[str, Any] | None = None,
                 timeout: float | None = None) -> Any:
        return self._request(urllib.request.Request(self._url(path, params)), timeout)

    def post_json(self, path: str, params: dict[str, Any] | None = None,
                  body: Any = None, timeout: float | None = None) -> Any:
        data = json.dumps(body).encode() if body is not None else b""
        req = urllib.request.Request(
            self._url(path, params), data=data, method="POST",
            headers={"Content-Type": "application/json"},
        )
        return self._request(req, timeout)

    def put_json(self, path: str, body: Any, timeout: float | None = None) -> Any:
        req = urllib.request.Request(
            self._url(path, None), data=json.dumps(body).encode(), method="PUT",
            headers={"Content-Type": "application/json"},
        )
        return self._request(req, timeout)


# ---------------------------------------------------------------------------
# libiio
# ---------------------------------------------------------------------------


class TxHandle(Protocol):
    def stop(self) -> None: ...


def _parse_iio_attr_value(out: str) -> str:
    """Extract the value from ``iio_attr`` output (bare value or verbose form)."""
    text = out.strip()
    marker = "value '"
    if marker in text:
        start = text.rindex(marker) + len(marker)
        end = text.rindex("'")
        return text[start:end] if end >= start else text[start:]
    return text.splitlines()[-1].strip() if text else ""


def parse_context_attrs(out: str) -> dict[str, str]:
    """Parse ``iio_attr -C`` output (``key: value`` lines)."""
    attrs: dict[str, str] = {}
    for line in out.splitlines():
        line = line.strip()
        if not line or line.startswith("IIO context"):
            continue
        if ": " in line:
            key, value = line.split(": ", 1)
            attrs[key.strip()] = value.strip()
    return attrs


class _ProcTx:
    def __init__(self, proc: subprocess.Popen) -> None:
        self.proc = proc

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


class _PyTx:
    def __init__(self, ctx: Any, buf: Any) -> None:
        self.ctx = ctx
        self.buf = buf

    def stop(self) -> None:
        # Destroying the buffer stops the cyclic DMA.
        self.buf = None
        self.ctx = None


def _have_pylibiio() -> bool:
    try:
        import iio  # noqa: F401
    except Exception:  # pragma: no cover - depends on host
        return False
    return True


class Iio:
    """libiio access to one unit's iiod (``ip:<host>``).

    ``backend="auto"`` uses the ``iio_attr``/``iio_readdev``/``iio_writedev``
    command-line tools, which run in their own process with a timeout. The
    pylibiio 0.26 bindings crash the interpreter with an access violation on
    attribute reads under Python 3.14 (seen 2026-09-26), so they are used only
    when ``backend="pylibiio"`` is set explicitly.
    """

    def __init__(self, uri: str, backend: str = "auto", runner: Runner = run_process) -> None:
        self.uri = uri
        if backend == "auto":
            backend = "cli"
        self.backend = backend
        self._runner = runner

    # -- helpers ---------------------------------------------------------
    def _cli(self, args: list[str], timeout: float) -> str:
        res = self._runner(["iio_attr", "-u", self.uri, *args], timeout)
        if res.rc != 0:
            raise TransportError(f"iio_attr {' '.join(args)} failed: {res.stderr.strip()[:200]}")
        return res.stdout

    def _ctx(self, timeout: float) -> Any:
        import iio  # type: ignore[import-not-found]

        try:
            ctx = iio.Context(self.uri)
        except OSError as exc:
            raise TransportError(f"cannot open IIO context {self.uri}: {exc}") from exc
        ctx.set_timeout(int(timeout * 1000))
        return ctx

    @staticmethod
    def _find_attr(ctx: Any, dev: str, attr: str, chan: str | None, output: bool,
                   debug: bool) -> Any:
        device = ctx.find_device(dev)
        if device is None:
            raise PreconditionError(f"IIO device {dev!r} not found")
        if chan is not None:
            ch = device.find_channel(chan, output)
            if ch is None:
                raise PreconditionError(f"IIO channel {dev}/{chan} not found")
            attrs = ch.attrs
        else:
            attrs = device.debug_attrs if debug else device.attrs
        if attr not in attrs:
            raise PreconditionError(f"IIO attribute {dev}/{chan or ''}/{attr} not found")
        return attrs[attr]

    # -- API ---------------------------------------------------------------
    def context_attrs(self, timeout: float = 10.0) -> dict[str, str]:
        if self.backend == "pylibiio":
            ctx = self._ctx(timeout)
            return {str(k): str(v) for k, v in ctx.attrs.items()}
        return parse_context_attrs(self._cli(["-C"], timeout))

    def attr_get(self, dev: str, attr: str, chan: str | None = None, output: bool = False,
                 debug: bool = False, timeout: float = 10.0) -> str:
        if self.backend == "pylibiio":
            return str(self._find_attr(self._ctx(timeout), dev, attr, chan, output, debug).value)
        if chan is not None:
            args = ["-c", "-o" if output else "-i", dev, chan, attr]
        else:
            args = ["-D" if debug else "-d", dev, attr]
        return _parse_iio_attr_value(self._cli(args, timeout))

    def attr_set(self, dev: str, attr: str, value: str | float | int, chan: str | None = None,
                 output: bool = False, debug: bool = False, timeout: float = 10.0) -> None:
        if self.backend == "pylibiio":
            self._find_attr(self._ctx(timeout), dev, attr, chan, output, debug).value = str(value)
            return
        if chan is not None:
            args = ["-c", "-o" if output else "-i", dev, chan, attr, str(value)]
        else:
            args = ["-D" if debug else "-d", dev, attr, str(value)]
        self._cli(args, timeout)

    def capture(self, dev: str, channels: list[str], nsamples: int, out_path: Path,
                timeout: float = 30.0) -> Path:
        """Capture ``nsamples`` into one contiguous buffer (raw interleaved int16)."""
        out_path = Path(out_path)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        if self.backend == "pylibiio":
            import iio  # type: ignore[import-not-found]

            ctx = self._ctx(timeout)
            device = ctx.find_device(dev)
            if device is None:
                raise PreconditionError(f"IIO device {dev!r} not found")
            for name in channels:
                ch = device.find_channel(name, False)
                if ch is None:
                    raise PreconditionError(f"IIO channel {dev}/{name} not found")
                ch.enabled = True
            buf = iio.Buffer(device, nsamples)
            buf.refill()
            out_path.write_bytes(bytes(buf.read()))
            return out_path
        argv = ["iio_readdev", "-u", self.uri, "-b", str(nsamples), "-s", str(nsamples),
                dev, *channels]
        res = self._runner(argv, timeout, stdout_path=out_path)
        if res.rc != 0:
            raise TransportError(f"iio_readdev failed: {res.stderr.strip()[:200]}")
        return out_path

    def start_cyclic_tx(self, dev: str, channels: list[str], data_path: Path,
                        nsamples: int, timeout: float = 60.0) -> TxHandle:
        """Push one cyclic TX buffer (raw interleaved int16) and keep it looping."""
        if self.backend == "pylibiio":
            import iio  # type: ignore[import-not-found]

            ctx = self._ctx(timeout)
            device = ctx.find_device(dev)
            if device is None:
                raise PreconditionError(f"IIO device {dev!r} not found")
            for name in channels:
                ch = device.find_channel(name, True)
                if ch is None:
                    raise PreconditionError(f"IIO channel {dev}/{name} not found")
                ch.enabled = True
            buf = iio.Buffer(device, nsamples, True)
            buf.write(bytearray(Path(data_path).read_bytes()))
            buf.push()
            return _PyTx(ctx, buf)
        argv = ["iio_writedev", "-u", self.uri, "-c", "-b", str(nsamples), dev, *channels]
        fh = open(data_path, "rb")
        proc = subprocess.Popen(argv, stdin=fh, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        fh.close()
        return _ProcTx(proc)


def ping(host: str, count: int = 1, timeout_s: float = 1.0, runner: Runner = run_process,
         windows: bool | None = None) -> float | None:
    """ICMP ping via the OS tool; returns average RTT in ms or ``None``."""
    import platform

    if windows is None:
        windows = platform.system() == "Windows"
    if windows:
        argv = ["ping", "-n", str(count), "-w", str(int(timeout_s * 1000)), host]
    else:
        argv = ["ping", "-c", str(count), "-W", str(max(1, int(timeout_s))), host]
    try:
        res = runner(argv, timeout_s * count + 5)
    except (TransportTimeout, PreconditionError):
        return None
    if res.rc != 0:
        return None
    return parse_ping_rtt(res.stdout)


def parse_ping_rtt(text: str) -> float | None:
    """Average RTT (ms) from Windows or Linux/BusyBox ping output."""
    import re

    m = re.search(r"Average = (\d+)ms", text)
    if m:
        return float(m.group(1))
    m = re.search(r"= [\d.]+/([\d.]+)/[\d.]+", text)  # min/avg/max
    if m:
        return float(m.group(1))
    times = re.findall(r"time[=<]([\d.]+)\s*ms", text)
    if times:
        return sum(float(t) for t in times) / len(times)
    return None


def shell_join(args: Sequence[str]) -> str:
    return " ".join(shlex.quote(str(a)) for a in args)
