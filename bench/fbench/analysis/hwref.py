"""Guarded access to the hwval HDL Python references (``scanner-hdl/hwval_hdl``).

The references are the single source of truth for pattern generators and
checker constants (``pattern.prbs31_words``/``rate_inc``, ``ring_v2``
``PAD_MAGIC``/``HDR_MAGIC``/``ring_marker``, ``ingest.ad9361_bist_prbs``/
``pn9_pn11``, ``axi_memtest.expected_beat``…). They import amaranth, so they
load only where the HDL toolchain is installed (``.venv-hdl``); callers must
handle ``None`` and degrade to "not cross-checked".
"""

from __future__ import annotations

import importlib
import sys
from typing import Any

from .. import REPO_ROOT

_CACHE: dict[str, Any] = {}


def load(name: str) -> Any:
    """``hwval_hdl.<name>`` or ``None`` when it cannot be imported."""
    if name not in _CACHE:
        # hwval_hdl lives in scanner-hdl and imports maia_hdl and radio_core.
        for hdl in (str(REPO_ROOT / "scanner-hdl"), str(REPO_ROOT / "maia-hdl")):
            if hdl not in sys.path:
                sys.path.append(hdl)
        try:
            _CACHE[name] = importlib.import_module(f"hwval_hdl.{name}")
        except Exception:  # noqa: BLE001 - amaranth missing, module not written yet
            _CACHE[name] = None
    return _CACHE[name]


def rate_inc(rate_hz: float, clk_hz: float = 62.5e6) -> int | None:
    """LEGACY_RATE_INC / RINGV2_RATE_INC for ``rate_hz`` (None without the reference)."""
    mod = load("pattern")
    return None if mod is None else int(mod.rate_inc(rate_hz, clk_hz))
