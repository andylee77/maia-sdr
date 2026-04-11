# 028 -- Phase 6F.4: continue past CRC, IDEN_UPDATE/RFSS parser fixes, opcode histogram

**Date:** 2026-04-11
**Phase:** Phase 6F.4 (PS LSM TSBK pipeline -- on-target throughput tuning)
**Branch:** fishball-p25
**Status:** Implemented + unit-tested + diagnostic endpoints in place.
Pending second on-target verification (first 6F.3 verification revealed
the issues this phase fixes).

---

## TL;DR

Phase 6F.3 (doc 027) shipped multi-block TSBK reads and on-target
verification showed it was working: blocks/TSDU went from ~1.0 to
~1.6, message rate tripled. But three independent issues were
holding the decoder well below its theoretical throughput:

1. **Aborting on TSBK1 CRC fail.** When TSBK1 had a CRC error
   (~55% of the time on this site -- expected for the SNR), we
   stopped reading and never even attempted TSBK2/TSBK3. SDRTrunk's
   framer continues regardless, on the principle that the LB bit
   from a corrupted block is unreliable so safer to keep reading.
2. **Wrong IDEN_UPDATE opcode.** Our `TsbkOpcode::IdentifierUpdate`
   was mapped to **0x34**. SDRTrunk's `Opcode.java` shows that
   0x34 is `IDEN_UPDATE_VHF_UHF` (a VUHF-band variant Clay County
   does NOT broadcast). The standard FDMA `IDEN_UPDATE` is opcode
   **0x3D**, which we never decoded. This is why `bands_known`
   stayed at 0 even after 6F.3.
3. **RFSS bit-field offset bug.** `decode_rfss_sts_bcst` read RFSS
   from `payload[2]`, which is actually the SYSTEM low byte. On
   Clay County `system_id = 0x8A0`, and `0xA0 = 160` -- exactly
   the wrong value `/api/system` was reporting for `rfss_id`.

This phase fixes all three. It also adds two new diagnostic
endpoints (`/api/tsbk_opcodes` and `/api/recent_tsbks`) that
expose the opcode landscape and a per-block-labelled message feed
matching SDRTrunk's `decoded_messages.log` format -- so future
"why isn't X decoding?" investigations have a one-curl answer
instead of needing aligned-capture sampling.

---

## Decoder fixes

### Fix 1 -- continue past CRC failures (the throughput killer)

**Before** (6F.3 `process_tsdu_block`):

```rust
None => {
    self.tsbk_crc_failures += 1;
    // Cannot trust last_block when CRC fails -- stop.
    self.finalize_capture(...);
    return true;  // <-- TSDU is now done, even though TSBK2/3 follow
}
```

**After** (6F.4):

```rust
None => {
    self.tsbk_crc_failures += 1;
    self.tsbk_opcode_hist_fail[opcode_byte] += 1;
    block_failed = true;
    // ...fall through to the multi-block continuation branch
}
// ...later...
let cleanly_done = !block_failed && block_last_bit;
let max_reached = self.tsdu_blocks_decoded >= TsduDeinterleaver::MAX_BLOCKS;
if cleanly_done || max_reached { return true; }
// Otherwise extend du_expected_len and continue reading the next block.
```

This mirrors SDRTrunk's
`P25P1MessageFramer.dispatchTSBK()`:

```java
case TRUNKING_SIGNALING_BLOCK_1:
    if (tsbk1.isValid() && tsbk1.isLastBlock()) {
        mMessageAssembler = null;          // legitimate stop
    } else {
        mMessageAssembler.reconfigure(TSBK_2);  // KEEP READING
    }
```

The same logic applies to trellis failures. Only a clean
(`!block_failed`) block with `LB=1` set, or running off the end
of TSBK3, terminates the multi-block read.

**Expected throughput improvement on Clay County:** the on-target
6F.3 verification showed `tsbk_block_attempts/tsdu_attempts =
1.576`, capped because ~55% of TSBK1s failed CRC and we stopped.
With continue-past-failure, the ratio should approach 3.0 (since
SDRTrunk's reference recording shows essentially every TSDU on
this site is a 3-block frame). That's roughly a 2x increase in
TSBK block attempts and a corresponding ~2x increase in successful
TSBK decodes.

