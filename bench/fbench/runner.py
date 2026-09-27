"""Test registry, test contexts, run directories and suites.

A test is a function ``fn(ctx: TestContext) -> Outcome`` registered with
:func:`bench_test`. Tests acquire raw data into ``ctx.artifacts_dir`` and
then call an analysis function ``analyze(actx: AnalysisContext) -> Outcome``
that only reads artifacts and params, so ``fbench analyze <run_dir>`` can
re-run it offline.

The runner owns the safety-relevant sequencing:

1. TX tests: interlock check (exit 4 before anything touches hardware).
2. Maintenance tests: ``maint enter`` on every involved unit.
3. The test body.
4. ``finally``: ``tx off`` on the TX unit (always, even on exceptions), then
   ``maint exit`` (LIFO), then result.json + FINDINGS.md.
"""

from __future__ import annotations

import copy
import json
import logging
import traceback
from contextlib import ExitStack, contextmanager
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Iterator

from .config import BenchConfig, UnitConfig
from .errors import (
    VERDICT_EXIT,
    VERDICTS,
    AgentUnsupported,
    FbenchError,
    Inconclusive,
    PreconditionError,
    SafetyRefusal,
    TransportError,
    UsageError,
    worst_exit,
)
from .safety import LinkBudget, min_atten, require_tx_allowed
from .state import SessionState
from .units import identity_snapshot
from .util import dump_json, jsonable, parse_value

log = logging.getLogger("fbench.runner")

RESULT_SCHEMA = "fbench.result/1"
RESULT_KEYS = ("schema", "test", "run_id", "started", "ended", "verdict", "summary", "units",
               "params", "metrics", "thresholds", "maintenance_mode", "tx_used", "artifacts",
               "warnings", "errors")
UNIT_KEYS = ("serial", "image", "build", "transceiver", "label")
UNIT_MODES = ("any", "tx,rx", "A,B")

# ---------------------------------------------------------------------------
# Registry
# ---------------------------------------------------------------------------


@dataclass
class Outcome:
    summary: str
    verdict: str | None = None  # None -> derived from thresholds


@dataclass
class TestSpec:
    __test__ = False  # not a pytest class

    id: str
    tier: int
    units: str
    maintenance: bool
    tx: bool
    params: dict[str, Any]
    description: str
    pass_criteria: str
    func: Callable[["TestContext"], Outcome]
    analyze: Callable[["AnalysisContext"], Outcome] | None = None
    artifacts: tuple[str, ...] = ()
    suites: tuple[str, ...] = ()
    requires: tuple[str, ...] = ()
    duration_param: str | None = None
    tx_atten_param: str = "tx_atten_db"

    def describe(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "tier": self.tier,
            "units": self.units,
            "maintenance": self.maintenance,
            "tx": self.tx,
            "description": self.description,
            "pass_criteria": self.pass_criteria,
            "params": jsonable(self.params),
            "duration_param": self.duration_param,
            "requires": list(self.requires),
            "artifacts": list(self.artifacts),
            "suites": list(self.suites),
            "reanalyzable": self.analyze is not None,
        }


REGISTRY: dict[str, TestSpec] = {}

#: Explicit suites (ordered, with per-entry parameter overrides). Other suites
#: are collected from the ``suites=`` of each test in registration order.
SUITE_OVERRIDES: dict[str, list[tuple[str, dict[str, Any]]]] = {
    "smoke": [("sys.identity", {}), ("sys.audit", {}), ("sys.telemetry", {"seconds": 10}),
              ("iface.clk_freq", {})],
}
SUITE_NAMES = ("smoke", "interface", "transport", "memory", "rf", "hwval", "soak")


def bench_test(id: str, tier: int, units: str = "any", maintenance: bool = False,
               tx: bool = False, params: dict[str, Any] | None = None, description: str = "",
               pass_criteria: str = "", artifacts: tuple[str, ...] = (),
               suites: tuple[str, ...] = (), requires: tuple[str, ...] = (),
               analyze: Callable[["AnalysisContext"], Outcome] | None = None,
               duration_param: str | None = None,
               tx_atten_param: str = "tx_atten_db") -> Callable:
    """Register a test function (see module docstring)."""
    if units not in UNIT_MODES:
        raise ValueError(f"units must be one of {UNIT_MODES}")

    def deco(fn: Callable[["TestContext"], Outcome]) -> Callable[["TestContext"], Outcome]:
        if id in REGISTRY:
            raise ValueError(f"duplicate test id {id}")
        REGISTRY[id] = TestSpec(
            id=id, tier=tier, units=units, maintenance=maintenance, tx=tx,
            params=dict(params or {}), description=description, pass_criteria=pass_criteria,
            func=fn, analyze=analyze, artifacts=tuple(artifacts), suites=tuple(suites),
            requires=tuple(requires), duration_param=duration_param,
            tx_atten_param=tx_atten_param,
        )
        return fn

    return deco


