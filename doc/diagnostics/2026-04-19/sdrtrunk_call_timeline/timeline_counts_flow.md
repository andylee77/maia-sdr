# SDRTrunk call-window timeline — counts + flow

Call window: **22:15:10–22:15:28** (18s)

## Flowchart

```mermaid
flowchart TD
    subgraph CC["Control Channel — in window"]
        CC_T0["GRP_VCH_GRANT<br/>×10"]
        CC_K0["GRP_VCH_GRNT_UPD<br/>×41"]
        CC_IG["ignored by voice follower:<br/>ACK_RESPONSE_FNE ×12<br/>CCH BASE ×68<br/>GRP_AFFIL_QUERY ×3<br/>IDEN_UPDATE ×190<br/>IDEN_UPDATE_TDMA ×189<br/>LOCN_REG_RESPONS ×1<br/>MOTOROLA SYSTEM ×126<br/>MOTOROLA TDMA ×127<br/>MOTOROLA TRAFFIC ×127<br/>NET_STATUS_BCAST ×127<br/>OTHER ×2<br/>RFSS_STATUS_BCST ×127<br/>SEC_CCH_BROADCST ×126<br/>SNDCP_DCH_ANN_EX ×126<br/>SNDCP_DCH_GRANT ×6<br/>TDMA_SYNC_BCST ×127"]
    end
    T1["<b>T1</b> 858.4375/0-1189<br/>HDU×1 → LDU1×8 + LDU2×8<br/>TDU×0, TDULC×50<br/>FM: 1014, 3406070, 532610"]
    T2["<b>T2</b> 857.9875/0-1193<br/>HDU×1 → LDU1×2 + LDU2×1<br/>TDU×0, TDULC×44<br/>FM: (none — LC never clean)"]
    T3["<b>T3</b> 857.9875/0-1193<br/>HDU×2 → LDU1×10 + LDU2×8<br/>TDU×0, TDULC×47<br/>FM: 1014, 3599061, 532610"]
    CC_T0 -->|"retune + open new<br/>follow-session"| T1
    CC_K0 -.->|"keep-alive, no action"| KEEP["increments grants_seen;<br/>no traffic state change"]
    CC_IG -.->|"ignored"| NOP["no voice-follow action"]
    T1 -->|"call continues / channel handoff<br/>new PTT or CHAN change"| T2
    T2 -->|"call continues / channel handoff<br/>new PTT or CHAN change"| T3
    T3 -->|"sync loss → close follow-session"| END["<b>Totals in call window</b><br/>HDU=4  LDU1=20  LDU2=17<br/>TDU=0  TDULC=141<br/>TDULC/call = 35.2"]
```

## Control-channel message counts (in window)

| Message | Count | Effect on traffic side |
|---|--:|---|
| `IDEN_UPDATE` | 190 | FDMA band definition; decoder uses to resolve CHAN→Hz |
| `IDEN_UPDATE_TDMA` | 189 | TDMA band definition; same role as IDEN_UPDATE |
| `RFSS_STATUS_BCST` | 127 | periodic site status; decoder confirms connection |
| `NET_STATUS_BCAST` | 127 | periodic net status |
| `MOTOROLA TDMA` | 127 | Motorola vendor TDMA state (ignored) |
| `MOTOROLA TRAFFIC` | 127 | Motorola vendor traffic status (ignored) |
| `TDMA_SYNC_BCST` | 127 | microslot sync; used only for TDMA decoding |
| `SEC_CCH_BROADCST` | 126 | backup control-channel list |
| `SNDCP_DCH_ANN_EX` | 126 | data-channel availability advertisement |
| `MOTOROLA SYSTEM` | 126 | Motorola vendor system status (ignored) |
| `CCH BASE` | 68 | callsign ID broadcast (cosmetic) |
| `GRP_VCH_GRNT_UPD` | 41 | grant rebroadcast; no retune, call already active |
| `ACK_RESPONSE_FNE` | 12 | network ack of a prior unit action |
| `GRP_VCH_GRANT` | 10 | retune + open new traffic follow-session |
| `SNDCP_DCH_GRANT` | 6 | data-channel grant; voice follower ignores |
| `GRP_AFFIL_QUERY` | 3 | network queries unit affiliations |
| `OTHER` | 2 | (effect not mapped) |
| `LOCN_REG_RESPONS` | 1 | location-registration response |

Total CC messages in window: **1535**  (of which **10** triggered a voice follow-session)

## Traffic-channel message counts per follow-session

| Session | Freq / CHAN | Span | HDU | LDU1 | LDU2 | TDU | TDULC | LC-FAIL | Sources |
|---|---|--:|--:|--:|--:|--:|--:|--:|---|
| **T1** | 858.4375/0-1189 | 6s | 1 | 8 | 8 | 0 | 50 | 1 | 1014, 3406070, 532610 |
| **T2** | 857.9875/0-1193 | 3s | 1 | 2 | 1 | 0 | 44 | 0 | — |
| **T3** | 857.9875/0-1193 | 6s | 2 | 10 | 8 | 0 | 47 | 1 | 1014, 3599061, 532610 |

## Aggregate totals

- **HDU** (PTT presses observed): **4**
- **LDU1 / LDU2** (voice frame pairs): **20 / 17**
- **TDU** (real terminators): **0**
- **TDULC** (terminator tail): **141**
- **TDULC / HDU ratio**: **35.25** per call

## Key cause-effect rules observed

1. **`GRP_VCH_GRANT`** is the only control-channel message that moves the traffic decoder. Every session in this capture was opened by one.
2. **`GRP_VCH_GRNT_UPD`** is emitted ~every 200 ms while a call is active — it's a keep-alive rebroadcast, not a new grant. Fishball's `grants_seen` counter currently increments on every one; deduplicating by `(TG, CHAN)` within a rolling 1-2 s window would match SDRTrunk's semantic count.
3. Everything else (IDEN_UPDATE, RFSS/NET_STATUS, SEC_CCH, TDMA_SYNC, MOTOROLA vendor, SNDCP_*, GRP_AFFIL_*, ACK_RESPONSE, registration opcodes) is site state / bookkeeping — never causes a voice follow-session.
4. A follow-session ends on **SYNC LOSS**, not on TDU/TDULC. The TDULC tail continues as long as the decoder still holds bit-lock.
5. **LINK CONTROL CRC FAIL** on a LDU1 line means the in-frame LC bits didn't survive FEC; the `FM:` value on that line is unreliable. Fishball has no LC FEC today so it would treat those `FM:` values as real.
