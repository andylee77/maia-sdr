#!/usr/bin/env python3
"""Fishball P25 -- comprehensive on-target status + roadmap snapshot.

Hits every read-only API endpoint exposed by p25-httpd, pretty-prints
the most useful fields from each, and then -- the part this script
exists for -- compares the current observable state of the radio
against the project trunking roadmap and tells you **what to do
next** to move P25 trunking forward on this SDR.

Usage:
    python tools/p25_status_and_next_step.py [TARGET]

TARGET defaults to "192.168.2.1:8080" (Fishball Z7020 wired
Ethernet direct-connect). Pass another host:port if you've got
the radio on a different network.

The script is read-only -- it never POSTs, PUTs, or toggles any
register. Safe to run on a production radio. Pure-stdlib (no
external Python deps), works from git-bash on Windows.

Output is divided into sections:
  1. Reachability + binary identity
  2. System identity (NAC, WACN, RFSS, control channel, secondary CCH)
  3. Frequency band table
  4. Active grants (TG-deduped, source-preserved)
  5. Decoder pipeline counters (per pipeline + per opcode)
  6. HDL chain health (PLL state, NID validity, sync distance)
  7. Roadmap evaluation -- where we are vs where we're going
  8. Recommended next step

Exit codes:
  0  every read succeeded and the radio is operational
  1  one or more endpoints failed (network issue or stale binary)
  2  unreachable target
"""

from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request
from typing import Any

DEFAULT_TARGET = "192.168.2.1:8080"

# ── ANSI colours (works in modern Windows terminals + git-bash) ──
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
CYAN = "\033[36m"
DIM = "\033[2m"
BOLD = "\033[1m"
RESET = "\033[0m"

# Endpoints we'll hit. Order matters -- we use the result of /api/system
# to decide reachability before pulling the rest. Each entry: (path, label).
ENDPOINTS = [
    ("/api/system",          "system"),
    ("/api/grants",          "grants"),
    ("/api/bands",           "bands"),
    ("/api/stats",           "stats"),
    ("/api/decoder_compare", "decoder_compare"),
    ("/api/hdl_lsm",         "hdl_lsm"),
    ("/api/lsm",             "lsm"),
    ("/api/irq_stats",       "irq_stats"),
    ("/api/dibit_dump",      "dibit_dump"),
    ("/api/lsm_dibit_dump",  "lsm_dibit_dump"),
    ("/api/tsbk_opcodes",    "tsbk_opcodes"),
    ("/api/recent_tsbks",    "recent_tsbks"),
    ("/api/aliases",         "aliases"),
]


def fetch(target: str, path: str, timeout: float = 5.0) -> Any | None:
    url = f"http://{target}{path}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.URLError as e:
        print(f"{RED}!! GET {url} failed: {e}{RESET}", file=sys.stderr)
        return None
    except json.JSONDecodeError as e:
        print(f"{RED}!! GET {url}: invalid JSON: {e}{RESET}", file=sys.stderr)
        return None


def banner(text: str) -> None:
    print()
    print(f"{BOLD}{CYAN}== {text} =={RESET}")


def kv(label: str, value: Any, *, value_color: str = "") -> None:
    print(f"  {label:<28}{value_color}{value}{RESET}")


def fmt_pct(num: int, denom: int) -> str:
    if denom <= 0:
        return "  -- "
    return f"{100.0 * num / denom:5.1f}%"


def fmt_int(n: Any) -> str:
    if n is None:
        return "--"
    try:
        return f"{int(n):,}"
    except (TypeError, ValueError):
        return str(n)


# ──────────────────────────────────────────────────────────────────
# Section renderers
# ──────────────────────────────────────────────────────────────────


def render_reachability(target: str, sys: dict | None) -> bool:
    banner(f"1. Reachability  ->  http://{target}")
    if sys is None:
        kv("status", "UNREACHABLE", value_color=RED)
        return False
    kv("build tag", sys.get("build", "?"), value_color=GREEN)
    kv("system clock", sys.get("system_clock", "?"))
    return True


