#!/usr/bin/env python3
"""Fishball P25 on-target verification status script.

Hits all diagnostic endpoints on the running p25-httpd target,
pretty-prints the key metrics, and tells you whether each acceptance
criterion has been met.

Usage:
    python tools/p25_check.py [TARGET]

TARGET defaults to "192.168.2.1:8080" (Fishball Z7020 wired Ethernet
direct-connect address). Pass another host:port to point at a
different target.

Endpoints exercised:
    GET /api/system           -- build tag, NAC/WACN/RFSS/SITE
    GET /api/stats            -- basic decode stats + gain/RSSI
    GET /api/decoder_compare  -- pipeline counters
    GET /api/tsbk_opcodes     -- per-opcode + per-block-position hist
    GET /api/recent_tsbks     -- newest 50 TSBKs with TSBK1/2/3 labels
    GET /api/bands            -- frequency band table
    GET /api/grants           -- active voice grants
    GET /api/lsm              -- Phase 6D LSM decoder stats
    GET /api/hdl_lsm          -- PL HDL LSM chain stats + NID ring
    GET /api/irq_stats        -- per-source IRQ counters
    GET /api/traffic          -- Phase 7 traffic channel + IMBE stats
    GET /api/lsm_control      -- LSM control register state
    GET /api/sync_tune        -- sync distance histogram + threshold

Exit code: 0 if every acceptance check passes, 1 otherwise. Lets you
shove this in a `while sleep 30; do ...` loop on flash + watch.
"""

from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request

# ANSI colours -- works in modern Windows terminals + git-bash. We do
# this manually instead of pulling in `colorama` so the script has zero
# dependencies and runs on a fresh checkout.
GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
CYAN = "\033[36m"
DIM = "\033[2m"
BOLD = "\033[1m"
RESET = "\033[0m"


def fetch(target: str, path: str) -> dict | list:
    url = f"http://{target}{path}"
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return json.loads(r.read().decode("utf-8"))
    except urllib.error.URLError as e:
        print(f"{RED}!! GET {url} failed: {e}{RESET}")
        sys.exit(2)
    except json.JSONDecodeError as e:
        print(f"{RED}!! GET {url}: invalid JSON: {e}{RESET}")
        sys.exit(2)


def banner(text: str) -> None:
    print()
    print(f"{BOLD}{CYAN}== {text} =={RESET}")


def kv(label: str, value, ok: bool | None = None) -> None:
    """Print a label:value line. ok=True/False/None drives colour."""
    color = ""
    mark = "  "
    if ok is True:
        color = GREEN
        mark = "OK"
    elif ok is False:
        color = RED
        mark = "!!"
    print(f"  {color}[{mark}]{RESET} {label:32s} {value}")


