"""FT2232 DEBUG-port UART console (Zynq UART1, MIO 8/9, 115200 8N1).

Used by ``fbench console`` and the ``sys.boot_log`` test. pyserial is
optional: without it these features raise a clear precondition error.
Every received line is written as ``<host ISO timestamp>\\t<text>``.
"""

from __future__ import annotations

import re
import time
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, TextIO

from .errors import PreconditionError

FTDI_VID = 0x0403


def _serial_mod() -> Any:
    try:
        import serial  # type: ignore[import-not-found]
        import serial.tools.list_ports  # type: ignore[import-not-found]  # noqa: F401
    except ImportError as exc:
        raise PreconditionError("pyserial is not installed (pip install pyserial) — needed for "
                                "the UART console") from exc
    return serial


def list_ftdi_ports(lister: Callable[[], list[Any]] | None = None) -> list[dict[str, Any]]:
    """FTDI serial ports (VID 0x0403) as dicts."""
    if lister is None:
        lister = _serial_mod().tools.list_ports.comports
    out = []
    for p in lister():
        if getattr(p, "vid", None) == FTDI_VID:
            out.append({"device": p.device, "description": getattr(p, "description", ""),
                        "serial_number": getattr(p, "serial_number", None),
                        "location": getattr(p, "location", None),
                        "interface": getattr(p, "interface", None)})
    return out


def resolve_port(explicit: str | None, configured: str | None,
                 lister: Callable[[], list[Any]] | None = None) -> str:
    """Port choice: explicit > config > the single FTDI port (else list them)."""
    if explicit:
        return explicit
    if configured:
        return configured
    ports = list_ftdi_ports(lister)
    if len(ports) == 1:
        return str(ports[0]["device"])
    if not ports:
        raise PreconditionError("no FTDI (VID 0x0403) serial port found; connect the DEBUG "
                                "USB port or pass --port COMx")
    raise PreconditionError("several FTDI ports found; pass --port (FT2232 channel B is "
                            "usually the UART): " +
                            ", ".join(f"{p['device']} ({p['description']})" for p in ports),
                            ports=ports)


def capture(port: str, baud: int, seconds: float, out_path: Path, until: str | None = None,
            send: str | None = None, echo: TextIO | None = None,
            serial_factory: Callable[..., Any] | None = None,
            clock: Callable[[], float] = time.monotonic) -> dict[str, Any]:
    """Read lines for ``seconds`` (or until ``until`` matches) into ``out_path``."""
    if serial_factory is None:
        serial_factory = _serial_mod().Serial
    pattern = re.compile(until) if until else None
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        ser = serial_factory(port, baud, timeout=0.2)
    except Exception as exc:  # noqa: BLE001 - SerialException etc.
        raise PreconditionError(f"cannot open {port}: {exc}") from exc
    lines = 0
    matched = None
    pending = b""
    deadline = clock() + seconds
    try:
        if send:
            ser.write((send.rstrip("\n") + "\n").encode())
        with open(out_path, "w", encoding="utf-8", newline="\n") as fh:
            while clock() < deadline:
                chunk = ser.readline()
                if not chunk:
                    continue
                pending += chunk
                if not pending.endswith(b"\n"):
                    continue
                text = pending.decode("utf-8", "replace").rstrip("\r\n")
                pending = b""
                stamp = datetime.now().astimezone().isoformat(timespec="milliseconds")
                fh.write(f"{stamp}\t{text}\n")
                fh.flush()
                lines += 1
                if echo is not None:
                    echo.write(text + "\n")
                if pattern is not None and pattern.search(text):
                    matched = text
                    break
            if pending:
                stamp = datetime.now().astimezone().isoformat(timespec="milliseconds")
                fh.write(f"{stamp}\t{pending.decode('utf-8', 'replace').rstrip()}\n")
                lines += 1
    finally:
        try:
            ser.close()
        except Exception:  # noqa: BLE001
            pass
    return {"port": port, "baud": baud, "lines": lines, "matched": matched,
            "path": out_path.as_posix()}
