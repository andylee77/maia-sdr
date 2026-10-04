"""``fbench setup …`` and ``fbench boot …``: per-bench and per-boot provisioning.

Tezuka's rootfs is a ramfs, so everything under ``/root`` and ``/proc/sys``
is lost at reboot. Persistent material lives on the SD card under
``/mnt/sd/bench`` (the only place the bench writes, rule 6):

- ``setup keys`` (once per card): installs the host public key into
  ``/mnt/sd/bench/keys/authorized_keys`` — with ``--password-prompt`` (or
  ``--password-env``) through paramiko for units that do not authorize the key
  yet (factory firmware) — then links it.
- ``setup session`` (every boot): merges the SD key file into
  ``/root/.ssh/authorized_keys`` (a regular 0600 ramfs file, so dropbear's
  permission check never sees FAT modes), enables ``ip_forward`` on the
  forwarding unit, marks the agent executable, reads ``maint status``.
- ``setup net``: host route ``10.25.0.0/24 via 192.168.2.1``; B's default
  route; ``--apply-env`` writes the persistent u-boot network variables.
- ``setup agent``: deploys the agent binary and ``share/*.json``.
"""

from __future__ import annotations

import hashlib
import platform
import re
import shlex
from pathlib import Path
from typing import Any

from .config import BenchConfig
from .errors import FbenchError, PreconditionError, TransportError, UsageError
from .state import SessionState

#: Bench addresses dropbear may reverse-resolve (PC on USB, both eth ends).
BENCH_HOSTS = (
    ("192.168.2.10", "fbench-pc-usb-a"),
    ("192.168.12.10", "fbench-pc-usb-b"),
    ("10.25.0.1", "fbench-unit-a-eth"),
    ("10.25.0.2", "fbench-unit-b-eth"),
)
HOSTS_CMD = "; ".join(
    f"grep -q '^{ip} ' /etc/hosts || echo '{ip} {name}' >> /etc/hosts" for ip, name in BENCH_HOSTS
) + "; true"

KEY_LINK_CMD = (
    "K={root}/keys/authorized_keys; A=/root/.ssh/authorized_keys; "
    "mkdir -p /root/.ssh && chmod 700 /root/.ssh; "
    "if [ -s \"$K\" ]; then "
    "if [ -L \"$A\" ]; then rm -f \"$A\"; fi; "
    "touch \"$A\"; cat \"$K\" \"$A\" | sort -u > \"$A.tmp\" && mv \"$A.tmp\" \"$A\"; "
    "chmod 600 \"$A\"; echo linked; else echo no-sd-keys; fi"
)


def ordered_units(cfg: BenchConfig, names: list[str] | None) -> list[str]:
    """Forwarders first so units reached through them are reachable."""
    sel = names or list(cfg.units)
    for n in sel:
        cfg.unit(n)
    return sorted(sel, key=lambda n: (cfg.unit(n).via is not None, n))


# ---------------------------------------------------------------------------
# net
# ---------------------------------------------------------------------------


def host_route_present(cfg: BenchConfig, text: str, windows: bool) -> bool:
    net, gw = re.escape(cfg.network.bench_subnet), re.escape(cfg.network.gateway)
    if windows:
        return re.search(rf"^\s*{net}\s+\S+\s+{gw}\b", text, re.MULTILINE) is not None
    return re.search(rf"{net}/\d+\s+via\s+{gw}\b", text) is not None


def setup_net(cfg: BenchConfig, services: Any, units: list[str] | None, apply_env: bool = False,
              skip_route: bool = False) -> dict[str, Any]:
    out: dict[str, Any] = {"route": None, "units": {}}
    windows = platform.system() == "Windows"
    if not skip_route:
        if windows:
            res = services.run_local(["route", "print", "-4"], 15.0)
        else:
            res = services.run_local(["ip", "route", "show"], 15.0)
        if host_route_present(cfg, res.stdout, windows):
            out["route"] = "present"
        else:
            if windows:
                add = ["route", "add", cfg.network.bench_subnet, "mask", cfg.network.bench_mask,
                       cfg.network.gateway]
            else:
                add = ["ip", "route", "add", f"{cfg.network.bench_subnet}/24", "via",
                       cfg.network.gateway]
            r2 = services.run_local(add, 15.0)
            out["route"] = "added" if r2.rc == 0 else "missing"
            if r2.rc != 0:
                out["route_hint"] = ("run as administrator: " + " ".join(add) +
                                     " (add -p on Windows to persist)")
    for name in ordered_units(cfg, units):
        unit = cfg.unit(name)
        rep: dict[str, Any] = {}
        try:
            ssh = services.ssh(name)
            if unit.forwarder:
                rc, _, err = ssh.run("echo 1 > /proc/sys/net/ipv4/ip_forward && "
                                     "cat /proc/sys/net/ipv4/ip_forward", 15.0)
                rep["ip_forward"] = rc == 0
            if unit.via:
                gw = cfg.unit(unit.via).eth_ip
                rc, _, _ = ssh.run(f"ip route | grep -q '^default' || ip route add default via "
                                   f"{gw}", 15.0)
                rep["default_route"] = rc == 0
            if apply_env and unit.uboot_env:
                cmds = " && ".join(f"fw_setenv {shlex.quote(k)} {shlex.quote(v)}"
                                   for k, v in unit.uboot_env.items())
                rc, _, err = ssh.run(cmds, 30.0)
                rep["uboot_env"] = "applied" if rc == 0 else f"failed: {err.strip()[:200]}"
            elif unit.uboot_env:
                rep["uboot_env"] = "not applied (pass --apply-env)"
            rep["ok"] = True
        except FbenchError as exc:
            rep = {"ok": False, "error": exc.message}
        out["units"][name] = rep
    out["ok"] = out["route"] in (None, "present", "added") and all(
        r.get("ok") for r in out["units"].values())
    return out


