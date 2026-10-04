"""Unit identity: offline summaries and the ``units --probe`` probe.

Identity rules (nothing is keyed by ``hw_serial`` alone):

1. ``fpga_dna`` (PL device DNA, read via the agent on the hwval image) is
   authoritative. A DNA that differs from the configured or last-seen value
   is a warning ("different board behind this address?").
2. The IIO ``hw_serial`` only says which SD card/firmware is inserted; it is
   matched against ``known_serials`` as a hint. An unknown serial is a
   warning, never a failure. A serial listed under another unit is a warning
   too.
3. The IP address is how we reach a unit, never who it is (F14).

Firmware family: ``tezuka`` (fw_version starts with/contains "tezuka", or the
agent reports Tezuka markers) or ``factory`` (plutosdr-fw ``v0.38``-style
version). Image: ``p25``/``hwval`` (Tezuka + bitstream product ID),
``factory`` or ``unknown``.
"""

from __future__ import annotations

import re
from typing import Any

from .config import BenchConfig, UnitConfig, normalize_dna
from .errors import FbenchError

P25_PRODUCT_ID = 0x72616431  # "rad1", the radio core
HWVAL_ID = 0x68777631  # "hwv1"


def unit_summary(u: UnitConfig) -> dict[str, Any]:
    """Offline description of a configured unit."""
    return {
        "name": u.name,
        "label": u.label,
        "description": u.description,
        "transceiver": u.transceiver,
        "known_serials": list(u.known_serials),
        "fpga_dna": u.fpga_dna or None,
        "host": u.host,
        "usb_ip": u.usb_ip or None,
        "eth_ip": u.eth_ip or None,
        "via": u.via,
        "forwarder": u.forwarder,
        "expected_image": u.image,
        "has_jp5": u.has_jp5,
        "dram_part": u.dram_part or None,
    }


def firmware_family(iio_attrs: dict[str, str] | None, info: dict[str, Any] | None) -> str:
    """``tezuka`` | ``factory`` | ``unknown`` from IIO context attrs / agent info."""
    info = info or {}
    fam = str(info.get("firmware", "") or info.get("firmware_family", "")).lower()
    if fam in ("tezuka", "factory"):
        return fam
    if info.get("tezuka_markers") or str(info.get("image", "")) in ("p25", "hwval", "maia"):
        return "tezuka"
    if str(info.get("image", "")) == "factory":
        return "factory"
    fw = str((iio_attrs or {}).get("fw_version", "") or info.get("fw_version", "")).strip()
    if "tezuka" in fw.lower():
        return "tezuka"
    if re.match(r"^v\d+\.\d+", fw):
        return "factory"
    return "unknown"


def detect_image(iio_attrs: dict[str, str] | None, info: dict[str, Any] | None,
                 http_system: dict[str, Any] | None) -> str:
    """Best guess of the running image: p25 | hwval | factory | unknown."""
    info = info or {}
    img = str(info.get("image", "")).lower()
    if img in ("p25", "hwval", "maia", "factory"):
        return img
    bit = info.get("bitstream") or {}
    name = str(bit.get("name", "")).lower()
    if name in ("p25", "hwval"):
        return name
    pid = bit.get("product_id")
    if pid is not None:
        try:
            pid_i = int(str(pid), 0) if not isinstance(pid, int) else pid
        except ValueError:
            pid_i = -1
        if pid_i == P25_PRODUCT_ID:
            return "p25"
        if pid_i == HWVAL_ID:
            return "hwval"
    if http_system and http_system.get("build"):
        return "p25"
    if firmware_family(iio_attrs, info) == "factory":
        return "factory"
    return "unknown"


def serial_hint(cfg: BenchConfig, unit: UnitConfig, serial: str | None) -> dict[str, Any]:
    """Match a reported serial against ``known_serials`` (a hint only)."""
    out: dict[str, Any] = {"serial": serial, "known": False, "matched_unit": None,
                           "warnings": []}
    if not serial:
        out["warnings"].append("no hw_serial reported")
        return out
    if serial in unit.known_serials:
        out["known"] = True
        out["matched_unit"] = unit.name
        return out
    others = [u.name for u in cfg.units_for_serial(serial) if u.name != unit.name]
    if others:
        out["matched_unit"] = others[0]
        out["warnings"].append(
            f"serial {serial} is listed for unit {others[0]}, not {unit.name}: "
            "a different board (or SD card) answers at this address?")
    else:
        out["warnings"].append(
            f"serial {serial} is not in units.{unit.name}.known_serials "
            "(new SD card? add it to the config)")
    return out