def load_tests() -> dict[str, TestSpec]:
    """Import all test modules (idempotent) and return the registry."""
    from . import tests  # noqa: F401  (registers on import)

    return REGISTRY


def get_spec(test_id: str) -> TestSpec:
    load_tests()
    if test_id not in REGISTRY:
        raise UsageError(f"unknown test {test_id!r}; see `fbench list`")
    return REGISTRY[test_id]


def suites() -> dict[str, list[tuple[str, dict[str, Any]]]]:
    load_tests()
    out: dict[str, list[tuple[str, dict[str, Any]]]] = {}
    for name in SUITE_NAMES:
        if name in SUITE_OVERRIDES:
            out[name] = list(SUITE_OVERRIDES[name])
        else:
            out[name] = [(t.id, {}) for t in REGISTRY.values() if name in t.suites]
    return out


def expand(target: str) -> tuple[str | None, list[tuple[TestSpec, dict[str, Any]]]]:
    """``target`` is a test id or a suite name -> (suite or None, [(spec, overrides)])."""
    load_tests()
    if target in REGISTRY:
        return None, [(REGISTRY[target], {})]
    all_suites = suites()
    if target in all_suites:
        return target, [(REGISTRY[tid], dict(ov)) for tid, ov in all_suites[target]]
    raise UsageError(f"unknown test or suite {target!r}; suites: {', '.join(SUITE_NAMES)}")


# ---------------------------------------------------------------------------
# Parameters and roles
# ---------------------------------------------------------------------------


def coerce(value: Any, default: Any, key: str) -> Any:
    """Coerce a parsed override to the type of the default value."""
    if isinstance(value, str):
        value = parse_value(value)
    try:
        if isinstance(default, bool):
            if isinstance(value, bool):
                return value
            if value in (0, 1):
                return bool(value)
            raise ValueError
        if isinstance(default, int) and not isinstance(default, bool):
            if isinstance(value, bool):
                raise ValueError
            if isinstance(value, (int, float)) and float(value).is_integer():
                return int(value)
            raise ValueError
        if isinstance(default, float):
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                raise ValueError
            return float(value)
        if isinstance(default, list):
            items = value if isinstance(value, list) else [value]
            if default:
                return [coerce(v, default[0], key) for v in items]
            return items
        if isinstance(default, str):
            return value if isinstance(value, str) else json.dumps(value) \
                if isinstance(value, (list, dict)) else str(value)
    except ValueError:
        raise UsageError(f"parameter {key}: cannot use {value!r} (expected "
                         f"{type(default).__name__})") from None
    return value


def build_params(spec: TestSpec, overrides: dict[str, Any], duration: float | None = None
                 ) -> dict[str, Any]:
    params = copy.deepcopy(spec.params)
    for key, raw in overrides.items():
        if key not in params:
            raise UsageError(f"unknown parameter {key!r} for {spec.id}; known: "
                             f"{', '.join(sorted(params)) or '(none)'}")
        params[key] = coerce(raw, params[key], key)
    if duration is not None:
        if spec.duration_param is None:
            raise UsageError(f"{spec.id} has no duration parameter; use -p instead")
        params[spec.duration_param] = coerce(duration, params[spec.duration_param],
                                             spec.duration_param)
    return params


def resolve_roles(spec: TestSpec, cfg: BenchConfig, unit: str | None = None,
                  tx: str | None = None, rx: str | None = None) -> list[dict[str, str]]:
    """Unit assignments for one test (a list: ``--unit A,B`` runs "any" tests twice)."""
    if spec.units == "any":
        names = [u.strip() for u in unit.split(",")] if unit else [cfg.default_unit]
        for n in names:
            cfg.unit(n)
        return [{"dut": n} for n in names]
    if spec.units == "tx,rx":
        roles = {"tx": tx or cfg.rf.default_tx, "rx": rx or cfg.rf.default_rx}
        for n in roles.values():
            cfg.unit(n)
        return [roles]
    names = [u.strip() for u in unit.split(",")] if unit and "," in unit else list(cfg.units)[:2]
    if len(names) != 2 or names[0] == names[1]:
        raise UsageError(f"{spec.id} needs two distinct units (e.g. --unit A,B)")
    for n in names:
        cfg.unit(n)
    return [{"a": names[0], "b": names[1]}]