# ---------------------------------------------------------------------------
# keys
# ---------------------------------------------------------------------------


def public_key(cfg: BenchConfig) -> str:
    path = Path(cfg.ssh.identity).expanduser().with_suffix(".pub") \
        if not str(cfg.ssh.identity).endswith(".pub") else Path(cfg.ssh.identity).expanduser()
    if not path.exists():
        path = Path(str(Path(cfg.ssh.identity).expanduser()) + ".pub")
    if not path.exists():
        raise PreconditionError(f"public key not found next to {cfg.ssh.identity}")
    return path.read_text(encoding="utf-8").strip()


def key_install_cmds(cfg: BenchConfig, pub: str) -> list[str]:
    root = cfg.agent.remote_root
    k = f"{root}/keys/authorized_keys"
    q = shlex.quote(pub)
    return [f"mkdir -p {root}/keys && touch {k} && (grep -qxF {q} {k} || echo {q} >> {k})",
            KEY_LINK_CMD.format(root=root)]


def setup_keys(cfg: BenchConfig, services: Any, units: list[str] | None,
               password: str | None = None) -> dict[str, Any]:
    out: dict[str, Any] = {"units": {}}
    pub = public_key(cfg) if password else None
    for name in ordered_units(cfg, units):
        rep: dict[str, Any] = {}
        try:
            if password:
                results = services.paramiko_exec(name, password, key_install_cmds(cfg, pub or ""))
                rep["installed"] = all(rc == 0 for rc, _, _ in results)
                rep["link"] = results[-1][1].strip() if results else None
            else:
                rc, text, err = services.ssh(name).run(KEY_LINK_CMD.format(
                    root=cfg.agent.remote_root), 20.0)
                rep["link"] = text.strip() if rc == 0 else f"failed: {err.strip()[:200]}"
            rep["batch_ssh"] = services.ssh(name).run("true", 15.0)[0] == 0
            rep["ok"] = bool(rep["batch_ssh"])
        except TransportError as exc:
            rep = {"ok": False, "error": exc.message,
                   "hint": f"fbench setup keys --unit {name} --password-prompt"}
        except FbenchError as exc:
            rep = {"ok": False, "error": exc.message}
        except Exception as exc:  # noqa: BLE001 - paramiko auth errors etc.
            rep = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
        out["units"][name] = rep
    out["ok"] = all(r.get("ok") for r in out["units"].values())
    return out


# ---------------------------------------------------------------------------
# agent
# ---------------------------------------------------------------------------


def setup_agent(cfg: BenchConfig, services: Any, units: list[str] | None,
                binary: Path | None = None) -> dict[str, Any]:
    local = Path(binary) if binary else cfg.paths.repo_root / cfg.agent.local_binary
    if not local.exists():
        raise PreconditionError(f"agent binary not found: {local} (build with "
                                "bench/scripts/build_agent.sh or pass --binary)")
    share = sorted(Path(cfg.paths.share_dir).glob("*.json"))
    out: dict[str, Any] = {"binary": local.as_posix(), "sha256": sha256_file(local),
                           "units": {}}
    for name in ordered_units(cfg, units):
        try:
            out["units"][name] = {"ok": True, **services.agent.deploy(name, local, share)}
        except FbenchError as exc:
            out["units"][name] = {"ok": False, "error": exc.message}
    out["ok"] = all(r.get("ok") for r in out["units"].values())
    return out


# ---------------------------------------------------------------------------
# session
# ---------------------------------------------------------------------------