def dna_check(unit: UnitConfig, dna: str | None, last_seen: str | None) -> dict[str, Any]:
    out: dict[str, Any] = {"dna": normalize_dna(dna) if dna else None, "authoritative": False,
                           "warnings": []}
    if not dna:
        return out
    got = normalize_dna(dna)
    if unit.fpga_dna:
        out["authoritative"] = True
        if normalize_dna(unit.fpga_dna) != got:
            out["warnings"].append(
                f"DNA {got} != configured {normalize_dna(unit.fpga_dna)} for unit {unit.name}: "
                "this is a different board")
    elif last_seen and normalize_dna(last_seen) != got:
        out["warnings"].append(f"DNA changed since last probe ({normalize_dna(last_seen)} -> "
                               f"{got}) for unit {unit.name}")
    else:
        out["warnings"].append(f"record fpga_dna = \"{got}\" for unit {unit.name} in the config")
    return out


def info_dna(info: dict[str, Any] | None) -> str | None:
    """PL device DNA from ``info`` (``fpga_dna``, ``bitstream.fpga_dna`` or ``dna``)."""
    info = info or {}
    bit = info.get("bitstream") or {}
    return info.get("fpga_dna") or bit.get("fpga_dna") or info.get("dna")


def _try(fn: Any, *args: Any, **kw: Any) -> tuple[Any, str | None]:
    try:
        return fn(*args, **kw), None
    except FbenchError as exc:
        return None, exc.message
    except Exception as exc:  # noqa: BLE001 - probe must never crash
        return None, f"{type(exc).__name__}: {exc}"


def probe_unit(cfg: BenchConfig, services: Any, unit: UnitConfig,
               last_seen_dna: str | None = None, timeout: float = 10.0) -> dict[str, Any]:
    """Probe reachability and identity of one unit (best effort, never raises)."""
    res: dict[str, Any] = {"name": unit.name, "label": unit.label,
                           "transceiver": unit.transceiver, "host": unit.host,
                           "warnings": [], "errors": {}}
    res["ping_ms"] = services.ping(unit.host, 1, 1.0)
    attrs, err = _try(services.iio(unit.name).context_attrs, timeout)
    res["iio"] = {k: attrs.get(k) for k in ("hw_model", "hw_serial", "fw_version",
                                            "hw_model_variant")} if attrs else None
    if err:
        res["errors"]["iio"] = err
    ssh_ok = False
    try:
        rc, _, _ = services.ssh(unit.name).run("true", timeout)
        ssh_ok = rc == 0
    except Exception as exc:  # noqa: BLE001
        res["errors"]["ssh"] = str(exc)
    res["ssh"] = ssh_ok
    info = None
    if ssh_ok:
        ver, err = _try(services.agent.version, unit.name)
        res["agent_version"] = (ver or {}).get("version") if ver else None
        if err:
            res["errors"]["agent"] = err
        if ver:
            info, err = _try(services.agent.info, unit.name)
            if err:
                res["errors"]["agent_info"] = err
    else:
        res["agent_version"] = None
    http_sys = None
    if unit.image in ("p25", "unknown") or (info or {}).get("image") == "p25":
        http_sys, _ = _try(services.http(unit.name).get_json, "/api/system", None, 3.0)
    res["p25_build"] = (http_sys or {}).get("build") if isinstance(http_sys, dict) else None
    res["firmware_family"] = firmware_family(attrs, info)
    res["image"] = detect_image(attrs, info, http_sys if isinstance(http_sys, dict) else None)
    serial = (attrs or {}).get("hw_serial") or (info or {}).get("serial")
    hint = serial_hint(cfg, unit, serial)
    res["serial"] = hint
    res["warnings"] += hint.pop("warnings")
    dna = info_dna(info)
    if dna is None and ssh_ok and res["image"] == "hwval" and res.get("agent_version"):
        hid, _ = _try(services.agent.hwval, unit.name, "id")
        dna = (hid or {}).get("fpga_dna") or (hid or {}).get("dna")
    dcheck = dna_check(unit, dna, last_seen_dna)
    res["dna"] = dcheck
    res["warnings"] += dcheck.pop("warnings")
    if unit.image not in ("unknown",) and res["image"] not in ("unknown", unit.image):
        res["warnings"].append(f"expected image {unit.image}, found {res['image']}")
    if res["firmware_family"] == "factory":
        res["warnings"].append("factory firmware: Tier 0 read-only tests over libiio only; "
                               "no agent unless SSH works")
    res["reachable"] = bool(attrs) or ssh_ok or res["ping_ms"] is not None
    return res


def identity_snapshot(unit: UnitConfig, info: dict[str, Any] | None,
                      iio_attrs: dict[str, str] | None = None,
                      http_system: dict[str, Any] | None = None) -> dict[str, Any]:
    """result.json ``units`` entry: serial, image, build (+ transceiver/label)."""
    info = info or {}
    serial = (iio_attrs or {}).get("hw_serial") or info.get("serial")
    build = info.get("build") or (http_system or {}).get("build") or \
        (iio_attrs or {}).get("fw_version")
    return {
        "serial": serial,
        "image": detect_image(iio_attrs, info, http_system) if (info or iio_attrs or http_system)
        else unit.image,
        "build": build,
        "transceiver": unit.transceiver,
        "label": unit.label,
    }