# ---------------------------------------------------------------------------
# Contexts
# ---------------------------------------------------------------------------


class AnalysisContext:
    """Params, artifacts and result bookkeeping (no hardware access)."""

    def __init__(self, spec: TestSpec, params: dict[str, Any], run_dir: Path,
                 roles: dict[str, str] | None = None,
                 units_meta: dict[str, dict[str, Any]] | None = None,
                 logger: logging.Logger | None = None) -> None:
        self.spec = spec
        self.params = params
        self.run_dir = Path(run_dir)
        self.artifacts_dir = self.run_dir / "artifacts"
        self.roles = dict(roles or {})
        self.units_meta = dict(units_meta or {})
        self.log = logger or log
        self.metrics: dict[str, Any] = {}
        self.thresholds: dict[str, dict[str, Any]] = {}
        self.warnings: list[str] = []
        self.errors: list[str] = []
        self.artifacts: list[str] = []
        self.failed: list[str] = []

    # -- metrics / thresholds ---------------------------------------------------
    def metric(self, name: str, value: Any, *, min: float | None = None,
               max: float | None = None, eq: Any = None, severity: str = "fail") -> bool | None:
        """Record a metric; with a threshold, evaluate it (``severity`` fail|warn).

        Returns True/False when a threshold was given, else None. ``None``
        values with a threshold count as a failure to measure (warning).
        """
        value = jsonable(value)
        self.metrics[name] = value
        if min is None and max is None and eq is None:
            return None
        th: dict[str, Any] = {}
        if min is not None:
            th["min"] = min
        if max is not None:
            th["max"] = max
        if eq is not None:
            th["eq"] = jsonable(eq)
        if severity != "fail":
            th["severity"] = severity
        self.thresholds[name] = th
        if value is None:
            self.warnings.append(f"{name}: not measured")
            return False
        ok = True
        if min is not None and value < min:
            ok = False
        if max is not None and value > max:
            ok = False
        if eq is not None and value != jsonable(eq):
            ok = False
        if not ok:
            msg = f"{name}={value} outside threshold {th}"
            if severity == "warn":
                self.warnings.append(msg)
            else:
                self.failed.append(msg)
        return ok

    def warn(self, msg: str) -> None:
        self.log.warning(msg)
        self.warnings.append(msg)

    def verdict_from_thresholds(self) -> str:
        return "fail" if self.failed else "pass"

    def outcome(self, summary: str, verdict: str | None = None) -> Outcome:
        return Outcome(summary=summary, verdict=verdict)

    # -- artifacts ----------------------------------------------------------------
    def artifact_path(self, name: str) -> Path:
        path = self.artifacts_dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        rel = f"artifacts/{Path(name).as_posix()}"
        if rel not in self.artifacts:
            self.artifacts.append(rel)
        return path

    def save_json(self, name: str, obj: Any) -> Path:
        path = self.artifact_path(name)
        dump_json(obj, path)
        return path

    def has_artifact(self, name: str) -> bool:
        return (self.artifacts_dir / name).exists()

    def load_json(self, name: str) -> Any:
        path = self.artifacts_dir / name
        if not path.exists():
            raise Inconclusive(f"artifact {name} missing in {self.run_dir}")
        rel = f"artifacts/{Path(name).as_posix()}"
        if rel not in self.artifacts:
            self.artifacts.append(rel)
        return json.loads(path.read_text(encoding="utf-8"))

    def unit_label(self, role: str) -> str:
        name = self.roles.get(role, role)
        meta = self.units_meta.get(name, {})
        return f"{name} ({meta.get('transceiver', '?')})"


