#!/usr/bin/env python3
"""Launcher: ``python bench/fbench.py <verb> …`` from anywhere in the repo.

Equivalent to the ``fbench`` console script (``pip install -e bench``).
"""

from __future__ import annotations

import sys
from pathlib import Path

_BENCH = Path(__file__).resolve().parent
if str(_BENCH) not in sys.path:
    sys.path.insert(0, str(_BENCH))

from fbench.cli import main  # noqa: E402  (package dir shadows this module name)

if __name__ == "__main__":
    sys.exit(main())
