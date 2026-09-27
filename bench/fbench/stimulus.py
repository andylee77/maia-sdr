"""RF stimulus (TX) and capture (RX) helpers for the rf.* tests.

TX tone sources, in order of preference for ``stimulus="auto"``:

1. ``dds`` — the axi_ad9361 DDS (``cf-ad9361-dds-core-lpc`` altvoltage
   frequency/scale/phase/raw). Present on the hwval image and on factory
   firmware; compiled out on the P25 image (scale writes have no effect).
2. ``pattern`` — the DAC pattern generator via the agent (DATA_SEL=1 with
   PAT_DATA_1 == PAT_DATA_2 gives a constant I, i.e. a CW at the TX LO). The
   TX LO is offset by ``offset_hz`` so the tone lands away from the RX LO.
3. ``cyclic`` — a cyclic IIO TX buffer pushed from the host (libiio).

Rule 3 of the design doc is implemented here: the TX attenuation is written
first, then the source is enabled; the runner issues ``tx off`` afterwards.
"""

from __future__ import annotations

import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np

from .analysis import sigmf
from .analysis.tone import AD9361_FULL_SCALE, iq_from_ci16
from .errors import PreconditionError, UsageError

DDS_I_F1, DDS_I_F2, DDS_Q_F1, DDS_Q_F2 = "altvoltage0", "altvoltage1", "altvoltage2", "altvoltage3"
TX_LO_CHAN = "altvoltage1"
RX_LO_CHAN = "altvoltage0"
PATTERN_REGS = ("DAC_CHAN0_CNTRL_7", "DAC_CHAN1_CNTRL_7")


def parse_number(text: str) -> float:
    """``"105.25 dB"`` / ``"8000000"`` -> float."""
    return float(str(text).strip().split()[0])


def check_transceiver(ctx: Any, unit: str, lo_hz: float, rf_bw_hz: float | None = None) -> None:
    """Warn outside the LO range; refuse RF bandwidth above the transceiver limit."""
    u = ctx.cfg.unit(unit)
    lim = u.limits
    if not lim["lo_min_hz"] <= lo_hz <= lim["lo_max_hz"]:
        ctx.warn(f"unit {unit} ({u.transceiver}): LO {lo_hz / 1e6:.3f} MHz outside the "
                 f"specified {lim['lo_min_hz'] / 1e6:g}..{lim['lo_max_hz'] / 1e6:g} MHz "
                 "(driver may allow it; performance unspecified)")
    if rf_bw_hz is not None and rf_bw_hz > lim["rf_bw_max_hz"]:
        raise UsageError(f"unit {unit} ({u.transceiver}): RF bandwidth {rf_bw_hz / 1e6:g} MHz "
                         f"exceeds {lim['rf_bw_max_hz'] / 1e6:g} MHz")


def reverse_direction_note(ctx: Any) -> None:
    """Recommend the opposite direction so transceiver and board effects separate."""
    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    if ctx.cfg.link_for(rx, tx) is not None:
        ctx.warnings.append(
            f"direction {tx}->{rx} ({ctx.cfg.unit(tx).transceiver} TX, "
            f"{ctx.cfg.unit(rx).transceiver} RX): also run --tx {rx} --rx {tx} to separate "
            "transceiver from board effects")


def direction_metrics(ctx: Any) -> None:
    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    ctx.metric("direction", f"{tx}->{rx}")
    ctx.metric("tx_unit", tx)
    ctx.metric("rx_unit", rx)
    ctx.metric("tx_transceiver", ctx.cfg.unit(tx).transceiver)
    ctx.metric("rx_transceiver", ctx.cfg.unit(rx).transceiver)


@dataclass
class RxState:
    lo_hz: float
    fs_hz: float
    gain_mode: str | None = None
    gain_db: float | None = None


def rx_state(ctx: Any, unit: str) -> RxState:
    phy = ctx.cfg.iio.phy_device
    lo = parse_number(ctx.iio_get(unit, phy, "frequency", RX_LO_CHAN, True))
    fs = parse_number(ctx.iio_get(unit, phy, "sampling_frequency", "voltage0", False))
    try:
        mode = ctx.iio_get(unit, phy, "gain_control_mode", "voltage0", False).strip()
        gain = parse_number(ctx.iio_get(unit, phy, "hardwaregain", "voltage0", False))
    except Exception:  # noqa: BLE001 - optional
        mode, gain = None, None
    return RxState(lo, fs, mode, gain)


def set_rx_gain(ctx: Any, unit: str, mode: str | None, gain_db: float | None) -> None:
    phy = ctx.cfg.iio.phy_device
    if mode:
        ctx.iio_set(unit, phy, "gain_control_mode", mode, "voltage0", False)
    if gain_db is not None and (mode or "manual") == "manual":
        ctx.iio_set(unit, phy, "hardwaregain", f"{gain_db:g}", "voltage0", False)


