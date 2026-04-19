# SDRTrunk Unified Timeline v2 — Full-Activity Capture

Source: SDRTrunk event logs from **2026-04-18 22:42:52 – 22:43:58** (66 seconds).
Site: Clay County NAC 0x8A1, control channel 860.9625 MHz, 3 traffic channels active.
Generated file: [timeline_unified_v2.log](timeline_unified_v2.log) (4,714 events).

This is a much denser snapshot than [the prior 18-second v1 timeline](../sdrtrunk_call_timeline/timeline_unified.log) (252 events). It covers 10 traffic-channel captures (T1/T2/T3 at 857.9875 / 858.4375 / 858.4625 MHz) plus the matching CC.

## Event distribution

### Control channel (1 minute, 1 stream)

| Opcode | Count | Notes |
|---|---:|---|
| `MOTOROLA` (vendor) | 600 | `SYSTEM LOADING`, `TDMA DATA CHANNEL NOT ACTIVE`, `TRAFFIC CHANNEL` |
| `IDEN_UPDATE_TDMA` | 299 | Every ~200 ms, 3 unique band IDs |
| `IDEN_UPDATE` (FDMA) | 299 | Every ~200 ms, 3 unique band IDs |
| `TDMA_SYNC_BCST` | 200 | System time broadcast |
| `SEC_CCH_BROADCST` | 200 | Backup CCH channels A/B |
| `RFSS_STATUS_BCST` | 200 | RFSS 1 / Site 1 |
| `NET_STATUS_BCAST` | 200 | WACN 0xBEE00, System 0x8A0 |
| `SNDCP_DCH_ANN_EX` | 199 | Data channel announcement |
| `GRP_VCH_GRNT_UPD` | 150 | Periodic voice grant refresh |
| `CCH BASE STAT ID` | 108 | Callsign ID broadcast (opcode TBD) |
| `SNDCP_DCH_PAG_RQ` | 45 | Data page requests |
| `SNDCP_DCH_GRANT` | 33 | Data channel grants |
| `GRP_VCH_GRANT` | 30 | **Voice call grants with `FM:<src>`** |
| `ACK_RESPONSE_FNE` | 30 | FNE-side acknowledgements |
| `GRP_AFFIL_RESP` | 6 | Group affiliation responses |
| `UNIT_REG_RESPONS` | 4 | Unit registration responses |
| `DE_REGIST_ACK` | 2 | Unit de-registration |

### Traffic channels (3 streams)

| Event | T1 (858.4375) | T2 (857.9875) | T3 (858.4625) | Total |
|---|---:|---:|---:|---:|
| HDU | 5 | 0 | 8 | 13 |
| LDU1 | 34 | 0 | 61 | 95 |
| LDU2 | 30 | 0 | 58 | 88 |
| TDU (plain) | 2 | 0 | 3 | 5 |
| TDULC_STD | 198 | 1,211 | 194 | 1,603 |
| TDULC_MOT (end code) | 3 | 0 | 5 | 8 |

*T2 was captured during an end-of-call TDULC burst (no HDU/LDU — purely the Motorola-heavy tail that follows SYNC LOSS mid-capture).*

## Source attribution across the session

### Unique `FM:<src>` observed in LDU1 LC (traffic channel, mid-call)

```text
1001, 1003, 1014, 532610*, 3100023, 3400030, 3402104, 3406093,
3412077, 3412416, 3412538, 3416052, 3416142, 3436029, 3599067,
3599074
```

*`532610` / `TO:8322` are the canonical **LC CRC FAIL** corrupt values — SDRTrunk prints them with `***LINK CONTROL CRC FAIL***`; they are noise, not real IDs.*

### Unique `BY:<src>` observed in Motorola `TALK_COMPLETE` TDULC (traffic channel, end of speaker)

```text
3100023, 3400027, 3400030, 3402043, 3402110, 3404043, 3404048,
3406093, 3412051, 3412098, 3412558, 3412574, 3412714, 3412772,
3416052, 3416142, 3416323, 3421553, 3422017, 3436029, 3599077
```

21 distinct speakers emitted an end code in the window — that's the set our recorder should be able to stamp into filenames once the Hamming+RS + Golay+RS chains are live.

### Talkgroup universe

Voice TGs on this site (small): **300** (primary dispatch).
Everything else in the TO: set is a radio-to-radio or SNDCP page target, not a voice TG.

## Source-attribution flow (what SDRTrunk actually does)

Triggered on every call start:

```text
PRIORITY 1: CC side — GRP_VCH_GRANT
  • `FM:<src>`       ← primary source (known BEFORE retune)
  • TO:<tg>        ← primary TG
  • SERVICE OPTIONS:PRI<n> [ENC]  ← primary encryption flag
                                    (bit 6 of service_options byte)

PRIORITY 2: TC side — HDU (fires on retune)
  • TALKGROUP:<tg>  ← confirms TG
  • [UN]ENCRYPTED   ← confirms encryption flag
    (no source — HDU doesn't carry it)

PRIORITY 3: TC side — LDU1 embedded LC (Hamming + RS decoded)
  • `FM:<src>`       ← refines source mid-call
  • TO:<tg>        ← confirms TG
  • SERVICE OPTIONS ← refines encryption flag
  (first few LDU1s typically show FM:0 because the LC block spans
   multiple LDU1 repeats before enough hexbits have accumulated for
   RS to converge)

PRIORITY 4: TC side — TDULC MOTOROLA TALK_COMPLETE (end of speaker)
  • BY:<src>       ← confirms last speaker
  (only on Motorola-infrastructure sites — other vendors omit)
```

## Our implementation vs SDRTrunk's flow

| Field | Fishball source in 2026-04-19 build | SDRTrunk |
|---|---|---|
| `TO:<tg>` primary | `GRP_VCH_GRANT.talkgroup` (CC) | same |
| `FM:<src>` primary | **`GRP_VCH_GRANT.source` (CC) → `ImbeForwarder.current_source` → `AudioChunk.source` → recorder** | same |
| `FM:<src>` refine | `LDU1.LC` via Hamming(10,6) + RS(24,12,13) | same |
| `FM:<src>` confirm | `TDULC MOTOROLA.BY` via Golay(24,12) + RS(24,12,13) | same |
| ENCRYPTED primary | `GRP_VCH_GRANT.service_options` bit 6 → `ImbeForwarder.call_encrypted` | same |
| ENCRYPTED refine | (not yet wired — deferred; CC flag is sufficient per memory) | via HDU + LDU1 LC service options |

All four source-attribution priorities are wired into the 2026-04-19 build — the recorder stamps `FM:<src>` from whichever priority arrives first and later priorities refresh the value.

## See also

[decode_gaps.md](decode_gaps.md) — systematic comparison of every event type in this unified log against our current parsers, grouped by severity. Calls out the 3 TDULC variants (RFSS STATUS, GROUP VOICE UPDATE, CALL TERMINATION = **1,171 events** in a 66-second window) that currently fall into our "Other" bucket, the HDU/LDU2/ESS chains we don't decode at all, and the field-level formatter gaps on the ~15 TSBK opcodes we DO parse but under-render.
