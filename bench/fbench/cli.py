"""``fbench`` command line (design doc 5.1).

Every verb accepts ``--json`` (one JSON object on stdout, logs on stderr),
``--config PATH``, ``--timeout S`` and ``-v``, before or after the verb.
Exit codes: 0 pass, 1 fail, 2 error/usage, 3 precondition, 4 safety refusal,
5 inconclusive. No verb prompts except ``setup keys --password-prompt``.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import traceback
from pathlib import Path
from typing import Any, Callable, Sequence

from . import __version__
from .config import BenchConfig, load_config
from .errors import (
    ExitCode,
    FbenchError,
    PreconditionError,
    SafetyRefusal,
    UsageError,
    worst_exit,
)
from .util import dump_json, jsonable, parse_value

log = logging.getLogger("fbench")

VERBS = ("units", "setup", "list", "describe", "run", "status", "analyze", "compare", "boot",
         "safety", "agent", "reg", "tx", "regmaps", "console")


class _Parser(argparse.ArgumentParser):
    """argparse that raises instead of exiting (so --json errors stay JSON)."""

    def error(self, message: str) -> None:  # type: ignore[override]
        raise UsageError(f"{self.prog}: {message}")


def _common(p: argparse.ArgumentParser, suppress: bool) -> None:
    d = argparse.SUPPRESS if suppress else None
    p.add_argument("--json", action="store_true", default=d if suppress else False,
                   help="machine-readable output on stdout")
    p.add_argument("--config", default=d, help="bench config (default bench/config/bench.toml)")
    p.add_argument("--timeout", type=float, default=d, help="default per-call timeout (s)")
    p.add_argument("-v", "--verbose", action="count", default=d if suppress else 0)


def build_parser() -> argparse.ArgumentParser:
    p = _Parser(prog="fbench", description="Fishball hardware validation bench CLI")
    _common(p, suppress=False)
    p.add_argument("--version", action="version", version=f"fbench {__version__}")
    sub = p.add_subparsers(dest="verb", parser_class=_Parser)

    def verb(name: str, help_: str) -> argparse.ArgumentParser:
        sp = sub.add_parser(name, help=help_)
        _common(sp, suppress=True)
        return sp

    sp = verb("units", "list configured units; --probe checks reachability and identity")
    sp.add_argument("--probe", action="store_true")
    sp.add_argument("--unit")

    sp = verb("setup", "one-time and per-session setup")
    sp.add_argument("what", choices=["net", "keys", "agent", "session", "all"])
    sp.add_argument("--unit")
    sp.add_argument("--password-prompt", action="store_true",
                    help="ask for the root password once (setup keys only)")
    sp.add_argument("--password-env", metavar="VAR", help="read the root password from $VAR")
    sp.add_argument("--apply-env", action="store_true",
                    help="write persistent u-boot network variables (setup net)")
    sp.add_argument("--skip-route", action="store_true", help="do not touch the host route")
    sp.add_argument("--binary", help="agent binary to deploy")

    sp = verb("list", "test catalog")
    sp.add_argument("--tier", type=int)
    sp.add_argument("--suite")

    sp = verb("describe", "parameters, requirements, pass criteria, artifacts")
    sp.add_argument("test")

    sp = verb("run", "run a test or suite; exit code = verdict")
    sp.add_argument("target")
    sp.add_argument("--unit")
    sp.add_argument("--tx")
    sp.add_argument("--rx")
    sp.add_argument("-p", "--param", action="append", default=[], metavar="KEY=VALUE")
    sp.add_argument("--duration", type=float)
    sp.add_argument("--dry-run", action="store_true",
                    help="resolve params, roles and the interlock without touching hardware")

    sp = verb("status", "last runs, unit state (maintenance, TX)")
    sp.add_argument("--limit", type=int, default=10)
    sp.add_argument("--probe", action="store_true", help="ask each agent for maint status")

    sp = verb("analyze", "re-run analysis offline on a run dir")
    sp.add_argument("run_dirs", nargs="+")

    sp = verb("compare", "metric deltas between runs (first = baseline)")
    sp.add_argument("run_dirs", nargs="+")

    sp = verb("boot", "dual-image swap: status | install | p25 | hwval")
    sp.add_argument("unit")
    sp.add_argument("action", choices=["status", "install", "p25", "hwval"])
    sp.add_argument("--image", help="image name for install")
    sp.add_argument("--from", dest="src", help="directory with BOOT.bin + devicetree.dtb")
    sp.add_argument("--no-reboot", action="store_true")
    sp.add_argument("--wait", type=float, default=180.0)

    sp = verb("safety", "link budget and interlock verdict")
    sp.add_argument("--tx")
    sp.add_argument("--rx")
    sp.add_argument("--tx-atten", type=float)
    sp.add_argument("--tx-port")

    sp = verb("agent", "raw passthrough: fbench agent <unit> -- <agent args>")
    sp.add_argument("unit", nargs="?")
    sp.add_argument("--contract", action="store_true",
                    help="print the agent contract assumed by the host")

    sp = verb("reg", "allow-listed register access via the agent")
    sp.add_argument("unit")
    sp.add_argument("core")
    sp.add_argument("op", choices=["read", "write"])
    sp.add_argument("reg")
    sp.add_argument("value", nargs="?")
    sp.add_argument("--allow-side-effect", action="store_true",
                    help="permit reading a read-to-clear register")

    sp = verb("tx", "emergency: max attenuation + all TX sources off")
    sp.add_argument("unit")
    sp.add_argument("action", choices=["off"])

    sp = verb("regmaps", "build or show register maps (bench/share/*.json)")
    sp.add_argument("action", choices=["build", "show"])
    sp.add_argument("--svd")
    sp.add_argument("--out")
    sp.add_argument("--core")

    sp = verb("console", "FT2232 DEBUG UART console capture")
    sp.add_argument("unit", nargs="?")
    sp.add_argument("--port")
    sp.add_argument("--baud", type=int)
    sp.add_argument("--seconds", type=float, default=30.0)
    sp.add_argument("--until")
    sp.add_argument("--send")
    sp.add_argument("--list", action="store_true", help="list FTDI serial ports and exit")
    return p


# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------


class Env:
    """Lazily loaded config and services for one invocation."""

    def __init__(self, ns: argparse.Namespace,
                 services_factory: Callable[[BenchConfig], Any] | None) -> None:
        self.ns = ns
        self._cfg: BenchConfig | None = None
        self._services: Any = None
        self._factory = services_factory

    @property
    def cfg(self) -> BenchConfig:
        if self._cfg is None:
            self._cfg = load_config(getattr(self.ns, "config", None))
            if getattr(self.ns, "timeout", None):
                self._cfg.agent.default_timeout_s = float(self.ns.timeout)
        return self._cfg

    @property
    def services(self) -> Any:
        if self._services is None:
            if self._factory is not None:
                self._services = self._factory(self.cfg)
            else:
                from .services import Services

                self._services = Services(self.cfg)
        return self._services


def _parse_params(items: list[str]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for item in items:
        if "=" not in item:
            raise UsageError(f"-p expects KEY=VALUE, got {item!r}")
        k, v = item.split("=", 1)
        out[k.strip()] = parse_value(v)
    return out


# ---------------------------------------------------------------------------
# Verbs (each returns (payload, exit_code))
# ---------------------------------------------------------------------------


def cmd_units(env: Env) -> tuple[dict[str, Any], int]:
    from .state import SessionState
    from .units import probe_unit, unit_summary

    cfg = env.cfg
    names = [env.ns.unit] if env.ns.unit else list(cfg.units)
    for n in names:
        cfg.unit(n)
    if not env.ns.probe:
        return {"probed": False, "units": [unit_summary(cfg.unit(n)) for n in names]}, 0
    state = SessionState(cfg.paths.state_dir)
    st = state.load().get("units", {})
    rows = []
    for n in names:
        res = probe_unit(cfg, env.services, cfg.unit(n), st.get(n, {}).get("dna"),
                         env.ns.timeout or 10.0)
        rows.append({**unit_summary(cfg.unit(n)), "probe": res})
        if res.get("reachable"):
            state.update_unit(n, last_image=res.get("image"),
                              last_serial=(res.get("serial") or {}).get("serial"),
                              **({"dna": res["dna"]["dna"]} if res.get("dna", {}).get("dna")
                                 else {}))
    code = 0 if all(r["probe"].get("reachable") for r in rows) else int(ExitCode.PRECONDITION)
    return {"probed": True, "units": rows,
            "warnings": [f"{r['name']}: {w}" for r in rows for w in r["probe"]["warnings"]]}, code


def cmd_setup(env: Env) -> tuple[dict[str, Any], int]:
    from . import provision

    cfg, ns = env.cfg, env.ns
    units = [ns.unit] if ns.unit else None
    password = None
    if ns.password_env:
        password = os.environ.get(ns.password_env)
        if not password:
            raise PreconditionError(f"${ns.password_env} is empty")
    elif ns.password_prompt:
        if ns.what not in ("keys", "all"):
            raise UsageError("--password-prompt applies to `setup keys` / `setup all`")
        import getpass

        password = getpass.getpass(f"root password for {ns.unit or 'the units'}: ")
    steps = ["net", "keys", "agent", "session"] if ns.what == "all" else [ns.what]
    out: dict[str, Any] = {}
    for step in steps:
        if step == "net":
            out["net"] = provision.setup_net(cfg, env.services, units, ns.apply_env,
                                             ns.skip_route)
        elif step == "keys":
            out["keys"] = provision.setup_keys(cfg, env.services, units, password)
        elif step == "agent":
            out["agent"] = provision.setup_agent(cfg, env.services, units,
                                                 Path(ns.binary) if ns.binary else None)
        else:
            out["session"] = provision.setup_session(cfg, env.services, units)
    ok = all(v.get("ok") for v in out.values())
    return out, 0 if ok else int(ExitCode.PRECONDITION)


def cmd_list(env: Env) -> tuple[dict[str, Any], int]:
    from .runner import load_tests, suites

    reg = load_tests()
    all_suites = suites()
    if env.ns.suite and env.ns.suite not in all_suites:
        raise UsageError(f"unknown suite {env.ns.suite!r}")
    ids = [tid for tid, _ in all_suites[env.ns.suite]] if env.ns.suite else list(reg)
    rows = []
    for tid in ids:
        s = reg[tid]
        if env.ns.tier is not None and s.tier != env.ns.tier:
            continue
        rows.append({"id": s.id, "tier": s.tier, "units": s.units, "maintenance": s.maintenance,
                     "tx": s.tx, "suites": list(s.suites), "description": s.description})
    return {"tests": rows, "count": len(rows),
            "suites": {k: [t for t, _ in v] for k, v in all_suites.items()}}, 0


def cmd_describe(env: Env) -> tuple[dict[str, Any], int]:
    from .runner import REGISTRY, load_tests, suites

    load_tests()
    name = env.ns.test
    if name in REGISTRY:
        return {"test": REGISTRY[name].describe()}, 0
    all_suites = suites()
    if name in all_suites:
        return {"suite": name, "tests": [{"id": t, "overrides": ov}
                                         for t, ov in all_suites[name]]}, 0
    raise UsageError(f"unknown test or suite {name!r}")


def cmd_run(env: Env) -> tuple[dict[str, Any], int]:
    from .runner import plan_target, run_target

    ns = env.ns
    overrides = _parse_params(ns.param)
    if ns.dry_run:
        suite, plan = plan_target(ns.target, env.cfg, ns.unit, ns.tx, ns.rx, overrides,
                                  ns.duration)
        refused = [p for p in plan if p.get("safety") and not p["safety"]["allowed"]]
        return {"dry_run": True, "suite": suite, "plan": plan}, \
            int(ExitCode.SAFETY) if refused else 0
    suite, results = run_target(ns.target, env.cfg, env.services, ns.unit, ns.tx, ns.rx,
                                overrides, ns.duration, ns.timeout)
    code = worst_exit([r.exit_code for r in results])
    payload: dict[str, Any] = {"suite": suite, "runs": [r.brief() for r in results]}
    if len(results) == 1:
        payload["verdict"] = results[0].verdict
    return payload, code


def cmd_status(env: Env) -> tuple[dict[str, Any], int]:
    from .state import SessionState

    cfg = env.cfg
    runs = []
    diag = cfg.paths.diagnostics_dir
    if diag.exists():
        for res in diag.glob("*/bench/run_*/result.json"):
            try:
                d = json.loads(res.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            runs.append({"run_id": d.get("run_id"), "test": d.get("test"),
                         "verdict": d.get("verdict"), "started": d.get("started"),
                         "summary": d.get("summary"), "run_dir": res.parent.as_posix()})
    runs.sort(key=lambda r: str(r.get("started")), reverse=True)
    state = SessionState(cfg.paths.state_dir).load()
    out: dict[str, Any] = {"runs": runs[: env.ns.limit], "session": state}
    alerts = []
    for name, st in (state.get("units") or {}).items():
        if st.get("maintenance"):
            alerts.append(f"{name}: maintenance mode active since {st.get('maintenance_since')}")
        if st.get("tx_active"):
            alerts.append(f"{name}: TX flagged active since {st.get('tx_since')} — "
                          f"run `fbench tx {name} off`")
    if env.ns.probe:
        live = {}
        for name in cfg.units:
            try:
                live[name] = env.services.agent.maint(name, "status")
            except FbenchError as exc:
                live[name] = {"error": exc.message}
        out["live"] = live
    out["alerts"] = alerts
    return out, 0


def cmd_analyze(env: Env) -> tuple[dict[str, Any], int]:
    from .runner import reanalyze

    results = [reanalyze(Path(d)) for d in env.ns.run_dirs]
    return {"runs": [r.brief() for r in results]}, worst_exit([r.exit_code for r in results])


def cmd_compare(env: Env) -> tuple[dict[str, Any], int]:
    from .compare import compare

    return compare([Path(d) for d in env.ns.run_dirs])


def cmd_boot(env: Env) -> tuple[dict[str, Any], int]:
    from .provision import boot

    ns = env.ns
    return boot(env.cfg, env.services, ns.unit, ns.action, ns.image,
                Path(ns.src) if ns.src else None, not ns.no_reboot, ns.wait)


def cmd_safety(env: Env) -> tuple[dict[str, Any], int]:
    from .safety import all_budgets, link_budget

    cfg, ns = env.cfg, env.ns
    atten = ns.tx_atten if ns.tx_atten is not None else cfg.rf.tx_atten_default_db
    if ns.tx or ns.rx:
        if not (ns.tx and ns.rx):
            raise UsageError("safety needs both --tx and --rx (or neither)")
        budgets = [link_budget(cfg, ns.tx, ns.rx, atten, ns.tx_port)]
    else:
        budgets = all_budgets(cfg, atten)
    allowed = all(b.allowed for b in budgets) and bool(budgets)
    return {"allowed": allowed, "tx_atten_db": atten,
            "cabled_confirmed": cfg.rf.cabled_confirmed,
            "budgets": [b.to_dict() for b in budgets],
            "rule": "refuse when P_rx > rx_abs_max_dbm (strict); warn when P_rx > "
                    "rx_linear_max_dbm; P_rx = tx_max_dbm - tx_atten - pad_db"}, \
        0 if allowed else int(ExitCode.SAFETY)


def cmd_agent(env: Env, passthrough: list[str]) -> tuple[dict[str, Any], int]:
    from .agent import CONTRACT

    if env.ns.contract:
        return {"contract": CONTRACT}, 0
    if not env.ns.unit or not passthrough:
        raise UsageError("usage: fbench agent <unit> -- <agent args>")
    reply = env.services.agent.run(env.ns.unit, passthrough, env.ns.timeout)
    return {"unit": env.ns.unit, "reply": reply}, 0


def cmd_reg(env: Env) -> tuple[dict[str, Any], int]:
    from .regmaps import find_reg, load_regmaps

    ns, cfg = env.ns, env.cfg
    cfg.unit(ns.unit)
    maps = load_regmaps(cfg.paths.share_dir)
    if ns.core not in maps:
        raise PreconditionError(f"no register map for core {ns.core!r} in {cfg.paths.share_dir}"
                                f" (have {', '.join(sorted(maps))}); run `fbench regmaps build`")
    r = find_reg(maps, ns.core, ns.reg)
    info = {"unit": ns.unit, "core": ns.core, "reg": r.name, "offset": hex(r.offset),
            "address": hex(r.address), "access": r.access}
    if ns.op == "read":
        if r.access == "wo":
            raise SafetyRefusal(f"{r.name} is write-only")
        if r.read_side_effect and not ns.allow_side_effect:
            raise SafetyRefusal(f"{r.name} is read-to-clear (clears sticky status, F2); pass "
                                "--allow-side-effect to read it anyway")
        value = env.services.agent.reg_read(ns.unit, ns.core, r.name,
                                            force_side_effects=bool(ns.allow_side_effect))
        return {**info, "value": f"0x{value:08X}", "fields": r.decode(value),
                "expected": r.expected, "matches_expected": r.check(value)}, 0
    if r.access == "ro":
        raise SafetyRefusal(f"{r.name} is read-only in the allow-list")
    if ns.value is None:
        raise UsageError("reg write needs a value")
    value = int(ns.value, 0)
    reply = env.services.agent.reg_write(ns.unit, ns.core, r.name, value)
    return {**info, "written": f"0x{value:08X}", "reply": reply}, 0


def cmd_tx(env: Env) -> tuple[dict[str, Any], int]:
    from .runner import host_tx_off
    from .state import SessionState

    cfg, unit = env.cfg, env.ns.unit
    cfg.unit(unit)
    report: dict[str, Any] = {"unit": unit, "errors": []}
    try:
        report["agent"] = env.services.agent.tx_off(unit)
        report["via"] = "agent"
    except FbenchError as exc:
        report["errors"].append(f"agent: {exc.message}")
        host = host_tx_off(cfg, env.services, unit)
        report["via"] = "host-iio"
        report["host"] = host
        if not host.get("ok"):
            return report, int(ExitCode.ERROR)
    SessionState(cfg.paths.state_dir).set_tx(unit, False)
    return report, 0


def cmd_regmaps(env: Env) -> tuple[dict[str, Any], int]:
    from . import BENCH_DIR
    from .regmaps import P25_SVD, build_all, load_regmaps

    ns = env.ns
    if ns.action == "build":
        out_dir = Path(ns.out) if ns.out else BENCH_DIR / "share"
        written = build_all(out_dir, Path(ns.svd) if ns.svd else P25_SVD)
        maps = load_regmaps(out_dir)
        return {"written": [p.as_posix() for p in written],
                "cores": {k: len(v) for k, v in maps.items()}}, 0
    maps = load_regmaps(Path(ns.out) if ns.out else env.cfg.paths.share_dir)
    if ns.core:
        if ns.core not in maps:
            raise UsageError(f"unknown core {ns.core!r}")
        return {"core": ns.core, "regs": [
            {"name": r.name, "block": r.block, "offset": hex(r.offset), "address": hex(r.address),
             "access": r.access, "read_side_effect": r.read_side_effect,
             "expected": r.expected} for r in maps[ns.core].values()]}, 0
    return {"cores": {k: len(v) for k, v in maps.items()}}, 0


def cmd_console(env: Env) -> tuple[dict[str, Any], int]:
    from .console import capture, list_ftdi_ports, resolve_port
    from .runner import make_run_dir

    ns = env.ns
    services = env.services
    if ns.list:
        return {"ports": list_ftdi_ports(services.list_serial_ports)}, 0
    if not ns.unit:
        raise UsageError("console needs a unit (or --list)")
    unit = env.cfg.unit(ns.unit)
    port = resolve_port(ns.port, unit.console_port or None, services.list_serial_ports)
    run_dir, run_id = make_run_dir(env.cfg, f"console_{ns.unit}", services.now())
    rep = capture(port, ns.baud or unit.console_baud, ns.seconds,
                  run_dir / "artifacts" / "console.log", ns.until, ns.send,
                  echo=sys.stderr, serial_factory=services.serial_open,
                  clock=services.monotonic)
    rep["run_dir"] = run_dir.as_posix()
    dump_json({"unit": ns.unit, **rep}, run_dir / "console.json")
    code = 0
    if ns.until and rep["matched"] is None:
        code = int(ExitCode.INCONCLUSIVE)
    return rep, code


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------


def _human(verb: str, payload: dict[str, Any]) -> str:
    lines: list[str] = []
    if "error" in payload and not payload.get("ok", True):
        return f"error: {payload['error']}"
    if verb in ("run", "analyze") and "runs" in payload:
        for r in payload["runs"]:
            lines.append(f"{r['verdict'].upper():13s} {r['test']:24s} {r['summary']}")
            lines.append(f"{'':13s} -> {r['run_dir']}")
        return "\n".join(lines)
    if verb == "list":
        for t in payload["tests"]:
            flags = ("M" if t["maintenance"] else " ") + ("T" if t["tx"] else " ")
            lines.append(f"T{t['tier']} {flags} {t['id']:22s} {t['units']:6s} {t['description']}")
        lines.append("")
        lines += [f"suite {k}: {', '.join(v)}" for k, v in payload["suites"].items()]
        return "\n".join(lines)
    if verb == "safety":
        for b in payload["budgets"]:
            p = "n/a" if b["p_rx_dbm"] is None else f"{b['p_rx_dbm']:+.2f} dBm"
            lines.append(f"{b['tx']} -> {b['rx']}: P_rx {p} (atten {b['tx_atten_db']:g} dB, "
                         f"pad {b['pad_db']}) {'ALLOWED' if b['allowed'] else 'REFUSED'}")
            lines += [f"   refuse: {r}" for r in b["reasons"]]
            lines += [f"   warn:   {w}" for w in b["warnings"]]
        return "\n".join(lines)
    return json.dumps(jsonable(payload), indent=2)


def emit(ns: argparse.Namespace, verb: str, payload: dict[str, Any], code: int) -> int:
    doc = {"ok": code == 0, "verb": verb, "exit_code": int(code), **payload}
    if getattr(ns, "json", False):
        sys.stdout.write(dump_json(doc) + "\n")
    else:
        sys.stdout.write(_human(verb, doc) + "\n")
    sys.stdout.flush()
    return int(code)


def _setup_logging(verbosity: int) -> None:
    level = logging.WARNING if verbosity <= 0 else (logging.INFO if verbosity == 1
                                                     else logging.DEBUG)
    root = logging.getLogger("fbench")
    root.setLevel(logging.DEBUG)
    if not any(getattr(h, "_fbench", False) for h in root.handlers):
        h = logging.StreamHandler(sys.stderr)
        h._fbench = True  # type: ignore[attr-defined]
        h.setFormatter(logging.Formatter("fbench %(levelname)s: %(message)s"))
        root.addHandler(h)
    for h in root.handlers:
        if getattr(h, "_fbench", False):
            h.setLevel(level)


DISPATCH: dict[str, Callable[[Env], tuple[dict[str, Any], int]]] = {
    "units": cmd_units, "setup": cmd_setup, "list": cmd_list, "describe": cmd_describe,
    "run": cmd_run, "status": cmd_status, "analyze": cmd_analyze, "compare": cmd_compare,
    "boot": cmd_boot, "safety": cmd_safety, "reg": cmd_reg, "tx": cmd_tx,
    "regmaps": cmd_regmaps, "console": cmd_console,
}


def main(argv: Sequence[str] | None = None,
         services_factory: Callable[[BenchConfig], Any] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    passthrough: list[str] = []
    if "--" in args:
        i = args.index("--")
        args, passthrough = args[:i], args[i + 1:]
    want_json = "--json" in args
    parser = build_parser()
    verb = "fbench"
    try:
        ns = parser.parse_args(args)
        for key, default in (("json", False), ("config", None), ("timeout", None),
                             ("verbose", 0)):
            if not hasattr(ns, key) or getattr(ns, key) is None and default is not None:
                setattr(ns, key, default)
        _setup_logging(ns.verbose or 0)
        if not ns.verb:
            parser.print_help(sys.stderr)
            return int(ExitCode.ERROR)
        verb = ns.verb
        env = Env(ns, services_factory)
        if verb == "agent":
            payload, code = cmd_agent(env, passthrough)
        else:
            if passthrough:
                raise UsageError("'--' passthrough is only valid for `fbench agent`")
            payload, code = DISPATCH[verb](env)
        return emit(ns, verb, payload, code)
    except FbenchError as exc:
        ns_json = argparse.Namespace(json=want_json)
        return emit(ns_json, verb, exc.to_dict(), int(exc.exit_code))
    except KeyboardInterrupt:
        return emit(argparse.Namespace(json=want_json), verb, {"error": "interrupted"},
                    int(ExitCode.ERROR))
    except Exception as exc:  # noqa: BLE001 - report bugs as JSON, exit 2
        log.debug("%s", traceback.format_exc())
        payload = {"error": f"{type(exc).__name__}: {exc}", "kind": "InternalError"}
        if logging.getLogger("fbench").isEnabledFor(logging.DEBUG):
            payload["traceback"] = traceback.format_exc()
        return emit(argparse.Namespace(json=want_json), verb, payload, int(ExitCode.ERROR))


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
