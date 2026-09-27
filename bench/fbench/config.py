"""Bench configuration (``bench/config/bench.toml``).

The file describes the units (physical boards with a human label; identity
comes from the PL device DNA when available, with IIO ``hw_serial`` only a
hint because it depends on the SD card inserted — never from the IP, F14),
the RF links with their pads, and the safety limits of design doc section 3.
Every key has a default so a partial file is valid.
"""

from __future__ import annotations

import tomllib
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from . import BENCH_DIR, REPO_ROOT
from .errors import ConfigError

DEFAULT_CONFIG_PATH: Path = BENCH_DIR / "config" / "bench.toml"

#: Supported transceivers and their datasheet tuning/bandwidth limits.
TRANSCEIVER_LIMITS: dict[str, dict[str, float]] = {
    "AD9361": {"lo_min_hz": 70e6, "lo_max_hz": 6e9, "rf_bw_max_hz": 56e6},
    "AD9363": {"lo_min_hz": 325e6, "lo_max_hz": 3.8e9, "rf_bw_max_hz": 20e6},
}

IMAGES: tuple[str, ...] = ("p25", "hwval", "maia", "factory", "unknown")


@dataclass
class SafetyConfig:
    tx_max_dbm: float = 20.0
    rx_abs_max_dbm: float = -10.0
    rx_linear_max_dbm: float = -30.0
    tx_atten_max_db: float = 89.75
    min_pad_db: float = 30.0


@dataclass
class UnitConfig:
    """One physical board.

    Identity: the IIO ``hw_serial`` depends on which SD card/firmware is
    inserted (the same board reports different serials from its Tezuka and
    factory cards), so it is NOT the board identity. ``known_serials`` lists
    the serials this board has been seen with; ``fpga_dna`` (57-bit PL device
    DNA, exposed by the hwval bitstream) is authoritative when known.
    """

    name: str
    label: str = ""
    description: str = ""
    known_serials: list[str] = field(default_factory=list)
    fpga_dna: str = ""
    hw_model: str = ""
    host: str = ""
    usb_ip: str = ""
    eth_ip: str = ""
    via: str | None = None
    forwarder: bool = False
    image: str = "unknown"
    transceiver: str = "AD9361"
    dram_part: str = ""
    has_jp5: bool = False
    ssh_user: str = "root"
    http_port: int = 8080
    iiod_port: int = 30431
    tx_max_dbm: float = 20.0
    rx_abs_max_dbm: float = -10.0
    rx_linear_max_dbm: float = -30.0
    ps_clk_hz: float = 33_333_333.0
    dram_taa_min_ns: float = 13.125
    console_port: str = ""  # FT2232 DEBUG UART (Zynq UART1); "" = auto-detect FTDI
    console_baud: int = 115200
    uboot_env: dict[str, str] = field(default_factory=dict)
    # 40 MHz reference error in ppm (negative = low), e.g. an off-air calibration
    # plus rf.cw_ppm; used to trim replay TX LOs. None = unknown.
    ref_ppm: float | None = None

    @property
    def iio_uri(self) -> str:
        return f"ip:{self.host}"

    @property
    def limits(self) -> dict[str, float]:
        return TRANSCEIVER_LIMITS.get(self.transceiver, TRANSCEIVER_LIMITS["AD9361"])


@dataclass
class LinkConfig:
    """One cabled RF path, e.g. ``A.TX1A -> B.RX1A`` through ``pad_db``."""

    tx: str
    rx: str
    pad_db: float
    note: str = ""

    @staticmethod
    def _split(endpoint: str) -> tuple[str, str]:
        if "." not in endpoint:
            raise ConfigError(f"RF link endpoint {endpoint!r} must be UNIT.PORT (e.g. A.TX1A)")
        unit, port = endpoint.split(".", 1)
        return unit, port

    @property
    def tx_unit(self) -> str:
        return self._split(self.tx)[0]

    @property
    def tx_port(self) -> str:
        return self._split(self.tx)[1]

    @property
    def rx_unit(self) -> str:
        return self._split(self.rx)[0]

    @property
    def rx_port(self) -> str:
        return self._split(self.rx)[1]


@dataclass
class RfConfig:
    cabled_confirmed: bool = False
    test_freq_hz: int = 858_100_000
    p25_cc_freq_hz: int = 860_962_500
    p25_rx_lo_hz: int = 858_100_000
    default_tx: str = "A"
    default_rx: str = "B"
    tx_port: str = "TX1A"
    tx_atten_default_db: float = 40.0
    p25_clip: str = ""
    p25_clip_start_s: float = 0.0
    p25_clip_seconds: float = 0.0  # 0 = 10 s
    # Unit whose uncorrected reference is baked into the site clips ("" = unknown).
    p25_clip_recorder: str = ""
    # SDRTrunk recordings dir (per-call .mbe) for replay ground truth ("" = none).
    p25_truth_dir: str = ""
    links: list[LinkConfig] = field(default_factory=list)


