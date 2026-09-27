"""fbench — host-side bench CLI for the Fishball hardware validation suite.

Contract: doc/HW_VALIDATION_SUITE.md (sections 3, 5, 9). Every verb prints one
JSON object with ``--json`` and exits with a deterministic code (see
:mod:`fbench.errors`).
"""

from __future__ import annotations

from pathlib import Path

__version__ = "0.1.0"

#: ``bench/`` directory (parent of this package).
BENCH_DIR: Path = Path(__file__).resolve().parent.parent
#: Repository root (parent of ``bench/``).
REPO_ROOT: Path = BENCH_DIR.parent