def render_system_identity(sys: dict) -> None:
    banner("2. System identity (PS LSM software decoder)")
    kv("NAC",                 sys.get("nac", "--"))
    kv("WACN",                sys.get("wacn", "--"))
    kv("System ID",           sys.get("system_id", "--"))
    kv("RFSS / Site",         f"{sys.get('rfss_id', '?')} / {sys.get('site_id', '?')}")
    kv("LRA",                 sys.get("lra", "--"))
    kv("Control channel",     sys.get("control_channel", "--"))
    kv("Secondary CCH A/B",
       f"{sys.get('secondary_cch_a', '--')} / {sys.get('secondary_cch_b', '--')}")
    kv("SNDCP DL/UL",
       f"{sys.get('sndcp_downlink_channel', '--')} / "
       f"{sys.get('sndcp_uplink_channel', '--')}")


def render_bands(bands: list) -> None:
    banner("3. Frequency band table")
    if not bands:
        kv("bands_known", "0  (no IDEN_UPDATE seen yet)", value_color=YELLOW)
        return
    print(f"  {'id':>3}  {'base MHz':>11}  {'spacing kHz':>12}  "
          f"{'tx offset MHz':>14}  {'BW kHz':>8}")
    for b in bands:
        print(f"  {b['identifier']:>3}  "
              f"{b['base_frequency_mhz']:>11.5f}  "
              f"{b['channel_spacing_khz']:>12.3f}  "
              f"{b['transmit_offset_mhz']:>14.3f}  "
              f"{b['bandwidth_khz']:>8.2f}")


def render_grants(grants: list, aliases: dict | None) -> None:
    banner(f"4. Active grants (n={len(grants)})  -- TG-deduped, source-preserved")
    if not grants:
        kv("active grants", "(none)", value_color=DIM)
        return
    print(f"  {'channel':<10}  {'TG':>6}  {'src':>10}  "
          f"{'freq MHz':>10}  {'age':>5}  alias")
    for g in grants:
        tg = g.get("talkgroup", 0)
        alias_field = g.get("talkgroup_alias")
        if not alias_field and aliases is not None:
            alias_field = aliases.get(str(tg)) or aliases.get(tg) or ""
        src = g.get("source")
        src_str = f"{src}" if src is not None else "--"
        freq = g.get("frequency_mhz")
        freq_str = f"{freq:.4f}" if freq is not None else "--"
        print(f"  {g['channel']:<10}  {tg:>6}  {src_str:>10}  "
              f"{freq_str:>10}  {g.get('age_secs', 0):>4}s  {alias_field or ''}")


def render_decoder_compare(dc: dict) -> None:
    banner("5. Decoder pipeline counters (decoder_compare)")
    rows = [
        ("ps_lsm",    "PS LSM (HDL dibit-fed)"),
        ("ps_iq_lsm", "PS IQ-LSM (raw IQ + soft sync)"),
        ("ps_c4fm",   "PS C4FM (HDL c4fm dibit-fed)"),
    ]
    print(f"  {'pipeline':<32}  {'NIDs ok':>9}  {'TSBK CRC':>12}  "
          f"{'pass%':>6}  grants  bands")
    for key, label in rows:
        p = dc.get(key, {})
        nid_ok = p.get("nid_decoded_ok", 0)
        nid_at = p.get("nid_attempts", 0)
        crc_ok = p.get("tsbk_crc_ok", 0)
        crc_at = p.get("tsbk_block_attempts", 0)
        pct = fmt_pct(crc_ok, crc_at)
        print(f"  {label:<32}  "
              f"{fmt_int(nid_ok):>9}  "
              f"{fmt_int(crc_ok):>5}/{fmt_int(crc_at):<6}  "
              f"{pct:>6}  "
              f"{p.get('active_grants', 0):>6}  "
              f"{p.get('bands_known', 0):>5}")
    pl = dc.get("pl_hdl", {})
    print()
    print(f"  pl_hdl (FPGA) -- valid NIDs:  "
          f"{fmt_int(pl.get('valid_nids', 0))} / "
          f"{fmt_int(pl.get('total_nids', 0))}  "
          f"({pl.get('valid_pct', 0.0):.1f}%)  "
          f"winner_nac={pl.get('winner_nac', '?')}  "
          f"drop_count={pl.get('drop_count', 0)}")


