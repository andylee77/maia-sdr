"""Boot-log (UART console) extraction for ``sys.boot_log`` and ``fbench console``.

Input: lines as recorded by :mod:`fbench.console` (``<host_ts>\\t<text>``, or
bare text). Output: versions, markers and counted problem lines.
"""

from __future__ import annotations

import re
from typing import Iterable

_PATTERNS: dict[str, re.Pattern[str]] = {
    "uboot_version": re.compile(r"U-Boot\s+(\d{4}\.\d{2}[^\s(]*)"),
    "kernel_version": re.compile(r"Linux version\s+(\S+)"),
    "fw_version": re.compile(r"(?:fw_version|Firmware)\W+(\S+)", re.IGNORECASE),
}
_MARKERS: dict[str, re.Pattern[str]] = {
    "kernel_panic": re.compile(r"Kernel panic", re.IGNORECASE),
    "unable_to": re.compile(r"\bUnable to\b"),
    "failed": re.compile(r"\bfail(ed|ure)?\b", re.IGNORECASE),
    "usb_gadget": re.compile(r"RNDIS|g_ether|\budc\b|gadget", re.IGNORECASE),
    "iiod_start": re.compile(r"iiod", re.IGNORECASE),
    "scanner_start": re.compile(r"Starting scanner"),
    "watchdog_reset": re.compile(r"watchdog|WDT|reset reason|REBOOT_STATUS", re.IGNORECASE),
    "login_prompt": re.compile(r"login:"),
    "tezuka": re.compile(r"tezuka", re.IGNORECASE),
    "plutosdr_fw": re.compile(r"pluto", re.IGNORECASE),
}
#: Markers whose lines are copied into the findings (problem indicators).
PROBLEM_MARKERS = ("kernel_panic", "unable_to", "failed", "watchdog_reset")


def split_line(line: str) -> tuple[str | None, str]:
    """``"<ts>\\t<text>"`` -> (ts, text); bare text -> (None, text)."""
    if "\t" in line:
        ts, text = line.split("\t", 1)
        if re.match(r"^\d{4}-\d{2}-\d{2}T", ts):
            return ts, text
    return None, line


def parse_boot_log(lines: Iterable[str], max_examples: int = 20) -> dict:
    """Extract versions, marker counts and example lines from a boot log."""
    info: dict[str, str | None] = {k: None for k in _PATTERNS}
    counts = {k: 0 for k in _MARKERS}
    examples: dict[str, list[dict]] = {k: [] for k in _MARKERS}
    n = 0
    for raw in lines:
        ts, text = split_line(raw.rstrip("\r\n"))
        n += 1
        for key, pat in _PATTERNS.items():
            if info[key] is None:
                m = pat.search(text)
                if m:
                    info[key] = m.group(1)
        for key, pat in _MARKERS.items():
            if pat.search(text):
                counts[key] += 1
                if len(examples[key]) < max_examples:
                    examples[key].append({"line": n, "ts": ts, "text": text.strip()[:240]})
    family = "tezuka" if counts["tezuka"] else ("factory" if counts["plutosdr_fw"] else "unknown")
    return {
        "lines": n,
        **info,
        "firmware_family": family,
        "counts": counts,
        "examples": examples,
        "reached_login": counts["login_prompt"] > 0,
        "problems": sum(counts[k] for k in PROBLEM_MARKERS),
    }
