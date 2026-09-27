"""Service factory: one object that hands out transports for each unit.

The CLI builds a real :class:`Services`; host tests substitute a fake with the
same methods, so no code path needs hardware to be exercised.
"""

from __future__ import annotations

import time
from datetime import datetime
from typing import Any, Sequence

from .agent import AgentClient
from .config import BenchConfig
from .transport import CmdResult, Http, Iio, Ssh, SshLike, ping, run_process


class Services:
    def __init__(self, cfg: BenchConfig) -> None:
        self.cfg = cfg
        self._ssh: dict[str, SshLike] = {}
        self._iio: dict[str, Any] = {}
        self._agent: AgentClient | None = None

    # -- transports ---------------------------------------------------------
    def ssh(self, unit: str) -> SshLike:
        if unit not in self._ssh:
            u = self.cfg.unit(unit)
            self._ssh[unit] = Ssh(
                unit=unit,
                host=u.host,
                user=u.ssh_user,
                identity=self.cfg.ssh.identity,
                known_hosts=self.cfg.known_hosts,
                connect_timeout=self.cfg.ssh.connect_timeout_s,
                card=self._card_serial(unit),
            )
        return self._ssh[unit]

    def _card_serial(self, unit: str) -> str | None:
        """Firmware serial of the SD card the unit booted (IIO ``hw_serial``).
        Best effort: any IIO failure falls back to the unit-only alias."""
        try:
            attrs = self.iio(unit).context_attrs(5.0)
        except Exception:
            return None
        serial = (attrs or {}).get("hw_serial")
        return str(serial) if serial else None

    def http(self, unit: str, timeout: float = 5.0) -> Http:
        u = self.cfg.unit(unit)
        return Http(u.host, u.http_port, timeout)

    def ws_audio(self, unit: str) -> Any:
        """A started ``/ws/audio`` recorder on the unit's p25-httpd."""
        from .wsaudio import WsAudioRecorder

        u = self.cfg.unit(unit)
        return WsAudioRecorder(u.host, u.http_port).start()

    def iio(self, unit: str) -> Any:
        if unit not in self._iio:
            u = self.cfg.unit(unit)
            uri = f"ip:{u.host}" if u.iiod_port == 30431 else f"ip:{u.host}:{u.iiod_port}"
            self._iio[unit] = Iio(uri, self.cfg.iio.backend)
        return self._iio[unit]

    @property
    def agent(self) -> AgentClient:
        if self._agent is None:
            self._agent = AgentClient(self.cfg, self.ssh)
        return self._agent

    # -- host utilities -----------------------------------------------------
    def ping(self, host: str, count: int = 1, timeout_s: float = 1.0) -> float | None:
        return ping(host, count, timeout_s)

    def run_local(self, argv: Sequence[str], timeout: float) -> CmdResult:
        return run_process(argv, timeout)

    def paramiko_exec(self, unit: str, password: str, commands: list[str],
                      timeout: float = 30.0) -> list[tuple[int, str, str]]:
        """Run commands with password auth (only for ``setup keys --password-*``)."""
        try:
            import paramiko  # type: ignore[import-not-found]
        except ImportError as exc:  # pragma: no cover - host dependent
            from .errors import PreconditionError

            raise PreconditionError("paramiko is required for password setup") from exc
        u = self.cfg.unit(unit)
        client = paramiko.SSHClient()
        client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
        client.connect(u.host, username=u.ssh_user, password=password, timeout=timeout,
                       allow_agent=False, look_for_keys=False)
        results = []
        try:
            for cmd in commands:
                _, stdout, stderr = client.exec_command(cmd, timeout=timeout)
                rc = stdout.channel.recv_exit_status()
                results.append((rc, stdout.read().decode(), stderr.read().decode()))
        finally:
            client.close()
        return results

    def tcp_send(self, host: str, port: int, mb: int, timeout: float = 60.0) -> dict[str, Any]:
        """Send ``mb`` MiB to ``host:port`` (host side of net.link)."""
        import socket

        chunk = b"\xa5" * (1 << 16)
        total = mb << 20
        sent = 0
        t0 = time.monotonic()
        with socket.create_connection((host, port), timeout=timeout) as sock:
            while sent < total:
                n = min(len(chunk), total - sent)
                sock.sendall(chunk[:n])
                sent += n
        dt = max(time.monotonic() - t0, 1e-9)
        return {"bytes": sent, "seconds": dt, "mbs": sent / dt / 1e6}

    # -- serial console ---------------------------------------------------------
    def list_serial_ports(self) -> list[Any]:
        from .console import _serial_mod

        return list(_serial_mod().tools.list_ports.comports())

    def serial_open(self, port: str, baud: int, timeout: float = 0.2) -> Any:
        from .console import _serial_mod

        return _serial_mod().Serial(port, baud, timeout=timeout)

    # -- time -----------------------------------------------------------------
    def sleep(self, seconds: float) -> None:
        if seconds > 0:
            time.sleep(seconds)

    def monotonic(self) -> float:
        return time.monotonic()

    def now(self) -> datetime:
        return datetime.now().astimezone()
