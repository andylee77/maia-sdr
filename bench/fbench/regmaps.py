"""Register maps (schema ``fbench.regmap/1``) for the agent allow-lists.

``fbench regmaps build`` writes:

- ``share/p25_regs.json`` — ``p25_core`` at 0x7C46_0000 from
  ``p25-httpd/p25-pac/p25.svd``. Read-to-clear (Rsticky) registers carry
  ``"read_side_effect": true``. Vacant banks 0x120-0x17F and 0x1E0-0x1FF are
  forbidden simply by not being listed (reading them hangs AXI-Lite, F15).
- ``share/adi_regs.json`` — ``axi_ad9361`` ADC/DAC cores and the RX/TX
  ``axi_dmac`` key registers.
- ``share/ps_regs.json`` — SLCR/DDRC/L2C audit registers, with ``expected``
  values where known. PS registers are marked ``ro``: the bench never writes
  them.

Single-core files use the schema verbatim. Files with several cores (adi, ps)
wrap them as ``{"schema": "fbench.regmap/1", "cores": [<core map>, ...]}``;
each core map is itself a complete ``fbench.regmap/1`` object. The loader
accepts both forms.

``expected`` is ``null``, a hex string (whole register) or
``{"mask": "0x…", "value": "0x…"}``.

Agent built-ins: the agent embeds richer default maps (``bench/agent/maps``)
and a share file *replaces* the built-in core of the same name. So when those
built-ins are present, the ADI/PS maps are generated as a merge: the agent's
registers and names are the base, the host adds ``expected`` values,
``read_side_effect`` flags, more restrictive access, extra registers (e.g.
DDRIOB) and ``aliases`` (the host's name where the two differ). The output is
always a superset of the agent's built-in allow-list.
"""

from __future__ import annotations

import json
import re
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from . import BENCH_DIR, REPO_ROOT
from .errors import ConfigError, SafetyRefusal

SCHEMA = "fbench.regmap/1"
P25_SVD = REPO_ROOT / "p25-httpd" / "p25-pac" / "p25.svd"
P25_BASE = 0x7C460000
P25_WINDOW = 0x200

#: p25_core registers whose read clears Rsticky bits (finding F2).
P25_READ_SIDE_EFFECT = frozenset({0x0C, 0xA4, 0xC4, 0x60, 0x80, 0xE0, 0x184, 0x1A0, 0x1C0})
#: p25_core banks (32-byte aligned) -> block name.
P25_BANKS: dict[int, str] = {
    0x000: "control",
    0x020: "ddc",
    0x040: "traffic_ddc",
    0x060: "traffic_iq",
    0x080: "iq",
    0x0A0: "lsm",
    0x0C0: "traffic_lsm",
    0x0E0: "wideband_iq",
    0x100: "lsm_seeds",
    0x180: "spectrometer",
    0x1A0: "pre_diff_iq",
    0x1C0: "traffic_pre_diff_iq",
}
P25_VACANT = ((0x120, 0x180), (0x1E0, 0x200))
P25_EXPECTED = {0x0: "0x70323566"}  # product_id "p25f"

_ACCESS = {"read-only": "ro", "read-write": "rw", "write-only": "wo",
           "writeOnce": "wo", "read-writeOnce": "rw"}


def _hex(v: int) -> str:
    return f"0x{v:X}"


def reg(name: str, offset: int, access: str = "ro", desc: str = "",
        fields: list[dict] | None = None, reset: int = 0, expected: Any = None,
        read_side_effect: bool = False) -> dict[str, Any]:
    return {
        "name": name,
        "offset": _hex(offset),
        "access": access,
        "width": 32,
        "reset": _hex(reset),
        "snapshot": None,
        "desc": desc,
        "fields": fields or [],
        "read_side_effect": read_side_effect,
        "expected": expected,
    }


def fld(name: str, lsb: int, width: int, access: str = "rw", desc: str = "") -> dict[str, Any]:
    return {"name": name, "lsb": lsb, "width": width, "access": access, "desc": desc}