class TestContext(AnalysisContext):
    """Analysis context plus hardware access for one run."""

    __test__ = False  # not a pytest class

    def __init__(self, cfg: BenchConfig, services: Any, spec: TestSpec, params: dict[str, Any],
                 run_dir: Path, run_id: str, roles: dict[str, str], timeout: float | None,
                 state: SessionState, logger: logging.Logger) -> None:
        units_meta = {n: {"transceiver": cfg.unit(n).transceiver, "label": cfg.unit(n).label}
                      for n in roles.values()}
        super().__init__(spec, params, run_dir, roles, units_meta, logger)
        self.cfg = cfg
        self.services = services
        self.run_id = run_id
        self.timeout = timeout
        self.state = state
        self.budget: LinkBudget | None = None
        self.maintenance_units: list[str] = []
        self.tx_off_failed = False
        self.maint_exit_failed = False
        self._caps: dict[str, dict[str, Any]] = {}
        self._remote_dirs: set[str] = set()

    # -- units --------------------------------------------------------------------
    def unit(self, role: str) -> UnitConfig:
        return self.cfg.unit(self.roles[role])

    @property
    def agent(self) -> Any:
        return self.services.agent

    def caps(self, unit: str) -> dict[str, Any]:
        """Cached capability probe: agent present? image? (never raises)."""
        if unit in self._caps:
            return self._caps[unit]
        caps: dict[str, Any] = {"agent": False, "image": self.cfg.unit(unit).image,
                                "info": None, "agent_error": None}
        try:
            self.agent.version(unit)
            caps["agent"] = True
            try:
                info = self.agent.info(unit)
                caps["info"] = info
                from .units import detect_image

                img = detect_image(None, info, None)
                if img != "unknown":
                    caps["image"] = img
            except FbenchError as exc:
                caps["agent_error"] = exc.message
        except FbenchError as exc:
            caps["agent_error"] = exc.message
        if not caps["agent"]:
            # No agent (e.g. factory firmware): identify the image over libiio.
            try:
                from .units import detect_image

                attrs = self.services.iio(unit).context_attrs(5.0)
                caps["iio"] = attrs
                img = detect_image(attrs, None, None)
                if img != "unknown":
                    caps["image"] = img
            except Exception:  # noqa: BLE001 - best effort
                pass
        self._caps[unit] = caps
        return caps

    def has_agent(self, unit: str) -> bool:
        return bool(self.caps(unit)["agent"])

    def require_agent(self, unit: str) -> None:
        caps = self.caps(unit)
        if not caps["agent"]:
            raise PreconditionError(
                f"fbench-agent not available on unit {unit}: {caps['agent_error']}. "
                "Run `fbench setup session` / `fbench setup agent` (factory firmware units "
                "support only libiio-based tests)", unit=unit)

    def require_image(self, unit: str, *images: str) -> str:
        img = self.caps(unit)["image"]
        if img not in images:
            raise PreconditionError(f"unit {unit} runs image {img!r}; this test needs "
                                    f"{' or '.join(images)} (see `fbench boot`)", unit=unit)
        return img

    # -- remote files -----------------------------------------------------------
    def remote_run_dir(self, unit: str) -> str:
        path = f"{self.cfg.agent.remote_root}/runs/{self.run_id}"
        if unit not in self._remote_dirs:
            rc, _, err = self.services.ssh(unit).run(f"mkdir -p {path}", 15.0)
            if rc != 0:
                raise PreconditionError(f"cannot create {path} on {unit}: {err.strip()}")
            self._remote_dirs.add(unit)
        return path

    def pull(self, unit: str, remote: str, name: str, timeout: float = 300.0) -> Path:
        local = self.artifact_path(name)
        self.services.ssh(unit).get(remote, local, timeout)
        return local

    # -- IIO (agent preferred, host libiio fallback) --------------------------------
    def iio_get(self, unit: str, dev: str, attr: str, chan: str | None = None,
                output: bool = False) -> str:
        if self.has_agent(unit):
            return self.agent.iio_attr_get(unit, dev, attr, chan, output)
        return self.services.iio(unit).attr_get(dev, attr, chan, output)

    def iio_set(self, unit: str, dev: str, attr: str, value: Any, chan: str | None = None,
                output: bool = False, tx_ok: bool = False) -> None:
        """Set an IIO attribute. ``tx_ok`` (agent ``--tx-ok``) only after the interlock."""
        if tx_ok and not self.spec.tx:
            raise SafetyRefusal(f"{self.spec.id} is not a TX test: refusing a TX-enabling write")
        if self.has_agent(unit):
            self.agent.iio_attr_set(unit, dev, attr, value, chan, output, tx_ok=tx_ok)
        else:
            self.services.iio(unit).attr_set(dev, attr, value, chan, output)

    def iio_debug_set(self, unit: str, dev: str, attr: str, value: Any) -> None:
        if self.has_agent(unit):
            self.agent.iio_debug_set(unit, dev, attr, value)
        else:
            self.services.iio(unit).attr_set(dev, attr, value, debug=True)

    # -- TX safety ----------------------------------------------------------------
    def tx_off(self, unit: str) -> dict[str, Any]:
        """Max attenuation + sources off: agent first, host libiio as fallback."""
        report: dict[str, Any] = {"unit": unit, "via": None, "errors": []}
        try:
            reply = self.agent.tx_off(unit)
            report.update(via="agent", reply=reply)
            self.state.set_tx(unit, False)
            return report
        except FbenchError as exc:
            report["errors"].append(f"agent: {exc.message}")
        report.update(host_tx_off(self.cfg, self.services, unit))
        if not report.get("ok"):
            raise FbenchError(f"TX OFF FAILED on {unit}: {report['errors']}")
        self.state.set_tx(unit, False)
        return report

    def mark_tx_active(self, unit: str) -> None:
        self.state.set_tx(unit, True, self.run_id)

    def sleep(self, seconds: float) -> None:
        self.services.sleep(seconds)

    def http(self, unit: str) -> Any:
        return self.services.http(unit)


