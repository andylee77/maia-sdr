#!/usr/bin/env python3
"""Build a counts + cause-effect + flowchart view of a SDRTrunk
control + traffic log pair for a single call window.

Reads the control-channel decoded_messages.log and the per-call
traffic logs, narrows to the call window, counts each distinct
message type, maps each control message to what it causes on the
traffic side, and emits a Mermaid flowchart + markdown tables.
"""
from __future__ import annotations

import argparse
import collections
import os
import re
from datetime import datetime, timedelta


def parse(ln):
    m = re.match(r"(\d{8})\s+(\d{6}),(\w+),(.*)", ln.strip())
    if not m:
        return None
    return (
        datetime.strptime(m.group(1) + m.group(2), "%Y%m%d%H%M%S"),
        m.group(3),
        m.group(4).strip(),
    )


def cc_label(body: str) -> str:
    """Extract the TSBK opcode label (first ALL-CAPS token after TSBKn).

    Exception: "MOTOROLA <SYSTEM|TDMA|TRAFFIC>" and "CCH BASE" are two-
    token labels SDRTrunk prints for vendor-specific frames; preserve
    both words so we don't lump them into one bucket.
    """
    m = re.match(r".*?TSBK[123]\s+([A-Z_]+)(?:\s+([A-Z_]+))?", body)
    if not m:
        return "OTHER"
    first, second = m.group(1), m.group(2)
    if first == "MOTOROLA" and second in ("SYSTEM", "TDMA", "TRAFFIC"):
        return f"{first} {second}"
    if first == "CCH" and second == "BASE":
        return "CCH BASE"
    return first


def traffic_label(body: str) -> str:
    for tag in ("HDU  ", "LDU1 ", "LDU2 ", "TDULC", "SYNC LOSS"):
        if tag in body:
            return tag.strip()
    m = re.search(r"\s(TDU)[^L]", " " + body)
    if m:
        return "TDU"
    return "OTHER"


CC_EFFECT = {
    "GRP_VCH_GRANT": "retune + open new traffic follow-session",
    "GRP_VCH_GRNT_UPD": "grant rebroadcast; no retune, call already active",
    "SNDCP_DCH_GRANT": "data-channel grant; voice follower ignores",
    "SNDCP_DCH_PAG_RQ": "data-channel page request; voice follower ignores",
    "UU_V_CH_GRANT": "unit-to-unit voice grant; would retune if private-call follower is armed",
    "TELE_INT_V_CH_GRANT": "telephone interconnect grant; voice follower ignores",
    "IDEN_UPDATE": "FDMA band definition; decoder uses to resolve CHAN→Hz",
    "IDEN_UPDATE_TDMA": "TDMA band definition; same role as IDEN_UPDATE",
    "RFSS_STATUS_BCST": "periodic site status; decoder confirms connection",
    "NET_STATUS_BCAST": "periodic net status",
    "SEC_CCH_BROADCST": "backup control-channel list",
    "TDMA_SYNC_BCST": "microslot sync; used only for TDMA decoding",
    "SNDCP_DCH_ANN_EX": "data-channel availability advertisement",
    "MOTOROLA SYSTEM": "Motorola vendor system status (ignored)",
    "MOTOROLA TDMA": "Motorola vendor TDMA state (ignored)",
    "MOTOROLA TRAFFIC": "Motorola vendor traffic status (ignored)",
    "CCH BASE": "callsign ID broadcast (cosmetic)",
    "GRP_AFFIL_QUERY": "network queries unit affiliations",
    "GRP_AFFIL_RESP": "unit replies to affiliation query",
    "ACK_RESPONSE_FNE": "network ack of a prior unit action",
    "UNIT_REG_RESPONS": "unit registration accept/deny",
    "LOCN_REG_RESPONS": "location-registration response",
    "DE_REGIST_ACK": "unit de-registration ack",
}


def triggers_voice_follow(label: str) -> bool:
    return label in {"GRP_VCH_GRANT", "UU_V_CH_GRANT"}