def core(name: str, base: int, size: int, blocks: list[dict]) -> dict[str, Any]:
    return {"schema": SCHEMA, "core": name, "base": _hex(base), "size": size, "blocks": blocks}


def block(name: str, offset: int, regs: list[dict]) -> dict[str, Any]:
    return {"name": name, "offset": _hex(offset), "regs": regs}


# ---------------------------------------------------------------------------
# p25_core from SVD
# ---------------------------------------------------------------------------


def _bit_range(text: str) -> tuple[int, int]:
    m = re.fullmatch(r"\[(\d+):(\d+)\]", text.strip())
    if not m:
        raise ConfigError(f"bad SVD bitRange {text!r}")
    hi, lo = int(m.group(1)), int(m.group(2))
    return lo, hi - lo + 1


def build_p25(svd_path: Path = P25_SVD) -> dict[str, Any]:
    root = ET.parse(svd_path).getroot()
    banks: dict[int, list[dict]] = {}
    for r in root.iter("register"):
        name = (r.findtext("name") or "").strip()
        off = int((r.findtext("addressOffset") or "0").strip(), 0)
        if any(lo <= off < hi for lo, hi in P25_VACANT):
            raise ConfigError(f"SVD register {name} at 0x{off:X} lies in a vacant bank")
        access = _ACCESS.get((r.findtext("access") or "read-write").strip(), "rw")
        fields = []
        for f in r.iter("field"):
            lsb, width = _bit_range(f.findtext("bitRange") or "[31:0]")
            faccess = _ACCESS.get((f.findtext("access") or "").strip(), access)
            fields.append(fld((f.findtext("name") or "").strip(), lsb, width, faccess,
                              (f.findtext("description") or "").strip()))
        side = off in P25_READ_SIDE_EFFECT
        desc = (r.findtext("description") or name).strip()
        if side:
            desc += " — READ CLEARS Rsticky bits (F2); read only when intended"
        banks.setdefault(off & ~0x1F, []).append(
            reg(name, off, access, desc, fields, expected=P25_EXPECTED.get(off),
                read_side_effect=side)
        )
    blocks = []
    for bank_off in sorted(banks):
        bname = P25_BANKS.get(bank_off, f"bank_{bank_off:03x}")
        regs = sorted(banks[bank_off], key=lambda x: int(x["offset"], 16))
        blocks.append(block(bname, bank_off, regs))
    m = core("p25", P25_BASE, P25_WINDOW, blocks)
    # Defence in depth: the agent also honours explicit vacant ranges.
    m["vacant"] = [[_hex(lo), _hex(hi)] for lo, hi in P25_VACANT]
    return m


# ---------------------------------------------------------------------------
# ADI axi_ad9361 + axi_dmac
# ---------------------------------------------------------------------------

AD9361_BASE = 0x79020000
RX_DMAC_BASE = 0x7C400000
TX_DMAC_BASE = 0x7C420000


def _adc_channel(ch: int) -> list[dict]:
    b = 0x0400 + ch * 0x40
    return [
        reg(f"CHAN{ch}_CNTRL", b + 0x00, "rw", "ADC channel control", [
            fld("ENABLE", 0, 1), fld("PN_TYPE_OWR", 1, 1), fld("FORMAT_ENABLE", 4, 1),
            fld("FORMAT_TYPE", 5, 1), fld("FORMAT_SIGNEXT", 6, 1), fld("DCFILT_ENB", 8, 1),
            fld("IQCOR_ENB", 9, 1), fld("PN_SEL_OWR", 10, 1), fld("LB_OWR", 11, 1)]),
        reg(f"CHAN{ch}_STATUS", b + 0x04, "w1c", "PN monitor status (write 1 to clear)", [
            fld("OVER_RANGE", 0, 1, "w1c"), fld("PN_OOS", 1, 1, "w1c"),
            fld("PN_ERR", 2, 1, "w1c")]),
        reg(f"CHAN{ch}_CNTRL_1", b + 0x10, "rw", "DC filter offset/coefficient", [
            fld("DCFILT_OFFSET", 16, 16), fld("DCFILT_COEFF", 0, 16)]),
        reg(f"CHAN{ch}_CNTRL_2", b + 0x14, "rw", "IQ correction coefficients", [
            fld("IQCOR_COEFF_1", 16, 16), fld("IQCOR_COEFF_2", 0, 16)]),
        reg(f"CHAN{ch}_CNTRL_3", b + 0x18, "rw", "PN select / data select", [
            fld("ADC_PN_SEL", 16, 4, desc="0 PN9, 1 PN23A, 4 PN7, 5 PN15, 6 PN23, 7 PN31, "
                                          "9 custom, 11 ramp"),
            fld("ADC_DATA_SEL", 0, 4)]),
    ]


