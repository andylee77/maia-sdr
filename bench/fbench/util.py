"""Small helpers shared by the CLI, runner and tests."""

from __future__ import annotations

import json
import math
from datetime import datetime
from pathlib import Path
from typing import Any


def jsonable(obj: Any) -> Any:
    """Convert numpy scalars/arrays, Paths and dataclass-like objects for JSON."""
    try:
        import numpy as np
    except ImportError:  # pragma: no cover
        np = None  # type: ignore[assignment]
    if isinstance(obj, dict):
        return {str(k): jsonable(v) for k, v in obj.items()}
    if isinstance(obj, (list, tuple, set)):
        return [jsonable(v) for v in obj]
    if isinstance(obj, Path):
        return obj.as_posix()
    if isinstance(obj, datetime):
        return obj.isoformat(timespec="seconds")
    if np is not None:
        if isinstance(obj, np.ndarray):
            return jsonable(obj.tolist())
        if isinstance(obj, np.bool_):
            return bool(obj)
        if isinstance(obj, np.integer):
            return int(obj)
        if isinstance(obj, np.floating):
            obj = float(obj)
    if isinstance(obj, float) and (math.isnan(obj) or math.isinf(obj)):
        return None
    if hasattr(obj, "to_dict"):
        return jsonable(obj.to_dict())
    return obj


def dump_json(obj: Any, path: Path | None = None, indent: int = 2) -> str:
    text = json.dumps(jsonable(obj), indent=indent, sort_keys=False)
    if path is not None:
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        Path(path).write_text(text + "\n", encoding="utf-8")
    return text


def parse_value(raw: str) -> Any:
    """Parse a ``-p key=value`` value: JSON when possible, else the string."""
    text = raw.strip()
    low = text.lower()
    if low in ("true", "yes", "on"):
        return True
    if low in ("false", "no", "off"):
        return False
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        pass
    if "," in text and not text.startswith(("{", "[")):
        return [parse_value(part) for part in text.split(",")]
    return text


def fmt_hz(hz: float | None) -> str:
    if hz is None:
        return "-"
    for unit, div in (("GHz", 1e9), ("MHz", 1e6), ("kHz", 1e3)):
        if abs(hz) >= div:
            return f"{hz / div:.6g} {unit}"
    return f"{hz:.6g} Hz"
