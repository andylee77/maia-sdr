# Decode Gaps vs SDRTrunk — Full Activity Review

Analysis of [timeline_unified_v2.log](timeline_unified_v2.log) (4,714 events, 2026-04-18 22:42:52–22:43:58, Clay County NAC 0x8A1) against the current [tsbk.rs](../../../p25-httpd/src/p25/tsbk.rs), [voice_frame.rs](../../../p25-httpd/src/p25/voice_frame.rs), and [history.rs](../../../p25-httpd/src/httpd/api/history.rs) parsers.

Gaps are grouped by severity. Each item lists **what SDRTrunk emits**, **what we emit**, and **what's missing**.

---

## CRITICAL gaps — opcode/frame types we drop entirely

### 1. TDULC variants beyond GVCU / Motorola TALK_COMPLETE

SDRTrunk recognises multiple LCW opcodes inside TDULC. We only recognise 2 of them; the rest fall into our `TdulcLcw::Other` bucket and get dumped as `TDULC OTHER OP:0xXX MFID:0xXX` on the activity feed. **Cumulative miscount: 1,171 frames in the 66-second window.**

| SDRTrunk label | Occurrences | What we do | Missing fields |
|---|---:|---|---|
| `TDULC RFSS STATUS BROADCAST` | 752 | `Other { opcode, mfid }` | LRA, system_id, site_id, rfss_id, channel, service options |
| `TDULC GROUP VOICE CHANNEL UPDATE` | 387 | `Other` | talkgroup_a, channel_a, talkgroup_b, channel_b |
| `TDULC CALL TERMINATION BY:<id>` | 32 | `Other` | `BY:<radio_id>` (LCW opcode 0x0F standard, MFID 0x00) |

**Fix:** extend `TdulcLcw` enum in [voice_frame.rs](../../../p25-httpd/src/p25/voice_frame.rs) to cover standard LCW opcodes 0x02 (Group Voice Update), 0x0F (Call Termination), and the broadcast variants (0x3A RFSS STS BCST LC, 0x3B NET STS BCST LC). Each adds one match arm after the existing MFID==0x90 gate.

### 2. HDU body parsing — NEVER decoded

SDRTrunk: `HDU TALKGROUP:300 UNENCRYPTED` or `HDU TALKGROUP:417 ENCRYPTION:AES-256 KEY:8259 MI:F8D6849E60979BD900`.
Us: just count the DUID — zero payload extraction.

**What we miss:**

- **Algorithm ID** (AES-256, DES, etc.) — critical when a call IS encrypted but we've never seen it before (no control-channel GRP_VCH_GRANT history yet)
- **Key ID** (tells operators which key to load)
- **Message Indicator (MI)** — the 72-bit IV required to decrypt AES-256 P25 voice (if we ever add decrypt support)
- **Talkgroup** (we already get from grant — redundant but would let us *verify*)

**Why it hasn't been wired:** HDU uses **Golay(18,6,8) + RS(63,47,17)** — *different* FEC primitives than TDULC / LDU1 LC. Requires porting 2 more decoders (~250 lines total). Deferred — memory `project_hdu_payload_parsing_deferred.md`.

**Impact today:** encrypted-call AES key lookup is blocked, and we can't show the operator which algorithm/key is in use for a specific call.

### 3. LDU2 ESS (Encryption Sync Signature) — NEVER decoded

SDRTrunk on a clear call: `LDU2 VOICE LSD:0000 UNENCRYPTED`.
SDRTrunk on an AES-256 call: `LDU2 VOICE LSD:4587 ENCRYPTION:AES-256 KEY:8259 MSG INDICATOR:55369B917909FA3400`.
Us: count LDU2 events, extract 9 IMBE frames for the vocoder.

**What we miss:**

- **LSD (Link Status Data)** — 16-bit per-LDU signaling indicator (important for some systems; SDRTrunk displays it)
- **ESS continuously-refreshed MI** — required for AES decryption key stream across ~every 360 ms