def _dac_channel(ch: int) -> list[dict]:
    b = 0x4400 + ch * 0x40
    return [
        reg(f"DAC_CHAN{ch}_PAT_DATA", b + 0x10, "rw",
            "Pattern data; PAT_DATA_1 == PAT_DATA_2 with DATA_SEL=1 gives CW at the TX LO", [
                fld("PAT_DATA_2", 16, 16), fld("PAT_DATA_1", 0, 16)]),
        reg(f"DAC_CHAN{ch}_CNTRL_6", b + 0x14, "rw", "IQ correction", [
            fld("IQCOR_ENB", 2, 1), fld("IQCOR_COEFF_1", 16, 16)]),
        reg(f"DAC_CHAN{ch}_CNTRL_7", b + 0x18, "rw", "Data select", [
            fld("DAC_DATA_SEL", 0, 4, desc="0 DDS, 1 SED/pattern, 2 DMA, 3 zero, 6 PN7, "
                                           "7 PN15, 8 loopback, 10 PN23, 11 PN31")]),
    ]


def build_adi() -> dict[str, Any]:
    adc_common = [
        reg("VERSION", 0x0000, "ro", "Core version"),
        reg("SCRATCH", 0x0008, "rw", "Scratch (bus test)"),
        reg("CONFIG", 0x000C, "ro", "Synthesis configuration"),
        reg("RSTN", 0x0040, "rw", "Core reset (writing 0 resets the ADC interface)", [
            fld("RSTN", 0, 1), fld("MMCM_RSTN", 1, 1)]),
        reg("CNTRL", 0x0044, "rw", "Interface control", [
            fld("PIN_MODE", 0, 1), fld("DDR_EDGESEL", 1, 1), fld("R1_MODE", 2, 1),
            fld("SYNC", 3, 1)]),
        reg("CLK_FREQ", 0x0054, "ro",
            "Interface clock frequency: f = value * 100 MHz / 2^16 (1.526 kHz per count)"),
        reg("CLK_RATIO", 0x0058, "ro", "Interface clock ratio"),
        reg("STATUS", 0x005C, "ro", "Interface status", [fld("STATUS", 0, 1, "ro")]),
        reg("UI_STATUS", 0x0088, "w1c", "DMA status (write 1 to clear)", [
            fld("DMA_STATUS", 0, 1, "ro"), fld("DMA_UNF", 1, 1, "w1c"),
            fld("DMA_OVF", 2, 1, "w1c")]),
    ]
    idelay = [reg(f"IDELAY_LANE{i}", 0x0800 + 4 * i, "rw", f"FPGA IDELAY tap, lane {i}",
                  [fld("DELAY", 0, 5)]) for i in range(7)]
    adc = core("adi_adc", AD9361_BASE, 0x4000, [
        block("adc", 0x0000, adc_common),
        block("adc_chan0", 0x0400, _adc_channel(0)),
        block("adc_chan1", 0x0440, _adc_channel(1)),
        block("adc_idelay", 0x0800, idelay),
    ])
    dac_common = [
        reg("DAC_VERSION", 0x4000, "ro", "Core version"),
        reg("DAC_SCRATCH", 0x4008, "rw", "Scratch (bus test)"),
        reg("DAC_CONFIG", 0x400C, "ro", "Synthesis configuration"),
        reg("DAC_CNTRL_1", 0x4044, "rw", "Control 1", [fld("SYNC", 0, 1)]),
        reg("DAC_CNTRL_2", 0x4048, "rw", "Control 2", [
            fld("DATA_FORMAT", 4, 1), fld("R1_MODE", 5, 1), fld("PAR_ENB", 6, 1),
            fld("PAR_TYPE", 7, 1)]),
        reg("DAC_RATECNTRL", 0x404C, "rw", "Rate control", [fld("RATE", 0, 8)]),
        reg("DAC_CLK_FREQ", 0x4054, "ro", "Interface clock frequency (x 100 MHz / 2^16)"),
        reg("DAC_CLKSEL", 0x4060, "rw", "Clock select"),
        reg("DAC_DMA_STATUS", 0x4088, "w1c", "DMA underflow (write 1 to clear)", [
            fld("DAC_DUNF", 0, 1, "w1c")]),
    ]
    dac = core("adi_dac", AD9361_BASE, 0x8000, [
        block("dac", 0x4000, dac_common),
        block("dac_chan0", 0x4400, _dac_channel(0)),
        block("dac_chan1", 0x4440, _dac_channel(1)),
    ])

    def dmac(name: str, base: int) -> dict[str, Any]:
        drv = " (owned by the Linux axi-dmac driver; do not write)"
        return core(name, base, 0x1000, [
            block("id", 0x000, [
                reg("VERSION", 0x000, "ro", "Core version"),
                reg("PERIPHERAL_ID", 0x004, "ro", "Peripheral ID"),
                reg("SCRATCH", 0x008, "rw", "Scratch (bus test)"),
                reg("IDENTIFICATION", 0x00C, "ro", "'DMAC'", expected="0x444D4143"),
                reg("INTERFACE_DESCRIPTION", 0x010, "ro", "Bus widths/types"),
            ]),
            block("irq", 0x080, [
                reg("IRQ_MASK", 0x080, "rw", "IRQ mask" + drv),
                reg("IRQ_PENDING", 0x084, "w1c", "IRQ pending (write 1 to clear)" + drv),
                reg("IRQ_SOURCE", 0x088, "ro", "IRQ source"),
            ]),
            block("transfer", 0x400, [
                reg("CONTROL", 0x400, "rw", "Enable/pause" + drv,
                    [fld("ENABLE", 0, 1), fld("PAUSE", 1, 1)]),
                reg("TRANSFER_ID", 0x404, "ro", "Next transfer ID"),
                reg("TRANSFER_SUBMIT", 0x408, "rw", "Submit" + drv),
                reg("FLAGS", 0x40C, "rw", "Cyclic/last flags" + drv),
                reg("DEST_ADDRESS", 0x410, "rw", "Destination address" + drv),
                reg("SRC_ADDRESS", 0x414, "rw", "Source address" + drv),
                reg("X_LENGTH", 0x418, "rw", "Transfer length - 1" + drv),
                reg("Y_LENGTH", 0x41C, "rw", "2-D length - 1" + drv),
                reg("TRANSFER_DONE", 0x428, "ro", "Done bitmap"),
                reg("ACTIVE_TRANSFER_ID", 0x42C, "ro", "Active transfer ID"),
                reg("STATUS", 0x430, "ro", "Status"),
                reg("CURRENT_DEST_ADDRESS", 0x434, "ro", "Current destination address"),
                reg("CURRENT_SRC_ADDRESS", 0x438, "ro", "Current source address"),
            ]),
        ])

    return {"schema": SCHEMA, "cores": [adc, dac, dmac("rx_dmac", RX_DMAC_BASE),
                                        dmac("tx_dmac", TX_DMAC_BASE)]}