def render_opcodes(oc: dict) -> None:
    banner("6. TSBK opcode coverage")
    by_pos = oc.get("by_position", {})
    print(f"  per-block: "
          f"TSBK1 {by_pos.get('tsbk1', {}).get('crc_ok_pct', 0):.1f}% / "
          f"TSBK2 {by_pos.get('tsbk2', {}).get('crc_ok_pct', 0):.1f}% / "
          f"TSBK3 {by_pos.get('tsbk3', {}).get('crc_ok_pct', 0):.1f}%   "
          f"(blocks/TSDU={oc.get('blocks_per_tsdu', 0):.2f})  "
          f"overall {oc.get('crc_ok_pct', 0):.1f}%")
    print()
    print(f"  {'opc':>4}  {'label':<24}  {'ok':>6}  {'fail':>6}  parsed")
    opcodes = oc.get("opcodes", [])[:12]
    for op in opcodes:
        parsed = op.get("parsed", False)
        flag = f"{GREEN}yes{RESET}" if parsed else f"{YELLOW}no {RESET}"
        # `opcode` field is already a "0xNN" string from p25-httpd.
        print(f"  {str(op.get('opcode', '?')):>4}  "
              f"{op.get('label', '?'):<24}  "
              f"{op.get('ok', 0):>6}  "
              f"{op.get('fail', 0):>6}  {flag}")


def render_hdl_health(hdl: dict | None) -> None:
    banner("7. HDL LSM chain health (PL gateware)")
    if hdl is None:
        kv("status", "(no /api/hdl_lsm response)", value_color=RED)
        return
    cum = hdl.get("cumulative", {})
    live = hdl.get("live", {})
    win = hdl.get("last_window", {})
    kv("uptime",            f"{hdl.get('uptime_secs', 0)}s")
    kv("running",           hdl.get("running", "?"))
    kv("last NID (ms ago)", hdl.get("last_nid_ms_ago", "?"))
    kv("cumulative NIDs",
       f"{fmt_int(cum.get('valid_nid_events', 0))} / "
       f"{fmt_int(cum.get('total_nid_events', 0))} valid "
       f"({cum.get('valid_pct', 0):.1f}%)")
    kv("last NAC / DUID",
       f"{live.get('last_nac', '?')} / {live.get('last_duid', '?')}")
    kv("PLL register (now)", live.get("pll_dbg", "?"))
    kv("sample point (now)", live.get("sp_dbg", "?"))
    kv("sync distance (now)", live.get("sync_distance", "?"))
    kv("BCH busy / in window",
       f"{live.get('bch_busy', '?')} / {live.get('in_nid_window', '?')}")
    kv("1s window: PLL min/max",
       f"{win.get('pll_min', '?')} / {win.get('pll_max', '?')}")
    kv("1s window: sp min/max",
       f"{win.get('sp_min', '?')} / {win.get('sp_max', '?')}")
    kv("1s window: NIDs valid/total",
       f"{win.get('valid_count', '?')} / {win.get('event_count', '?')}")
    kv("1s window: iq KB/s",   win.get("iq_kbps", "?"))
    kv("1s window: iq buf rolls", win.get("iq_buf_rolls", "?"))


def render_irq(irq: dict | None) -> None:
    banner("8. IRQ source counters")
    if irq is None:
        kv("status", "(no /api/irq_stats response)", value_color=RED)
        return
    rates = irq.get("rate_per_sec", {})
    kv("uptime", f"{irq.get('uptime_secs', '?')}s")
    kv("total IRQs",
       f"{fmt_int(irq.get('total', 0))}  ({rates.get('total', 0):.2f}/s)")
    kv("dibit DMA",
       f"{fmt_int(irq.get('dibit', 0))}  ({rates.get('dibit', 0):.2f}/s)")
    kv("traffic dibit DMA",
       f"{fmt_int(irq.get('traffic', 0))}  ({rates.get('traffic', 0):.2f}/s)")
    kv("IQ DMA",
       f"{fmt_int(irq.get('iq', 0))}  ({rates.get('iq', 0):.2f}/s)")
    kv("LSM dibit DMA",
       f"{fmt_int(irq.get('lsm_dibit', 0))}  ({rates.get('lsm_dibit', 0):.2f}/s)")
    kv("last IRQ (ms ago)", irq.get("last_at_ms_ago", "?"))