def host_tx_off(cfg: BenchConfig, services: Any, unit: str) -> dict[str, Any]:
    """TX off through host libiio (units without agent, or agent failure)."""
    errors: list[str] = []
    phy, dds = cfg.iio.phy_device, cfg.iio.tx_device
    iio = services.iio(unit)
    ok = True
    for chan in ("voltage0", "voltage1"):
        try:
            iio.attr_set(phy, "hardwaregain", f"-{cfg.safety.tx_atten_max_db}", chan, True)
        except Exception as exc:  # noqa: BLE001
            if chan == "voltage0":
                ok = False
            errors.append(f"{chan} hardwaregain: {exc}")
    for chan in ("altvoltage0", "altvoltage1", "altvoltage2", "altvoltage3"):
        try:
            iio.attr_set(dds, "scale", "0", chan, True)
        except Exception as exc:  # noqa: BLE001  (DDS compiled out on P25 image)
            errors.append(f"{chan} scale: {exc}")
    return {"via": "host-iio", "ok": ok, "errors": errors}


# ---------------------------------------------------------------------------
# Run directories, FINDINGS, result.json
# ---------------------------------------------------------------------------


def make_run_dir(cfg: BenchConfig, test_id: str, started: datetime) -> tuple[Path, str]:
    day = cfg.paths.diagnostics_dir / started.strftime("%Y-%m-%d") / "bench"
    stamp = started.strftime("%Y%m%d_%H%M%S")
    run_id = f"{stamp}_{test_id}"
    path = day / f"run_{run_id}"
    n = 2
    while path.exists():
        run_id = f"{stamp}_{test_id}_{n}"
        path = day / f"run_{run_id}"
        n += 1
    (path / "artifacts").mkdir(parents=True)
    return path, run_id


def validate_result(d: Any) -> list[str]:
    """Problems with a result.json document (empty list = valid)."""
    probs: list[str] = []
    if not isinstance(d, dict):
        return ["result is not an object"]
    missing = [k for k in RESULT_KEYS if k not in d]
    extra = [k for k in d if k not in RESULT_KEYS]
    if missing:
        probs.append(f"missing keys: {missing}")
    if extra:
        probs.append(f"unexpected keys: {extra}")
    if d.get("schema") != RESULT_SCHEMA:
        probs.append("schema must be fbench.result/1")
    for k in ("test", "run_id", "started", "ended", "summary"):
        if k in d and not isinstance(d[k], str):
            probs.append(f"{k} must be a string")
    for k in ("started", "ended"):
        try:
            datetime.fromisoformat(str(d.get(k)))
        except ValueError:
            probs.append(f"{k} is not ISO 8601")
    if d.get("verdict") not in VERDICTS:
        probs.append(f"verdict must be one of {VERDICTS}")
    for k in ("units", "params", "metrics", "thresholds"):
        if k in d and not isinstance(d[k], dict):
            probs.append(f"{k} must be an object")
    for name, u in (d.get("units") or {}).items():
        if not isinstance(u, dict) or not all(key in u for key in ("serial", "image", "build")):
            probs.append(f"units.{name} needs serial/image/build")
    for name, th in (d.get("thresholds") or {}).items():
        if not isinstance(th, dict) or not set(th) <= {"min", "max", "eq", "severity"}:
            probs.append(f"thresholds.{name} must use min/max/eq/severity")
    for k in ("maintenance_mode", "tx_used"):
        if k in d and not isinstance(d[k], bool):
            probs.append(f"{k} must be a bool")
    for k in ("artifacts", "warnings", "errors"):
        if k in d and not (isinstance(d[k], list) and all(isinstance(x, str) for x in d[k])):
            probs.append(f"{k} must be a list of strings")
    for a in d.get("artifacts") or []:
        if not str(a).startswith("artifacts/"):
            probs.append(f"artifact path {a!r} must start with artifacts/")
    try:
        json.dumps(d)
    except (TypeError, ValueError) as exc:
        probs.append(f"not JSON-serialisable: {exc}")
    return probs


