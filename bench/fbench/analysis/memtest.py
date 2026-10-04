"""Decoding of hwval AXI memory-tester (mt0/mt1) results.

The bit-exact pattern reference lives in ``scanner-hdl/hwval_hdl/axi_memtest.py``
(``expected_beat``, ``pattern_pass_index``, ``lat_bin``, ``MODE_*``/``PAT_*``).
It imports amaranth, so it is loaded lazily and guarded: without it the
cross-check of the agent's expected value is skipped (a warning, not an
error).

Units: counters are 125 MHz ``mem``-domain cycles (8 ns). Latency histogram
bin k counts latencies in [2^k, 2^(k+1)) cycles (bin 0: <= 1 cycle).
"""

from __future__ import annotations

from typing import Any

MEM_CLK_HZ = 125e6
CYCLE_NS = 1e9 / MEM_CLK_HZ  # 8 ns
BEAT_BYTES = 8
# Fallback constants mirroring axi_memtest (used only when the import fails).
MODE_NAMES = {0: "write", 1: "read_verify", 2: "write_verify", 3: "read", 4: "byte_lane"}
PATTERN_NAMES = {0: "address", 1: "walk1", 2: "walk0", 3: "checker", 4: "prbs", 5: "zero",
                 6: "ones", 7: "toggle"}

def reference() -> Any:
    """``hwval_hdl.axi_memtest`` or ``None`` when it cannot be imported."""
    from .hwref import load

    return load("axi_memtest")


def idle_cycles_for_duty(burst_len: int, duty_pct: float) -> int | None:
    """``IDLE_CYCLES`` for an offered load of ``duty_pct`` (None = aggressor off).

    Offered load ~ burst_len / (burst_len + idle_cycles).
    """
    if duty_pct <= 0:
        return None
    if duty_pct >= 100:
        return 0
    d = duty_pct / 100.0
    return max(0, int(round(burst_len * (1 - d) / d)))


def bandwidth_mbs(bytes_: int, cycles: int) -> float | None:
    if not cycles:
        return None
    return bytes_ / (cycles * CYCLE_NS * 1e-9) / 1e6


def hist_percentile_ns(hist: list[int], q: float) -> float | None:
    """Upper edge (ns) of the log2 bin holding quantile ``q`` of a histogram."""
    total = sum(hist)
    if not total:
        return None
    acc = 0
    for k, n in enumerate(hist):
        acc += n
        if acc >= q * total:
            return (2 ** (k + 1)) * CYCLE_NS
    return (2 ** len(hist)) * CYCLE_NS


def _as_int(v: Any) -> int | None:
    if v is None:
        return None
    return v if isinstance(v, int) else int(str(v), 0)


def decode_first_error(run: dict[str, Any]) -> dict[str, Any] | None:
    """Failing bits of the first error: 64-bit beat bit -> DQ line / byte lane.

    A 64-bit AXI beat is two transfers on the x32 DDR bus, so beat bit ``b``
    maps to DQ ``b % 32`` and data-mask byte lane ``(b % 32) // 8``. When the
    reference module is available, the agent's expected value is checked
    against ``expected_beat(pattern, addr, pattern_pass_index(mode, pass), seed)``.
    """
    if not int(run.get("err_count", 0) or 0):
        return None
    addr = _as_int(run.get("first_err_addr"))
    exp = _as_int(run.get("first_err_exp"))
    act = _as_int(run.get("first_err_act"))
    out: dict[str, Any] = {"addr": hex(addr) if addr is not None else None}
    if exp is None or act is None:
        return out
    diff = exp ^ act
    bits = [b for b in range(64) if diff >> b & 1]
    out.update({
        "expected": f"0x{exp:016X}", "actual": f"0x{act:016X}", "xor": f"0x{diff:016X}",
        "bits": bits,
        "dq_lines": sorted({b % 32 for b in bits}),
        "byte_lanes": sorted({(b % 32) // 8 for b in bits}),
    })
    ref = reference()
    mode, pattern = run.get("mode"), run.get("pattern")
    if ref is not None and addr is not None and isinstance(pattern, int) and \
            isinstance(mode, int) and mode != 4:
        pidx = ref.pattern_pass_index(mode, int(run.get("first_err_pass", 0) or 0))
        want = ref.expected_beat(pattern, addr, pidx, int(_as_int(run.get("seed")) or 0))
        out["reference_expected"] = f"0x{want:016X}"
        out["reference_agrees"] = want == exp
    return out