@dataclass
class SshConfig:
    user: str = "root"
    identity: str = "~/.ssh/id_ed25519"
    connect_timeout_s: int = 5
    known_hosts: str = ""  # default: <state_dir>/known_hosts


@dataclass
class AgentConfig:
    remote_root: str = "/mnt/sd/bench"
    binary: str = "bin/fbench-agent"
    local_binary: str = "bench/agent/target/armv7-unknown-linux-musleabihf/release/fbench-agent"
    extra_args: list[str] = field(default_factory=list)
    default_timeout_s: float = 30.0

    @property
    def remote_binary(self) -> str:
        return f"{self.remote_root}/{self.binary}"


@dataclass
class NetworkConfig:
    host_ip: str = "192.168.2.10"
    bench_subnet: str = "10.25.0.0"
    bench_mask: str = "255.255.255.0"
    gateway: str = "192.168.2.1"


@dataclass
class IioConfig:
    backend: str = "auto"  # auto | pylibiio | cli
    rx_device: str = "cf-ad9361-lpc"
    tx_device: str = "cf-ad9361-dds-core-lpc"
    phy_device: str = "ad9361-phy"


@dataclass
class PathsConfig:
    repo_root: Path = REPO_ROOT
    diagnostics_dir: Path = REPO_ROOT / "doc" / "diagnostics"
    state_dir: Path = BENCH_DIR / ".state"
    share_dir: Path = BENCH_DIR / "share"


@dataclass
class BenchConfig:
    path: Path | None
    units: dict[str, UnitConfig]
    default_unit: str
    safety: SafetyConfig
    rf: RfConfig
    ssh: SshConfig
    agent: AgentConfig
    network: NetworkConfig
    iio: IioConfig
    paths: PathsConfig

    def unit(self, name: str) -> UnitConfig:
        if name not in self.units:
            known = ", ".join(self.units) or "(none)"
            raise ConfigError(f"unknown unit {name!r}; configured units: {known}")
        return self.units[name]

    def units_for_serial(self, serial: str) -> list[UnitConfig]:
        """Units whose ``known_serials`` contain ``serial`` (a hint, not identity)."""
        return [u for u in self.units.values() if serial and serial in u.known_serials]

    def unit_by_dna(self, dna: str) -> UnitConfig | None:
        """Authoritative board lookup by PL device DNA (case-insensitive hex)."""
        want = normalize_dna(dna)
        for u in self.units.values():
            if u.fpga_dna and normalize_dna(u.fpga_dna) == want:
                return u
        return None

    def links_from(self, tx_unit: str) -> list[LinkConfig]:
        return [lk for lk in self.rf.links if lk.tx_unit == tx_unit]

    def link_for(self, tx_unit: str, rx_unit: str, tx_port: str | None = None) -> LinkConfig | None:
        for lk in self.rf.links:
            if lk.tx_unit == tx_unit and lk.rx_unit == rx_unit:
                if tx_port is None or lk.tx_port == tx_port:
                    return lk
        return None

    @property
    def known_hosts(self) -> Path:
        if self.ssh.known_hosts:
            return _resolve(self.ssh.known_hosts, self.paths.repo_root)
        return self.paths.state_dir / "known_hosts"

    def to_dict(self) -> dict[str, Any]:
        d = {
            "path": str(self.path) if self.path else None,
            "default_unit": self.default_unit,
            "units": {k: asdict(v) for k, v in self.units.items()},
            "safety": asdict(self.safety),
            "rf": asdict(self.rf),
            "ssh": asdict(self.ssh),
            "agent": asdict(self.agent),
            "network": asdict(self.network),
            "iio": asdict(self.iio),
            "paths": {k: str(v) for k, v in asdict(self.paths).items()},
        }
        return d


def normalize_dna(dna: str | int) -> str:
    """Canonical lower-case hex form of a device DNA ("" when empty)."""
    if isinstance(dna, int):
        return f"0x{dna:x}"
    text = str(dna).strip().lower()
    if not text:
        return ""
    try:
        return f"0x{int(text, 16 if not text.startswith('0x') else 0):x}"
    except ValueError:
        return text


def _resolve(p: str | Path, root: Path) -> Path:
    path = Path(str(p)).expanduser()
    return path if path.is_absolute() else (root / path)


def _take(section: dict[str, Any], cls: type, where: str, **extra: Any) -> Any:
    """Build dataclass ``cls`` from a TOML table, rejecting unknown keys."""
    known = {f for f in cls.__dataclass_fields__}  # type: ignore[attr-defined]
    unknown = set(section) - known
    if unknown:
        raise ConfigError(f"unknown key(s) in [{where}]: {', '.join(sorted(unknown))}")
    try:
        return cls(**section, **extra)
    except TypeError as exc:  # wrong value types / missing required
        raise ConfigError(f"bad [{where}] section: {exc}") from exc