@dataclass
class RunResult:
    test: str
    run_id: str
    run_dir: Path
    verdict: str
    summary: str
    result: dict[str, Any] = field(default_factory=dict)

    @property
    def exit_code(self) -> int:
        return int(VERDICT_EXIT.get(self.verdict, 2))

    def brief(self) -> dict[str, Any]:
        return {"test": self.test, "run_id": self.run_id, "run_dir": self.run_dir.as_posix(),
                "verdict": self.verdict, "exit_code": self.exit_code, "summary": self.summary}


def _file_logger(run_dir: Path, run_id: str) -> tuple[logging.Logger, logging.Handler]:
    lg = logging.getLogger(f"fbench.run.{run_id}")
    lg.setLevel(logging.DEBUG)
    lg.propagate = True
    fh = logging.FileHandler(run_dir / "log.txt", encoding="utf-8")
    fh.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    fh.setLevel(logging.DEBUG)
    lg.addHandler(fh)
    return lg, fh


def _snapshot_units(ctx: TestContext) -> dict[str, dict[str, Any]]:
    """Identity per unit for result.json/units.json (best effort)."""
    snap: dict[str, dict[str, Any]] = {}
    full: dict[str, Any] = {}
    for role, name in ctx.roles.items():
        unit = ctx.cfg.unit(name)
        info = None
        attrs = None
        try:
            caps = ctx.caps(name)
            info = caps.get("info")
        except Exception:  # noqa: BLE001
            caps = {}
        if info is None:
            try:
                attrs = ctx.services.iio(name).context_attrs(5.0)
            except Exception:  # noqa: BLE001
                attrs = None
        ident = identity_snapshot(unit, info, attrs)
        if not (info or attrs):
            ident["image"] = caps.get("image", unit.image) if caps else unit.image
        snap[name] = {k: ident.get(k) for k in UNIT_KEYS}
        full.setdefault("roles", {})[role] = name
        full.setdefault("units", {})[name] = {
            **ident, "config": {"label": unit.label, "host": unit.host,
                                "known_serials": unit.known_serials,
                                "fpga_dna": unit.fpga_dna or None,
                                "transceiver": unit.transceiver},
            "agent": bool(caps.get("agent")) if caps else False,
            "agent_info": info, "iio_context": attrs,
        }
    dump_json(full, ctx.run_dir / "units.json")
    return snap


@contextmanager
def maintenance(ctx: TestContext, unit: str) -> Iterator[None]:
    """``maint enter`` ... ``maint exit`` (exit always attempted)."""
    caps = ctx.caps(unit)
    if not caps["agent"]:
        if caps["image"] == "p25":
            raise PreconditionError(f"maintenance mode on {unit} needs the agent "
                                    f"({caps['agent_error']})")
        ctx.log.info("unit %s has no agent (image %s): no p25-httpd to stop", unit,
                     caps["image"])
        yield
        return
    ctx.agent.maint(unit, "enter")
    ctx.state.set_maintenance(unit, True, ctx.run_id)
    ctx.maintenance_units.append(unit)
    ctx.log.info("maintenance mode entered on %s", unit)
    try:
        yield
    finally:
        try:
            ctx.agent.maint(unit, "exit")
            ctx.state.set_maintenance(unit, False)
            ctx.log.info("maintenance mode exited on %s", unit)
        except Exception as exc:  # noqa: BLE001
            ctx.maint_exit_failed = True
            ctx.errors.append(f"maint exit failed on {unit}: {exc} — p25-httpd may still be "
                              f"stopped; run `fbench agent {unit} -- maint exit`")


def _final_tx_off(ctx: TestContext, unit: str) -> None:
    try:
        rep = ctx.tx_off(unit)
        ctx.log.info("tx off on %s via %s", unit, rep.get("via"))
    except Exception as exc:  # noqa: BLE001
        ctx.tx_off_failed = True
        ctx.errors.append(f"TX OFF FAILED on {unit}: {exc} — run `fbench tx {unit} off` now")