def setup_session(cfg: BenchConfig, services: Any, units: list[str] | None) -> dict[str, Any]:
    state = SessionState(cfg.paths.state_dir)
    out: dict[str, Any] = {"units": {}, "warnings": []}
    for name in ordered_units(cfg, units):
        unit = cfg.unit(name)
        rep: dict[str, Any] = {}
        try:
            ssh = services.ssh(name)
            # dropbear reverse-resolves the client before its banner; with
            # the boards' resolv.conf pointing at an unreachable 8.8.8.8 that
            # costs ~6.5 s per connection. /etc/hosts entries for the bench
            # addresses make the lookup instant (ramfs: redone every boot).
            ssh.run(HOSTS_CMD, 20.0)
            rc, text, _ = ssh.run(KEY_LINK_CMD.format(root=cfg.agent.remote_root), 20.0)
            rep["keys"] = text.strip() if rc == 0 else "failed"
            if unit.forwarder:
                rc, _, _ = ssh.run("echo 1 > /proc/sys/net/ipv4/ip_forward", 15.0)
                rep["ip_forward"] = rc == 0
            ssh.run(f"chmod +x {cfg.agent.remote_binary} 2>/dev/null; true", 15.0)
            try:
                ms = services.agent.maint(name, "status")
                rep["maintenance"] = bool(ms.get("maintenance"))
                rep["agent"] = True
                if rep["maintenance"]:
                    out["warnings"].append(
                        f"{name} is in maintenance mode (the scanner stopped) from an earlier run; "
                        f"`fbench agent {name} -- maint exit` restores it")
                state.set_maintenance(name, rep["maintenance"],
                                      state.load().get("units", {}).get(name, {})
                                      .get("maintenance_run"))
            except FbenchError as exc:
                rep["agent"] = False
                rep["agent_error"] = exc.message
            rep["ok"] = True
        except TransportError as exc:
            rep = {"ok": False, "error": exc.message,
                   "hint": f"fbench setup keys --unit {name} --password-prompt "
                           "(needed when the SD card image lacks the bench host key)"}
        except FbenchError as exc:
            rep = {"ok": False, "error": exc.message}
        out["units"][name] = rep
    out["ok"] = all(r.get("ok") for r in out["units"].values())
    return out


# ---------------------------------------------------------------------------
# boot
# ---------------------------------------------------------------------------

BOOT_FILES = ("BOOT.bin", "devicetree.dtb")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _wait_ssh(services: Any, unit: str, want_up: bool, timeout: float) -> bool:
    deadline = services.monotonic() + timeout
    while services.monotonic() < deadline:
        try:
            up = services.ssh(unit).run("true", 10.0)[0] == 0
        except FbenchError:
            up = False
        if up == want_up:
            return True
        services.sleep(2.0)
    return False


def boot(cfg: BenchConfig, services: Any, unit: str, action: str, image: str | None = None,
         src_dir: Path | None = None, reboot: bool = True, wait_s: float = 180.0
         ) -> tuple[dict[str, Any], int]:
    """Dual-image swap (design doc 2.4). Returns (report, exit code)."""
    cfg.unit(unit)
    agent = services.agent
    if action == "status":
        return {"unit": unit, "status": agent.boot(unit, "status")}, 0
    if action == "install":
        if not image or src_dir is None:
            raise UsageError("boot install needs --image NAME and --from DIR")
        src = Path(src_dir)
        files = [src / f for f in BOOT_FILES]
        missing = [f.name for f in files if not f.exists()]
        if missing:
            raise PreconditionError(f"{src} lacks {', '.join(missing)}")
        # Stage outside images/<name>; the agent verifies the hashes while copying
        # into images/<name>. (`boot install NAME` WITHOUT --from acts as `select`
        # on the agent and would overwrite the SD boot files: --from is mandatory.)
        stage = f"{cfg.agent.remote_root}/images/.incoming_{image}"
        ssh = services.ssh(unit)
        ssh.run(f"mkdir -p {stage}", 15.0)
        hashes: dict[str, str] = {}
        for f in files:
            ssh.put(f, f"{stage}/{f.name}", 300.0)
            hashes[f.name] = sha256_file(f)
        rep = agent.boot(unit, "install", image, src=stage,
                         sha256_boot=hashes["BOOT.bin"], sha256_dtb=hashes["devicetree.dtb"])
        ssh.run(f"rm -rf {stage}", 30.0)
        sums = [f"{h}  {n}" for n, h in hashes.items()]
        return {"unit": unit, "installed": image, "sha256": sums, "agent": rep}, 0
    if action not in ("p25", "hwval"):
        raise UsageError("boot action must be status, install, p25 or hwval")
    sel = agent.boot(unit, "select", action)
    report: dict[str, Any] = {"unit": unit, "selected": action, "agent": sel}
    if not reboot:
        report["reboot"] = "skipped (--no-reboot)"
        return report, 0
    services.ssh(unit).run("sh -c 'sleep 1; reboot' >/dev/null 2>&1 &", 15.0)
    report["went_down"] = _wait_ssh(services, unit, False, 60.0)
    if not _wait_ssh(services, unit, True, wait_s):
        report["error"] = (f"{unit} did not come back within {wait_s:.0f} s; recovery: copy "
                           f"images/p25/* to the card root on the PC")
        return report, 3
    report["session"] = setup_session(cfg, services, [unit])
    info = agent.info(unit)
    from .units import detect_image

    running = detect_image(None, info, None)
    report["running_image"] = running
    SessionState(cfg.paths.state_dir).set_image(unit, running)
    return report, 0 if running == action else 1