# ---------------------------------------------------------------------------
# PS: SLCR, DDRC, L2C (PL310)
# ---------------------------------------------------------------------------

SLCR_BASE = 0xF8000000
DDRC_BASE = 0xF8006000
L2C_BASE = 0xF8F02000
_AUDIT = " (hardware RW; audit-only, the bench never writes it)"


def _pll_fields() -> list[dict]:
    return [fld("PLL_RESET", 0, 1, "ro"), fld("PLL_PWRDWN", 1, 1, "ro"),
            fld("PLL_BYPASS_QUAL", 3, 1, "ro"), fld("PLL_BYPASS_FORCE", 4, 1, "ro"),
            fld("PLL_FDIV", 12, 7, "ro", "Feedback divider: f = PS_CLK x FDIV")]


def _ddriob_fields() -> list[dict]:
    return [fld("INP_TYPE", 1, 2, "ro", "0 off, 1 VREF diff (SSTL), 2 differential, 3 LVCMOS"),
            fld("DCI_UPDATE_B", 3, 1, "ro"), fld("TERM_EN", 4, 1, "ro"),
            fld("DCI_TYPE", 5, 2, "ro", "0 off, 1 drive, 2 reserved, 3 termination"),
            fld("IBUF_DISABLE_MODE", 7, 1, "ro"), fld("TERM_DISABLE_MODE", 8, 1, "ro"),
            fld("OUTPUT_EN", 9, 2, "ro"), fld("PULLUP_EN", 11, 1, "ro")]