def rssi_db(ctx: Any, unit: str) -> float | None:
    try:
        return parse_number(ctx.iio_get(unit, ctx.cfg.iio.phy_device, "rssi", "voltage0", False))
    except Exception:  # noqa: BLE001
        return None


class ToneSource:
    """A CW source on one unit (see module docstring for methods)."""

    def __init__(self, ctx: Any, unit: str, method: str = "auto", level: float = 0.25) -> None:
        self.ctx = ctx
        self.unit = unit
        self.level = level
        self.method = self._choose(method)
        self.lo_hz: float | None = None
        self.offset_hz: float = 0.0
        self._saved: dict[str, int] = {}
        self._tx_handle: Any = None
        self._tmp: Path | None = None
        self.active = False

    def _choose(self, method: str) -> str:
        if method != "auto":
            if method not in ("dds", "pattern", "cyclic"):
                raise UsageError(f"stimulus must be auto|dds|pattern|cyclic, not {method!r}")
            if method == "pattern":
                self.ctx.require_agent(self.unit)
            return method
        image = self.ctx.caps(self.unit)["image"]
        if image in ("hwval", "factory"):
            return "dds"
        if self.ctx.has_agent(self.unit):
            return "pattern"
        return "cyclic"

    # -- configuration --------------------------------------------------------
    def set_atten(self, atten_db: float) -> None:
        # Less than max attenuation "enables TX" for the agent: --tx-ok (the
        # runner's interlock has already passed for this test).
        self.ctx.iio_set(self.unit, self.ctx.cfg.iio.phy_device, "hardwaregain",
                         f"{-abs(atten_db):g}", "voltage0", True, tx_ok=True)

    def configure(self, lo_hz: float, atten_db: float) -> None:
        """Attenuation FIRST (rule 3), then the TX LO."""
        self.set_atten(atten_db)
        self.lo_hz = lo_hz
        self.ctx.iio_set(self.unit, self.ctx.cfg.iio.phy_device, "frequency",
                         f"{int(round(lo_hz))}", TX_LO_CHAN, True)

    def tx_fs(self) -> float:
        return parse_number(self.ctx.iio_get(self.unit, self.ctx.cfg.iio.phy_device,
                                             "sampling_frequency", "voltage0", True))

    # -- start/stop ------------------------------------------------------------
    def start(self, offset_hz: float) -> float:
        """Enable a tone at ``LO + offset``; returns the actual baseband offset."""
        if self.lo_hz is None:
            raise UsageError("ToneSource.configure() must run before start()")
        self.offset_hz = offset_hz
        self.ctx.mark_tx_active(self.unit)
        self.active = True
        if self.method == "dds":
            self._start_dds(offset_hz)
        elif self.method == "pattern":
            # CW at the TX LO: move the LO by the offset.
            self.ctx.iio_set(self.unit, self.ctx.cfg.iio.phy_device, "frequency",
                             f"{int(round(self.lo_hz + offset_hz))}", TX_LO_CHAN, True)
            self._start_pattern()
        else:
            self.offset_hz = self._start_cyclic(offset_hz)
        self.ctx.log.info("tone on %s via %s at LO %+.0f Hz", self.unit, self.method,
                          self.offset_hz)
        return self.offset_hz

    def _start_dds(self, offset_hz: float) -> None:
        dev, u = self.ctx.cfg.iio.tx_device, self.unit
        for chan in (DDS_I_F2, DDS_Q_F2):
            self.ctx.iio_set(u, dev, "scale", "0", chan, True)
        for chan, phase in ((DDS_I_F1, 90000), (DDS_Q_F1, 0)):
            self.ctx.iio_set(u, dev, "frequency", f"{int(round(offset_hz))}", chan, True)
            self.ctx.iio_set(u, dev, "phase", str(phase), chan, True)
            self.ctx.iio_set(u, dev, "scale", f"{self.level:g}", chan, True, tx_ok=True)
        self.ctx.iio_set(u, dev, "raw", "1", DDS_I_F1, True, tx_ok=True)

    def _start_pattern(self) -> None:
        agent, u = self.ctx.agent, self.unit
        for reg in PATTERN_REGS:
            self._saved[reg] = agent.reg_read(u, "adi_dac", reg)
        amp = int(self.level * 32767) & 0xFFFF
        agent.reg_write(u, "adi_dac", "DAC_CHAN0_PAT_DATA", (amp << 16) | amp, tx_ok=True)
        agent.reg_write(u, "adi_dac", "DAC_CHAN1_PAT_DATA", 0, tx_ok=True)
        for reg in PATTERN_REGS:
            agent.reg_write(u, "adi_dac", reg, 1, tx_ok=True)
        agent.reg_write(u, "adi_dac", "DAC_CNTRL_1", 1, tx_ok=True)

    def _start_cyclic(self, offset_hz: float, n: int = 1 << 16) -> float:
        fs = self.tx_fs()
        k = int(round(offset_hz * n / fs))
        actual = k * fs / n
        t = np.arange(n)
        tone = self.level * 32767 * np.exp(2j * np.pi * k * t / n)
        inter = np.empty(2 * n, dtype="<i2")
        inter[0::2] = np.round(tone.real)
        inter[1::2] = np.round(tone.imag)
        fd, name = tempfile.mkstemp(prefix="fbench_tone_", suffix=".cs16")
        with open(fd, "wb") as fh:
            fh.write(inter.tobytes())
        self._tmp = Path(name)
        self._tx_handle = self.ctx.services.iio(self.unit).start_cyclic_tx(
            self.ctx.cfg.iio.tx_device, ["voltage0", "voltage1"], self._tmp, n)
        return actual

    def stop(self) -> None:
        """Stop the source (the runner's ``tx off`` follows regardless)."""
        if not self.active:
            return
        try:
            if self.method == "dds":
                dev = self.ctx.cfg.iio.tx_device
                for chan in (DDS_I_F1, DDS_I_F2, DDS_Q_F1, DDS_Q_F2):
                    self.ctx.iio_set(self.unit, dev, "scale", "0", chan, True)
            elif self.method == "pattern":
                for reg, val in self._saved.items():
                    self.ctx.agent.reg_write(self.unit, "adi_dac", reg, val, tx_ok=True)
            elif self._tx_handle is not None:
                self._tx_handle.stop()
        finally:
            self.set_atten(self.ctx.cfg.safety.tx_atten_max_db)
            self.active = False
            if self._tmp is not None:
                self._tmp.unlink(missing_ok=True)


