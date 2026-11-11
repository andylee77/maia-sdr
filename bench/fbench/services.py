"""Service factory: one object that hands out transports for each unit.

The CLI builds a real :class:`Services`; host tests substitute a fake with the
same methods, so no code path needs hardware to be exercised.
"""

from __future__ import annotations

import logging
import socket
import time
import urllib.error
from datetime import datetime
from typing import Any, Sequence

from .agent import AgentClient
from .config import BenchConfig
from .errors import TransportError, TransportTimeout
from .transport import CmdResult, Http, Iio, Ssh, SshLike, ping, run_process

log = logging.getLogger("fbench.services")

#: The unit's command that switches libiio's daemon on until the unit restarts, started apart
#: from the SSH session that asks: the USB gadget leaves the bus while it works, and a unit
#: reached over USB would take the session with it.
IIOD_ONCE = ("[ -x /usr/sbin/iio-switch ] && "
             "(setsid sh -c 'sleep 1; /usr/sbin/iio-switch once' > /dev/null 2>&1 < /dev/null &)")


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
        """A started ``/ws/audio`` recorder on the unit's scanner."""
        from .wsaudio import WsAudioRecorder

        u = self.cfg.unit(unit)
        return WsAudioRecorder(u.host, u.http_port).start()

    def iio(self, unit: str) -> Any:
        if unit not in self._iio:
            u = self.cfg.unit(unit)
            uri = f"ip:{u.host}" if u.iiod_port == 30431 else f"ip:{u.host}:{u.iiod_port}"
            self._iio[unit] = Iio(uri, self.cfg.iio.backend)
            self._iiod_on(unit)
        return self._iio[unit]

    def _port_open(self, host: str, port: int, timeout: float = 1.5) -> bool:
        try:
            with socket.create_connection((host, port), timeout=timeout):
                return True
        except OSError:
            return False

    def _ssh_by_any_card(self, unit: str) -> list[SshLike]:
        """SSH to the unit before the card in it is known. Each card has its own host key and so
        its own alias, and which card it is is read over libiio: one transport for each alias
        this bench has recorded for the unit, and the unit's bare alias last. Only the right
        card's connects; the others are refused for their key."""
        u = self.cfg.unit(unit)
        prefix = f"fbench-{unit}-"
        cards: list[str | None] = []
        try:
            for line in self.cfg.known_hosts.read_text(encoding="utf-8", errors="replace").splitlines():
                for alias in line.split(" ", 1)[0].split(","):
                    if alias.startswith(prefix) and alias[len(prefix):] not in cards:
                        cards.append(alias[len(prefix):])
        except OSError:
            pass
        return [Ssh(unit=unit, host=u.host, user=u.ssh_user, identity=self.cfg.ssh.identity,
                    known_hosts=self.cfg.known_hosts, connect_timeout=self.cfg.ssh.connect_timeout_s, card=card)
                for card in [*cards, None]]

    def _iiod_on(self, unit: str) -> bool:
        """A scanner image keeps libiio's daemon (iiod) off unless it is switched on. When its
        port is closed, have the unit switch it on until it restarts: through the scanner's API
        (an open unit, or an administrator's token in SCANNER_TOKEN), and with the scanner
        stopped or refusing, over SSH. Returns whether the port is open in the end. Best
        effort: a unit that cannot be asked fails where libiio is first used, as before."""
        u = self.cfg.unit(unit)
        if self._port_open(u.host, u.iiod_port):
            return True
        # asked: the unit took it. refused: it did not, or cannot be asked. unknown: no answer.
        outcome = "refused"
        try:
            self.http(unit, 15.0).put_json("/api/v1/access/iiod", {"on": True, "until_restart": True})
            outcome = "asked"
            log.info("%s: libiio's daemon was off: switched on until the unit restarts", unit)
        except TransportError as exc:
            if not isinstance(exc.__cause__, urllib.error.HTTPError):
                # No answer. The scanner is stopped; or it switched, and its answer was lost
                # with the link: a unit reached over its USB cable is off the bus meanwhile.
                outcome = "unknown"
            for ssh in self._ssh_by_any_card(unit):
                try:
                    rc, _, _ = ssh.run(IIOD_ONCE, 15.0)
                except (TransportError, TransportTimeout):
                    continue
                if rc == 0:
                    outcome = "asked"
                    log.info("%s: libiio's daemon was off: switched on over SSH until the unit restarts", unit)
                break
        if outcome == "refused":
            log.warning("%s: libiio's daemon (iiod) is off and could not be switched on from here: switch it on "
                        "in the radio's Settings, Access, or give the bench an administrator's token "
                        "(SCANNER_TOKEN)", unit)
            return False
        # A unit reached over its USB cable is away while its gadget returns to the bus.
        deadline = self.monotonic() + (20.0 if outcome == "asked" else 8.0)
        while self.monotonic() < deadline:
            if self._port_open(u.host, u.iiod_port):
                return True
            self.sleep(0.5)
        log.warning("%s: libiio's daemon did not come up after it was switched on", unit)
        return False

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