def build_ps() -> dict[str, Any]:
    slcr_clk = [
        reg("ARM_PLL_CTRL", 0x100, "ro", "ARM PLL" + _AUDIT, _pll_fields()),
        reg("DDR_PLL_CTRL", 0x104, "ro",
            "DDR PLL" + _AUDIT + "; FDIV 0x20 = 1066.7 MHz (DDR3-1066). FDIV 0x24 = overclock "
            "FSBL (600 MHz DDR): FAIL for measurement runs (CL7 -> tAA 11.7 ns < 13.125 ns)",
            _pll_fields(), expected={"mask": "0x0007F000", "value": "0x00020000"}),
        reg("IO_PLL_CTRL", 0x108, "ro", "IO PLL" + _AUDIT, _pll_fields()),
        reg("ARM_CLK_CTRL", 0x120, "ro", "CPU clock control" + _AUDIT),
        reg("DDR_CLK_CTRL", 0x124, "ro", "DDR clock control" + _AUDIT, [
            fld("DDR_3XCLKACT", 0, 1, "ro"), fld("DDR_2XCLKACT", 1, 1, "ro"),
            fld("DDR_3XCLK_DIVISOR", 20, 6, "ro"), fld("DDR_2XCLK_DIVISOR", 26, 6, "ro")],
            expected="0x0C200003"),
        reg("FPGA0_CLK_CTRL", 0x170, "ro", "FCLK0 control" + _AUDIT, [
            fld("SRCSEL", 4, 2, "ro"), fld("DIVISOR0", 8, 6, "ro"), fld("DIVISOR1", 20, 6, "ro")]),
        reg("FPGA1_CLK_CTRL", 0x180, "ro", "FCLK1 control" + _AUDIT, [
            fld("SRCSEL", 4, 2, "ro"), fld("DIVISOR0", 8, 6, "ro"), fld("DIVISOR1", 20, 6, "ro")]),
    ]
    slcr_sys = [
        reg("REBOOT_STATUS", 0x258, "ro", "Reboot reason (preserved across soft reset)"),
        reg("PSS_IDCODE", 0x530, "ro", "Device IDCODE (XC7Z020: 0x?3727093)",
            expected={"mask": "0x0FFFFFFF", "value": "0x03727093"}),
    ]
    ddriob_names = [
        ("DDRIOB_ADDR0", 0xB40), ("DDRIOB_ADDR1", 0xB44), ("DDRIOB_DATA0", 0xB48),
        ("DDRIOB_DATA1", 0xB4C), ("DDRIOB_DIFF0", 0xB50), ("DDRIOB_DIFF1", 0xB54),
        ("DDRIOB_CLOCK", 0xB58),
    ]
    ddriob = [reg(n, o, "ro", "DDR IOB configuration (report only)" + _AUDIT, _ddriob_fields())
              for n, o in ddriob_names]
    ddriob += [
        reg("DDRIOB_DRIVE_SLEW_ADDR", 0xB5C, "ro", "Drive/slew (report only)"),
        reg("DDRIOB_DRIVE_SLEW_DATA", 0xB60, "ro", "Drive/slew (report only)"),
        reg("DDRIOB_DRIVE_SLEW_DIFF", 0xB64, "ro", "Drive/slew (report only)"),
        reg("DDRIOB_DRIVE_SLEW_CLOCK", 0xB68, "ro", "Drive/slew (report only)"),
        reg("DDRIOB_DDR_CTRL", 0xB6C, "ro",
            "DDR IOB control (report only): VREF_SEL 0x2 = 0.675 V (SSTL135/DDR3L), "
            "0x4 = 0.75 V (SSTL15/DDR3)", [
                fld("VREF_INT_EN", 0, 1, "ro"),
                fld("VREF_SEL", 1, 4, "ro", "1 0.6 V LPDDR2, 2 0.675 V DDR3L, 4 0.75 V DDR3, "
                                            "8 0.9 V DDR2"),
                fld("VREF_EXT_EN", 5, 2, "ro"), fld("REFIO_EN", 9, 1, "ro")]),
        reg("DDRIOB_DCI_CTRL", 0xB70, "ro", "DCI control (report only)"),
        reg("DDRIOB_DCI_STATUS", 0xB74, "ro", "DCI status (report only)",
            [fld("DONE", 0, 1, "ro"), fld("LOCK", 13, 1, "ro")]),
    ]
    slcr = core("slcr", SLCR_BASE, 0x1000, [
        block("clocks", 0x100, slcr_clk),
        block("system", 0x258, slcr_sys),
        block("ddriob", 0xB40, ddriob),
    ])
    ddrc_mode = [
        reg("DRAM_EMR_REG", 0x02C, "ro", "EMR2 (bits 15:0) / EMR3 (31:16)" + _AUDIT, [
            fld("EMR2", 0, 16, "ro"), fld("EMR3", 16, 16, "ro")],
            expected={"mask": "0x0000FFFF", "value": "0x00000008"}),
        reg("DRAM_EMR_MR", 0x030, "ro", "MR0 (bits 15:0) / EMR1 (31:16); MR0 0x0B30 = CL7"
            + _AUDIT, [fld("MR", 0, 16, "ro"), fld("EMR", 16, 16, "ro")],
            expected="0x00040B30"),
    ]
    prio = []
    for i in range(4):
        prio.append(reg(f"AXI_PRIORITY_WR_PORT{i}", 0x208 + 4 * i, "ro",
                        f"Write port {i} priority" + _AUDIT))
    for i in range(4):
        prio.append(reg(f"AXI_PRIORITY_RD_PORT{i}", 0x218 + 4 * i, "ro",
                        f"Read port {i} priority" + _AUDIT))
    ddrc = core("ddrc", DDRC_BASE, 0x1000, [
        block("mode", 0x02C, ddrc_mode),
        block("priority", 0x208, prio),
    ])
    l2c = core("l2c", L2C_BASE, 0x1000, [
        block("control", 0x100, [
            reg("REG1_CONTROL", 0x100, "ro", "L2 enable" + _AUDIT, [fld("L2_ENABLE", 0, 1, "ro")],
                expected={"mask": "0x00000001", "value": "0x00000001"}),
            reg("REG1_AUX_CONTROL", 0x104, "ro", "Auxiliary control" + _AUDIT, [
                fld("DATA_PREFETCH", 28, 1, "ro"), fld("INSTR_PREFETCH", 29, 1, "ro"),
                fld("EARLY_BRESP", 30, 1, "ro")]),
        ]),
        block("prefetch", 0xF60, [
            reg("REG15_PREFETCH_CTRL", 0xF60, "ro", "Prefetch control" + _AUDIT, [
                fld("PREFETCH_OFFSET", 0, 5, "ro"), fld("DOUBLE_LINEFILL", 30, 1, "ro"),
                fld("DATA_PREFETCH", 28, 1, "ro"), fld("INSTR_PREFETCH", 29, 1, "ro")]),
        ]),
    ])
    return {"schema": SCHEMA, "cores": [slcr, ddrc, l2c]}