def capture_iq(ctx: Any, unit: str, nsamples: int, name: str, method: str = "auto",
               lo_hz: float | None = None, fs_hz: float | None = None) -> tuple[np.ndarray, float]:
    """Capture ``nsamples`` on ``unit`` into ``artifacts/<name>.sigmf-*``.

    ``method``: ``iio`` (host libiio, one contiguous buffer) or ``ring``
    (agent ``ring capture`` of the P25 wideband ring). Returns IQ with full
    scale 1.0 and the sample rate.
    """
    st = None
    if lo_hz is None or fs_hz is None:
        st = rx_state(ctx, unit)
    lo = lo_hz if lo_hz is not None else st.lo_hz  # type: ignore[union-attr]
    fs = fs_hz if fs_hz is not None else st.fs_hz  # type: ignore[union-attr]
    if method == "auto":
        method = "iio"
    raw_path = ctx.artifact_path(f"{name}.sigmf-data")
    if method == "iio":
        ctx.services.iio(unit).capture(ctx.cfg.iio.rx_device, ["voltage0", "voltage1"],
                                       int(nsamples), raw_path, timeout=60.0)
    elif method == "ring":
        ctx.require_agent(unit)
        remote = f"{ctx.remote_run_dir(unit)}/{name}.cs16"
        reply = ctx.agent.ring_capture(unit, "p25-wideband", int(nsamples) * 4, "uncached", remote)
        ctx.services.ssh(unit).get(reply.get("path", remote), raw_path, 300.0)
        fs = float(reply.get("sample_rate_hz", fs))
    else:
        raise UsageError(f"capture must be auto|iio|ring, not {method!r}")
    data = raw_path.read_bytes()
    if not data:
        raise PreconditionError(f"capture on {unit} returned no data")
    sigmf.write(raw_path, data, fs, lo, "ci16_le", f"fbench capture {name} on {unit}")
    ctx.artifact_path(f"{name}.sigmf-meta")
    return iq_from_ci16(data, AD9361_FULL_SCALE), fs


def load_capture(actx: Any, name: str) -> tuple[np.ndarray, float, float | None]:
    """Load ``artifacts/<name>.sigmf-*`` as full-scale-1.0 IQ."""
    base = actx.artifacts_dir / name
    if not base.with_suffix(".sigmf-meta").exists():
        from .errors import Inconclusive

        raise Inconclusive(f"capture {name} missing")
    iq, meta = sigmf.read(base)
    caps = meta.get("captures") or [{}]
    return (iq / AD9361_FULL_SCALE, float(meta["global"]["core:sample_rate"]),
            caps[0].get("core:frequency"))
