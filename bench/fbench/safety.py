"""RF safety interlock (design doc section 3).

Worst-case received power: ``P_rx = P_tx_max - tx_atten - pad_db``.

- Refuse (exit 4) when ``rf.cabled_confirmed`` is false, when no link entry
  exists for the TX unit/port, when ``tx_atten`` is outside
  ``[0, tx_atten_max_db]``, or when ``P_rx > rx_abs_max_dbm`` (strictly
  greater: a budget exactly at the limit is allowed).
- Warn (allowed) when ``P_rx > rx_linear_max_dbm`` or a pad is below
  ``min_pad_db``.

Example with the default bench: A -> B at 0 dB attenuation gives
20 - 0 - 30 = -10 dBm; -10 > -10 is false, so the level check passes with a
"above linear range" warning. With ``cabled_confirmed = false`` (the shipped
default) the interlock still refuses.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field

from .config import BenchConfig
from .errors import SafetyRefusal


@dataclass
class LinkBudget:
    tx: str
    rx: str
    tx_unit: str
    rx_unit: str
    pad_db: float | None
    tx_max_dbm: float
    tx_atten_db: float
    p_rx_dbm: float | None
    rx_abs_max_dbm: float
    rx_linear_max_dbm: float
    cabled_confirmed: bool
    level_ok: bool
    linear_ok: bool
    allowed: bool
    reasons: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict:
        return asdict(self)


def link_budget(cfg: BenchConfig, tx_unit: str, rx_unit: str, tx_atten_db: float,
                tx_port: str | None = None) -> LinkBudget:
    """Compute the worst-case budget for ``tx_unit -> rx_unit``."""
    tx = cfg.unit(tx_unit)
    rx = cfg.unit(rx_unit)
    port = tx_port or cfg.rf.tx_port
    link = cfg.link_for(tx_unit, rx_unit, port)
    reasons: list[str] = []
    warnings: list[str] = []

    if not cfg.rf.cabled_confirmed:
        reasons.append("rf.cabled_confirmed is false: confirm pads fitted, antennas removed, "
                       "unused ports terminated, then set it to true in the bench config")
    if not 0.0 <= tx_atten_db <= cfg.safety.tx_atten_max_db:
        reasons.append(f"tx_atten {tx_atten_db:g} dB outside [0, {cfg.safety.tx_atten_max_db:g}]")

    pad = link.pad_db if link else None
    p_rx = None
    level_ok = False
    linear_ok = False
    if link is None:
        reasons.append(f"no rf.links entry for {tx_unit}.{port} -> {rx_unit}")
    else:
        p_rx = tx.tx_max_dbm - tx_atten_db - link.pad_db
        level_ok = p_rx <= rx.rx_abs_max_dbm
        linear_ok = p_rx <= rx.rx_linear_max_dbm
        if not level_ok:
            reasons.append(
                f"worst-case P_rx {p_rx:+.2f} dBm > rx_abs_max {rx.rx_abs_max_dbm:+.2f} dBm "
                f"(tx_max {tx.tx_max_dbm:+.2f} - atten {tx_atten_db:g} - pad {link.pad_db:g}); "
                f"raise tx_atten to >= {tx.tx_max_dbm - link.pad_db - rx.rx_abs_max_dbm:g} dB"
            )
        elif not linear_ok:
            warnings.append(
                f"worst-case P_rx {p_rx:+.2f} dBm above the linear limit "
                f"{rx.rx_linear_max_dbm:+.2f} dBm (allowed; expect compression)"
            )
        if link.pad_db < cfg.safety.min_pad_db:
            warnings.append(f"pad {link.pad_db:g} dB below the recommended "
                            f"{cfg.safety.min_pad_db:g} dB")
    return LinkBudget(
        tx=link.tx if link else f"{tx_unit}.{port}",
        rx=link.rx if link else f"{rx_unit}.?",
        tx_unit=tx_unit,
        rx_unit=rx_unit,
        pad_db=pad,
        tx_max_dbm=tx.tx_max_dbm,
        tx_atten_db=tx_atten_db,
        p_rx_dbm=p_rx,
        rx_abs_max_dbm=rx.rx_abs_max_dbm,
        rx_linear_max_dbm=rx.rx_linear_max_dbm,
        cabled_confirmed=cfg.rf.cabled_confirmed,
        level_ok=level_ok,
        linear_ok=linear_ok,
        allowed=not reasons,
        reasons=reasons,
        warnings=warnings,
    )


def require_tx_allowed(cfg: BenchConfig, tx_unit: str, rx_unit: str, tx_atten_db: float,
                       tx_port: str | None = None) -> LinkBudget:
    """Return the budget or raise :class:`SafetyRefusal` (exit 4)."""
    budget = link_budget(cfg, tx_unit, rx_unit, tx_atten_db, tx_port)
    if not budget.allowed:
        raise SafetyRefusal("TX interlock refused: " + "; ".join(budget.reasons),
                            budget=budget.to_dict())
    return budget


def all_budgets(cfg: BenchConfig, tx_atten_db: float) -> list[LinkBudget]:
    """Budgets for every configured link at ``tx_atten_db``."""
    return [link_budget(cfg, lk.tx_unit, lk.rx_unit, tx_atten_db, lk.tx_port)
            for lk in cfg.rf.links]


def min_atten(values: list[float] | float) -> float:
    """Smallest attenuation of a sweep (the worst case for the interlock)."""
    if isinstance(values, (int, float)):
        return float(values)
    return float(min(values))
