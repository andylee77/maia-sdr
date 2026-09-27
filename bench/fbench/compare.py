"""``fbench compare``: metric deltas between runs (regression gate).

The first run is the baseline. A later run *regresses* when its verdict is
worse (pass -> fail/error) or when it violates a threshold the baseline met.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .errors import UsageError
from .runner import load_result

_RANK = {"pass": 0, "inconclusive": 1, "precondition": 2, "refused": 2, "fail": 3, "error": 3}


def _violates(value: Any, th: dict[str, Any]) -> bool:
    if value is None or th.get("severity") == "warn":
        return False
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return "eq" in th and value != th["eq"]
    if "min" in th and value < th["min"]:
        return True
    if "max" in th and value > th["max"]:
        return True
    return "eq" in th and value != th["eq"]


def compare(run_dirs: list[Path]) -> tuple[dict[str, Any], int]:
    if len(run_dirs) < 2:
        raise UsageError("compare needs at least two run dirs")
    results = [(Path(d), load_result(Path(d))) for d in run_dirs]
    base_dir, base = results[0]
    tests = {r["test"] for _, r in results}
    warnings = []
    if len(tests) > 1:
        warnings.append(f"runs are of different tests: {sorted(tests)}")
    metrics: dict[str, list[dict[str, Any]]] = {}
    for name, bval in (base.get("metrics") or {}).items():
        if not isinstance(bval, (int, float)) or isinstance(bval, bool):
            continue
        rows = []
        for d, r in results:
            v = (r.get("metrics") or {}).get(name)
            row: dict[str, Any] = {"run_id": r["run_id"], "value": v}
            if isinstance(v, (int, float)) and not isinstance(v, bool):
                row["delta"] = v - bval
                row["pct"] = (v - bval) / abs(bval) * 100 if bval else None
            rows.append(row)
        metrics[name] = rows
    regressions = []
    for d, r in results[1:]:
        if _RANK.get(r["verdict"], 3) > _RANK.get(base["verdict"], 3):
            regressions.append({"run_id": r["run_id"], "kind": "verdict",
                                "detail": f"{base['verdict']} -> {r['verdict']}"})
        for name, th in (r.get("thresholds") or {}).items():
            now = (r.get("metrics") or {}).get(name)
            before = (base.get("metrics") or {}).get(name)
            if _violates(now, th) and not _violates(before, th):
                regressions.append({"run_id": r["run_id"], "kind": "threshold", "metric": name,
                                    "detail": f"{before} -> {now} (threshold {th})"})
    report = {
        "baseline": {"run_id": base["run_id"], "run_dir": base_dir.as_posix(),
                     "verdict": base["verdict"]},
        "runs": [{"run_id": r["run_id"], "run_dir": d.as_posix(), "test": r["test"],
                  "verdict": r["verdict"]} for d, r in results],
        "metrics": metrics,
        "regressions": regressions,
        "warnings": warnings,
    }
    return report, (1 if regressions else 0)