def render_recent_tsbks(rt: dict | None, n: int = 8) -> None:
    banner(f"9. Most-recent TSBK activity (last {n})")
    if rt is None:
        return
    msgs = rt.get("messages", [])[:n]
    for m in msgs:
        age = m.get("age_secs", 0)
        block = m.get("block", "?")
        summary = m.get("summary", "")
        print(f"  -{age:>5.1f}s  {block}  {summary}")


# ──────────────────────────────────────────────────────────────────
# Roadmap evaluator: where are we, what's next
# ──────────────────────────────────────────────────────────────────

# The roadmap is a flat list of feature/capability checks. Each
# check decides PASS/FAIL by querying the snapshot dicts. If a check
# fails, the recommended next step is its `next_step` field. We
# scan top-to-bottom and return the FIRST failing check -- that's
# the next thing to work on.
#
# Phase ordering reflects the actual implementation reality
# circa Phase 6G.1 (April 2026), not the original DEVPLAN.md
# (which is stale past Phase 5).

ROADMAP = [
    {
        "phase": "Phase 5",
        "name": "Hardware boots, p25-httpd reachable",
        "check": lambda s: s["sys"] is not None,
        "next_step": (
            "Board not reachable. Re-flash from tezuka_fw or check "
            "the wired Ethernet interface (default 192.168.2.1)."
        ),
    },
    {
        "phase": "Phase 6E",
        "name": "HDL LSM chain producing valid NIDs",
        "check": lambda s: (
            s["dc"] is not None
            and s["dc"].get("pl_hdl", {}).get("valid_nids", 0) > 100
            and s["dc"].get("pl_hdl", {}).get("valid_pct", 0) > 50
        ),
        "next_step": (
            "HDL LSM chain not producing valid NIDs. Investigate sync "
            "detector, BCH FEC, or PLL state via /api/hdl_lsm."
        ),
    },
    {
        "phase": "Phase 6F",
        "name": "PS LSM decoder building system identity",
        "check": lambda s: (
            s["sys"] is not None
            and s["sys"].get("nac")
            and s["sys"].get("wacn")
            and s["sys"].get("control_channel")
        ),
        "next_step": (
            "System identity is empty. Check that the PS LSM decoder "
            "is dispatching TSBKs (look at decoder_compare.ps_lsm.messages)."
        ),
    },
    {
        "phase": "Phase 6F",
        "name": "Frequency band table populated (>= 2 bands)",
        "check": lambda s: (s["bands"] is not None and len(s["bands"]) >= 2),
        "next_step": (
            "IDEN_UPDATE TSBKs not arriving / not parsing. Check "
            "/api/decoder_compare.ps_lsm.tsbk_crc_ok and "
            "/api/tsbk_opcodes for the IDEN_UPDATE counter."
        ),
    },
    {
        "phase": "Phase 6F",
        "name": "Active grants visible (>= 1 in last few minutes)",
        "check": lambda s: (s["grants"] is not None and len(s["grants"]) >= 1),
        "next_step": (
            "No active grants. The control channel may have no traffic "
            "right now -- this is normal off-hours. Wait a few minutes "
            "and re-check, or look at /api/recent_tsbks for any "
            "GRP_V_CH_GRANT events."
        ),
    },
    {
        "phase": "Phase 6F",
        "name": "Source RadioId preserved across grant updates",
        "check": lambda s: (
            s["sys"] is not None
            and "preserve-grant-source-id-on-update" in (s["sys"].get("build") or "")
        ),
        "next_step": (
            "Running binary predates commit 1e29839. Rebuild p25-httpd "
            "in tezuka_fw and re-flash so the dashboard's caller ID "
            "stops dropping to None on every grant refresh."
        ),
    },
    {
        "phase": "Phase 6G.1",
        "name": "HDL DC blocker shipped + enabled (build tag check)",
        "check": lambda s: (
            s["sys"] is not None
            and ("phase6g.1" in (s["sys"].get("build") or "")
                 or "dc-blocker" in (s["sys"].get("build") or ""))
        ),
        "next_step": (
            "Running binary predates the Phase 6G.1 HDL DC blocker. "
            "Rebuild p25-httpd from main and re-flash. The DC blocker "
            "is runtime-bypassable via lsm_control[2]."
        ),
    },
    # ── PHASES BELOW ARE NOT YET IMPLEMENTED ──
    # Each one's check() returns False, so the script stops at the
    # first one and reports it as "the next thing to build".
    {
        "phase": "Phase 6G.2 (planned)",
        "name": "Runtime DC blocker bypass via /api/lsm_control endpoint",
        "check": lambda s: False,
        "next_step": (
            "Add a small read/write endpoint exposing lsm_control bits "
            "(lsm_enable, lsm_dibit_dma_enable, lsm_dc_block_enable) "
            "so runtime A/B testing of the DC blocker doesn't require "
            "ssh + devmem on the board. ~30 lines in p25-httpd/src/"
            "httpd/mod.rs + a 3-bit setter in fpga.rs (already mostly "
            "there). Cheap, useful, no bitstream rebuild needed."
        ),
    },
    {
        "phase": "Phase 7 (Voice channel follow)",
        "name": "Second DDC + traffic decoder reachable from PS",
        "check": lambda s: False,
        "next_step": (
            "Phase 7 starts here. The current radio decodes the "
            "control channel and lists active grants in /api/grants, "
            "but does NOT retune to voice channels. To make this a "
            "real trunking radio:\n"
            "  1. Instantiate a second DDC chain in p25_top.py "
            "(parallel to the existing one), with its own NCO/decimator/"
            "demod pipeline. The c4fm chain already exists in p25_hdl/ "
            "but is fed by the same DDC as LSM -- it needs its own "
            "DDC instance for independent retune.\n"
            "  2. Add a voice_channel register bank for PS to write "
            "the target frequency offset.\n"
            "  3. In p25-httpd, on every GroupVoiceChannelGrant for "
            "an interesting talkgroup (configured via /api/aliases or "
            "a new monitor list), compute the offset from the active "
            "frequency band table and write it.\n"
            "  4. Confirm the second chain locks within ~60 ms (P25 "
            "spec is ~200 ms) -- DEVPLAN.md Phase 3 budgeted for this.\n"
            "Total HDL cost: ~18 DSP48E1, ~3000 LUTs. Z7020 has "
            "plenty of headroom (~16% utilization for 2 channels)."
        ),
    },
    {
        "phase": "Phase 7 (Voice frame extraction)",
        "name": "LDU1/LDU2 sync + IMBE frame extraction working",
        "check": lambda s: False,
        "next_step": (
            "Once voice channel follow works (above), the next step "
            "is to actually decode voice. P25 voice uses LDU1 and LDU2 "
            "frames -- different sync words from the TSDU we already "
            "decode for the control channel. Each LDU carries 9 IMBE "
            "frames (88 bits each) protected by Trellis + RS FEC. "
            "Reuse the existing trellis decoder from the TSBK path."
        ),
    },
    {
        "phase": "Phase 7 (Audio output)",
        "name": "IMBE/AMBE vocoder + PCM out",
        "check": lambda s: False,
        "next_step": (
            "Voice frames are extracted but not yet decoded to audio. "
            "Options for the vocoder:\n"
            "  - mbelib (open-source, FFI from Rust, license-questionable)\n"
            "  - codec2 (open-source, not bit-compatible with IMBE)\n"
            "  - DVSI hardware (proper but adds a chip)\n"
            "Output: stream PCM via RTP over the existing Ethernet, "
            "or pipe to a USB audio device on the Zynq. RTP is the "
            "more flexible option since the radio is headless."
        ),
    },
    {
        "phase": "Phase 8 (PL port follow-ups)",
        "name": "TG history + alerting hooks",
        "check": lambda s: False,
        "next_step": (
            "Once trunking + voice work, the dashboard becomes useful "
            "as a P25 monitor. Worth adding:\n"
            "  - /api/talkgroups -- catalogue of every TG ever heard, "
            "with first-seen / last-seen / event-count / activity rate\n"
            "  - WebSocket alerting filter -- subscribe with a TG "
            "list and only get events for those TGs (currently the "
            "WS streams everything)\n"
            "  - Grant-following audio tap -- a single 'follow this "
            "TG and stream PCM to my client' endpoint that ties the "
            "voice-channel-follow + vocoder + RTP pieces together"
        ),
    },
]