def run_test(spec: TestSpec, cfg: BenchConfig, services: Any, roles: dict[str, str],
             params: dict[str, Any], timeout: float | None = None) -> RunResult:
    """Run one test end to end; always writes result.json and FINDINGS.md."""
    from .findings import write_findings

    state = SessionState(cfg.paths.state_dir)
    started = services.now()
    run_dir, run_id = make_run_dir(cfg, spec.id, started)
    logger, handler = _file_logger(run_dir, run_id)
    ctx = TestContext(cfg, services, spec, params, run_dir, run_id, roles, timeout, state, logger)
    dump_json(params, run_dir / "params.json")
    logger.info("run %s: test %s roles %s params %s", run_id, spec.id, roles, jsonable(params))
    verdict, summary = "error", ""
    units_snap: dict[str, dict[str, Any]] = {n: {"serial": None, "image": cfg.unit(n).image,
                                                 "build": None,
                                                 "transceiver": cfg.unit(n).transceiver,
                                                 "label": cfg.unit(n).label}
                                             for n in roles.values()}
    agent = getattr(services, "agent", None)
    if agent is not None and hasattr(agent, "run_id"):
        agent.run_id = run_id  # agent --run-id: bulk files under runs/<run_id>/
    try:
        if spec.tx:
            atten = min_atten(params[spec.tx_atten_param])
            ctx.budget = require_tx_allowed(cfg, roles["tx"], roles["rx"], atten,
                                            params.get("tx_port") or None)
            for w in ctx.budget.warnings:
                ctx.warn(w)
            ctx.save_json("link_budget.json", ctx.budget.to_dict())
        units_snap = _snapshot_units(ctx)
        with ExitStack() as stack:
            if spec.maintenance:
                for unit in dict.fromkeys(roles.values()):
                    stack.enter_context(maintenance(ctx, unit))
            if spec.tx:
                stack.callback(_final_tx_off, ctx, roles["tx"])
            outcome = spec.func(ctx)
        verdict = outcome.verdict or ctx.verdict_from_thresholds()
        summary = outcome.summary
    except SafetyRefusal as exc:
        verdict, summary = "refused", exc.message
        ctx.errors.append(exc.message)
    except Inconclusive as exc:
        verdict, summary = "inconclusive", exc.message
        ctx.warnings.append(exc.message)
    except (PreconditionError, TransportError, AgentUnsupported) as exc:
        verdict, summary = "precondition", exc.message
        ctx.errors.append(exc.message)
    except FbenchError as exc:
        verdict, summary = "error", exc.message
        ctx.errors.append(exc.message)
    except Exception as exc:  # noqa: BLE001 - a test bug must still produce a record
        verdict, summary = "error", f"{type(exc).__name__}: {exc}"
        ctx.errors.append(summary)
        logger.error("exception:\n%s", traceback.format_exc())
    if agent is not None and hasattr(agent, "run_id"):
        agent.run_id = None
    if ctx.failed:
        for f in ctx.failed:
            logger.info("threshold failed: %s", f)
    if (ctx.tx_off_failed or ctx.maint_exit_failed) and verdict != "refused":
        verdict = "error"
        summary = f"{summary} [cleanup failure — see errors]"
    ended = services.now()
    result = {
        "schema": RESULT_SCHEMA,
        "test": spec.id,
        "run_id": run_id,
        "started": started.isoformat(timespec="seconds"),
        "ended": ended.isoformat(timespec="seconds"),
        "verdict": verdict,
        "summary": summary,
        "units": units_snap,
        "params": jsonable(params),
        "metrics": jsonable(ctx.metrics),
        "thresholds": jsonable(ctx.thresholds),
        "maintenance_mode": bool(ctx.maintenance_units),
        "tx_used": bool(spec.tx),
        "artifacts": list(ctx.artifacts),
        "warnings": [str(w) for w in ctx.warnings],
        "errors": [str(e) for e in ctx.errors],
    }
    dump_json(result, run_dir / "result.json")
    write_findings(run_dir, result, ctx.failed)
    state.set_last_run({"run_id": run_id, "test": spec.id, "verdict": verdict,
                        "run_dir": run_dir.as_posix()})
    logger.info("verdict %s: %s", verdict, summary)
    logger.removeHandler(handler)
    handler.close()
    return RunResult(spec.id, run_id, run_dir, verdict, summary, result)