# ---------------------------------------------------------------------------
# Merge with the agent's built-in maps
# ---------------------------------------------------------------------------

AGENT_MAPS = BENCH_DIR / "agent" / "maps"
_CORE_KEYS_SKIP = {"schema", "core", "base", "size", "blocks", "source"}
_ACCESS_RANK = {"ro": 0, "w1c": 1, "wo": 1, "rw": 2}


def agent_builtin_maps(agent_dir: Path = AGENT_MAPS) -> dict[str, dict]:
    """The agent's built-in core maps (empty when the agent tree is absent)."""
    out: dict[str, dict] = {}
    if not Path(agent_dir).is_dir():
        return out
    for path in sorted(Path(agent_dir).glob("*.json")):
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(doc, dict) or doc.get("schema") != SCHEMA:
            continue
        for cmap in _iter_core_maps(doc):
            out[str(cmap["core"])] = cmap
    return out


def _off(v: Any) -> int:
    return v if isinstance(v, int) else int(str(v), 0)


def _norm_reg(r: dict) -> dict:
    base = reg(str(r["name"]), _off(r["offset"]), str(r.get("access", "ro")),
               str(r.get("desc", "")), list(r.get("fields", [])), expected=r.get("expected"),
               read_side_effect=bool(r.get("read_side_effect", False)))
    base["width"] = r.get("width", 32)
    base["reset"] = r.get("reset", base["reset"])
    base["snapshot"] = r.get("snapshot")
    if r.get("aliases"):
        base["aliases"] = list(r["aliases"])
    return base