def evaluate_roadmap(snapshot: dict) -> tuple[list[dict], dict | None]:
    """Walk the roadmap; return (passing_phases, first_failing_phase)."""
    passing = []
    for entry in ROADMAP:
        try:
            ok = entry["check"](snapshot)
        except Exception as e:
            print(f"{YELLOW}!! roadmap check '{entry['name']}' raised: {e}{RESET}",
                  file=sys.stderr)
            ok = False
        if ok:
            passing.append(entry)
        else:
            return passing, entry
    return passing, None


def render_roadmap(snapshot: dict) -> int:
    banner("10. Trunking implementation roadmap")
    passing, first_failing = evaluate_roadmap(snapshot)
    for entry in passing:
        print(f"  {GREEN}[OK]{RESET}  {entry['phase']:<26}  {entry['name']}")
    if first_failing is None:
        print()
        print(f"  {GREEN}{BOLD}== ALL CHECKS PASS =={RESET}")
        print(f"  Every roadmap milestone in this script is met.")
        print(f"  Add new entries to ROADMAP[] in this file as new")
        print(f"  features are implemented.")
        return 0
    print(f"  {YELLOW}[..]{RESET}  {first_failing['phase']:<26}  "
          f"{first_failing['name']}  {YELLOW}<-- next{RESET}")
    print()
    print(f"{BOLD}{CYAN}== Next step =={RESET}")
    print(f"{BOLD}{first_failing['phase']}: {first_failing['name']}{RESET}")
    print()
    # Word-wrap the next-step text at ~76 cols, preserving paragraphs.
    text = first_failing["next_step"]
    for para in text.split("\n"):
        if not para.strip():
            print()
            continue
        # Naive wrap.
        words = para.split()
        line = ""
        for w in words:
            if len(line) + 1 + len(w) > 76:
                print(f"  {line}")
                line = w
            else:
                line = (line + " " + w) if line else w
        if line:
            print(f"  {line}")
    return 0