### Fix 2 -- IDEN_UPDATE 0x3D (FDMA), 0x34 (VUHF), 0x33 (TDMA)

There are THREE distinct IDEN_UPDATE opcodes in P25:

| Opcode | SDRTrunk Java class             | Bandwidth field | Offset field    |
|-------:|---------------------------------|-----------------|-----------------|
| **0x3D** | `FrequencyBandUpdate`         | 9 bits @ 20-28  | 8 bits  @ 30-37 |
| 0x34   | `FrequencyBandUpdateVUHF`       | 4 bits @ 20-23  | 13 bits @ 25-37 |
| 0x33   | `FrequencyBandUpdateTDMA`       | 4 bits ch type  | 13 bits @ 25-37 |

The 6F.3 codebase had ONE parser, mapped to opcode 0x34, with the
VUHF bit layout. Clay County (and most P25 sites in the US)
broadcasts the standard FDMA `IDEN_UPDATE` (0x3D) for FDMA bands
and `IDEN_UPDATE_TDMA` (0x33) for TDMA bands. We were never
decoding either of them.

**Phase 6F.4 changes to `p25-httpd/src/p25/tsbk.rs`:**

- `TsbkOpcode` enum split into three variants:
  `IdentifierUpdate` (0x3D), `IdentifierUpdateVuhf` (0x34),
  `IdentifierUpdateTdma` (0x33). Both VUHF and TDMA paths still
  emit `TsbkMessage::IdentifierUpdate` (one variant) since the
  downstream `FrequencyBand` consumer cares about
  `identifier/bw/offset/spacing/base_freq` regardless of source.
- Three new decoder functions: `decode_iden_update_fdma`,
  `decode_iden_update_vuhf`, `decode_iden_update_tdma`. All three
  use the new `bits()` helper which reads `n` bits from absolute
  bit position `start` in the 12-byte TSBK, MSB-first per byte
  -- matching SDRTrunk's `getMessage().getInt(int[] positions)`
  semantics exactly.