def merge_with_agent(mine: dict, builtin: dict | None) -> dict:
    """Superset of ``builtin`` (agent names) annotated with ``mine``."""
    if builtin is None:
        return mine
    mine_regs = {int(r["offset"], 16): r for b in mine["blocks"] for r in b["regs"]}
    seen: set[int] = set()
    blocks = []
    for blk in builtin.get("blocks", []):
        regs = []
        for raw in blk.get("regs", []):
            r = _norm_reg(raw)
            off = int(r["offset"], 16)
            m = mine_regs.get(off)
            if m is not None:
                seen.add(off)
                if m["name"].lower() != r["name"].lower():
                    r.setdefault("aliases", []).append(m["name"])
                if m["expected"] is not None:
                    r["expected"] = m["expected"]
                r["read_side_effect"] = r["read_side_effect"] or m["read_side_effect"]
                if _ACCESS_RANK.get(m["access"], 2) < _ACCESS_RANK.get(r["access"], 2):
                    r["access"] = m["access"]
                if m["desc"] and m["desc"] != r["desc"]:
                    r["desc"] = f"{r['desc']} | {m['desc']}" if r["desc"] else m["desc"]
                if not r["fields"]:
                    r["fields"] = m["fields"]
            regs.append(r)
        blocks.append({"name": blk["name"], "offset": _hex(_off(blk.get("offset", 0))),
                       "regs": regs})
    extra = [r for off, r in sorted(mine_regs.items()) if off not in seen]
    if extra:
        blocks.append({"name": "fbench_extra", "offset": extra[0]["offset"], "regs": extra})
    out = core(mine["core"], int(mine["base"], 16),
               max(int(mine["size"]), int(builtin.get("size", 0) or 0)), blocks)
    for k, v in builtin.items():
        if k not in _CORE_KEYS_SKIP:
            out[k] = v
    out["source"] = ("bench/share via `fbench regmaps build`: agent built-in map + host "
                     "annotations (expected, read_side_effect, aliases, extra registers)")
    return out


def merge_doc(doc: dict, builtins: dict[str, dict]) -> dict:
    cores = [merge_with_agent(c, builtins.get(c["core"])) for c in _iter_core_maps(doc)]
    return {"schema": SCHEMA, "cores": cores} if "cores" in doc else cores[0]