# ──────────────────────────────────────────────────────────────────
# Main
# ──────────────────────────────────────────────────────────────────


def main() -> int:
    target = sys.argv[1] if len(sys.argv) >= 2 else DEFAULT_TARGET

    print(f"{BOLD}Fishball P25 status snapshot  ->  http://{target}{RESET}")

    # Phase 1: get /api/system first to verify reachability.
    sys_resp = fetch(target, "/api/system")
    if not render_reachability(target, sys_resp):
        return 2
    snapshot: dict[str, Any] = {"sys": sys_resp}

    # Phase 2: pull the rest in catalogue order, tolerating
    # individual failures (mark as None and continue).
    failures = 0
    for path, label in ENDPOINTS[1:]:  # already pulled /api/system
        snapshot[label] = fetch(target, path)
        if snapshot[label] is None:
            failures += 1

    # Phase 3: render every section in catalogue order.
    render_system_identity(sys_resp)

    if snapshot.get("bands") is not None:
        render_bands(snapshot["bands"])
    if snapshot.get("grants") is not None:
        render_grants(snapshot["grants"], snapshot.get("aliases"))
    if snapshot.get("decoder_compare") is not None:
        render_decoder_compare(snapshot["decoder_compare"])
        # Rename to match the dict keys the roadmap evaluator uses.
        snapshot["dc"] = snapshot["decoder_compare"]
    if snapshot.get("tsbk_opcodes") is not None:
        render_opcodes(snapshot["tsbk_opcodes"])
    render_hdl_health(snapshot.get("hdl_lsm"))
    render_irq(snapshot.get("irq_stats"))
    if snapshot.get("recent_tsbks") is not None:
        render_recent_tsbks(snapshot["recent_tsbks"])

    # Phase 4: roadmap evaluation -- the actual point of the script.
    snapshot["dc"] = snapshot.get("decoder_compare")
    snapshot["bands"] = snapshot.get("bands")
    snapshot["grants"] = snapshot.get("grants")
    render_roadmap(snapshot)

    # Phase 5: footer.
    print()
    if failures > 0:
        print(f"{YELLOW}{failures} endpoint(s) failed -- output may be partial.{RESET}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