**FEC required:** RS(24,16,9) over the ESS hexbits — *different from* RS(24,12,13). ~100-line port.

### 4. `CCH BASE STAT ID` — unknown opcode

SDRTrunk: `TSBK3 CCH BASE STAT ID CHAN:0-1593 CWID:` — 108 occurrences.

Looks like either a Motorola vendor opcode OR opcode 0x38 `SYS_SRV_BCST` in an unexpected variant. Currently classifies as `Other` in our histogram. Needs reverse engineering from the raw TSBK bytes to confirm what opcode SDRTrunk is mapping this to (their label doesn't include `TSBK<n> LABEL` in the expected format).

**Fix path:** capture a `CCH BASE STAT ID` TSBK via `/api/control_iq_capture_aligned`, trellis-decode to 12 bytes, identify the opcode byte.

### 5. PDU (Packet Data Unit) and SNDCP data — NEVER decoded

SDRTrunk: `PDU RESPONSE TO:3402099 ALL BLOCKS RECEIVED` (22), `IPPKT LLID:3599067 NSAPI:1 IP FROM:10.51.1.116 TO:10.71.203.126 UDP PORT FROM:4001 TO:4001 LRRP TRIGGERED LOCATION START` (4).
Us: nothing — PDU DUID 0xC never triggers the voice handler, the framer counts it and moves on.

**Impact:** LRRP GPS position reports from radios are invisible to us. Low priority for scanner use, but missed for fleet-tracking / dispatch insight.

---

## HIGH-VALUE gaps — opcode IS parsed but fields are incomplete

### 6. `GRP_VCH_GRANT` / `GRP_VCH_GRNT_UPD` — service options text

SDRTrunk: `GRP_VCH_GRANT FM:3416052 TO:417 CHAN:0-1189 PRI4 ENCRYPTED CIRCUIT` (20 occurrences on TG 417).
Us: `GRP_V_CH_GRANT CH:0-1189 TG:417 SRC:3416052 [ENC]`.

**Missing:** `PRI<n>` priority level (we extract but don't render), `CIRCUIT`/`PACKET` session mode flag, `DUPLEX` flag. All three live in the service_options byte we already extract — pure summary-formatter gap in [history.rs](../../../p25-httpd/src/httpd/api/history.rs).

### 7. `GRP_VCH_GRNT_UPD` two-TG variant

SDRTrunk: `GRP_VCH_GRNT_UPD GROUP A:300 CHAN A:0-1189 GROUP B:417 CHAN B:0-1117` (37 occurrences).
Us: `GRP_V_CH_GRANT_UPDT CH_A:0-1189 TG_A:300 CH_B:0-1117 TG_B:417` ✓ (both TG pairs shown)

**Status: OK.** Both pairs are present. Only cosmetic label difference.

### 8. `ACK_RESPONSE_FNE` — service type not decoded to label

SDRTrunk: `ACK_RESPONSE_FNE FM:16777213 TO:3416052 ACKNOWLEDGING:GRP_V_REQ` (18), `ACKNOWLEDGING:CAN_SRV_REQ` (12).
Us: `ACK_RESP_FNE SVC:0x<raw_service_type_byte> SRC:<src> TGT:<tgt>`.

**Missing:** the `ACKNOWLEDGING:<service>` label — we emit the raw service_type byte but don't map it to SDRTrunk's service-type enum (GRP_V_REQ, CAN_SRV_REQ, U_REG_REQ, etc., per `SpecialServiceType.java`).

### 9. `RFSS_STATUS_BCST` — missing SYSTEM id + service options

SDRTrunk: `RFSS_STATUS_BCST SYSTEM:2208/x8A0 RFSS:1/x01 SITE:1/x01 LRA:0/x00 CHAN:0-1593 ACTIVE NETWORK CONNECTION SERVICE OPTIONS:[DATA, VOICE, REGISTRATION]`.
Us: `RFSS_STATUS_BCST LRA:0 RFSS:1 SITE:1 CH:0-1593`.

**Missing:**

- `SYSTEM:<id>` — we have the bits but don't pack them into our enum
- `ACTIVE NETWORK CONNECTION` flag — single bit we don't extract
- `SERVICE OPTIONS:[DATA, VOICE, REGISTRATION]` — system-services byte we don't extract

### 10. `NET_STATUS_BCAST` — missing LRA + services

SDRTrunk: `NET_STATUS_BCAST WACN:781824/xBEE00 SYSTEM:2208/x8A0 LRA:0/x00 CHAN:0-1593 SERVICES:[DATA, VOICE, REGISTRATION]`.
Us: `NET_STS_BCST WACN:BEE00 SYS:8A0 CH:0-1593`.

**Missing:** `LRA:<n>`, `SERVICES:[...]` — raw bytes available, need extracting.

### 11. `SEC_CCH_BROADCST` — missing service options

SDRTrunk: `SEC_CCH_BROADCST RFSS:1/x01 SITE:1/x01 CHAN A:0-1277 SERVICE OPTIONS:[BACKUP CONTROL CHANNEL] CHAN B:0-1509 SERVICE OPTIONS:[BACKUP CONTROL CHANNEL]`.
Us: `SEC_CCH_BROADCST RFSS:1 SITE:1 A:0-1277 B:0-1509`.

**Missing:** per-channel service options flags (BACKUP CONTROL CHANNEL marker).

### 12. `IDEN_UPDATE_TDMA` — missing timeslots + vocoder

SDRTrunk: `IDEN_UPDATE_TDMA ID:3 OFFSET:30000000 SPACING:12500 BASE:762006250 TDMA BW:12500 TIMESLOTS:2 VOCODER:HALF_RATE`.
Us: `IDEN_UPDATE ID:3 OFFSET:30000000 SPACING:12500 BASE:762006250 BW:<channel_type>`.

**Missing:** `TIMESLOTS:<n>`, `VOCODER:<type>`. The TDMA variant carries channel-type info that encodes both; we fold it into `bw` and don't decode.

### 13. `TDMA_SYNC_BCST` — missing seconds/timezone/leap/rollover

SDRTrunk: `TDMA_SYNC_BCST SYSTEM TIME UNLOCKED: 2026-04-18 21:46:40.350 -0500 LEAP-SECOND CORRECTION:0.0mS MICROSLOT-MINUTE ROLLOVER:SLOW`.
Us: `TDMA_SYNC_BCST 2026-04-18 21:46 UNLOCKED`.

**Missing:** seconds, milliseconds, UTC offset, leap-second correction, microslot-minute rollover rate. Useful for long-term clock-sync diagnostics.

### 14. `SNDCP_DCH_ANN_EX` — missing DAC / service options

SDRTrunk: `SNDCP_DCH_ANN_EX CHAN:0-1193/----- AUTONOMOUS/REQUESTED-ACCESS DAC:1 **:0 SERVICE OPTIONS:NSAPI:0 HALF DUPLEX CIRCUIT MODE`.
Us: `SNDCP_DCH_ANN_EX DL:0-1193 UL:15-4095 AUTO REQ`.

**Missing:** `DAC:<n>` (Data Access Code), `NSAPI:<n>`, `HALF DUPLEX CIRCUIT MODE` service-options flags.

### 15. `MOTOROLA` vendor TSBK raw bytes

SDRTrunk: `MOTOROLA SYSTEM LOADING MSG:09900F00000000000000EF7F` (600 total across 3 vendor subtypes).
Us: `MOT MFID:0x90 OP:0x<raw>`.

**Missing:** raw hex payload. SDRTrunk appends it so operators can reverse-engineer; we drop it. Also missing: the three vendor subtypes:

- `MOTOROLA SYSTEM LOADING` (opcode ~0x00 with MFID 0x90)
- `MOTOROLA TDMA DATA CHANNEL NOT ACTIVE` (opcode ~0x16)
- `MOTOROLA TRAFFIC CHANNEL` (opcode ~0x16 with different service bits)

Recognising them by `opcode_raw` + dumping the 8-byte payload as hex would close this.

---

## MEDIUM-VALUE gaps — traffic-channel telemetry

### 16. `LDU1 LC` — we ignore service options + LSD

SDRTrunk on a clear call: `LDU1 VOICE LSD:0000 GROUP VOICE CHANNEL USER FM:3400030 TO:300 SERVICE OPTIONS:PRI4 CIRCUIT`.
SDRTrunk on an AES-256 call: `LDU1 VOICE LSD:8265 GROUP VOICE CHANNEL USER FM:3416052 TO:417 SERVICE OPTIONS:PRI4 ENCRYPTED CIRCUIT`.

Us: via `parse_ldu1_lcw` + `parse_ldu1_source` we extract FM (source) and TG. We DON'T extract:

- **Service options byte** — specifically the `ENCRYPTED` flag (a second path to the encryption status after the CC grant). Would let us confirm encryption mid-call even if we missed the grant.
- **`LSD:<hex>`** — 16-bit Link Status Data per LDU (every LDU1 + LDU2 has its own LSD).

Both are cheap to add — we already have the 72-bit LC in hand after the RS decode, just need to also extract bits 16-23 (service options).

### 17. `TDULC MOTOROLA TALK COMPLETE` — we ignore UNK1 / UNK2

SDRTrunk: `TDULC MOTOROLA TALK COMPLETE BY:3416052 UNK1:10 UNK2:10`.
Us: `TDULC MOTOROLA TALK COMPLETE BY:3416052 TG:417`.

**Missing:** UNK1 and UNK2 bytes. SDRTrunk hasn't reverse-engineered them either (always `10`), but for parity we should carry them through.

---

## LOW-VALUE gaps — noise / diagnostic

### 18. `SYNC LOSS` events

SDRTrunk: `<-> SYNC LOSS - BITS PROCESSED [N]` — 26 occurrences in the window.
Us: our framer tracks `sync_distance` / `best_sync_distance` internally, but we don't emit a typed event when sync is lost.

**Fix:** push a `TRF_SYNC_LOSS` / `CC_SYNC_LOSS` event to the activity feed whenever the decoder state transitions from `ReadingNid` / `ReadingDataUnit` back to `Hunting`.

### 19. `**CRC-FAILED**` partial messages

SDRTrunk shows: `TSBK2 **CRC-FAILED** GRP_VCH_GRANT FM:3736029 TO:6 CHAN:0-0 PRI0 CIRCUIT` — even when CRC fails, it prints what it WOULD have been.
Us: we count `crc_failures` but don't render the failed payload.

Limited value for correctness but useful for monitoring — we're discarding potential insight.

---

## Proposed implementation order (highest ROI first)

1. **Extend `TdulcLcw` enum** for `GROUP VOICE CHANNEL UPDATE`, `CALL TERMINATION`, `RFSS STATUS BROADCAST`, `NET STATUS BROADCAST` — 1,171 frames/minute move out of `Other`, activity feed becomes readable.
2. **LDU1 LC service_options extraction** — gives mid-call encryption flag refresh for free (bits already in hand).
3. **Summary-formatter updates** in [history.rs](../../../p25-httpd/src/httpd/api/history.rs) — render `PRI<n>`, `CIRCUIT/PACKET`, `ENCRYPTED`, `ACKNOWLEDGING:<service>` labels, `SERVICE OPTIONS:[...]` flags for the TSBKs we already fully parse. ~100 lines of string formatting.
4. **`CCH BASE STAT ID` reverse-engineer** — capture one on-target, identify the opcode.
5. **Motorola vendor subtype labels** + raw hex payload passthrough.
6. **HDU parsing (Golay18 + RS 63,47,17)** — enables AES key/MI extraction for encrypted calls. ~250 lines.
7. **LDU2 ESS parsing (RS 24,16,9)** — ongoing encryption MI refresh across the call. ~100 lines.
8. **SYNC LOSS events** — typed activity feed entry.
9. **PDU / LRRP decoding** — niche.
10. **CRC-FAILED partial-message rendering** — niche.