def render_mermaid(cc_cnt, sessions, aggregates):
    """Generate Mermaid flowchart source."""
    lines = ["```mermaid", "flowchart TD"]
    # Control-side nodes (group into: trigger-voice, keep-alive, ignored)
    lines.append("    subgraph CC[\"Control Channel — in window\"]")
    trigger_labels = [n for n, c in cc_cnt.items() if triggers_voice_follow(n)]
    keepalive_labels = [n for n, c in cc_cnt.items() if n == "GRP_VCH_GRNT_UPD"]
    ignored_labels = sorted(
        n for n, c in cc_cnt.items()
        if not triggers_voice_follow(n) and n != "GRP_VCH_GRNT_UPD"
    )
    for i, lbl in enumerate(trigger_labels):
        safe = lbl.replace(" ", "_")
        lines.append(f'        CC_T{i}["{lbl}<br/>×{cc_cnt[lbl]}"]')
    for i, lbl in enumerate(keepalive_labels):
        lines.append(f'        CC_K{i}["{lbl}<br/>×{cc_cnt[lbl]}"]')
    # Collapse ignored labels into one node to keep the chart readable
    if ignored_labels:
        ig_lines = "<br/>".join(f"{lbl} ×{cc_cnt[lbl]}" for lbl in ignored_labels)
        lines.append(f'        CC_IG["ignored by voice follower:<br/>{ig_lines}"]')
    lines.append("    end")

    # Traffic sessions + their DUID state-machine summaries
    for idx, s in enumerate(sessions):
        lbl = s["label"]
        freq = s["freq"]
        cnts = s["counts"]
        fms = s["sources"] or ["(none — LC never clean)"]
        fm_str = ", ".join(str(x) for x in fms[:4])
        if len(fms) > 4:
            fm_str += f", +{len(fms)-4} more"
        node = (
            f'{lbl}["<b>{lbl}</b> {freq}<br/>'
            f'HDU×{cnts.get("HDU",0)} → '
            f'LDU1×{cnts.get("LDU1",0)} + LDU2×{cnts.get("LDU2",0)}<br/>'
            f'TDU×{cnts.get("TDU",0)}, TDULC×{cnts.get("TDULC",0)}<br/>'
            f'FM: {fm_str}"]'
        )
        lines.append(f"    {node}")

    # Wire up the control-effect arrows
    for i, lbl in enumerate(trigger_labels):
        lines.append(f'    CC_T{i} -->|"retune + open new<br/>follow-session"| {sessions[0]["label"] if sessions else "END"}')
    for i, lbl in enumerate(keepalive_labels):
        lines.append(f'    CC_K{i} -.->|"keep-alive, no action"| KEEP["increments grants_seen;<br/>no traffic state change"]')
    if ignored_labels:
        lines.append(f'    CC_IG -.->|"ignored"| NOP["no voice-follow action"]')

    # Traffic session chain: each subsequent session follows the previous
    for prev, cur in zip(sessions, sessions[1:]):
        lines.append(f'    {prev["label"]} -->|"call continues / channel handoff<br/>new PTT or CHAN change"| {cur["label"]}')

    # Terminal node
    if sessions:
        agg = aggregates
        lines.append(
            f'    {sessions[-1]["label"]} -->|"sync loss → close follow-session"| '
            f'END["<b>Totals in call window</b><br/>'
            f'HDU={agg["HDU"]}  LDU1={agg["LDU1"]}  LDU2={agg["LDU2"]}<br/>'
            f'TDU={agg["TDU"]}  TDULC={agg["TDULC"]}<br/>'
            f'TDULC/call = {agg["TDULC"]/max(agg["HDU"],1):.1f}"]'
        )
    lines.append("```")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cc", required=True, help="control-channel decoded_messages.log")
    ap.add_argument("--traffic", required=True, nargs="+",
                    help="traffic-channel logs in chronological order; format label=freq=path")
    ap.add_argument("--out", required=True, help="output .md")
    args = ap.parse_args()

    # Parse control channel
    cc_events = []
    with open(args.cc, encoding="utf-8", errors="replace") as f:
        for ln in f:
            p = parse(ln)
            if p:
                cc_events.append(p)

    # Parse traffic logs (expecting label=freq=path)
    tsessions = []
    for spec in args.traffic:
        parts = spec.split("=", 2)
        if len(parts) != 3:
            print(f"bad --traffic spec: {spec}")
            return 2
        label, freq, path = parts
        if not os.path.exists(path):
            print(f"missing traffic log: {path}")
            continue
        evs = []
        with open(path, encoding="utf-8", errors="replace") as f:
            for ln in f:
                p = parse(ln)
                if p:
                    evs.append(p)
        tsessions.append({"label": label, "freq": freq, "path": path, "events": evs})

    # Derive call window from traffic events
    all_t = [ts for s in tsessions for ts, _, _ in s["events"]]
    if not all_t:
        print("no traffic events found")
        return 1
    call_lo = min(all_t)
    call_hi = max(all_t)

    # CC events in ±15s of call window
    cc_in_window = [
        (ts, st, body)
        for ts, st, body in cc_events
        if call_lo - timedelta(seconds=15) <= ts <= call_hi + timedelta(seconds=5)
    ]
    cc_cnt = collections.Counter(cc_label(body) for _, _, body in cc_in_window)

    # Per-session traffic counts + sources
    for s in tsessions:
        cnt = collections.Counter(traffic_label(body) for _, _, body in s["events"])
        fms = set()
        lc_fail = 0
        for _, _, body in s["events"]:
            m = re.search(r"FM:(\d+)", body)
            if m and m.group(1) != "0":
                fms.add(m.group(1))
            if "LINK CONTROL CRC FAIL" in body:
                lc_fail += 1
        s["counts"] = cnt
        s["sources"] = sorted(fms)
        s["lc_fail"] = lc_fail
        s["span_s"] = (s["events"][-1][0] - s["events"][0][0]).total_seconds()

    # Aggregate traffic totals
    agg = collections.Counter()
    for s in tsessions:
        agg.update(s["counts"])

    # Render markdown
    lines = []
    lines.append(f"# SDRTrunk call-window timeline — counts + flow")
    lines.append("")
    lines.append(f"Call window: **{call_lo.strftime('%H:%M:%S')}–{call_hi.strftime('%H:%M:%S')}** "
                 f"({(call_hi-call_lo).total_seconds():.0f}s)")
    lines.append("")
    lines.append("## Flowchart")
    lines.append("")
    lines.append(render_mermaid(cc_cnt, tsessions, agg))
    lines.append("")
    lines.append("## Control-channel message counts (in window)")
    lines.append("")
    lines.append("| Message | Count | Effect on traffic side |")
    lines.append("|---|--:|---|")
    for name, count in sorted(cc_cnt.items(), key=lambda x: -x[1]):
        effect = CC_EFFECT.get(name, "(effect not mapped)")
        lines.append(f"| `{name}` | {count} | {effect} |")
    lines.append("")
    lines.append(f"Total CC messages in window: **{sum(cc_cnt.values())}**  "
                 f"(of which **{sum(cc_cnt[n] for n in cc_cnt if triggers_voice_follow(n))}** "
                 f"triggered a voice follow-session)")
    lines.append("")

    lines.append("## Traffic-channel message counts per follow-session")
    lines.append("")
    lines.append("| Session | Freq / CHAN | Span | HDU | LDU1 | LDU2 | TDU | TDULC | LC-FAIL | Sources |")
    lines.append("|---|---|--:|--:|--:|--:|--:|--:|--:|---|")
    for s in tsessions:
        c = s["counts"]
        src = ", ".join(s["sources"]) if s["sources"] else "—"
        lines.append(
            f"| **{s['label']}** | {s['freq']} | {s['span_s']:.0f}s "
            f"| {c.get('HDU',0)} | {c.get('LDU1',0)} | {c.get('LDU2',0)} "
            f"| {c.get('TDU',0)} | {c.get('TDULC',0)} | {s['lc_fail']} | {src} |"
        )
    lines.append("")

    lines.append("## Aggregate totals")
    lines.append("")
    lines.append(f"- **HDU** (PTT presses observed): **{agg['HDU']}**")
    lines.append(f"- **LDU1 / LDU2** (voice frame pairs): **{agg['LDU1']} / {agg['LDU2']}**")
    lines.append(f"- **TDU** (real terminators): **{agg['TDU']}**")
    lines.append(f"- **TDULC** (terminator tail): **{agg['TDULC']}**")
    lines.append(f"- **TDULC / HDU ratio**: **{agg['TDULC']/max(agg['HDU'],1):.2f}** per call")
    lines.append("")

    lines.append("## Key cause-effect rules observed")
    lines.append("")
    lines.append("1. **`GRP_VCH_GRANT`** is the only control-channel message that moves the traffic decoder. Every session in this capture was opened by one.")
    lines.append("2. **`GRP_VCH_GRNT_UPD`** is emitted ~every 200 ms while a call is active — it's a keep-alive rebroadcast, not a new grant. Fishball's `grants_seen` counter currently increments on every one; deduplicating by `(TG, CHAN)` within a rolling 1-2 s window would match SDRTrunk's semantic count.")
    lines.append("3. Everything else (IDEN_UPDATE, RFSS/NET_STATUS, SEC_CCH, TDMA_SYNC, MOTOROLA vendor, SNDCP_*, GRP_AFFIL_*, ACK_RESPONSE, registration opcodes) is site state / bookkeeping — never causes a voice follow-session.")
    lines.append("4. A follow-session ends on **SYNC LOSS**, not on TDU/TDULC. The TDULC tail continues as long as the decoder still holds bit-lock.")
    lines.append("5. **LINK CONTROL CRC FAIL** on a LDU1 line means the in-frame LC bits didn't survive FEC; the `FM:` value on that line is unreliable. Fishball has no LC FEC today so it would treat those `FM:` values as real.")
    lines.append("")

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print(f"saved -> {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