- The legacy `decode_iden_update` (which had ~80 lines of failed
  hand-rolled byte-shift extraction with TODO comments admitting
  it didn't work) was deleted.

### Fix 3 -- RFSS / SITE bit-field offsets

SDRTrunk `RFSSStatusBroadcast.java`:

| Field          | Bits  | Width |
|----------------|-------|-------|
| LRA            | 16-23 | 8     |
| (active conn)  | 27    | 1     |
| SYSTEM         | 28-39 | 12    |
| **RFSS**       | 40-47 | 8     |
| **SITE**       | 48-55 | 8     |
| FREQ_BAND      | 56-59 | 4     |
| CHANNEL_NUMBER | 60-71 | 12    |
| SYSTEM_SERVICE | 72-79 | 8     |

In our `payload[]` array (which starts at bit 16 of the TSBK):

| Field    | Index    | Notes                              |
|----------|---------|------------------------------------|
| LRA      | `[0]`    | bit 16-23                         |
| SYSTEM   | `[1..2]` | high nibble in `[1]`, rest in `[2]` |
| **RFSS** | **`[3]`** | (was reading `[2]` -- the SYSTEM low byte = 0xA0 = **160** which we kept reporting) |
| **SITE** | **`[4]`** | (was reading `[3]`) |
| Channel  | `[5..6]` | (was `[4..5]`)    |

The off-by-one shift on every field after RFSS was the bug. The
fix uses the same `bits()` helper as the IDEN parsers above, so
field offsets are now declared as absolute bit positions matching
SDRTrunk's tables 1:1.

---

## Diagnostic infrastructure additions

### `/api/tsbk_opcodes`

New endpoint that returns:

```json
{
  "tsdu_attempts":             1532,
  "tsbk_block_attempts_total": 4441,
  "blocks_per_tsdu":           2.90,
  "crc_ok_total":              2876,
  "crc_fail_total":            1565,
  "crc_ok_pct":                64.8,
  "by_position": {
    "tsbk1": { "attempts": 1532, "crc_ok": 1107, "crc_ok_pct": 72.3 },
    "tsbk2": { "attempts": 1462, "crc_ok":  931, "crc_ok_pct": 63.7 },
    "tsbk3": { "attempts": 1447, "crc_ok":  838, "crc_ok_pct": 57.9 }
  },
  "mfid_breakdown": {
    "standard_0x00": 1830,
    "motorola_0x90":  942,
    "harris_0xA4":      0,
    "other":          104
  },
  "opcodes": [
    { "opcode": "0x3D", "label": "IDEN_UPDATE",      "ok": 412, "fail": 188, "parsed": true },
    { "opcode": "0x33", "label": "IDEN_UPDATE_TDMA", "ok": 405, "fail": 195, "parsed": true },
    { "opcode": "0x3A", "label": "RFSS_STATUS_BCST", "ok": 280, "fail":  91, "parsed": true },
    ...
  ]
}
```

The illustrative numbers above are projections, not real on-target
data yet -- second-pass verification will fill them in.

This endpoint is the **definitive** answer to "what's actually on
the wire and which parsers are missing". The per-position rates
expose multi-block alignment health (if TSBK2/TSBK3 success rates
are dramatically below TSBK1, the continuation status-dibit math is
wrong). The MFID breakdown tells us how much of our traffic is
vendor proprietary (always skipped by `block.decode()`).

The opcode label table covers every OSP opcode in SDRTrunk's
`Opcode.java`, with `parsed: true/false` flagging which ones we
actually turn into a `TsbkMessage`. After 6F.4 the parsed set is:

- 0x00 GRP_V_CH_GRANT
- 0x02 GRP_V_CH_GRANT_UPDT
- 0x33 IDEN_UPDATE_TDMA  **(new)**
- 0x34 IDEN_UPDATE_VUHF  **(new)**
- 0x3A RFSS_STATUS_BCST  (offsets fixed)
- 0x3B NET_STATUS_BCAST
- 0x3C ADJ_STS_BCAST
- 0x3D IDEN_UPDATE       **(new)**

= 8 opcodes covering ~64% of non-vendor traffic on Clay County per
the SDRTrunk reference log analysis.

### `/api/recent_tsbks`

New endpoint that returns the newest 50 decoded TSBKs in the
`recent_messages` ring buffer, each tagged with its originating
TSBK1/2/3 block label and a SDRTrunk-style summary string:

```json
{
  "count": 50,
  "messages": [
    { "age_secs": 0.34, "block": "TSBK2",
      "summary": "RFSS_STATUS_BCST LRA:0 RFSS:1 SITE:1 CH:0-1593" },
    { "age_secs": 0.71, "block": "TSBK1",
      "summary": "NET_STATUS_BCAST WACN:BEE00 SYS:8A0 CH:0-1593" },
    ...
  ]
}
```

This is what the user explicitly asked for in the 6F.3 verification
session: "can we show what block the message comes from like the
sdrtrunk decoded message csv file". The block label is plumbed all
the way from `process_tsdu_block` via the new `block_idx` argument
to `handle_tsbk`.

### Per-block-position counters in the decoder state

New fields on `ControlChannelDecoder`:

- `tsbk_opcode_hist_ok: [u64; 64]` -- per-opcode CRC-OK count
- `tsbk_opcode_hist_fail: [u64; 64]` -- per-opcode CRC-FAIL count
- `tsbk_mfid_hist_ok: [u64; 4]` -- standard / Motorola / Harris / other
- `tsbk_block_attempts_by_pos: [u64; 3]` -- TSBK1/2/3 attempts
- `tsbk_crc_ok_by_pos: [u64; 3]` -- TSBK1/2/3 CRC successes

All five are populated from inside `process_tsdu_block` and exposed
via the new `/api/tsbk_opcodes` endpoint.

### `recent_messages` tuple now `(Instant, u8, TsbkMessage)`

Was `(Instant, TsbkMessage)`. The new `u8` is the block index
(0/1/2 = TSBK1/TSBK2/TSBK3). Plumbed through `handle_tsbk` and
`tsbk_to_event` so WebSocket event summaries also start with
`[TSBK1]/[TSBK2]/[TSBK3]`.

### `BUILD_TAG` bump

```rust
pub const BUILD_TAG: &str =
    "2026-04-11-phase6f.4-multiblock-continue-iden-rfss-fix-opcodehist";
```

So `/api/system` reflects exactly which build is on target -- the
6F.3 verification session burned 10 minutes confused by a
build_tag that hadn't been bumped.

---

## Tests

`cargo test p25::` = 28 tests green. Full crate test suite = 52
green. Notable test changes:

- `test_opcode_parsing` updated to assert all three IDEN_UPDATE
  variants (0x33 → TDMA, 0x34 → VUHF, 0x3D → standard FDMA).
- `test_multi_block_tsbk_e2e` payload byte layout updated -- the
  hand-crafted RFSS_STS_BCST TSBK now uses SDRTrunk-correct field
  offsets (RFSS at `payload[3]`, SITE at `payload[4]`,
  channel at `payload[5..6]`). The test caught the original
  rfss_id offset bug when 6F.4 wired the new bit-extractor in.

## Verification helper

`tools/p25_check_phase6f4.py` is a zero-dependency Python script
that hits all the diagnostic endpoints in sequence and renders a
coloured status report with explicit acceptance checks:

- Build tag is `phase6f.4`
- `rfss_id == 1` (was 160)
- `blocks_per_tsdu >= 2.5` (was 1.6)
- `bands_known >= 6`
- `WACN matches Clay County (BEE00)`

Exits 0 on all-pass, 1 on partial, 2 on connection failure. Lets
you `while sleep 30; do python tools/p25_check_phase6f4.py; done`
on the workstation while the target is decoding.

```bash
python tools/p25_check_phase6f4.py             # default 192.168.2.1:8080
python tools/p25_check_phase6f4.py fishball.local:8080
```

---

## Acceptance criteria for 6F.4 on-target verification

| Criterion | 6F.3 baseline | 6F.4 target |
|---|---:|---:|
| `blocks_per_tsdu` | 1.6 | **>= 2.5** |
| `tsbk_crc_ok` rate | 42% | similar (~40-55%) |
| `bands_known` | 0 | **>= 6** |
| `rfss_id` | 160 | **1** |
| `messages / sec` | ~1 | **~3-5** |
| `/api/recent_tsbks` shows TSBK1/2/3 labels | n/a | yes |
| `/api/tsbk_opcodes` shows real opcode mix | n/a | yes |

If any of these fail post-flash, the new `/api/tsbk_opcodes`
endpoint gives the per-position attempt + CRC rate breakdown that
will localize the bug within one round-trip.

---

## Phase 6F.5 amendment (same day) -- two follow-up fixes

After Phase 6F.4 flashed and verified ALL acceptance criteria
(blocks_per_tsdu hit 3.00, bands_known=6, rfss_id=1, WACN
populated, active_grants picked up live calls), two real bugs
surfaced in the longer 410 s diagnostic run:

### 6F.5 fix 1 -- TDMA IDEN_UPDATE offset multiplier

The 6F.4 TDMA parser used `mag * 250_000` for the transmit
offset, copying the FDMA/VUHF formula. SDRTrunk's
`FrequencyBandUpdateTDMA.getTransmitOffset()` actually does:

```java
long offset = getMessage().getLong(TRANSMIT_OFFSET) * getChannelSpacing();
```

The TDMA offset is in units of `channel_spacing`, NOT 250 kHz.
On Clay County's 12.5 kHz TDMA bands the ratio is `250000 /
12500 = 20`, so band 5 reported `-780,000,000` instead of
`-39,000,000`. Verified:

| Band | Our 6F.4 offset | SDRTrunk reference | Ratio |
|---:|---:|---:|---:|
| 2 | -900,000,000 | -45,000,000 | 20× |
| 3 | +600,000,000 | +30,000,000 | 20× |
| 5 | -780,000,000 | -39,000,000 | 20× |

Fix is one line in `decode_iden_update_tdma`:

```rust
let mut xmit_offset = (offset_mag as i32) * (spacing as i32);
```

(was `* 250_000`).

### 6F.5 fix 2 -- SYNC_THRESHOLD 4 → 8 (the throughput killer)

After 6F.4 verification we were reading 3.00 blocks/TSDU but
still only getting 3.56 TSDUs/sec versus the theoretical 13.3
TSDUs/sec at 4800 sym/s with 360-dibit TSDUs. The ~73% gap is
upstream of the TSBK pipeline, in the dibit-based hard sync
correlator.

Diagnostic comparison across all 3 decoder paths in one 410 s run:

| Path                                | Sync hits / sec |
|-------------------------------------|----------------:|
| ps_lsm @ threshold 4                |             4.6 |
| pl_hdl HDL hard correlator          |             5.1 |
| **ps_phase6d soft-decision IQ corr** |         **9.0** |
| theoretical max (4800 ÷ 360)         |            13.3 |

The Phase 6D path uses soft-decision sync correlation on raw IQ
samples and catches almost twice as many syncs. Both hard
correlators (PS dibit + PL HDL) lose ~65 % of real syncs because
the dibit slicer is still running with a residual DC bias
producing ~60 / 40 inner / outer ratio. The P25 frame-sync
pattern is all outer symbols, so the bias puts ~5-6 systematic
bit errors in every 48-bit sync window. At threshold 4 less than
half of real syncs fall within the matching radius.

The dibit dump showed:

```
sync hits = 1897 (4.63/sec)
sync near = 13831 (33.75/sec)   ← real syncs at distance 5..14
sync best dist = 24
inner/outer dibit ratio: 60.0% / 40.0% (target ~50/50)
```

13831 near-misses / second suggest the population of
"5–8 bit error syncs" is huge. Raising the threshold from 4 to 8
should catch those without increasing the false-positive rate
meaningfully:

- `P(48-bit random word within distance 8 of fixed pattern)`
  ≈ `5 × 10⁻⁶`
- At ~2400 sliding windows / sec → ~0.012 false syncs / sec
  (negligible)
- BCH(63,16,11) NID FEC catches any false-positive sync that
  does land

Expected post-flash sync rate: ~12 / sec, approaching the
13.3 / sec theoretical max. That should push us to roughly **30
TSBK blocks / sec** which is the user's stated target.

Phase 6F.2e history: dropped 10 → 4 because the LSM stream was
"much cleaner" than legacy C4FM. That was optimistic. The right
long-term fix is the HDL DC blocker (still on the DEVPLAN); 8 is
the right operating point until then.

### 6F.5 status

- `cargo test p25::` = 28 green, full crate = 52 green.
- `BUILD_TAG` bumped to
  `2026-04-11-phase6f.5-tdma-offset-fix-and-sync-threshold-8`.
- `tools/p25_check_phase6f4.py` accepts both `phase6f.4` and
  `phase6f.5` build tags.

Re-flash + re-run `python tools/p25_check_phase6f4.py` after the
build to verify the throughput jump.

---

## Open follow-ups still pending after 6F.4

These remain on the queue (none are blockers):

- **`active_grants` populates only when calls are active.** SDRTrunk
  reference shows ~12 GRP_VCH_GRANT_UPD per minute on this site,
  so we should see at least one within ~5 seconds of arming. If
  they STILL stay at 0 after 6F.4 ships, the issue is parser-side
  (likely bit offsets in `decode_grp_v_ch_grant_update`).
- **Stub recognition for SCCB (0x39), TDMA_SYNC_BCST (0x30),
  SNDCP_DCH_ANN_EX (0x16), CCH_BASE_STAT_ID** -- these account for
  ~27% of non-vendor traffic. They don't drive bands or grants but
  reducing the `unknown_opcode` bucket clears the diagnostic noise.
- **Heartbeat observability fix** (~30 lines `main.rs`, queued
  from doc 025).
- **Dashboard live activity feed** -- now feasible since
  `/api/recent_tsbks` exists. The dashboard's existing
  `/ws/events` already broadcasts these events; the dashboard
  just needs to render them.