# ---------------------------------------------------------------------------
# Build / load
# ---------------------------------------------------------------------------


def build_all(out_dir: Path = BENCH_DIR / "share", svd_path: Path = P25_SVD,
              agent_dir: Path = AGENT_MAPS) -> list[Path]:
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    builtins = agent_builtin_maps(agent_dir)
    outputs = {
        "p25_regs.json": build_p25(svd_path),
        "adi_regs.json": merge_doc(build_adi(), builtins),
        "ps_regs.json": merge_doc(build_ps(), builtins),
    }
    written = []
    for name, data in outputs.items():
        path = out_dir / name
        path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        written.append(path)
    return written


@dataclass
class RegDef:
    core: str
    base: int
    block: str
    name: str
    offset: int
    access: str
    read_side_effect: bool
    expected: Any
    fields: list[dict]
    desc: str
    aliases: tuple[str, ...] = ()

    @property
    def address(self) -> int:
        return self.base + self.offset

    def decode(self, value: int) -> dict[str, int]:
        return {f["name"]: (value >> f["lsb"]) & ((1 << f["width"]) - 1) for f in self.fields}

    def check(self, value: int) -> bool | None:
        """Compare ``value`` with ``expected`` (None when nothing is expected)."""
        return check_expected(self.expected, value)


def check_expected(expected: Any, value: int) -> bool | None:
    if expected is None:
        return None
    if isinstance(expected, dict):
        mask = int(str(expected["mask"]), 0)
        return (value & mask) == (int(str(expected["value"]), 0) & mask)
    return value == int(str(expected), 0)


def _iter_core_maps(doc: dict) -> list[dict]:
    if doc.get("schema") != SCHEMA:
        raise ConfigError(f"not a {SCHEMA} document")
    if "cores" in doc:
        return list(doc["cores"])
    return [doc]


def load_regmaps(share_dir: Path) -> dict[str, dict[str, RegDef]]:
    """Load every ``*_regs.json`` in ``share_dir``: core -> reg name -> RegDef."""
    out: dict[str, dict[str, RegDef]] = {}
    for path in sorted(Path(share_dir).glob("*.json")):
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(doc, dict) or doc.get("schema") != SCHEMA:
            continue
        for cmap in _iter_core_maps(doc):
            base = _off(cmap["base"])
            regs = out.setdefault(str(cmap["core"]), {})
            for blk in cmap.get("blocks", []):
                for r in blk.get("regs", []):
                    regs[str(r["name"])] = RegDef(
                        core=str(cmap["core"]), base=base, block=str(blk["name"]),
                        name=str(r["name"]), offset=_off(r["offset"]),
                        access=str(r.get("access", "ro")),
                        read_side_effect=bool(r.get("read_side_effect", False)),
                        expected=r.get("expected"), fields=list(r.get("fields", [])),
                        desc=str(r.get("desc", "")),
                        aliases=tuple(str(x) for x in r.get("aliases", []) or []),
                    )
    return out


def find_reg(maps: dict[str, dict[str, RegDef]], core_name: str, reg_name: str) -> RegDef:
    """Look up a register by name (case-insensitive) or hex offset."""
    if core_name not in maps:
        raise ConfigError(f"no register map for core {core_name!r} "
                          f"(have: {', '.join(sorted(maps)) or 'none'}); run `fbench regmaps build`")
    regs = maps[core_name]
    for r in regs.values():
        if r.name.lower() == reg_name.lower():
            return r
    for r in regs.values():
        if reg_name.lower() in (a.lower() for a in r.aliases):
            return r
    try:
        off = int(reg_name, 0)
    except ValueError:
        off = None
    if off is not None:
        for r in regs.values():
            if r.offset == off:
                return r
    raise SafetyRefusal(f"register {reg_name!r} is not in the {core_name} allow-list "
                        "(design doc rule 5; vacant p25_core banks hang the bus)")