def main() -> int:
    target = sys.argv[1] if len(sys.argv) >= 2 else "192.168.2.1:8080"
    print(f"{BOLD}Fishball P25 verification @ {target}{RESET}")

    # ── /api/system ──
    banner("System identity (/api/system)")
    sys_info = fetch(target, "/api/system")
    build = sys_info.get("build", "(none)")
    nac = sys_info.get("nac")
    wacn = sys_info.get("wacn")
    sysid = sys_info.get("system_id")
    rfss = sys_info.get("rfss_id")
    site = sys_info.get("site_id")
    cc = sys_info.get("control_channel")

    import re
    build_ok = bool(re.search(r"phase[67]", build or ""))
    rfss_ok = rfss == 1
    wacn_ok = wacn == "BEE00"

    kv("build tag", build, build_ok)
    kv("NAC", nac, nac == "8A1")
    kv("WACN", wacn, wacn_ok)
    kv("System ID", sysid, sysid == "8A0")
    kv("RFSS ID", f"{rfss} (expected 1 in 6F.4)", rfss_ok)
    kv("Site ID", site, site == 1)
    kv("Control channel", cc, cc == "0-1593")

    # ── /api/decoder_compare → ps_lsm ──
    banner("Decoder compare (ps_lsm slice)")
    dc = fetch(target, "/api/decoder_compare")
    ps = dc.get("ps_lsm", {})
    tsdu_attempts = ps.get("tsdu_attempts", 0)
    block_attempts = ps.get("tsbk_block_attempts", 0)
    crc_ok = ps.get("tsbk_crc_ok", 0)
    crc_fail = ps.get("tsbk_crc_failures", 0)
    messages = ps.get("messages", 0)
    bands_known = ps.get("bands_known", 0)
    active_grants = ps.get("active_grants", 0)
    nid_decoded = ps.get("nid_decoded_ok", 0)
    nid_attempts = ps.get("nid_attempts", 0)
    total_dibits = ps.get("total_dibits", 0)

    blocks_per_tsdu = block_attempts / max(tsdu_attempts, 1)
    crc_ok_pct = 100.0 * crc_ok / max(block_attempts, 1)
    nid_pct = 100.0 * nid_decoded / max(nid_attempts, 1)
    seconds = total_dibits / 4800.0
    # 6F.10: report rates derived from CRC-OK count instead of the
    # `recent_messages` ring buffer length, which capped at 100
    # pre-6F.10 and saturated in ~7 seconds at the steady-state
    # throughput. The new headline metric is "useful TSBK blocks per
    # second" which is what `tsbk_crc_ok / uptime` measures.
    crc_ok_per_sec = crc_ok / max(seconds, 1.0)
    blocks_per_sec = block_attempts / max(seconds, 1.0)
    tsdu_per_sec = tsdu_attempts / max(seconds, 1.0)

    # 6F.3 was 1.6 blocks/TSDU, 6F.4 target is closer to 3.0
    bptd_ok = blocks_per_tsdu >= 2.5
    # 6F.9 throughput targets: 30 TSBK attempts/s, 14+ CRC OK/s.
    crc_rate_ok = crc_ok_per_sec >= 14.0

    kv("uptime (estimated)", f"{seconds:.0f} s")
    kv("nid_attempts", f"{nid_attempts} ({nid_attempts/max(seconds,1):.2f}/s)")
    kv("nid_decoded_ok", f"{nid_decoded} ({nid_pct:.1f}%)", nid_pct >= 75)
    kv("tsdu_attempts", f"{tsdu_attempts} ({tsdu_per_sec:.2f}/s)")
    kv("tsbk_block_attempts", f"{block_attempts} ({blocks_per_sec:.2f}/s)")
    kv("blocks per TSDU", f"{blocks_per_tsdu:.2f} (target ~3.0)", bptd_ok)
    kv("tsbk_crc_ok",
       f"{crc_ok} ({crc_ok_pct:.1f}% pass, {crc_ok_per_sec:.2f}/s)",
       crc_rate_ok)
    kv("tsbk_crc_fail", crc_fail)
    kv("recent_msgs ring",
       f"{messages} (capped at 1000 in 6F.10, was 100)")
    kv("bands_known", f"{bands_known} (target >= 6 with FDMA + TDMA bands)",
       bands_known >= 6)
    kv("active_grants", f"{active_grants} (depends on call activity)")

    # ── /api/decoder_compare → ps_iq_lsm slice (6F.9+) ──
    banner("IQ-LSM decoder (Phase 6D soft sync -> TSBK, 6F.9+)")
    iq = dc.get("ps_iq_lsm")
    if iq is None:
        print(f"  {DIM}(ps_iq_lsm not in response -- pre-6F.9 build?){RESET}")
    else:
        iq_tsdu = iq.get("tsdu_attempts", 0)
        iq_blocks = iq.get("tsbk_block_attempts", 0)
        iq_crc_ok = iq.get("tsbk_crc_ok", 0)
        iq_nid_ok = iq.get("nid_decoded_ok", 0)
        iq_nid_att = iq.get("nid_attempts", 0)
        iq_msgs = iq.get("messages", 0)
        iq_bands = iq.get("bands_known", 0)
        iq_grants = iq.get("active_grants", 0)
        iq_tsdu_per_s = iq_tsdu / max(seconds, 1.0)
        iq_block_per_s = iq_blocks / max(seconds, 1.0)
        iq_crc_per_s = iq_crc_ok / max(seconds, 1.0)
        iq_nid_pct = 100.0 * iq_nid_ok / max(iq_nid_att, 1)
        iq_crc_pct = 100.0 * iq_crc_ok / max(iq_blocks, 1)

        # 6F.10: compare CRC-OK rates instead of the saturated
        # recent_messages ring count.
        ratio = iq_crc_per_s / max(crc_ok_per_sec, 0.001)
        combined_crc_per_s = crc_ok_per_sec + iq_crc_per_s

        kv("nid_attempts", iq_nid_att)
        kv("nid_decoded_ok", f"{iq_nid_ok} ({iq_nid_pct:.1f}%)")
        kv("tsdu_attempts", f"{iq_tsdu} ({iq_tsdu_per_s:.2f}/s)")
        kv("tsbk_block_attempts", f"{iq_blocks} ({iq_block_per_s:.2f}/s)")
        kv("tsbk_crc_ok",
           f"{iq_crc_ok} ({iq_crc_pct:.1f}% pass, {iq_crc_per_s:.2f}/s)")
        kv("recent_msgs ring", f"{iq_msgs}")
        kv("bands_known", iq_bands)
        kv("active_grants", iq_grants)
        kv("vs ps_lsm CRC OK/s",
           f"{ratio:.2f}x ({crc_ok_per_sec:.2f} -> {iq_crc_per_s:.2f})",
           ratio >= 0.5)
        kv("COMBINED CRC OK/s",
           f"{combined_crc_per_s:.2f}/s (target >= 30/s)",
           combined_crc_per_s >= 30.0)

    # ── /api/lsm_dibit_dump  (sync distance histogram, 6F.6+) ──
    banner("Sync distance histogram (6F.6+)")
    dd = fetch(target, "/api/lsm_dibit_dump")
    s = dd.get("sync", {})
    hist = s.get("distance_hist")
    if hist:
        total = sum(hist)
        if total == 0:
            print(f"  {DIM}(no events captured yet){RESET}")
        else:
            # Find max bucket value for bar scaling.
            max_bucket = max(hist) or 1
            print(f"  {DIM}Total sync events seen: {total}, "
                  f"threshold = {s.get('threshold')}{RESET}")
            print(f"  {'dist':>4s} {'count':>8s} {'pct':>6s}")
            for i, n in enumerate(hist):
                if n == 0 and i > 0 and i < 24:
                    continue
                pct = 100.0 * n / total
                bar_len = int(40 * n / max_bucket)
                bar = "#" * bar_len
                color = ""
                if i <= s.get("threshold", 14):
                    color = GREEN  # within threshold (a hit)
                label = f"{i:>4d}" if i < 24 else ">=24"
                print(f"  {color}{label:>4s} {n:>8d} {pct:>5.1f}% {bar}{RESET}")
    else:
        print(f"  {DIM}(distance_hist not in response -- pre-6F.6 build?){RESET}")

    # ── /api/tsbk_opcodes ──
    banner("Per-opcode + per-block histogram (/api/tsbk_opcodes)")
    op = fetch(target, "/api/tsbk_opcodes")

    print(f"  {DIM}blocks/TSDU = {op.get('blocks_per_tsdu', 0):.2f}, "
          f"CRC ok = {op.get('crc_ok_total', 0)} / "
          f"{op.get('crc_ok_total', 0) + op.get('crc_fail_total', 0)}"
          f" ({op.get('crc_ok_pct', 0):.1f}%){RESET}")

    pos = op.get("by_position", {})
    print()
    print(f"  {BOLD}Per-block-position attempts + CRC OK rate:{RESET}")
    for label in ("tsbk1", "tsbk2", "tsbk3"):
        d = pos.get(label, {})
        a = d.get("attempts", 0)
        ok = d.get("crc_ok", 0)
        pct = d.get("crc_ok_pct", 0.0)
        bar = "#" * int(pct / 4)
        print(f"    {label.upper():6s} attempts={a:6d} crc_ok={ok:5d} "
              f"({pct:5.1f}%) {DIM}{bar}{RESET}")

    mfid = op.get("mfid_breakdown", {})
    print()
    print(f"  {BOLD}MFID breakdown (CRC OK only):{RESET}")
    for k, v in mfid.items():
        print(f"    {k:18s} {v}")

    print()
    print(f"  {BOLD}Top opcodes (CRC OK):{RESET}")
    print(f"    {'opcode':8s} {'label':22s} {'ok':>7s} {'fail':>7s} parsed")
    print(f"    {'-'*8} {'-'*22} {'-'*7} {'-'*7} ------")
    for entry in op.get("opcodes", [])[:25]:
        oc = entry.get("opcode", "")
        lbl = entry.get("label", "")
        ok = entry.get("ok", 0)
        fail = entry.get("fail", 0)
        parsed = "yes" if entry.get("parsed") else " - "
        color = GREEN if entry.get("parsed") else ""
        print(f"    {color}{oc:8s} {lbl:22s} {ok:7d} {fail:7d} {parsed}{RESET}")

    # ── /api/bands ──
    banner("Frequency band table (/api/bands)")
    bands = fetch(target, "/api/bands")
    if not bands:
        print(f"  {RED}(empty -- IDEN_UPDATE not landing){RESET}")
    else:
        print(f"  {'id':>3s} {'base MHz':>13s} {'spacing kHz':>12s} "
              f"{'offset MHz':>11s} {'BW kHz':>9s}")
        for b in bands:
            print(f"  {b.get('identifier', '?'):>3} "
                  f"{b.get('base_frequency_mhz', 0):>13.5f} "
                  f"{b.get('channel_spacing_khz', 0):>12.3f} "
                  f"{b.get('transmit_offset_mhz', 0):>11.3f} "
                  f"{b.get('bandwidth_khz', 0):>9.2f}")

    # ── /api/grants ──
    banner("Active voice grants (/api/grants)")
    grants = fetch(target, "/api/grants")
    if not grants:
        print(f"  {DIM}(none -- no active calls right now){RESET}")
    else:
        for g in grants:
            print(f"  ch={g.get('channel')} TG={g.get('talkgroup')}"
                  f" src={g.get('source')} freq={g.get('frequency_mhz')}"
                  f" age={g.get('age_secs')}s")

    # ── /api/recent_tsbks ──
    banner("Recent TSBKs (/api/recent_tsbks)")
    rt = fetch(target, "/api/recent_tsbks")
    msgs = rt.get("messages", [])
    if not msgs:
        print(f"  {RED}(empty){RESET}")
    else:
        print(f"  {DIM}showing newest {min(15, len(msgs))} of "
              f"{rt.get('count', 0)}{RESET}")
        for m in msgs[:15]:
            block = m.get("block", "?")
            age = m.get("age_secs", 0)
            summary = m.get("summary", "")
            print(f"  -{age:6.1f}s  {block}  {summary}")

    # ── /api/stats ──
    banner("Basic stats (/api/stats)")
    st = fetch(target, "/api/stats")
    kv("system_acquired", st.get("system_acquired"), st.get("system_acquired"))
    kv("dibit_count (C4FM HDL)", st.get("dibit_count"))
    kv("overflow", st.get("overflow"), not st.get("overflow"))
    kv("rx_gain_db", f"{st.get('rx_gain_db', 0):.1f} dB")
    kv("rx_rssi_db", f"{st.get('rx_rssi_db', 0):.2f} dB")

    # ── /api/lsm ──
    banner("LSM decoder (/api/lsm)")
    lsm = fetch(target, "/api/lsm")
    lsm_up = lsm.get("uptime_secs", 0)
    lsm_running = lsm.get("running", False)
    kv("running", lsm_running, lsm_running)
    kv("uptime", f"{lsm_up}s")
    kv("wakeups", lsm.get("wakeups"))
    kv("IQ samples", f"{lsm.get('iq_samples', 0):,} ({lsm.get('iq_samples_per_sec', 0):.0f}/s)")
    kv("dibits", f"{lsm.get('dibits', 0):,} ({lsm.get('dibits_per_sec', 0):.0f}/s)")
    kv("hard / soft syncs", f"{lsm.get('hard_events', 0):,} / {lsm.get('soft_events', 0):,}")
    kv("overflow resets", lsm.get("overflow_resets", 0),
       lsm.get("overflow_resets", 0) == 0)
    ls = lsm.get("last_sync", {})
    kv("last sync", f"NAC={ls.get('nac')} DUID={ls.get('duid')} "
       f"FEC={'OK' if ls.get('fec_corrected') else 'FAIL'} "
       f"({ls.get('age_ms', '?')}ms ago)")

    # ── /api/hdl_lsm ──
    banner("HDL LSM chain (/api/hdl_lsm)")
    hdl = fetch(target, "/api/hdl_lsm")
    cum = hdl.get("cumulative", {})
    live = hdl.get("live", {})
    win = hdl.get("last_window", {})
    hdl_valid = cum.get("valid_nid_events", 0)
    hdl_total = cum.get("total_nid_events", 0)
    hdl_pct = cum.get("valid_pct", 0)
    kv("cumulative NIDs", f"{hdl_valid:,} / {hdl_total:,} ({hdl_pct:.1f}%)",
       hdl_pct >= 75)
    kv("last NAC / DUID", f"0x{live.get('last_nac', '?')} / {live.get('last_duid', '?')}")
    kv("drop count", live.get("last_drop_count", 0),
       live.get("last_drop_count", 0) == 0)
    kv("PLL / SP (now)", f"{live.get('pll_dbg', '?')} / {live.get('sp_dbg', '?')}")
    kv("sync distance (now)", live.get("sync_distance", "?"))
    kv("dibit / iq overflow ticks", f"{cum.get('dibit_overflow_ticks', 0)} / "
       f"{cum.get('iq_overflow_ticks', 0)}")
    kv("last 1s window NIDs", f"{win.get('valid_count', '?')} / "
       f"{win.get('event_count', '?')}")
    kv("last 1s PLL min/max", f"{win.get('pll_min', '?')} / {win.get('pll_max', '?')}")
    kv("last 1s SP min/max", f"{win.get('sp_min', '?')} / {win.get('sp_max', '?')}")

    # ── /api/irq_stats ──
    banner("IRQ stats (/api/irq_stats)")
    irq = fetch(target, "/api/irq_stats")
    irq_rates = irq.get("rate_per_sec", {})
    kv("total IRQs", f"{irq.get('total', 0):,} ({irq_rates.get('total', 0):.1f}/s)")
    kv("IQ DMA", f"{irq.get('iq', 0):,} ({irq_rates.get('iq', 0):.1f}/s)")
    kv("C4FM dibit DMA", f"{irq.get('dibit', 0):,} ({irq_rates.get('dibit', 0):.2f}/s)")
    kv("LSM dibit DMA", f"{irq.get('lsm_dibit', 0):,} ({irq_rates.get('lsm_dibit', 0):.2f}/s)")
    kv("traffic DMA", f"{irq.get('traffic', 0):,} ({irq_rates.get('traffic', 0):.2f}/s)")
    kv("last IRQ (ms ago)", irq.get("last_at_ms_ago"))
    kv("uptime", f"{irq.get('uptime_secs', 0)}s")

    # ── /api/traffic (Phase 7) ──
    banner("Traffic channel (/api/traffic)")
    tr = fetch(target, "/api/traffic")
    tr_state = tr.get("state", "?")
    tr_phase = tr.get("phase", "?")
    follower = tr.get("follower_enabled", False)
    kv("phase", tr_phase)
    kv("state", tr_state)
    kv("follower enabled", follower, follower)
    kv("grants seen", f"{tr.get('grants_seen', 0):,}")
    kv("retunes", tr.get("retunes", 0))

    cur_tg = tr.get("current_talkgroup")
    cur_ch = tr.get("current_channel")
    cur_freq = tr.get("current_frequency_hz")
    if cur_tg is not None:
        kv("current call", f"TG={cur_tg} CH={cur_ch} freq={cur_freq}Hz")
    else:
        kv("current call", f"{DIM}(idle){RESET}")

    kv("last NAC", tr.get("last_nac_hex", "?"))
    kv("last DUID", f"{tr.get('last_duid_label', '?')} ({tr.get('last_duid_hex', '?')})")
    kv("last retune (s ago)", f"{tr.get('last_retune_secs_ago', '?')}")
    kv("last offset Hz", tr.get("last_offset_hz"))

    imbe = tr.get("imbe", {})
    ldu1 = imbe.get("ldu1_count", 0)
    ldu2 = imbe.get("ldu2_count", 0)
    imbe_total = imbe.get("imbe_frames_extracted", 0)
    hdu_cnt = imbe.get("hdu_count", 0)
    tdu_cnt = imbe.get("tdu_count", 0)
    tdu_lc = imbe.get("tdu_lc_count", 0)
    expected_imbe = (ldu1 + ldu2) * 9
    imbe_match = imbe_total == expected_imbe if (ldu1 + ldu2) > 0 else None

    cur_enc = tr.get("current_call_encrypted")
    if cur_enc is not None:
        kv("current call encrypted", "YES" if cur_enc else "No")

    print()
    kv("HDUs seen", hdu_cnt)
    kv("LDU1 count", f"{ldu1:,}")
    kv("LDU2 count", f"{ldu2:,}")
    kv("TDU count", tdu_cnt)
    kv("TDU_LC count", f"{tdu_lc:,}")
    kv("IMBE frames extracted", f"{imbe_total:,}", imbe_match)
    kv("expected (LDU*9)", f"{expected_imbe:,}")
    imbe_dropped = imbe.get("imbe_frames_dropped", 0)
    kv("IMBE frames dropped", f"{imbe_dropped:,}",
       imbe_dropped == 0 if imbe_dropped is not None else None)
    kv("last IMBE (s ago)", f"{imbe.get('last_imbe_secs_ago', '?')}")

    # Phase 7D: vocoder stats
    voc_pcm = imbe.get("vocoder_pcm_produced", 0)
    voc_err = imbe.get("vocoder_errors", 0)
    voc_enc = imbe.get("vocoder_frames_encrypted", 0)
    print()
    kv("vocoder PCM produced", f"{voc_pcm:,}")
    kv("vocoder errors (>4 bit)", f"{voc_err:,}")
    kv("vocoder frames encrypted", f"{voc_enc:,}")

    tr_irq = tr.get("irq", {})
    kv("traffic DMA IRQs", tr_irq.get("traffic_dma_total"))
    kv("traffic LSM dibit IRQs", tr_irq.get("traffic_lsm_dibit_total"))

    # ── /api/lsm_control ──
    banner("LSM control register (/api/lsm_control)")
    lc = fetch(target, "/api/lsm_control")
    kv("lsm_enable", lc.get("lsm_enable"), lc.get("lsm_enable"))
    kv("lsm_dibit_dma_enable", lc.get("lsm_dibit_dma_enable"),
       lc.get("lsm_dibit_dma_enable"))
    kv("lsm_dc_block_enable", lc.get("lsm_dc_block_enable"),
       lc.get("lsm_dc_block_enable"))

    # ── /api/sync_tune (summary only) ──
    banner("Sync tune (/api/sync_tune)")
    sy = fetch(target, "/api/sync_tune")
    kv("current threshold", sy.get("current_threshold"))
    kv("total observations", f"{sy.get('total_observations', 0):,}")
    hist = sy.get("histogram", [])
    if hist:
        d0 = hist[0] if len(hist) > 0 else 0
        thresh = sy.get("current_threshold", 6)
        in_thresh = sum(hist[:thresh + 1]) if len(hist) > thresh else 0
        total_obs = sy.get("total_observations", 1)
        kv("dist=0 (exact match)", f"{d0:,} ({100.0*d0/max(total_obs,1):.2f}%)")
        kv(f"dist<=threshold ({thresh})", f"{in_thresh:,} ({100.0*in_thresh/max(total_obs,1):.2f}%)")

    # ── Acceptance summary ──
    banner("Acceptance summary")
    # Build tag: accept any phase6 or phase7 tag

    checks = [
        ("Build tag recognized", build_ok),
        ("RFSS = 1", rfss_ok),
        ("blocks_per_tsdu >= 2.5", bptd_ok),
        ("bands_known >= 6", bands_known >= 6),
        ("WACN matches Clay County (BEE00)", wacn_ok),
        ("LSM decoder running", lsm_running),
        ("HDL LSM valid >= 75%", hdl_pct >= 75),
        ("traffic follower enabled", follower),
        ("IMBE frames == (LDU1+LDU2)*9", imbe_match if imbe_match is not None else False),
        ("IMBE frames not dropped", imbe_dropped == 0),
        ("vocoder PCM produced > 0 (7D)", voc_pcm > 0),
    ]
    all_passed = True
    for label, ok in checks:
        kv(label, "PASS" if ok else "FAIL", ok)
        all_passed = all_passed and ok

    print()
    if all_passed:
        print(f"{BOLD}{GREEN}== Acceptance: ALL PASS =={RESET}")
        return 0
    else:
        print(f"{BOLD}{YELLOW}== Acceptance: PARTIAL "
              f"-- see failures above =={RESET}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