def parse_config(data: dict[str, Any], path: Path | None = None) -> BenchConfig:
    """Validate a parsed TOML document and return a :class:`BenchConfig`."""
    bench = dict(data.get("bench", {}))
    safety = _take(dict(data.get("safety", {})), SafetyConfig, "safety")
    ssh = _take(dict(data.get("ssh", {})), SshConfig, "ssh")
    agent = _take(dict(data.get("agent", {})), AgentConfig, "agent")
    network = _take(dict(data.get("network", {})), NetworkConfig, "network")
    iio = _take(dict(data.get("iio", {})), IioConfig, "iio")
    if iio.backend not in ("auto", "pylibiio", "cli"):
        raise ConfigError(f"[iio] backend must be auto|pylibiio|cli, not {iio.backend!r}")

    paths_raw = dict(data.get("paths", {}))
    unknown = set(paths_raw) - {"diagnostics_dir", "state_dir", "share_dir"}
    if unknown:
        raise ConfigError(f"unknown key(s) in [paths]: {', '.join(sorted(unknown))}")
    paths = PathsConfig()
    for key in ("diagnostics_dir", "state_dir", "share_dir"):
        if key in paths_raw:
            # Relative paths are relative to the repository root.
            setattr(paths, key, _resolve(paths_raw[key], REPO_ROOT))

    units: dict[str, UnitConfig] = {}
    for name, raw in dict(data.get("units", {})).items():
        raw = dict(raw)
        # Per-unit safety values default to the [safety] table.
        raw.setdefault("tx_max_dbm", safety.tx_max_dbm)
        raw.setdefault("rx_abs_max_dbm", safety.rx_abs_max_dbm)
        raw.setdefault("rx_linear_max_dbm", safety.rx_linear_max_dbm)
        raw.setdefault("ssh_user", ssh.user)
        unit = _take(raw, UnitConfig, f"units.{name}", name=name)
        if not unit.host:
            unit.host = unit.eth_ip or unit.usb_ip
        if not unit.host:
            raise ConfigError(f"[units.{name}] needs host, usb_ip or eth_ip")
        if unit.transceiver not in TRANSCEIVER_LIMITS:
            raise ConfigError(
                f"[units.{name}] transceiver must be one of {sorted(TRANSCEIVER_LIMITS)}"
            )
        if unit.image not in IMAGES:
            raise ConfigError(f"[units.{name}] image must be one of {IMAGES}")
        units[name] = unit
    if not units:
        raise ConfigError("config defines no [units.*]")
    for u in units.values():
        if u.via and u.via not in units:
            raise ConfigError(f"[units.{u.name}] via={u.via!r} is not a configured unit")

    rf_raw = dict(data.get("rf", {}))
    links_raw = rf_raw.pop("links", [])
    rf = _take(rf_raw, RfConfig, "rf")
    for i, lk in enumerate(links_raw):
        link = _take(dict(lk), LinkConfig, f"rf.links[{i}]")
        for unit_name in (link.tx_unit, link.rx_unit):
            if unit_name not in units:
                raise ConfigError(f"rf.links[{i}] references unknown unit {unit_name!r}")
        if not link.tx_port.upper().startswith("TX") or not link.rx_port.upper().startswith("RX"):
            raise ConfigError(f"rf.links[{i}] must go from a TX port to an RX port")
        rf.links.append(link)
    if rf.p25_clip_recorder and rf.p25_clip_recorder not in units:
        raise ConfigError(f"[rf] p25_clip_recorder={rf.p25_clip_recorder!r} is not a "
                          "configured unit")

    default_unit = str(bench.get("default_unit", next(iter(units))))
    if default_unit not in units:
        raise ConfigError(f"[bench] default_unit={default_unit!r} is not a configured unit")
    return BenchConfig(
        path=path,
        units=units,
        default_unit=default_unit,
        safety=safety,
        rf=rf,
        ssh=ssh,
        agent=agent,
        network=network,
        iio=iio,
        paths=paths,
    )


def load_config(path: str | Path | None = None) -> BenchConfig:
    """Load ``path`` (default ``bench/config/bench.toml``)."""
    cfg_path = Path(path) if path else DEFAULT_CONFIG_PATH
    if not cfg_path.exists():
        raise ConfigError(f"config file not found: {cfg_path}")
    try:
        with open(cfg_path, "rb") as fh:
            data = tomllib.load(fh)
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"cannot parse {cfg_path}: {exc}") from exc
    return parse_config(data, cfg_path)
