"""The real service factory's own logic: libiio's daemon switched on when it is found off."""

from __future__ import annotations

import urllib.error
from typing import Any

from fbench.config import BenchConfig
from fbench.errors import TransportError
from fbench.services import IIOD_ONCE, Services


class _Http:
    """`fail`: "refused" answers 403 (a listener's token), "lost" gets no answer."""

    def __init__(self, calls: list[Any], fail: str | None) -> None:
        self.calls, self.fail = calls, fail

    def put_json(self, path: str, body: Any, timeout: float | None = None) -> Any:
        self.calls.append(("http", path, body))
        if self.fail == "refused":
            raise TransportError("HTTP 403 for /api/v1/access/iiod") from urllib.error.HTTPError(path, 403, "Forbidden", None, None)  # type: ignore[arg-type]
        if self.fail == "lost":
            raise TransportError("HTTP request failed for /api/v1/access/iiod") from ConnectionResetError()
        return {"available": True, "wanted": False, "running": True}


class _Ssh:
    """`rc` None: the unit refuses this alias (another card's host key)."""

    def __init__(self, calls: list[Any], rc: int | None) -> None:
        self.calls, self.rc = calls, rc

    def run(self, cmd: str, timeout: float) -> tuple[int, str, str]:
        self.calls.append(("ssh", cmd))
        if self.rc is None:
            raise TransportError("unit unreachable over SSH: REMOTE HOST IDENTIFICATION HAS CHANGED")
        return self.rc, "", ""


class _Services(Services):
    """The factory with its network stubbed: `opens_after` port probes fail before one succeeds
    once the unit was asked; before that the port is as `open_at_first` says."""

    def __init__(self, cfg: BenchConfig, open_at_first: bool, http_fails: str | None = None,
                 ssh_rc: tuple[int | None, ...] = (0,), opens_after: int | None = 2) -> None:
        super().__init__(cfg)
        self.calls: list[Any] = []
        self.open_at_first, self.http_fails, self.ssh_rc = open_at_first, http_fails, ssh_rc
        self.opens_after = opens_after
        self.probes = 0
        self.clock = 0.0

    def _port_open(self, host: str, port: int, timeout: float = 1.5) -> bool:
        self.probes += 1
        if self.probes == 1:
            return self.open_at_first
        return self.opens_after is not None and self.probes > self.opens_after

    def http(self, unit: str, timeout: float = 5.0) -> Any:
        return _Http(self.calls, self.http_fails)

    def _ssh_by_any_card(self, unit: str) -> Any:
        return [_Ssh(self.calls, rc) for rc in self.ssh_rc]

    def sleep(self, seconds: float) -> None:
        self.clock += seconds

    def monotonic(self) -> float:
        self.clock += 0.01
        return self.clock


def test_a_daemon_that_answers_is_left_alone(cfg: BenchConfig) -> None:
    s = _Services(cfg, open_at_first=True)
    s.iio("A")
    assert s.calls == [] and s.probes == 1


def test_a_daemon_found_off_is_switched_on_through_the_scanner_until_the_restart(cfg: BenchConfig) -> None:
    s = _Services(cfg, open_at_first=False)
    assert s._iiod_on("A") is True
    assert s.calls == [("http", "/api/v1/access/iiod", {"on": True, "until_restart": True})]
    # The gadget is back on the bus a moment later: the port was asked for until it opened.
    assert s.probes == 3


def test_with_the_scanner_stopped_or_refusing_the_unit_is_asked_over_ssh(cfg: BenchConfig) -> None:
    s = _Services(cfg, open_at_first=False, http_fails="refused")
    assert s._iiod_on("B") is True
    assert [c[0] for c in s.calls] == ["http", "ssh"] and s.calls[1][1] == IIOD_ONCE
    # The switch works apart from the session that asks for it, which a USB link would drop.
    assert "setsid" in IIOD_ONCE and "iio-switch once" in IIOD_ONCE


def test_the_card_in_the_unit_is_not_known_yet_so_each_alias_recorded_is_tried(cfg: BenchConfig) -> None:
    # Two cards' keys are on record: the first is not the card in the unit.
    s = _Services(cfg, open_at_first=False, http_fails="refused", ssh_rc=(None, 0, 0))
    assert s._iiod_on("A") is True
    assert [c[0] for c in s.calls] == ["http", "ssh", "ssh"], "the third is not tried once one connected"
    # The aliases come from the bench's own record, the unit's bare one last.
    cfg.known_hosts.parent.mkdir(parents=True, exist_ok=True)
    cfg.known_hosts.write_text("fbench-A-CARD1 ssh-ed25519 AAAA\nfbench-B-OTHER ssh-ed25519 BBBB\n"
                               "fbench-A-CARD2,192.168.2.1 ssh-ed25519 CCCC\nfbench-A-CARD1 ssh-rsa DDDD\n", encoding="utf-8")
    assert [x.host_key_alias for x in Services(cfg)._ssh_by_any_card("A")] == ["fbench-A-CARD1", "fbench-A-CARD2", "fbench-A"]
    cfg.known_hosts.unlink()
    assert [x.host_key_alias for x in Services(cfg)._ssh_by_any_card("A")] == ["fbench-A"]


def test_a_unit_without_the_switch_is_not_waited_for(cfg: BenchConfig) -> None:
    s = _Services(cfg, open_at_first=False, http_fails="refused", ssh_rc=(1,))
    assert s._iiod_on("A") is False
    assert s.probes == 1 and s.clock < 1.0


def test_an_answer_lost_with_the_link_the_switch_took_down_is_not_a_failure(cfg: BenchConfig) -> None:
    # Over the unit's USB cable: the scanner switched, the gadget left the bus, and neither the
    # answer nor SSH got through; the port opens when the gadget is back.
    s = _Services(cfg, open_at_first=False, http_fails="lost", ssh_rc=(None,), opens_after=6)
    assert s._iiod_on("A") is True
    assert [c[0] for c in s.calls] == ["http", "ssh"]
    # A unit that is simply not there is given up sooner than one that said it would switch.
    gone = _Services(cfg, open_at_first=False, http_fails="lost", ssh_rc=(None,), opens_after=None)
    assert gone._iiod_on("A") is False
    assert 8.0 <= gone.clock < 12.0


def test_a_daemon_that_does_not_come_up_is_given_up_after_a_while(cfg: BenchConfig) -> None:
    s = _Services(cfg, open_at_first=False, opens_after=None)
    assert s._iiod_on("A") is False
    assert 20.0 <= s.clock < 25.0


def test_the_daemon_is_looked_at_once_a_unit(cfg: BenchConfig) -> None:
    s = _Services(cfg, open_at_first=True)
    assert s.iio("A") is s.iio("A")
    assert s.probes == 1