def run_target(target: str, cfg: BenchConfig, services: Any, unit: str | None = None,
               tx: str | None = None, rx: str | None = None,
               overrides: dict[str, Any] | None = None, duration: float | None = None,
               timeout: float | None = None) -> tuple[str | None, list[RunResult]]:
    """Run a test or suite. ``overrides`` apply to every test that has the key."""
    suite, entries = expand(target)
    overrides = dict(overrides or {})
    plans = []
    for spec, suite_ov in entries:
        ov = dict(suite_ov)
        if suite is None:
            ov.update(overrides)
        else:
            ov.update({k: v for k, v in overrides.items() if k in spec.params})
        dur = duration if (suite is None or spec.duration_param) else None
        params = build_params(spec, ov, dur)
        for roles in resolve_roles(spec, cfg, unit, tx, rx):
            plans.append((spec, roles, params))
    results = [run_test(spec, cfg, services, roles, params, timeout)
               for spec, roles, params in plans]
    if suite is not None:
        write_suite_summary(cfg, suite, results, services.now())
    return suite, results


def plan_target(target: str, cfg: BenchConfig, unit: str | None = None, tx: str | None = None,
                rx: str | None = None, overrides: dict[str, Any] | None = None,
                duration: float | None = None) -> tuple[str | None, list[dict[str, Any]]]:
    """Dry run: resolved params, roles and interlock verdicts, no hardware access."""
    from .safety import link_budget

    suite, entries = expand(target)
    overrides = dict(overrides or {})
    plan = []
    for spec, suite_ov in entries:
        ov = dict(suite_ov)
        ov.update(overrides if suite is None else
                  {k: v for k, v in overrides.items() if k in spec.params})
        params = build_params(spec, ov, duration if (suite is None or spec.duration_param)
                              else None)
        for roles in resolve_roles(spec, cfg, unit, tx, rx):
            entry: dict[str, Any] = {"test": spec.id, "roles": roles, "params": jsonable(params),
                                     "maintenance": spec.maintenance, "tx": spec.tx}
            if spec.tx:
                b = link_budget(cfg, roles["tx"], roles["rx"],
                                min_atten(params[spec.tx_atten_param]),
                                params.get("tx_port") or None)
                entry["safety"] = b.to_dict()
            plan.append(entry)
    return suite, plan


def write_suite_summary(cfg: BenchConfig, suite: str, results: list[RunResult],
                        when: datetime) -> Path:
    day = cfg.paths.diagnostics_dir / when.strftime("%Y-%m-%d") / "bench"
    path = day / f"suite_{when.strftime('%Y%m%d_%H%M%S')}_{suite}.json"
    dump_json({"schema": "fbench.suite/1", "suite": suite,
               "exit_code": worst_exit([r.exit_code for r in results]),
               "runs": [r.brief() for r in results]}, path)
    return path


def load_result(run_dir: Path) -> dict[str, Any]:
    path = Path(run_dir) / "result.json"
    if not path.exists():
        raise UsageError(f"no result.json in {run_dir}")
    return json.loads(path.read_text(encoding="utf-8"))


def reanalyze(run_dir: Path) -> RunResult:
    """``fbench analyze``: re-run a test's analysis on its pulled artifacts."""
    from .findings import write_findings

    run_dir = Path(run_dir)
    old = load_result(run_dir)
    spec = get_spec(old["test"])
    if spec.analyze is None:
        raise PreconditionError(f"{spec.id} has no offline analysis (acquisition-only test)")
    units_doc: dict[str, Any] = {}
    try:
        units_doc = json.loads((run_dir / "units.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        pass
    roles = units_doc.get("roles", {})
    meta = {n: {"transceiver": (u or {}).get("transceiver")}
            for n, u in (units_doc.get("units") or {}).items()}
    actx = AnalysisContext(spec, dict(old.get("params", {})), run_dir, roles, meta)
    try:
        outcome = spec.analyze(actx)
        verdict = outcome.verdict or actx.verdict_from_thresholds()
        summary = outcome.summary
    except Inconclusive as exc:
        verdict, summary = "inconclusive", exc.message
    new = dict(old)
    new.update({
        "verdict": verdict, "summary": summary, "metrics": jsonable(actx.metrics),
        "thresholds": jsonable(actx.thresholds),
        "artifacts": sorted(set(old.get("artifacts", [])) | set(actx.artifacts)),
        "warnings": list(actx.warnings), "errors": list(old.get("errors", [])),
    })
    dump_json(new, run_dir / "result.json")
    write_findings(run_dir, new, actx.failed)
    return RunResult(spec.id, str(old["run_id"]), run_dir, verdict, summary, new)
