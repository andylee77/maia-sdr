"""Session state persisted in ``bench/.state/session.json``.

Tracks what the host believes about each unit between invocations:
maintenance mode (the scanner stopped), TX activity, last seen image. A crash
between ``maint enter`` and ``maint exit`` leaves the flag set so
``fbench status`` shows it.
"""

from __future__ import annotations

import json
import os
from datetime import datetime
from pathlib import Path
from typing import Any


def _now() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


class SessionState:
    def __init__(self, state_dir: Path) -> None:
        self.path = Path(state_dir) / "session.json"

    def load(self) -> dict[str, Any]:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
            return data if isinstance(data, dict) else {"units": {}}
        except (OSError, json.JSONDecodeError):
            return {"units": {}}

    def save(self, data: dict[str, Any]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")
        os.replace(tmp, self.path)

    def update_unit(self, unit: str, **fields: Any) -> dict[str, Any]:
        data = self.load()
        entry = data.setdefault("units", {}).setdefault(unit, {})
        entry.update(fields)
        entry["updated"] = _now()
        self.save(data)
        return entry

    def set_maintenance(self, unit: str, active: bool, run_id: str | None = None) -> None:
        self.update_unit(unit, maintenance=active, maintenance_run=run_id if active else None,
                         maintenance_since=_now() if active else None)

    def set_tx(self, unit: str, active: bool, run_id: str | None = None) -> None:
        self.update_unit(unit, tx_active=active, tx_run=run_id if active else None,
                         tx_since=_now() if active else None)

    def set_image(self, unit: str, image: str) -> None:
        self.update_unit(unit, last_image=image, last_seen=_now())

    def set_last_run(self, run: dict[str, Any]) -> None:
        data = self.load()
        data["last_run"] = run
        self.save(data)
